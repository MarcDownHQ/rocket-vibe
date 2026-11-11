//! Mattermost's Calls plugin (`com.mattermost.calls`), audio only: the signalling
//! runs on a WebSocket of its own to the server's `/api/v4/websocket`, its actions
//! and events named `custom_com.mattermost.calls_*`. This client offers, the
//! server's SFU answers and offers again as people come and go. Protocol notes:
//! brain/architecture/native-calls.md.

#[cfg(feature = "client")]
mod client;

#[cfg(feature = "client")]
pub use client::{Session, join};

use serde_json::{Value, json};

use crate::{Candidate, Member};

pub const PREFIX: &str = "custom_com.mattermost.calls";

/// What joining needs: the server, the account's token, the channel.
#[derive(Debug, Clone)]
pub struct Config {
    /// `https://host[/subpath]`.
    pub server: String,
    pub token: String,
    pub channel_id: String,
    /// This account's user id, left out of the roster.
    pub user_id: String,
}

impl Config {
    pub fn socket_url(&self) -> Option<String> {
        let base = self.server.trim_end_matches('/');
        let rest = base
            .strip_prefix("https://")
            .map(|r| format!("wss://{r}"))
            .or_else(|| base.strip_prefix("http://").map(|r| format!("ws://{r}")))?;
        Some(format!("{rest}/api/v4/websocket"))
    }
}

/// A JSON action frame.
pub fn action(name: &str, seq: u64, data: Value) -> String {
    json!({"action": format!("{PREFIX}_{name}"), "seq": seq, "data": data}).to_string()
}

/// The authentication frame a socket opens with.
pub fn authentication(seq: u64, token: &str) -> String {
    json!({"action": "authentication_challenge", "seq": seq, "data": {"token": token}}).to_string()
}

/// A session description as the plugin wants it: a MessagePack frame whose
/// `data.data` is the zlib of `{type, sdp}` as JSON.
pub fn sdp_frame(seq: u64, kind: &str, sdp: &str) -> Vec<u8> {
    let description = json!({"type": kind, "sdp": sdp}).to_string();
    let zipped = miniz_oxide::deflate::compress_to_vec_zlib(description.as_bytes(), 6);
    let mut out = Vec::with_capacity(zipped.len() + 64);
    out.push(0x83);
    str_pack(&mut out, "action");
    str_pack(&mut out, &format!("{PREFIX}_sdp"));
    str_pack(&mut out, "seq");
    out.push(0xcf);
    out.extend_from_slice(&seq.to_be_bytes());
    str_pack(&mut out, "data");
    out.push(0x81);
    str_pack(&mut out, "data");
    let n = zipped.len();
    if n < 256 {
        out.extend_from_slice(&[0xc4, n as u8]);
    } else if n < 65_536 {
        out.push(0xc5);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    out.extend_from_slice(&zipped);
    out
}

fn str_pack(out: &mut Vec<u8>, s: &str) {
    let n = s.len();
    if n < 32 {
        out.push(0xa0 | n as u8);
    } else if n < 256 {
        out.extend_from_slice(&[0xd9, n as u8]);
    } else {
        out.push(0xda);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(s.as_bytes());
}

/// A local candidate as the `ice` action carries it.
pub fn ice(seq: u64, candidate: &Candidate) -> String {
    let init = json!({"candidate": candidate.line, "sdpMid": candidate.mid, "sdpMLineIndex": 0});
    action("ice", seq, json!({"data": init.to_string()}))
}

/// One participant's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub session: String,
    pub user: String,
    pub muted: bool,
}

/// What a frame from the server means to the call.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hello {
        connection: String,
    },
    /// The join was taken.
    Joined,
    Refused(String),
    /// The server's offer or answer, or one of its candidates.
    Offer(String),
    Answer(String),
    Candidate(Candidate),
    /// Everyone in the call, from the state sent after joining.
    State(Vec<Person>),
    Arrived(Person),
    Left {
        session: String,
    },
    Muted {
        session: String,
        muted: bool,
    },
    Voice {
        session: String,
        on: bool,
    },
    /// The call is over (its last member left, or the host ended it).
    CallEnded,
    /// This session was removed by the host.
    Removed,
    Other,
}

