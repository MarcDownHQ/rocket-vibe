//! A Teams call the app carries itself (brain/architecture/native-calls.md): this
//! side speaks Teams' signalling over the account's tokens and socket, the voice
//! sidecar offers and carries the media. Audio only.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rv_voice_protocol::RemoteMember;
use serde_json::Value;

use crate::session::Session;
use crate::sync::Backend;
use crate::teams::calling::{self, CallEvent, Caller};
use crate::voice::VoiceController;
use rv_provider::rest::RestError;

/// How long a placed call rings before this side gives up, should Teams never say.
const RINGING: Duration = Duration::from_secs(90);

/// Who a call goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// This chat's people.
    Room(String),
    /// Microsoft's Test Call bot, which plays back what it hears and rings no one.
    TestCall,
}

struct Identity {
    mri: String,
    name: String,
    endpoint: String,
    participant: String,
    route: String,
    random: String,
}

impl Identity {
    fn caller(&self) -> Caller<'_> {
        Caller {
            mri: &self.mri,
            name: &self.name,
            endpoint: &self.endpoint,
            participant: &self.participant,
            surl: &self.route,
            random: &self.random,
            client: calling::CLIENT,
        }
    }
}

impl Session {
    /// Places a call on `voice`: `Ok` once Teams took it, the call then running
    /// on its own until it ends or the voice session is left. An error leaves
    /// nothing running, for the caller to open the web page instead.
    pub async fn place_teams_call(
        self: &Arc<Self>,
        voice: &VoiceController,
        target: Target,
        label: &str,
    ) -> Result<(), RestError> {
        let Backend::Teams(teams) = self.backend() else { return Err(RestError::incomplete("call: not Teams")) };
        let route = teams.call_route().ok_or_else(|| RestError::incomplete("call: socket not registered"))?;
        let (to, thread) = match &target {
            Target::TestCall => (Vec::new(), None),
            Target::Room(rid) => {
                teams.call_targets(rid).ok_or_else(|| RestError::incomplete("call: nobody to ring here"))?
            }
        };
        let identity = Identity {
            mri: self.info.user_id.clone(),
            name: self.info.username.clone(),
            endpoint: calling::random_id(),
            participant: calling::random_id(),
            route,
            random: calling::random_id(),
        };
        voice
            .connect_external(label, &identity.endpoint)
            .await
            .map_err(|e| RestError::incomplete(&format!("call: {e}")))?;
        let Some(offer) = voice.local_offer().await else {
            voice.disconnect().await;
            return Err(RestError::incomplete("call: no offer"));
        };
        let chain = calling::random_id();
        let mut pushes = self.teams_call_pushes.subscribe();
        let body = calling::place_call(
            &identity.caller(),
            &to,
            thread.as_deref(),
            &calling::teams_offer(&offer),
            &calling::random_id(),
        );
        let created = match teams.service.calling_post(calling::EPCONV, &chain, &body).await {
            Ok(created) => created,
            Err(e) => {
                voice.disconnect().await;
                return Err(e);
            }
        };
        let session = self.clone();
        let voice = voice.clone();
        tokio::spawn(async move {
            let links = created.get("links").cloned().unwrap_or(Value::Null);
            let test = target == Target::TestCall;
            drive(&session, &voice, &identity, &chain, &links, test, &mut pushes).await;
        });
        Ok(())
    }
}

async fn drive(
    session: &Arc<Session>,
    voice: &VoiceController,
    identity: &Identity,
    chain: &str,
    links: &Value,
    test: bool,
    pushes: &mut tokio::sync::broadcast::Receiver<Value>,
) {
    let Backend::Teams(teams) = session.backend() else { return };
    let mut changes = voice.changes();
    let mut roster: BTreeMap<String, String> = BTreeMap::new();
    let mut answered = false;
    let ringing = tokio::time::sleep(RINGING);
    tokio::pin!(ringing);
    let leave = loop {
        tokio::select! {
            _ = &mut ringing, if !answered => {
                voice.remote_ended("no_answer").await;
                break true;
            }
            change = changes.recv() => {
                if change.is_err() || voice.snapshot().ended.is_some() {
                    break true;
                }
            }
            push = pushes.recv() => {
                let envelope = match push {
                    Ok(envelope) => envelope,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break true,
                };
                if !calling::is_call_callback(&envelope, &identity.endpoint) {
                    continue;
                }
                match calling::call_event(&envelope) {
                    CallEvent::Accepted { sdp, acknowledge, .. } if !answered => {
                        let Some(answer) = calling::webrtc_answer(&sdp) else {
                            voice.remote_ended("connect_failed").await;
                            break true;
                        };
                        answered = true;
                        voice.remote_answer(&answer).await;
                        if let Some(link) = acknowledge {
                            let body = calling::acknowledge_acceptance(&identity.caller());
                            let _ = teams.service.calling_post(&link, chain, &body).await;
                        }
                        if test && let Some(add) = links.get("addParticipant").and_then(Value::as_str) {
                            let body = calling::invite_test_bot(&identity.caller(), &calling::random_id());
                            let _ = teams.service.calling_post(add, chain, &body).await;
                        }
                    }
                    CallEvent::Accepted { .. } | CallEvent::Other => {}
                    CallEvent::Roster(changed) => {
                        for change in changed {
                            if change.mri.eq_ignore_ascii_case(&identity.mri) {
                                continue;
                            }
                            if change.present {
                                roster.insert(change.mri, change.name);
                            } else {
                                roster.remove(&change.mri);
                            }
                        }
                        let members = roster
                            .iter()
                            .map(|(id, name)| RemoteMember { id: id.clone(), name: name.clone(), muted: false })
                            .collect();
                        voice.remote_roster(members).await;
                    }
                    CallEvent::Ended { .. } => {
                        voice.remote_ended("remote_ended").await;
                        break false;
                    }
                }
            }
        }
    };
    if leave && let Some(link) = links.get("leave").and_then(Value::as_str) {
        let _ = teams.service.calling_post(link, chain, &calling::leave_call(&identity.caller())).await;
    }
}
