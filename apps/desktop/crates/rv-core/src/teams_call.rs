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
/// How long after the answer a conversation's end push is believed.
const SETTLED: Duration = Duration::from_secs(5);

/// Who a call goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// This chat's people.
    Room(String),
    /// These accounts directly, by MRI, with no chat of this client's to go through
    /// (someone of another tenant, whom its people search does not find).
    People(Vec<String>),
    /// Microsoft's Test Call bot, which plays back what it hears and rings no one.
    TestCall,
    /// The call ringing in this chat, answered here.
    Incoming(String),
}

struct Identity {
    mri: String,
    name: String,
    endpoint: String,
    participant: String,
    route: String,
    random: String,
    /// The `x-microsoft-skype-chain-id` of every request of this call.
    chain: String,
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
        if let Target::Incoming(rid) = &target {
            return self.answer_teams_call(voice, rid, route, label).await;
        }
        let (to, thread) = match &target {
            Target::Incoming(_) => unreachable!("answered above"),
            Target::TestCall => (Vec::new(), None),
            Target::People(people) => (people.clone(), None),
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
            chain: calling::random_id(),
        };
        voice
            .connect_external(label, &identity.endpoint)
            .await
            .map_err(|e| RestError::incomplete(&format!("call: {e}")))?;
        let Some(offer) = voice.local_offer().await else {
            voice.disconnect().await;
            return Err(RestError::incomplete("call: no offer"));
        };
        let chain = identity.chain.clone();
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
            let test = target == Target::TestCall;
            drive(&session, &voice, &identity, &created, test, None, &mut pushes).await;
        });
        Ok(())
    }
}

