//! Teams' calling stack (brain/architecture/native-calls.md), the parts with no
//! network: what its pushes carry and what its requests take; its SDP dialect is
//! in [`crate::sdp`].

use std::io::Read;

use base64::Engine;
use serde_json::{Value, json};

/// Microsoft's Test Call bot: it plays back what it hears, and rings no one.
pub const ECHO_BOT: &str = "cf28171e-fcfd-47e4-a1d6-79460b0b3ca0";

/// Where a call is placed.
pub const EPCONV: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/epconv";

/// How this client presents itself to the calling stack.
pub const CLIENT: &str = "SkypeSpaces/1415/26100212018/os=linux; osVer=undefined; deviceType=computer; browser=chrome";

/// `mediaAnswer`'s body answering a [`CallEvent::Negotiation`].
pub fn answer_negotiation(answer: &str, media_leg: Option<&str>) -> Value {
    json!({"mediaAnswer": {
        "callModalities": ["Audio"],
        "mediaContent": {
            "blob": answer,
            "contentType": "application/sdp-ngc-1.0",
            "mediaLegId": media_leg,
            "escalationOccurring": false,
            "newOffer": false,
        },
    }})
}

/// What a push to a call's callback means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEvent {
    /// The other side took the call: its SDP, and the link acknowledging it.
    Accepted {
        sdp: String,
        acknowledge: Option<String>,
        call_leg: Option<String>,
        keep_alive_seconds: Option<u64>,
    },
    /// The call is over, with Teams' own wording when it gives one.
    Ended {
        phrase: Option<String>,
    },
    /// Teams offers anew on the running call (the other side held or resumed it, or
    /// it moved): answer on `answer_link`, or the media stops.
    Negotiation {
        sdp: String,
        answer_link: String,
        media_leg: Option<String>,
    },
    /// Who is in the call now, as a delta: an account with no endpoint left.
    Roster(Vec<RosterChange>),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterChange {
    pub mri: String,
    pub name: String,
    pub present: bool,
}

fn roster_changes(body: &Value) -> Option<Vec<RosterChange>> {
    let participants = body.get("participants")?.as_object()?;
    Some(
        participants
            .iter()
            .map(|(mri, entry)| RosterChange {
                mri: mri.clone(),
                name: entry.pointer("/details/displayName").and_then(Value::as_str).unwrap_or_default().to_owned(),
                present: entry.get("endpoints").and_then(Value::as_object).is_some_and(|e| !e.is_empty()),
            })
            .collect(),
    )
}

/// A pushed request's JSON body, unwrapped from its gzip when it has one.
pub fn pushed_body(envelope: &Value) -> Value {
    let raw = envelope.get("body").cloned().unwrap_or(Value::Null);
    let Value::String(text) = raw else { return raw };
    let gzip = envelope
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("X-Microsoft-Skype-Content-Encoding")))
        .and_then(|(_, v)| v.as_str())
        == Some("gzip");
    let text = if gzip { gunzip(&text).unwrap_or(text) } else { text };
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// A ring's `gp` (base64 JSON) or `cp` (base64 gzip JSON).
pub fn decode_push_part(encoded: &str) -> Option<Value> {
    if let Some(text) = gunzip(encoded) {
        return serde_json::from_str(&text).ok();
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let plain =
        engine.decode(encoded).or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)).ok()?;
    serde_json::from_slice(&plain).ok()
}

fn gunzip(encoded: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let mut out = String::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_string(&mut out).ok()?;
    Some(out)
}

/// Whether a pushed request is for a call this endpoint placed.
pub fn is_call_callback(envelope: &Value, endpoint: &str) -> bool {
    envelope.get("url").and_then(Value::as_str).is_some_and(|u| u.contains(&format!("callAgent/{endpoint}/")))
}