/// Reads a text frame; `connection` is this socket's own id, which the server's
/// per-connection events name.
pub fn incoming(text: &str, connection: &str, channel: &str) -> Incoming {
    let Ok(frame) = serde_json::from_str::<Value>(text) else { return Incoming::Other };
    let Some(event) = frame.get("event").and_then(Value::as_str) else { return Incoming::Other };
    let data = frame.get("data").unwrap_or(&Value::Null);
    let s = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    if event == "hello" {
        return Incoming::Hello { connection: s("connection_id") };
    }
    let Some(name) = event.strip_prefix(PREFIX).and_then(|n| n.strip_prefix('_')) else { return Incoming::Other };
    let mine = data.get("connID").and_then(Value::as_str) == Some(connection);
    let here = data.get("channelID").or_else(|| data.get("channel_id")).and_then(Value::as_str) == Some(channel)
        || frame.pointer("/broadcast/channel_id").and_then(Value::as_str) == Some(channel);
    match name {
        "join" if mine => Incoming::Joined,
        "error" if mine => Incoming::Refused(s("data")),
        "signal" if mine => signal(&s("data")),
        "call_state" if here => {
            let call = data.get("call").and_then(Value::as_str).and_then(|c| serde_json::from_str::<Value>(c).ok());
            let people = call
                .as_ref()
                .and_then(|c| c.get("sessions"))
                .and_then(Value::as_array)
                .map(|all| all.iter().filter_map(person).collect())
                .unwrap_or_default();
            Incoming::State(people)
        }
        "user_joined" if here => person(data).map_or(Incoming::Other, Incoming::Arrived),
        "user_left" if here => Incoming::Left { session: s("session_id") },
        "user_muted" | "user_unmuted" if here => {
            Incoming::Muted { session: s("session_id"), muted: name == "user_muted" }
        }
        "user_voice_on" | "user_voice_off" if here => {
            Incoming::Voice { session: s("session_id"), on: name == "user_voice_on" }
        }
        "call_end" if here => Incoming::CallEnded,
        "user_removed" if here && (mine || s("session_id") == connection) => Incoming::Removed,
        _ => Incoming::Other,
    }
}

