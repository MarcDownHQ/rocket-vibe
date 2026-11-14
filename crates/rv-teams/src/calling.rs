//! Teams' calling stack (brain/architecture/native-calls.md), the parts with no
//! network: its SDP dialect against plain WebRTC's, and what its pushes carry.

use std::io::Read;

use base64::Engine;
use serde_json::{Value, json};

/// Microsoft's Test Call bot: it plays back what it hears, and rings no one.
pub const ECHO_BOT: &str = "cf28171e-fcfd-47e4-a1d6-79460b0b3ca0";

/// Where a call is placed.
pub const EPCONV: &str = "https://api.flightproxy.teams.microsoft.com/api/v2/epconv";

/// How this client presents itself to the calling stack.
pub const CLIENT: &str = "SkypeSpaces/1415/26100212018/os=linux; osVer=undefined; deviceType=computer; browser=chrome";

/// A plain WebRTC offer, audio only, in Teams' dialect: its profile is written
/// `RTP/SAVP`, its extension URIs with backslashes, and the audio section carries
/// its label and the range of the source it sends. The media line names a real
/// address, taken from a candidate: port 9 and `0.0.0.0` read as disabled media
/// to Teams (`DisabledMediaNotSupported`). UDP candidates only, without the
/// extras Teams does not parse.
pub fn teams_offer(sdp: &str) -> String {
    let ssrc = sdp.lines().find_map(|l| l.strip_prefix("a=ssrc:")?.split(' ').next()).map(str::to_owned);
    let default = sdp.lines().filter_map(candidate_fields).find(|c| c.udp && !c.address.contains(':'));
    let port = default.as_ref().map_or("9", |c| c.port);
    let address = default.as_ref().map_or("0.0.0.0", |c| c.address);
    let mut out = String::with_capacity(sdp.len() + 128);
    let mut push = |line: &str| {
        out.push_str(line);
        out.push_str("\r\n");
    };
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=audio ") {
            let (_, rest) = rest.split_once(' ').unwrap_or(("", rest));
            push(&format!("m=audio {port} {}", rest.replacen("UDP/TLS/RTP/SAVPF", "RTP/SAVP", 1)));
        } else if line.starts_with("c=") {
            push(&format!("c=IN IP4 {address}"));
        } else if line.starts_with("a=rtcp:") {
            push(&format!("a=rtcp:{port}"));
        } else if line.starts_with("a=extmap:") {
            push(&line.replace('/', "\\"));
        } else if line == "a=rtcp-rsize" {
        } else if line.starts_with("a=candidate:") {
            let Some(c) = candidate_fields(line).filter(|c| c.udp) else { continue };
            push(&format!("a=candidate:{}", c.plain));
        } else if line.starts_with("a=mid:") {
            push(line);
            push("a=label:main-audio");
            push("a=x-signaling-fb:* x-message app recv:dsh");
            if let Some(ssrc) = &ssrc {
                push(&format!("a=x-ssrc-range:{ssrc}-{ssrc}"));
            }
        } else {
            push(line);
        }
    }
    out
}

struct OfferCandidate<'a> {
    udp: bool,
    address: &'a str,
    port: &'a str,
    /// Up to its type and related address, without `generation`, `ufrag`, `network-id`.
    plain: String,
}

fn candidate_fields(line: &str) -> Option<OfferCandidate<'_>> {
    let fields: Vec<&str> = line.strip_prefix("a=candidate:")?.split(' ').collect();
    let end = fields.iter().position(|f| *f == "generation").unwrap_or(fields.len());
    Some(OfferCandidate {
        udp: fields.get(2)?.eq_ignore_ascii_case("udp"),
        address: fields.get(4)?,
        port: fields.get(5)?,
        plain: fields[..end].join(" "),
    })
}