pub fn call_event(envelope: &Value) -> CallEvent {
    let body = pushed_body(envelope);
    if let Some(accepted) = body.get("callAcceptance").or_else(|| body.get("mediaAnswer"))
        && let Some(sdp) = accepted.pointer("/mediaContent/blob").and_then(Value::as_str)
    {
        return CallEvent::Accepted {
            sdp: sdp.to_owned(),
            acknowledge: accepted.pointer("/links/acknowledgement").and_then(Value::as_str).map(str::to_owned),
            call_leg: accepted.pointer("/links/callLeg").and_then(Value::as_str).map(str::to_owned),
            keep_alive_seconds: accepted.get("callKeepAliveInterval").and_then(Value::as_u64),
        };
    }
    if let Some(negotiation) = body.get("mediaNegotiation")
        && let (Some(sdp), Some(link)) = (
            negotiation.pointer("/mediaContent/blob").and_then(Value::as_str),
            negotiation.pointer("/links/mediaAnswer").and_then(Value::as_str),
        )
    {
        return CallEvent::Negotiation {
            sdp: sdp.to_owned(),
            answer_link: link.to_owned(),
            media_leg: negotiation.pointer("/mediaContent/mediaLegId").and_then(Value::as_str).map(str::to_owned),
        };
    }
    if envelope.get("url").and_then(Value::as_str).is_some_and(|u| u.contains("/rosterUpdate"))
        && let Some(changes) = roster_changes(&body)
    {
        return CallEvent::Roster(changes);
    }
    for key in ["callEnd", "conversationEnd", "sessionRejection"] {
        if let Some(end) = body.get(key) {
            return CallEvent::Ended { phrase: end.get("phrase").and_then(Value::as_str).map(str::to_owned) };
        }
    }
    CallEvent::Other
}

/// Whether two links name the same conversation (`.../conv/<id>/...`), whichever
/// proxy or query they went through.
pub fn same_conversation(a: &str, b: &str) -> bool {
    let id = |url: &str| url.split("/conv/").nth(1).and_then(|t| t.split(['/', '?']).next().map(str::to_owned));
    id(a).is_some() && id(a) == id(b)
}

/// A version 4 UUID, as the calling stack's ids are.
pub fn random_id() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Who places a call and from where.
#[derive(Debug, Clone)]
pub struct Caller<'a> {
    pub mri: &'a str,
    pub name: &'a str,
    pub endpoint: &'a str,
    pub participant: &'a str,
    pub surl: &'a str,
    pub random: &'a str,
    pub client: &'a str,
}

impl Caller<'_> {
    fn callback(&self, path: &str) -> String {
        format!("{}callAgent/{}/{}/{path}", self.surl, self.endpoint, self.random)
    }

    fn from(&self) -> Value {
        json!({"id": self.mri, "displayName": self.name, "endpointId": self.endpoint,
            "participantId": self.participant, "languageId": "en-us"})
    }
}

impl Caller<'_> {
    /// The conversation's own callbacks, as a call's request names them.
    fn conversation_request(&self, conversation_type: Option<&str>, suppress_dialout: bool) -> Value {
        let cb = |path: &str| self.callback(path);
        json!({
            "conversationType": conversation_type, "subject": null, "suppressDialout": suppress_dialout,
            "roster": {"type": "Delta", "rosterUpdate": cb("conversation/rosterUpdate/")},
            "properties": {"allowConversationWithoutHost": true, "enableGroupCallEventMessages": true,
                "enableGroupCallUpgradeMessage": false, "enableGroupCallMeetupGeneration": false},
            "links": {
                "conversationEnd": cb("conversation/conversationEnd/"),
                "conversationUpdate": cb("conversation/conversationUpdate/"),
                "localParticipantUpdate": cb("conversation/localParticipantUpdate/"),
                "addParticipantSuccess": cb("conversation/addParticipantSuccess/"),
                "addParticipantFailure": cb("conversation/addParticipantFailure/"),
                "addModalitySuccess": cb("conversation/addModalitySuccess/"),
                "addModalityFailure": cb("conversation/addModalityFailure/"),
                "confirmUnmute": cb("conversation/confirmUnmute/"),
                "receiveMessage": cb("conversation/receiveMessage/"),
            },
        })
    }

    /// The audio call this offer invites, with this client's callbacks for it.
    fn call_invitation(&self, offer: &str) -> Value {
        let cb = |path: &str| self.callback(path);
        json!({
            "callModalities": ["Audio"], "replaces": null, "transferor": null,
            "links": {"progress": cb("call/progress/"), "mediaAnswer": cb("call/mediaAnswer/"),
                "acceptance": cb("call/acceptance/"), "redirection": cb("call/redirection/"), "end": cb("call/end/")},
            "clientContentForMediaController": {"controlVideoStreaming": cb("call/controlVideoStreaming/"),
                "csrcInfo": cb("call/csrcInfo/")},
            "pstnContent": {"emergencyCallCountry": "", "platformName": self.client, "publicApiCall": false},
            "mediaContent": {"blob": offer, "contentType": "application/sdp-ngc-1.0", "requiredFeatures": "nonByPass"},
        })
    }
}

