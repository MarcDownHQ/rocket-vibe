//! A Teams group call's data channel (brain/architecture/native-calls.md): its
//! media server sends a video only on a section this side asked for it by
//! name. Each message is a frame, `0x10`, the header's remaining length, a kind
//! (`0x92` for the JSON this side speaks), `0x00`, a little-endian sequence
//! number, the sender's stream id, the receivers' count and stream ids (all
//! big-endian), `0x01`, then a JSON array.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::sdp::{Label, VideoSlot};

/// The label the web client gives the channel.
pub const LABEL: &str = "main-channel";

/// The media server's own address on the channel.
const SERVER: u32 = 0xffff_fffc;
const JSON: u8 = 0x92;

/// This side of the channel: its address (the data section's stream id) and the
/// counters its messages carry.
#[derive(Debug, Clone)]
pub struct Channel {
    address: u32,
    sent: u16,
    requests: u32,
}

impl Channel {
    pub fn new(address: u32) -> Self {
        Channel { address, sent: 0, requests: 0 }
    }

    fn frame(&mut self, message: Value) -> Vec<u8> {
        let mut out = vec![0x10, 0x0f, JSON, 0x00];
        out.extend_from_slice(&self.sent.to_le_bytes());
        out.extend_from_slice(&self.address.to_be_bytes());
        out.push(1);
        out.extend_from_slice(&SERVER.to_be_bytes());
        out.push(1);
        out.extend_from_slice(json!([message]).to_string().as_bytes());
        self.sent = self.sent.wrapping_add(1);
        out
    }

    /// The first message, with what this side takes and sends.
    pub fn syn(&mut self) -> Vec<u8> {
        self.frame(json!({
            "type": "syn",
            "client_capabilities": ["dsh", "bwe", "sr", "ssbwe", "leave", "vdclc", "heartbeat"],
        }))
    }

    /// Sent every second, as the server sends its own.
    pub fn heartbeat(&mut self) -> Vec<u8> {
        self.frame(json!({"type": "heartbeat", "state": 0}))
    }

    /// Asks for `source` (a roster's stream id) on the section of `stream`, or
    /// for nothing there with `None`.
    pub fn subscribe(&mut self, stream: u32, source: Option<i64>, screen: bool) -> Vec<u8> {
        self.requests += 1;
        let format = if screen {
            json!({"max-fs": 8160, "max-mbps": 135000, "max-fps": 1500, "profile-level-id": "64001f"})
        } else {
            json!({"max-fs": 8160, "max-mbps": 244800, "max-fps": 3000, "profile-level-id": "64001f"})
        };
        self.frame(json!({
            "type": "sr",
            "controlVideoStreaming": {
                "sequenceNumber": self.requests,
                "controlInfo": {"sourceId": source.unwrap_or(-1), "streamMsid": stream, "fmtParams": [format]},
            },
        }))
    }
}

/// The JSON messages of one frame; none for a frame of another kind.
pub fn messages(frame: &[u8]) -> Vec<Value> {
    if frame.len() < 4 || frame[0] != 0x10 || frame[2] != JSON {
        return Vec::new();
    }
    let body = frame.get(usize::from(frame[1]) + 1..).unwrap_or_default();
    match serde_json::from_slice(body) {
        Ok(Value::Array(messages)) => messages,
        _ => Vec::new(),
    }
}

/// A video someone in the call sends, by the roster's stream id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: i64,
    pub screen: bool,
    /// Whether it sends now.
    pub live: bool,
}

/// Which video goes on which of this side's sections.
#[derive(Debug, Clone, Default)]
pub struct Routing {
    /// The source on each section, by the section's stream id, and whose it is.
    routed: BTreeMap<u32, (i64, String)>,
}

impl Routing {
    /// The requests that bring `sources` (by whose they are) onto `slots`: a
    /// source that stopped leaves its section, a new one takes a free section of
    /// its kind, in order. Sources past the free sections are not shown.
    pub fn plan(
        &mut self,
        slots: &[VideoSlot],
        sources: &BTreeMap<String, Vec<Source>>,
    ) -> Vec<(u32, Option<i64>, bool)> {
        let live: Vec<(&String, &Source)> =
            sources.iter().flat_map(|(who, list)| list.iter().filter(|s| s.live).map(move |s| (who, s))).collect();
        let mut requests = Vec::new();
        let ids: Vec<u32> = self.routed.keys().copied().collect();
        for id in ids {
            let (source, _) = &self.routed[&id];
            let slot = slots.iter().find(|s| s.stream_id == id);
            if slot.is_none() || !live.iter().any(|(_, s)| s.id == *source) {
                self.routed.remove(&id);
                if let Some(slot) = slot {
                    requests.push((id, None, slot.label == Label::Screen));
                }
            }
        }
        for (who, source) in live {
            if self.routed.values().any(|(id, _)| *id == source.id) {
                continue;
            }
            let label = if source.screen { Label::Screen } else { Label::Camera };
            let Some(slot) = slots.iter().find(|s| s.label == label && !self.routed.contains_key(&s.stream_id)) else {
                continue;
            };
            self.routed.insert(slot.stream_id, (source.id, who.clone()));
            requests.push((slot.stream_id, Some(source.id), source.screen));
        }
        requests
    }