/// A pushed document's shape, for `RV_CALL_DEBUG=1`: no content but its numbers and
/// the words that name what it is.
fn shape(v: &Value, depth: usize) -> String {
    match v {
        Value::Object(o) if depth > 0 => {
            let parts: Vec<String> = o
                .iter()
                .map(|(k, c)| match c.as_str() {
                    Some(word) if matches!(k.as_str(), "type" | "phrase" | "reason" | "status") => {
                        format!("{k}: {word}")
                    }
                    _ => format!("{k}: {}", shape(c, depth - 1)),
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Object(o) => format!("{{{}}}", o.len()),
        Value::Array(a) => match a.first() {
            Some(first) if depth > 0 => format!("[{}: {}]", a.len(), shape(first, depth - 1)),
            _ => format!("[{}]", a.len()),
        },
        Value::String(s) => format!("s{}", s.len()),
        other => other.to_string(),
    }
}

impl Session {
    /// Answers the call ringing in `rid` on `voice`: attaches this endpoint to it,
    /// answers its offer through the sidecar and accepts it; then the call runs as a
    /// placed one does. An error leaves nothing running.
    async fn answer_teams_call(
        self: &Arc<Self>,
        voice: &VoiceController,
        rid: &str,
        route: String,
        label: &str,
    ) -> Result<(), RestError> {
        let Backend::Teams(teams) = self.backend() else { return Err(RestError::incomplete("call: not Teams")) };
        let (call_id, _caller, answer) = self
            .teams_answerable
            .lock()
            .unwrap()
            .remove(rid)
            .ok_or_else(|| RestError::incomplete("call: nothing rings here"))?;
        let identity = Identity {
            mri: self.info.user_id.clone(),
            name: self.info.username.clone(),
            endpoint: calling::random_id(),
            participant: calling::random_id(),
            route,
            random: calling::random_id(),
            chain: calling::random_id(),
        };
        let chain = identity.chain.clone();
        let mut pushes = self.teams_call_pushes.subscribe();
        let attached = teams
            .service
            .calling_post(&answer.attach, &chain, &calling::attach_call(&identity.caller(), &answer.conversation))
            .await?;
        let created =
            calling::attached(&attached).ok_or_else(|| RestError::incomplete("call: attach answered nothing"))?;
        let acceptance = created
            .pointer("/links/acceptance")
            .and_then(Value::as_str)
            .ok_or_else(|| RestError::incomplete("call: no acceptance link"))?
            .to_owned();
        let Some(plain) = calling::webrtc_answer(&answer.offer) else {
            return Err(RestError::incomplete("call: an offer without audio"));
        };
        voice
            .connect_external_answering(label, &identity.endpoint)
            .await
            .map_err(|e| RestError::incomplete(&format!("call: {e}")))?;
        let Some(local) = voice.answer_remote_offer(&plain).await else {
            voice.disconnect().await;
            return Err(RestError::incomplete("call: no answer"));
        };
        let body = calling::accept_call(
            &identity.caller(),
            &calling::teams_answer(&local, &answer.offer),
            answer.media_leg.as_deref(),
        );
        if let Err(e) = teams.service.calling_post(&acceptance, &chain, &body).await {
            voice.disconnect().await;
            return Err(e);
        }
        if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1") {
            eprintln!("teams call: answered {call_id}");
        }
        self.dismiss_call();
        let mid = calling::audio_mid(&plain).map(str::to_owned);
        let session = self.clone();
        let voice = voice.clone();
        tokio::spawn(async move {
            drive(&session, &voice, &identity, &created, false, mid, &mut pushes).await;
        });
        Ok(())
    }
}

/// `RV_CALL_DEBUG=1`: the shape of a ring: where its offer and links are.
pub(crate) fn debug_ring(envelope: &Value) {
    if std::env::var("RV_CALL_DEBUG").as_deref() != Ok("1") {
        return;
    }
    let body = calling::pushed_body(envelope);
    eprintln!("teams call: ring push {}", shape(&body, 1));
    for key in ["gp", "cp"] {
        if let Some(inner) = body.get(key).and_then(Value::as_str).and_then(calling::decode_push_part) {
            eprintln!("teams call: ring {key} {}", shape(&inner, 5));
        }
    }
}

/// A signalling step's failure, said: the call may die of it much later.
fn report(step: &str, outcome: Result<(), RestError>) {
    if let Err(e) = outcome {
        eprintln!("Teams call: {step}: {}", e.message);
    } else if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1") {
        eprintln!("teams call: {step} ok");
    }
}

/// Keeps the conversation's broker subscription, for as long as the call runs.
async fn hold_subscription(session: Arc<Session>, chain: String, first: String) {
    let Backend::Teams(teams) = session.backend() else { return };
    let mut url = first;
    let mut failures = 0u32;
    loop {
        match teams.service.calling_poll(&url, &chain).await {
            Ok(answer) => {
                failures = 0;
                if let Some(next) = calling::next_subscription(&answer) {
                    url = next;
                }
                if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1") {
                    eprintln!("teams call: broker answered {}", shape(&answer, 6));
                    if let Some(message) = answer.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()) {
                        let inner = serde_json::from_str(message).unwrap_or(Value::Null);
                        eprintln!("teams call: broker message {}", shape(&inner, 6));
                    }
                }
            }
            Err(e) => {
                failures += 1;
                eprintln!("Teams call: broker: {}", e.message);
                if failures > 5 {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// `RV_CALL_DEBUG=1`: what Teams pushes on a call, without its content.
fn debug(envelope: &Value, event: &CallEvent) {
    if std::env::var("RV_CALL_DEBUG").as_deref() != Ok("1") {
        return;
    }
    let url = envelope.get("url").and_then(Value::as_str).unwrap_or("");
    let path = url.split("callAgent/").nth(1).and_then(|t| t.splitn(3, '/').nth(2)).unwrap_or("");
    let body = calling::pushed_body(envelope);
    let keys: Vec<&str> = body.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    let kind = match event {
        CallEvent::Accepted { .. } => "accepted",
        CallEvent::Ended { .. } => "ended",
        CallEvent::Negotiation { .. } => "negotiation",
        CallEvent::Roster(_) => "roster",
        CallEvent::Other => "other",
    };
    eprintln!("teams call: push {path} -> {kind} {keys:?}");
}

async fn drive(
    session: &Arc<Session>,
    voice: &VoiceController,
    identity: &Identity,
    created: &Value,
    test: bool,
    // The audio section's mid of a call answered here, accepted already.
    accepted: Option<String>,
    pushes: &mut tokio::sync::broadcast::Receiver<Value>,
) {
    let Backend::Teams(teams) = session.backend() else { return };
    let chain = identity.chain.as_str();
    let links = created.get("links").unwrap_or(&Value::Null);
    let conversation = created.get("conversationController").and_then(Value::as_str).unwrap_or("");
    let mut changes = voice.changes();
    let mut roster: BTreeMap<String, String> = BTreeMap::new();
    let mut answered = accepted.is_some();
    let mut answered_at = answered.then(tokio::time::Instant::now);
    let mut session_mid: Option<String> = accepted;
    // Teams' conversation holds this client through this long poll's 30 s lease.
    let broker = links.get("subscribe").and_then(Value::as_str).map(|first| {
        let (session, chain, first) = (session.clone(), chain.to_owned(), first.to_owned());
        tokio::spawn(async move { hold_subscription(session, chain, first).await })
    });
    if let Some(link) = links.get("updateEndpointMetadata").and_then(Value::as_str) {
        report(
            "endpoint metadata",
            teams.service.calling_put(link, chain, &calling::endpoint_metadata(&identity.caller())).await,
        );
    }
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
                // The push that follows an answer is not yet known to be 115 rather than 116.
                if answered_at.is_some_and(|at| at.elapsed() > SETTLED)
                    && crate::teams::frames::conversation_over(&envelope)
                        .is_some_and(|over| calling::same_conversation(&over, conversation))
                {
                    voice.remote_ended("remote_ended").await;
                    break false;
                }
                if !calling::is_call_callback(&envelope, &identity.endpoint) {
                    continue;
                }
                let event = calling::call_event(&envelope);
                debug(&envelope, &event);
                match event {
                    CallEvent::Negotiation { sdp, answer_link, media_leg } => {
                        let debugging = std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1");
                        if debugging {
                            eprintln!("teams call: new offer {}", calling::sdp_sections(&sdp));
                        }
                        let mid = session_mid.as_deref().or_else(|| calling::audio_mid(&sdp)).unwrap_or("0");
                        let Some(plain) = calling::renegotiation_offer(&sdp, mid) else {
                            eprintln!("Teams call: a new offer without audio");
                            continue;
                        };
                        let Some(answer) = voice.answer_remote_offer(&plain).await else {
                            eprintln!("Teams call: a new offer went unanswered");
                            continue;
                        };
                        let answer = calling::teams_answer(&answer, &sdp);
                        if debugging {
                            eprintln!("teams call: answering {}", calling::sdp_sections(&answer));
                        }
                        let body = calling::answer_negotiation(&answer, media_leg.as_deref());
                        report("new offer's answer", teams.service.calling_post(&answer_link, chain, &body).await.map(|_| ()));
                    }
                    CallEvent::Accepted { sdp, acknowledge, call_leg, .. } if !answered => {
                        let Some(answer) = calling::webrtc_answer(&sdp) else {
                            voice.remote_ended("connect_failed").await;
                            break true;
                        };
                        answered = true;
                        answered_at = Some(tokio::time::Instant::now());
                        session_mid = calling::audio_mid(&answer).map(str::to_owned);
                        voice.remote_answer(&answer).await;
                        if let Some(link) = acknowledge {
                            let body = calling::acknowledge_acceptance(&identity.caller());
                            report("acknowledgement", teams.service.calling_post(&link, chain, &body).await.map(|_| ()));
                        }
                        if let Some(link) = call_leg {
                            let body = calling::register_on_leg(&identity.caller());
                            report("call leg", teams.service.calling_post(&link, chain, &body).await.map(|_| ()));
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
    if let Some(broker) = broker {
        broker.abort();
    }
    if leave && let Some(link) = links.get("leave").and_then(Value::as_str) {
        let _ = teams.service.calling_post(link, chain, &calling::leave_call(&identity.caller())).await;
    }
}