fn endpoint_state(sequence: u32) -> Value {
    json!({"endpointStateSequenceNumber": sequence, "endpointProperties":
        {"additionalEndpointProperties": {"infoShownInReportMode": "FullInformation"}}})
}

/// `epconv`'s body placing a call with this offer: to `to`'s accounts, or, with
/// none, Microsoft's Test Call.
pub fn place_call(caller: &Caller, to: &[String], thread: Option<&str>, offer: &str, modality_id: &str) -> Value {
    let test = to.is_empty();
    let oid = caller.mri.strip_prefix("8:orgid:").unwrap_or(caller.mri);
    let group_chat = match thread {
        _ if test => json!({"threadId": format!("19:{oid}_{ECHO_BOT}@unq.gbl.spaces"), "messageId": null}),
        Some(thread) => json!({"threadId": thread, "messageId": null}),
        None => Value::Null,
    };
    let targets: Vec<Value> = to.iter().map(|mri| json!({"id": mri, "participantId": modality_id})).collect();
    let mut body = json!({
        "conversationRequest": caller.conversation_request(None, test),
        "groupContext": null,
        "groupChat": group_chat,
        "participants": {"from": caller.from(), "to": targets},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "endpointMetadata": {"holographicCapabilities": 3},
        "meetingInfo": null,
        "endpointState": endpoint_state(1),
        "callInvitation": caller.call_invitation(offer),
    });
    if test {
        body["scenario"] = json!("UserInitiatedTestCall");
    }
    body
}

/// `epconv`'s body starting a channel's call, as Teams' Meet in a channel does:
/// a call of the channel's thread that rings no one, its people joining from
/// the channel.
pub fn start_channel_call(caller: &Caller, thread: &str, offer: &str) -> Value {
    json!({
        "conversationRequest": caller.conversation_request(Some("huddleGroupCall"), true),
        "groupContext": null,
        "groupChat": {"threadId": thread, "messageId": null},
        "participants": {"from": caller.from(), "to": []},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "endpointMetadata": {"holographicCapabilities": 3},
        "meetingInfo": null,
        "endpointState": endpoint_state(1),
        "callInvitation": caller.call_invitation(offer),
    })
}

/// A meeting's call, or a channel's, as the conversation service finds it: the
/// thread, the message it hangs off (`0` for the thread's own) and its organizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingJoin {
    pub thread: String,
    pub message: String,
    pub organizer: String,
    pub tenant: String,
}