fn person(v: &Value) -> Option<Person> {
    let session = v.get("session_id").and_then(Value::as_str)?.to_owned();
    let user = v.get("user_id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let muted = !v.get("unmuted").and_then(Value::as_bool).unwrap_or(false);
    Some(Person { session, user, muted })
}

fn signal(raw: &str) -> Incoming {
    let Ok(message) = serde_json::from_str::<Value>(raw) else { return Incoming::Other };
    let sdp = || message.get("sdp").and_then(Value::as_str).unwrap_or_default().to_owned();
    match message.get("type").and_then(Value::as_str) {
        Some("offer") => Incoming::Offer(sdp()),
        Some("answer") => Incoming::Answer(sdp()),
        Some("candidate") => {
            let c = message.get("candidate").unwrap_or(&Value::Null);
            let c = match c {
                Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
                other => other.clone(),
            };
            let line = c.get("candidate").and_then(Value::as_str).unwrap_or_default();
            if line.is_empty() {
                return Incoming::Other;
            }
            let mid = c.get("sdpMid").and_then(Value::as_str).unwrap_or_default().to_owned();
            Incoming::Candidate(Candidate { mid, line: line.strip_prefix("a=").unwrap_or(line).to_owned() })
        }
        _ => Incoming::Other,
    }
}

/// The people in the call but this account's own sessions, for the engine.
pub fn members(people: &[Person], me: &str) -> Vec<Member> {
    people
        .iter()
        .filter(|p| p.user != me)
        .map(|p| Member { id: p.session.clone(), name: String::new(), muted: p.muted })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "conn1";
    const CH: &str = "chan";

    #[test]
    fn the_server_says_who_is_there_and_what_they_do() {
        let state = json!({"event": "custom_com.mattermost.calls_call_state", "data": {"channel_id": CH,
            "call": json!({"sessions": [{"session_id": "s1", "user_id": "u1", "unmuted": true},
                {"session_id": "s2", "user_id": "me", "unmuted": false}]}).to_string()}});
        let Incoming::State(people) = incoming(&state.to_string(), ME, CH) else { panic!() };
        assert_eq!(members(&people, "me"), vec![Member { id: "s1".into(), name: String::new(), muted: false }]);
        let joined = json!({"event": "custom_com.mattermost.calls_user_joined",
            "data": {"channelID": CH, "session_id": "s3", "user_id": "u3"}});
        assert_eq!(
            incoming(&joined.to_string(), ME, CH),
            Incoming::Arrived(Person { session: "s3".into(), user: "u3".into(), muted: true })
        );
        let other_channel = json!({"event": "custom_com.mattermost.calls_user_joined",
            "data": {"channelID": "elsewhere", "session_id": "s3", "user_id": "u3"}});
        assert_eq!(incoming(&other_channel.to_string(), ME, CH), Incoming::Other);
        let voice = json!({"event": "custom_com.mattermost.calls_user_voice_on", "data": {"channelID": CH, "session_id": "s1"}});
        assert_eq!(incoming(&voice.to_string(), ME, CH), Incoming::Voice { session: "s1".into(), on: true });
        let hello = json!({"event": "hello", "data": {"connection_id": "c9"}});
        assert_eq!(incoming(&hello.to_string(), "", CH), Incoming::Hello { connection: "c9".into() });
    }

    #[test]
    fn only_this_connections_signals_are_read() {
        let signal = |conn: &str, data: Value| {
            json!({"event": "custom_com.mattermost.calls_signal", "data": {"connID": conn, "data": data.to_string()}})
                .to_string()
        };
        assert_eq!(
            incoming(&signal(ME, json!({"type": "answer", "sdp": "v=0"})), ME, CH),
            Incoming::Answer("v=0".into())
        );
        assert_eq!(incoming(&signal("other", json!({"type": "offer", "sdp": "v=0"})), ME, CH), Incoming::Other);
        let candidate = json!({"type": "candidate", "candidate": {"candidate": "candidate:1 1 udp 1 1.2.3.4 8443 typ host", "sdpMid": "0"}});
        assert_eq!(
            incoming(&signal(ME, candidate), ME, CH),
            Incoming::Candidate(Candidate {
                mid: "0".into(),
                line: "candidate:1 1 udp 1 1.2.3.4 8443 typ host".into()
            })
        );
        let refused = json!({"event": "custom_com.mattermost.calls_error", "data": {"connID": ME, "data": "insufficient permissions"}});
        assert_eq!(incoming(&refused.to_string(), ME, CH), Incoming::Refused("insufficient permissions".into()));
    }

    #[test]
    fn a_description_goes_out_as_messagepack_around_its_zlib() {
        let frame = sdp_frame(7, "offer", "v=0\r\n");
        assert_eq!(frame[0], 0x83);
        let at = frame.windows(4).position(|w| w == b"data").unwrap();
        let tail = &frame[at + 4..];
        assert_eq!(&tail[..1], &[0x81]);
        assert_eq!(&tail[1..6], &[0xa4, b'd', b'a', b't', b'a']);
        assert_eq!(tail[6], 0xc4);
        let zipped = &tail[8..8 + tail[7] as usize];
        let plain = miniz_oxide::inflate::decompress_to_vec_zlib(zipped).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap(), json!({"type": "offer", "sdp": "v=0\r\n"}));
        assert!(frame.windows(31).any(|w| w == b"custom_com.mattermost.calls_sdp"));
    }

    #[test]
    fn the_socket_is_the_servers_own() {
        let config = |server: &str| Config {
            server: server.into(),
            token: String::new(),
            channel_id: String::new(),
            user_id: String::new(),
        };
        assert_eq!(
            config("https://mm.example/sub/").socket_url().as_deref(),
            Some("wss://mm.example/sub/api/v4/websocket")
        );
        assert_eq!(
            config("http://localhost:8065").socket_url().as_deref(),
            Some("ws://localhost:8065/api/v4/websocket")
        );
        assert_eq!(config("ftp://x").socket_url(), None);
    }
}