/// Teams' answer in plain WebRTC: only its audio section, with the profile and
/// extension URIs libwebrtc reads, and the UDP candidates without their extras.
pub fn webrtc_answer(sdp: &str) -> Option<String> {
    #[derive(PartialEq)]
    enum In {
        Head,
        Audio,
        Other,
    }
    let mut head = Vec::new();
    let mut audio: Option<Vec<String>> = None;
    let mut at = In::Head;
    for line in sdp.lines() {
        if line.starts_with("m=") {
            at = if line.starts_with("m=audio ") && audio.is_none() {
                audio = Some(vec![line.replacen("RTP/SAVP", "UDP/TLS/RTP/SAVPF", 1)]);
                In::Audio
            } else {
                In::Other
            };
            continue;
        }
        if at == In::Head {
            if !line.starts_with("a=group:") && !line.starts_with("a=x-") {
                head.push(line.to_owned());
            }
            continue;
        }
        if at == In::Other {
            continue;
        }
        let Some(lines) = audio.as_mut() else { continue };
        if line.starts_with("a=x-") || line.starts_with("a=label:") || line.starts_with("a=ptime:") {
            continue;
        }
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            let fields: Vec<&str> = candidate.split(' ').collect();
            if fields.get(2).is_none_or(|p| !p.eq_ignore_ascii_case("udp")) {
                continue;
            }
            let end = fields.iter().position(|f| *f == "MTURNID").unwrap_or(fields.len());
            lines.push(format!("a=candidate:{}", fields[..end].join(" ")));
            continue;
        }
        if line.starts_with("a=extmap:") {
            lines.push(line.replace('\\', "/"));
            continue;
        }
        lines.push(line.to_owned());
    }
    let audio = audio?;
    let mid = audio.iter().find_map(|l| l.strip_prefix("a=mid:"))?.to_owned();
    let mut out = String::new();
    for line in head {
        out.push_str(&line);
        out.push_str("\r\n");
        if line.starts_with("t=") {
            out.push_str(&format!("a=group:BUNDLE {mid}\r\n"));
        }
    }
    for line in audio {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    Some(out)
}

/// The first audio section's `a=mid`.
pub fn audio_mid(sdp: &str) -> Option<&str> {
    sdp.lines().skip_while(|l| !l.starts_with("m=audio ")).find_map(|l| l.strip_prefix("a=mid:"))
}

fn with_mid(sdp: &str, mid: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        if line.starts_with("a=mid:") {
            out.push_str(&format!("a=mid:{mid}\r\n"));
        } else if line.starts_with("a=group:BUNDLE") {
            out.push_str(&format!("a=group:BUNDLE {mid}\r\n"));
        } else {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out
}

/// A new offer of Teams' in plain WebRTC, made to fit what `negotiated` (the call's
/// current plain SDP) set: libwebrtc refuses a later offer whose section has another
/// `mid`, or that gives an extension id another URI. Teams' hold offer moves id 3 from
/// transport-wide-cc to its own feedback extension: such an extension is left out, so
/// the answer declines it.
pub fn renegotiation_offer(sdp: &str, negotiated: Option<&str>) -> Option<String> {
    let plain = webrtc_answer(sdp)?;
    let Some(negotiated) = negotiated else { return Some(plain) };
    let active = extensions(negotiated);
    let mut kept = String::with_capacity(plain.len());
    for line in plain.lines() {
        let clashes = extension(line).is_some_and(|(id, uri)| {
            active.iter().any(|&(other_id, other_uri)| (other_id == id) != (other_uri == uri))
        });
        if !clashes {
            kept.push_str(line);
            kept.push_str("\r\n");
        }
    }
    Some(audio_mid(negotiated).map_or_else(|| kept.clone(), |mid| with_mid(&kept, mid)))
}

/// An `a=extmap` line's id and URI.
fn extension(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.strip_prefix("a=extmap:")?.split(' ');
    let id = fields.next()?.split('/').next()?;
    Some((id, fields.next()?))
}

fn extensions(sdp: &str) -> Vec<(&str, &str)> {
    sdp.lines().filter_map(extension).collect()
}

/// `RV_CALL_DEBUG=1`: each section of an SDP: its kind, `mid` and direction.
pub fn sdp_sections(sdp: &str) -> String {
    let mut out = Vec::new();
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            let mut fields = rest.split(' ');
            out.push(format!("{} port {}", fields.next().unwrap_or(""), fields.next().unwrap_or("")));
        } else if let (Some(last), Some(mid)) = (out.last_mut(), line.strip_prefix("a=mid:")) {
            let last: &mut String = last;
            last.push_str(&format!(" mid {mid}"));
        } else if let Some(last) = out.last_mut()
            && matches!(line, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive")
        {
            last.push_str(&format!(" {}", &line[2..]));
        }
    }
    out.join("; ")
}