/// The call to join in a thread, from its threads document (`threadProperties=1`):
/// the one running (`awareness_conversationLiveState:*`, `Active`), else the
/// thread's meeting (`meeting`); `None` when it has neither.
pub fn meeting_join(doc: &Value, thread: &str, tenant: &str) -> Option<MeetingJoin> {
    let properties = doc.get("properties").or_else(|| doc.get("threadProperties"))?.as_object()?;
    let parsed = |v: &Value| match v {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        Value::Object(_) => Some(v.clone()),
        _ => None,
    };
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let live = properties
        .iter()
        .filter(|(key, _)| key.starts_with("awareness_conversationLiveState"))
        .filter_map(|(_, v)| parsed(v))
        .find(|state| state.get("status").and_then(Value::as_str) == Some("Active"));
    let meeting = properties.get("meeting").and_then(parsed);
    let (organizer, tenant_id, message) = match (&live, &meeting) {
        (Some(live), _) => {
            let info = live.get("meetingInfo").unwrap_or(&Value::Null);
            let initiator = || {
                live.get("groupCallInitiator")
                    .and_then(Value::as_str)
                    .and_then(|m| m.strip_prefix("8:orgid:"))
                    .map(str::to_owned)
            };
            (text(info, "organizerId").or_else(initiator)?, text(info, "tenantId"), None)
        }
        (None, Some(meeting)) => (
            text(meeting, "organizerId")?,
            text(meeting, "tenantId"),
            meeting.get("meetingJoinUrl").and_then(Value::as_str).and_then(join_message),
        ),
        (None, None) => return None,
    };
    Some(MeetingJoin {
        thread: thread.to_owned(),
        message: message.unwrap_or_else(|| "0".to_owned()),
        organizer,
        tenant: tenant_id.unwrap_or_else(|| tenant.to_owned()),
    })
}

/// The message a meeting link joins (`/l/meetup-join/<thread>/<message>`).
fn join_message(link: &str) -> Option<String> {
    let url = url::Url::parse(link).ok()?;
    let mut segments = url.path_segments()?;
    (segments.next()? == "l" && segments.next()? == "meetup-join").then_some(())?;
    segments.nth(1).filter(|m| !m.is_empty()).map(str::to_owned)
}

/// `epconv`'s body resolving a meeting's conversation, before its call is joined.
pub fn join_meeting(caller: &Caller, join: &MeetingJoin) -> Value {
    let mut request = caller.conversation_request(None, false);
    if let Some(request) = request.as_object_mut() {
        request.remove("conversationType");
        request.remove("suppressDialout");
    }
    json!({
        "conversationRequest": request,
        "groupContext": null,
        "groupChat": {"threadId": join.thread, "messageId": join.message},
        "participants": {"from": caller.from()},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "endpointMetadata": {"holographicCapabilities": 3},
        "meetingInfo": {"tenantId": join.tenant, "organizerId": join.organizer},
        "endpointState": endpoint_state(0),
    })
}

/// The conversation controller's body joining the meeting's call with this offer.
pub fn join_meeting_call(caller: &Caller, join: &MeetingJoin, offer: &str) -> Value {
    json!({
        "conversationRequest": caller.conversation_request(None, true),
        "contentSharing": null,
        "participants": {"from": caller.from(), "to": []},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "endpointMetadata": {"holographicCapabilities": 3},
        "groupContext": null,
        "groupChat": {"threadId": join.thread, "messageId": join.message},
        "meetingInfo": {"organizerId": join.organizer, "tenantId": join.tenant},
        "endpointState": endpoint_state(1),
        "callInvitation": caller.call_invitation(offer),
    })
}

/// The acknowledgement a `callAcceptance` waits for: without it the call
/// controller ends the call (430). Names this client's callbacks for the call leg.
pub fn acknowledge_acceptance(caller: &Caller) -> Value {
    let cb = |path: &str| caller.callback(path);
    json!({"callAcceptanceAcknowledgement": {
        "links": {
            "acknowledgement": cb("call/acknowledgement/"),
            "mediaRenegotiation": cb("call/mediaRenegotiation/"),
            "replacement": cb("call/replacement/"),
            "progress": cb("call/progress/"),
            "mediaAnswer": cb("call/mediaAnswer/"),
            "newMediaOffer": cb("call/newMediaOffer/"),
            "redirection": cb("call/redirection/"),
            "balanceUpdate": cb("call/balanceUpdate/"),
            "acceptance": cb("call/acceptance/"),
            "controlVideoStreaming": cb("call/controlVideoStreaming/"),
            "csrcInfo": cb("call/csrcInfo/"),
            "end": cb("call/end/"),
            "retargetCompletion": cb("call/retargetCompletion/"),
            "transfer": cb("call/transfer/"),
            "transferAcceptance": cb("call/transferAcceptance/"),
            "transferCompletion": cb("call/transferCompletion/"),
            "holdCompletion": cb("call/holdCompletion/"),
            "resumeCompletion": cb("call/resumeCompletion/"),
        },
    }})
}