    /// Whose video each section shows: its stream on this side, whose, and
    /// whether it is a screen.
    pub fn shown(&self, slots: &[VideoSlot]) -> Vec<(String, String, bool)> {
        slots
            .iter()
            .filter_map(|slot| {
                let (_, who) = self.routed.get(&slot.stream_id)?;
                Some((slot.stream.clone(), who.clone(), slot.label == Label::Screen))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(stream_id: u32, label: Label) -> VideoSlot {
        VideoSlot { stream_id, stream: format!("teams-{stream_id}"), label }
    }

    #[test]
    fn a_request_is_framed_as_the_web_client_frames_it() {
        let mut channel = Channel::new(213);
        let syn = channel.syn();
        assert_eq!(&syn[..16], &[0x10, 0x0f, 0x92, 0, 0, 0, 0, 0, 0, 0xd5, 1, 0xff, 0xff, 0xff, 0xfc, 1]);
        assert_eq!(messages(&syn)[0]["type"], "syn");
        channel.heartbeat();
        let request = channel.subscribe(202, Some(415), false);
        assert_eq!(&request[4..6], &[2, 0], "the frames are counted");
        let message = &messages(&request)[0];
        assert_eq!(message["type"], "sr");
        assert_eq!(message["controlVideoStreaming"]["sequenceNumber"], 1);
        assert_eq!(message["controlVideoStreaming"]["controlInfo"]["sourceId"], 415);
        assert_eq!(message["controlVideoStreaming"]["controlInfo"]["streamMsid"], 202);
        let stop = channel.subscribe(202, None, false);
        assert_eq!(messages(&stop)[0]["controlVideoStreaming"]["controlInfo"]["sourceId"], -1);
    }

    #[test]
    fn the_servers_frames_are_read_whatever_their_receivers() {
        let mut frame = vec![0x10, 0x13, 0x92, 0, 0x56, 0, 0xff, 0xff, 0xff, 0xfc, 2, 0, 0, 0, 0xd5, 0, 0, 1, 0xaa, 1];
        frame.extend_from_slice(br#"[{"history":[414,201],"type":"dsh"}]"#);
        assert_eq!(messages(&frame)[0]["type"], "dsh");
        let mut other = vec![0x10, 0x0f, 0x81, 0, 0, 0, 0, 0, 0, 0xd5, 1, 0, 0, 1, 0xaa, 1];
        other.extend_from_slice(b"{}");
        assert!(messages(&other).is_empty());
        assert!(messages(&[0x10]).is_empty());
    }

    #[test]
    fn a_live_video_takes_a_free_section_of_its_kind_and_leaves_it_when_it_stops() {
        let slots = [slot(202, Label::Camera), slot(203, Label::Camera), slot(212, Label::Screen)];
        let mut routing = Routing::default();
        let camera = |id, live| Source { id, screen: false, live };
        let screen = |id, live| Source { id, screen: true, live };
        let mut sources = BTreeMap::new();
        sources.insert("8:a".to_owned(), vec![camera(415, false), screen(425, false)]);
        assert!(routing.plan(&slots, &sources).is_empty());
        sources.insert("8:a".to_owned(), vec![camera(415, true), screen(425, true)]);
        assert_eq!(routing.plan(&slots, &sources), vec![(202, Some(415), false), (212, Some(425), true)]);
        assert!(routing.plan(&slots, &sources).is_empty(), "asked once");
        sources.insert("8:b".to_owned(), vec![camera(515, true)]);
        assert_eq!(routing.plan(&slots, &sources), vec![(203, Some(515), false)]);
        assert_eq!(
            routing.shown(&slots),
            vec![
                ("teams-202".to_owned(), "8:a".to_owned(), false),
                ("teams-203".to_owned(), "8:b".to_owned(), false),
                ("teams-212".to_owned(), "8:a".to_owned(), true),
            ]
        );
        sources.insert("8:a".to_owned(), vec![camera(415, false), screen(425, false)]);
        assert_eq!(routing.plan(&slots, &sources), vec![(202, None, false), (212, None, true)]);
        sources.remove("8:b");
        assert_eq!(routing.plan(&slots, &sources), vec![(203, None, false)]);
        assert!(routing.shown(&slots).is_empty());
    }
}