/// A local WebRTC answer to Teams' `offer`, in Teams' dialect: the same sections in
/// the same order, each but the audio one refused with port 0, as its own client does.
pub fn teams_answer(answer: &str, offer: &str) -> String {
    let answer = audio_mid(offer).map_or_else(|| answer.to_owned(), |mid| with_mid(answer, mid));
    let audio = teams_offer(&answer);
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in offer.lines() {
        if line.starts_with("m=") {
            sections.push(Vec::new());
        }
        sections.last_mut().expect("never empty").push(line);
    }
    let mut out = String::with_capacity(audio.len() + 128);
    let (head, body) = audio.split_once("m=audio ").map_or((audio.as_str(), ""), |(h, b)| (h, b));
    out.push_str(head);
    let mut placed = false;
    for section in sections.iter().skip(1) {
        let line = section[0];
        if line.starts_with("m=audio ") && !placed {
            placed = true;
            out.push_str("m=audio ");
            out.push_str(body);
            continue;
        }
        let mut fields = line.splitn(3, ' ');
        let (kind, _port, rest) = (fields.next().unwrap_or(""), fields.next(), fields.next().unwrap_or(""));
        out.push_str(&format!("{kind} 0 {rest}\r\n"));
        if let Some(mid) = section.iter().find(|l| l.starts_with("a=mid:")) {
            out.push_str(mid);
            out.push_str("\r\n");
        }
    }
    out
}

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