/// `addParticipant`'s body inviting Microsoft's Test Call bot, the only one a Test
/// Call has: it is invited once the media is up, and rings no one.
pub fn invite_test_bot(caller: &Caller, participant: &str) -> Value {
    let oid = caller.mri.strip_prefix("8:orgid:").unwrap_or(caller.mri);
    json!({
        "disableUnmute": false,
        "participants": {
            "from": caller.from(),
            "to": [{"id": format!("28:{ECHO_BOT}"), "participantId": participant}],
        },
        "participantInvitationData": {},
        "replacementDetails": null,
        "groupContext": null,
        "groupChat": {"threadId": format!("19:{oid}_{ECHO_BOT}@unq.gbl.spaces"), "messageId": null},
        "links": {
            "addParticipantSuccess": caller.callback("conversation/addParticipantSuccess/"),
            "addParticipantFailure": caller.callback("conversation/addParticipantFailure/"),
        },
    })
}

/// The call leg's registration of this client's callbacks, where the call controller
/// sends what follows the acceptance.
pub fn register_on_leg(caller: &Caller) -> Value {
    let mut body = acknowledge_acceptance(caller);
    body["callParticipantUpdate"] = body["callAcceptanceAcknowledgement"].clone();
    body
}

/// `updateEndpointMetadata`'s body, as Teams' web client sends it once placed.
pub fn endpoint_metadata(caller: &Caller) -> Value {
    json!({"participants": {"from": caller.from()}, "endpointMetadata": {"holographicCapabilities": 3}})
}

/// The next long poll of the conversation's broker, from an answer: its lease is 30 s
/// (`expirationTimeInSec`), and a client that lets it lapse loses the call.
pub fn next_subscription(answer: &Value) -> Option<String> {
    answer.get("nextSubscribeUrl").and_then(Value::as_str).filter(|u| u.starts_with("https://")).map(str::to_owned)
}

/// `attach`'s body, answering a ring here: this endpoint's callbacks on the call,
/// and the join of its conversation (as Teams' web client sends them).
pub fn attach_call(caller: &Caller, conversation: &str) -> Value {
    let cb = |path: &str| caller.callback(path);
    json!({
        "attach": {"requireMediaContent": false, "links": {"end": cb("call/end/")},
            "locationContent": null, "networkContent": null, "areaContent": null},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "additionalActions": [{
            "name": "join",
            "url": conversation,
            "waitForResponse": true,
            "input": {
                "capabilities": null,
                "endpointCapabilities": 73463,
                "conversationRequest": {
                    "roster": {"type": "Delta", "rosterUpdate": cb("conversation/rosterUpdate/")},
                    "links": {
                        "conversationEnd": cb("conversation/conversationEnd/"),
                        "conversationUpdate": cb("conversation/conversationUpdate/"),
                        "localParticipantUpdate": cb("conversation/localParticipantUpdate/"),
                        "addParticipantSuccess": cb("conversation/addParticipantSuccess/"),
                        "addParticipantFailure": cb("conversation/addParticipantFailure/"),
                        "receiveMessage": cb("conversation/receiveMessage/"),
                    },
                },
                "endpointMetadata": {},
                "participants": {"from": caller.from()},
            },
        }],
    })
}

/// What an `attach` answered: the call's links (`acceptance`, `subscribe`) and the
/// conversation's (`leave`, `updateEndpointMetadata`), as one `links` object the
/// call's run reads like a placed call's, with the conversation's controller.
pub fn attached(answer: &Value) -> Option<Value> {
    let invitation = answer.pointer("/callInvitation/links")?.as_object()?;
    let joined = answer.pointer("/additionalActionResponses/0/output")?;
    let mut links = joined.get("links").and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, link) in invitation {
        links.entry(key.clone()).or_insert_with(|| link.clone());
    }
    Some(
        json!({"links": links, "conversationController": joined.get("conversationController").cloned().unwrap_or(Value::Null)}),
    )
}

/// `acceptance`'s body: the answer in Teams' dialect, under the offer's media leg.
pub fn accept_call(caller: &Caller, answer: &str, media_leg: Option<&str>) -> Value {
    let cb = |path: &str| caller.callback(path);
    json!({"callAcceptance": {
        "acceptedBy": caller.from(),
        "acceptedCallModalities": ["Audio"],
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "links": {
            "mediaRenegotiation": cb("call/mediaRenegotiation/"),
            "transfer": cb("call/transfer/"),
            "replacement": cb("call/replacement/"),
            "balanceUpdate": cb("call/balanceUpdate/"),
            "retargetCompletion": cb("call/retargetCompletion/"),
            "controlVideoStreaming": cb("call/controlVideoStreaming/"),
            "updateMediaDescriptions": cb("call/updateMediaDescriptions"),
        },
        "clientContentForMediaController": {
            "controlVideoStreaming": cb("call/controlVideoStreaming/"),
            "csrcInfo": cb("call/csrcInfo/"),
        },
        "mediaContent": {"blob": answer, "contentType": "application/sdp-ngc-1.0", "mediaLegId": media_leg},
        "pstnContent": {"emergencyCallCountry": "", "platformName": caller.client, "publicApiCall": false},
        "callKeepAliveInterval": null,
    }})
}