/// `epconv`'s body placing a call with this offer: to `to`'s accounts, or, with
/// none, Microsoft's Test Call.
pub fn place_call(caller: &Caller, to: &[String], thread: Option<&str>, offer: &str, modality_id: &str) -> Value {
    let cb = |path: &str| caller.callback(path);
    let test = to.is_empty();
    let oid = caller.mri.strip_prefix("8:orgid:").unwrap_or(caller.mri);
    let group_chat = match thread {
        _ if test => json!({"threadId": format!("19:{oid}_{ECHO_BOT}@unq.gbl.spaces"), "messageId": null}),
        Some(thread) => json!({"threadId": thread, "messageId": null}),
        None => Value::Null,
    };
    let targets: Vec<Value> = to.iter().map(|mri| json!({"id": mri, "participantId": modality_id})).collect();
    let mut body = json!({
        "conversationRequest": {
            "conversationType": null, "subject": null, "suppressDialout": test,
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
        },
        "groupContext": null,
        "groupChat": group_chat,
        "participants": {"from": caller.from(), "to": targets},
        "capabilities": null,
        "endpointCapabilities": 73463,
        "clientEndpointCapabilities": 114259690,
        "endpointMetadata": {"holographicCapabilities": 3},
        "meetingInfo": null,
        "endpointState": {"endpointStateSequenceNumber": 1, "endpointProperties":
            {"additionalEndpointProperties": {"infoShownInReportMode": "FullInformation"}}},
        "callInvitation": {
            "callModalities": ["Audio"], "replaces": null, "transferor": null,
            "links": {"progress": cb("call/progress/"), "mediaAnswer": cb("call/mediaAnswer/"),
                "acceptance": cb("call/acceptance/"), "redirection": cb("call/redirection/"), "end": cb("call/end/")},
            "clientContentForMediaController": {"controlVideoStreaming": cb("call/controlVideoStreaming/"),
                "csrcInfo": cb("call/csrcInfo/")},
            "pstnContent": {"emergencyCallCountry": "", "platformName": caller.client, "publicApiCall": false},
            "mediaContent": {"blob": offer, "contentType": "application/sdp-ngc-1.0", "requiredFeatures": "nonByPass"},
        },
    });
    if test {
        body["scenario"] = json!("UserInitiatedTestCall");
    }
    body
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
    fn a_new_offer_keeps_the_calls_mid_and_its_answer_the_offers() {
        let offer = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 7 8\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\n\
            a=inactive\r\na=mid:7\r\nm=video 5 RTP/SAVP 34\r\na=mid:8\r\n";
        let negotiated = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";
        let plain = renegotiation_offer(offer, Some(negotiated)).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=group:BUNDLE 0\r\n") && plain.contains("a=inactive") && !plain.contains("m=video"));
        let local = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=inactive\r\na=mid:0\r\n";
        let answer = teams_answer(local, offer);
        assert_eq!(audio_mid(&answer), Some("7"));
        assert!(answer.contains("a=group:BUNDLE 7\r\n") && answer.contains("m=video 0 ") && answer.contains("a=mid:8"));
        assert_eq!(sdp_sections(offer), "audio port 5 inactive mid 7; video port 5 mid 8");
    }

    #[test]
    fn a_new_offer_leaves_out_an_extension_id_the_call_gave_another_uri() {
        let negotiated = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n\
            a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
            a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
        let offer = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\na=mid:1\r\n\
            a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
            a=extmap:3 http:\\\\skype.com\\experiments\\rtp-hdrext\\fast_bandwidth_feedback#version_3\r\n\
            a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
            a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n";
        let plain = renegotiation_offer(offer, Some(negotiated)).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n"));
        assert!(plain.contains("a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n"));
        assert!(!plain.contains("a=extmap:3 ") && !plain.contains("a=extmap:5 "), "{plain}");
        assert_eq!(renegotiation_offer(offer, None).unwrap().matches("a=extmap:").count(), 4);
    }

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

    const OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
        a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
        a=rtcp:9 IN IP4 0.0.0.0\r\na=rtcp-rsize\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\na=ssrc:42 cname:x\r\n\
        a=candidate:1 1 udp 21 2001:db8::1 111 typ host generation 0 ufrag u network-id 1\r\n\
        a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0 ufrag u network-id 1\r\n\
        a=candidate:3 1 tcp 23 192.168.1.5 333 typ host tcptype passive generation 0\r\n";

    const ANSWER: &str = "v=0\r\no=- 455788 0 IN IP4 127.0.0.1\r\ns=session\r\nc=IN IP4 52.1.2.3\r\nb=CT:10000000\r\nt=0 0\r\n\
        a=group:BUNDLE 0 3\r\na=x-plaza-msi-range:1-100 101-200\r\n\
        m=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=x-signaling-fb:* x-message app send:dsh\r\na=rtcp:3480\r\n\
        a=setup:passive\r\na=ice-ufrag:u\r\na=ice-pwd:p\r\na=rtcp-mux\r\n\
        a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480 MTURNID 154\r\n\
        a=candidate:3 1 tcp-pass 18087935 52.1.2.3 3478 typ relay raddr 10.0.0.1 rport 3478\r\n\
        a=label:main-audio\r\na=mid:0\r\na=rtpmap:120 CN/48000\r\na=rtpmap:111 OPUS/48000/2\r\na=ptime:20\r\n\
        a=fingerprint:sha-256 AA\r\na=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\na=x-ssrc-range:1000-1000\r\n\
        m=video 0 RTP/SAVP 102\r\n\
        m=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\na=label:data\r\n";

    #[test]
    fn an_offer_speaks_teams_dialect() {
        let offer = teams_offer(OFFER);
        assert!(offer.contains("m=audio 222 RTP/SAVP 111\r\n"), "{offer}");
        assert!(offer.contains("c=IN IP4 192.168.1.5\r\n"), "{offer}");
        assert!(offer.contains("a=rtcp:222\r\n"), "{offer}");
        assert!(offer.contains("a=candidate:2 1 udp 22 192.168.1.5 222 typ host\r\n"), "{offer}");
        assert!(offer.contains("a=candidate:1 1 udp 21 2001:db8::1 111 typ host\r\n"), "{offer}");
        assert!(!offer.contains("tcptype"), "{offer}");
        assert!(!offer.contains("rtcp-rsize"), "{offer}");
        assert!(
            offer.contains("a=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\n"),
            "{offer}"
        );
        assert!(offer.contains("a=label:main-audio\r\n"));
        assert!(offer.contains("a=x-ssrc-range:42-42\r\n"));
    }

    #[test]
    fn an_answer_keeps_only_plain_webrtc_audio() {
        let answer = webrtc_answer(ANSWER).unwrap();
        assert!(answer.contains("a=group:BUNDLE 0\r\n"), "{answer}");
        assert!(answer.contains("m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n"));
        assert!(
            answer.contains("a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480\r\n"),
            "{answer}"
        );
        assert!(!answer.contains("MTURNID"));
        assert!(!answer.contains("tcp-pass"));
        assert!(!answer.contains("m=video"));
        assert!(!answer.contains("x-data"));
        assert!(!answer.contains("a=x-"));
        assert!(!answer.contains("a=label"));
        assert!(
            answer.contains("a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n"),
            "{answer}"
        );
        assert_eq!(webrtc_answer("v=0\r\nm=video 0 RTP/SAVP 1\r\n"), None);
    }

    #[test]
    fn an_offer_from_teams_is_answered_in_its_order() {
        let offer = "v=0\r\nt=0 0\r\nm=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\n\
            m=video 0 RTP/SAVP 102\r\na=mid:1\r\nm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\n";
        assert!(webrtc_answer(offer).unwrap().contains("m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n"));
        let local = "v=0\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=mid:0\r\na=sendonly\r\na=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n";
        let answer = teams_answer(local, offer);
        assert!(answer.contains("m=audio 222 RTP/SAVP 111\r\n"), "{answer}");
        assert!(answer.contains("a=sendonly\r\n"));
        assert!(answer.contains("m=video 0 RTP/SAVP 102\r\na=mid:1\r\n"), "{answer}");
        assert!(answer.contains("m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n"), "{answer}");
        assert!(answer.find("m=audio").unwrap() < answer.find("m=video").unwrap());
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
}