/// `leave`'s body.
pub fn leave_call(caller: &Caller) -> Value {
    json!({
        "participants": {"from": caller.from()},
        "conversationTransactionEnd": {"reason": "noError", "code": 0, "phrase": "ConversationEndNoModalityConnected"},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answering_a_ring_attaches_then_accepts_under_its_media_leg() {
        let caller = Caller {
            mri: "8:orgid:me",
            name: "Me",
            endpoint: "ep",
            participant: "p",
            surl: "https://t/",
            random: "r",
            client: CLIENT,
        };
        let attach = attach_call(&caller, "https://conv/c1");
        assert_eq!(attach["additionalActions"][0]["url"], "https://conv/c1");
        assert_eq!(attach["attach"]["links"]["end"], "https://t/callAgent/ep/r/call/end/");
        let answer = json!({
            "callInvitation": {"links": {"acceptance": "https://cc/accept", "subscribe": "https://b/1", "leave": "not this"}},
            "additionalActionResponses": [{"output": {"conversationController": "https://conv/c1",
                "links": {"leave": "https://conv/leave", "updateEndpointMetadata": "https://conv/meta"}}}],
        });
        let links = attached(&answer).unwrap();
        assert_eq!(links["links"]["acceptance"], "https://cc/accept");
        assert_eq!(links["links"]["subscribe"], "https://b/1");
        assert_eq!(links["links"]["leave"], "https://conv/leave", "the conversation's own leave");
        assert_eq!(links["conversationController"], "https://conv/c1");
        assert_eq!(attached(&json!({})), None);
        let accept = accept_call(&caller, "v=0", Some("LEG"));
        assert_eq!(accept["callAcceptance"]["mediaContent"]["mediaLegId"], "LEG");
        assert_eq!(accept["callAcceptance"]["acceptedBy"]["endpointId"], "ep");
    }

    #[test]
    fn a_conversation_is_known_through_its_proxy_and_query() {
        let direct = "https://conv-euno-05-prod-aks.conv.skype.com/conv/h5HP/";
        let proxied = "https://api.flightproxy.teams.microsoft.com/api/v2/ep/conv.skype.com/conv/h5HP?i=10";
        assert!(same_conversation(direct, proxied));
        assert!(!same_conversation(direct, "https://conv.skype.com/conv/other/"));
        assert!(!same_conversation("https://x/", "https://x/"));
    }

    #[test]
    fn the_broker_says_where_to_poll_next() {
        let answer = json!({"message": "", "nextSubscribeUrl": "https://x/subscribe/1/0", "expirationTimeInSec": 30});
        assert_eq!(next_subscription(&answer).as_deref(), Some("https://x/subscribe/1/0"));
        assert_eq!(next_subscription(&json!({"nextSubscribeUrl": "http://x"})), None);
    }

    #[test]
    fn a_new_offer_on_the_call_is_read() {
        let push = json!({"body": {"mediaNegotiation": {
            "links": {"mediaAnswer": "https://cc/answer"},
            "mediaContent": {"blob": "v=0", "mediaLegId": "LEG"},
        }}});
        assert_eq!(
            call_event(&push),
            CallEvent::Negotiation {
                sdp: "v=0".into(),
                answer_link: "https://cc/answer".into(),
                media_leg: Some("LEG".into()),
            }
        );
    }

    #[test]
    fn pushes_say_what_happened() {
        let accepted = json!({"url": "https://x/callAgent/e/r/call/acceptance/", "body": json!({"callAcceptance": {
            "callKeepAliveInterval": 2700,
            "links": {"acknowledgement": "https://cc/ack"},
            "mediaContent": {"blob": "v=0"}}}).to_string()});
        assert_eq!(
            call_event(&accepted),
            CallEvent::Accepted {
                sdp: "v=0".into(),
                acknowledge: Some("https://cc/ack".into()),
                call_leg: None,
                keep_alive_seconds: Some(2700),
            }
        );
        assert!(is_call_callback(&accepted, "e"));
        assert!(!is_call_callback(&accepted, "other"));
        let ended = json!({"body": {"callEnd": {"phrase": "Busy"}}});
        assert_eq!(call_event(&ended), CallEvent::Ended { phrase: Some("Busy".into()) });
        assert_eq!(call_event(&json!({"body": "{}"})), CallEvent::Other);
        let roster = json!({"url": "https://x/callAgent/e/r/conversation/rosterUpdate/", "body": {"participants": {
            "8:orgid:a": {"details": {"displayName": "Ana"}, "endpoints": {"x": {}}},
            "8:orgid:b": {"details": {"displayName": "Bo"}, "endpoints": {}},
        }}});
        assert_eq!(
            call_event(&roster),
            CallEvent::Roster(vec![
                RosterChange { mri: "8:orgid:a".into(), name: "Ana".into(), present: true },
                RosterChange { mri: "8:orgid:b".into(), name: "Bo".into(), present: false },
            ])
        );
    }

    #[test]
    fn a_gzip_push_opens() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"{"callEnd":{"phrase":"x"}}"#).unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        let envelope = json!({"headers": {"X-Microsoft-Skype-Content-Encoding": "gzip"}, "body": body});
        assert_eq!(call_event(&envelope), CallEvent::Ended { phrase: Some("x".into()) });
    }

    #[test]
    fn a_test_call_names_no_one() {
        let caller = Caller {
            mri: "8:orgid:me",
            name: "Me",
            endpoint: "e",
            participant: "p",
            surl: "https://t/",
            random: "r",
            client: "c",
        };
        let test = place_call(&caller, &[], None, "v=0", "m");
        assert_eq!(test["participants"]["to"], json!([]));
        assert_eq!(test["scenario"], "UserInitiatedTestCall");
        assert_eq!(test["callInvitation"]["links"]["acceptance"], "https://t/callAgent/e/r/call/acceptance/");
        let real = place_call(&caller, &["8:orgid:x".to_owned()], None, "v=0", "m");
        assert_eq!(real["groupChat"], Value::Null);
        let group = place_call(&caller, &["8:orgid:x".to_owned()], Some("19:t@thread.v2"), "v=0", "m");
        assert_eq!(group["groupChat"]["threadId"], "19:t@thread.v2");
        assert_eq!(real["participants"]["to"][0]["id"], "8:orgid:x");
        assert_eq!(real.get("scenario"), None);
    }

    #[test]
    fn a_thread_joins_its_running_call_else_its_meeting() {
        let live = json!({"status": "Active", "groupCallInitiator": "8:orgid:starter",
            "meetingInfo": {"organizerId": "org", "tenantId": "ten"}})
        .to_string();
        let meeting = json!({"organizerId": "planner", "tenantId": "ten2",
            "meetingJoinUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d"})
        .to_string();
        let running = json!({"properties": {"awareness_conversationLiveState:0": live, "meeting": meeting}});
        let join = meeting_join(&running, "19:t@thread.v2", "mine").unwrap();
        assert_eq!(
            join,
            MeetingJoin {
                thread: "19:t@thread.v2".into(),
                message: "0".into(),
                organizer: "org".into(),
                tenant: "ten".into()
            }
        );
        let planned = json!({"threadProperties": {"meeting": meeting,
            "awareness_conversationLiveState:0": json!({"status": "Ended"}).to_string()}});
        let join = meeting_join(&planned, "19:meeting_x@thread.v2", "mine").unwrap();
        assert_eq!((join.organizer.as_str(), join.tenant.as_str(), join.message.as_str()), ("planner", "ten2", "0"));
        let huddle = json!({"properties": {"awareness_conversationLiveState:0":
            json!({"status": "Active", "groupCallInitiator": "8:orgid:starter"}).to_string()}});
        let join = meeting_join(&huddle, "19:c@thread.tacv2", "mine").unwrap();
        assert_eq!((join.organizer.as_str(), join.tenant.as_str()), ("starter", "mine"));
        assert_eq!(meeting_join(&json!({"properties": {"topic": "x"}}), "19:c@thread.tacv2", "mine"), None);
    }

    #[test]
    fn a_meeting_link_names_its_message() {
        assert_eq!(
            join_message("https://teams.microsoft.com/l/meetup-join/19%3ab%40thread.tacv2/1700000000000?context=x")
                .as_deref(),
            Some("1700000000000")
        );
        assert_eq!(join_message("https://teams.microsoft.com/l/call/0/0?users=a"), None);
    }

    #[test]
    fn a_channel_call_and_a_meeting_join_speak_like_teams() {
        let caller = Caller {
            mri: "8:orgid:me",
            name: "Me",
            endpoint: "e",
            participant: "p",
            surl: "https://t/",
            random: "r",
            client: "c",
        };
        let start = start_channel_call(&caller, "19:c@thread.tacv2", "v=0");
        assert_eq!(start["conversationRequest"]["conversationType"], "huddleGroupCall");
        assert_eq!(start["groupChat"], json!({"threadId": "19:c@thread.tacv2", "messageId": null}));
        assert_eq!(start["participants"]["to"], json!([]));
        assert_eq!(start["callInvitation"]["mediaContent"]["blob"], "v=0");
        let join = MeetingJoin {
            thread: "19:m@thread.v2".into(),
            message: "0".into(),
            organizer: "org".into(),
            tenant: "ten".into(),
        };
        let resolve = join_meeting(&caller, &join);
        assert_eq!(resolve["meetingInfo"], json!({"tenantId": "ten", "organizerId": "org"}));
        assert_eq!(resolve["groupChat"]["messageId"], "0");
        assert!(resolve.get("callInvitation").is_none());
        assert!(resolve["conversationRequest"].get("conversationType").is_none());
        let call = join_meeting_call(&caller, &join, "v=0");
        assert_eq!(call["callInvitation"]["links"]["acceptance"], "https://t/callAgent/e/r/call/acceptance/");
        assert_eq!(call["meetingInfo"]["organizerId"], "org");
    }
}
