//! Huddle video: the remote sources the server lists (INDEX), which of them this
//! client receives on which of its offer's receiving sections, and what the
//! SUBSCRIBE says about them.

use std::collections::BTreeMap;

use super::proto::Message;
use crate::{RemoteVideo, VideoKind};

/// The suffix of the second attendee a client joins as to share its screen.
pub(crate) const CONTENT: &str = "#content";

/// A remote video the server sends, by the attendee sending it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub attendee: String,
    pub stream: u32,
    /// The person, as the roster names them ([`crate::Member::id`]).
    pub member: String,
    pub kind: VideoKind,
}

/// The video sources of an INDEX, this client's own left out, one per attendee:
/// its base stream when the server lists the temporal layers apart.
pub(crate) fn index_sources(index: &Message, me: &str) -> Vec<Source> {
    let mut sources: Vec<Source> = Vec::new();
    for source in index.messages(2) {
        if source.int(9) != Some(2) {
            continue;
        }
        let (Some(stream), Some(attendee)) = (source.int(1), source.string(8)) else { continue };
        let person = attendee.strip_suffix(CONTENT).unwrap_or(&attendee);
        if person == me || sources.iter().any(|s| s.attendee == attendee) {
            continue;
        }
        let kind = if attendee.ends_with(CONTENT) { VideoKind::Screen } else { VideoKind::Camera };
        let member = source.string(10).filter(|e| !e.is_empty()).unwrap_or_else(|| person.to_owned());
        let stream = source.int(15).unwrap_or(stream);
        sources.push(Source { attendee, stream: stream as u32, member, kind });
    }
    sources
}

/// A video section of an offer: its mid and whether it receives only.
fn video_sections(sdp: &str) -> Vec<(String, bool)> {
    let mut sections = Vec::new();
    let mut current: Option<(String, bool)> = None;
    let mut video = false;
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            if video && let Some(section) = current.take() {
                sections.push(section);
            }
            video = media.starts_with("video");
            current = Some((String::new(), false));
        } else if let Some(section) = current.as_mut() {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                section.0 = mid.trim().to_owned();
            } else if line.trim() == "a=recvonly" {
                section.1 = true;
            }
        }
    }
    if video && let Some(section) = current {
        sections.push(section);
    }
    sections
}

/// Whether an offer sends video: a video section neither receiving only nor inactive.
pub(crate) fn sends_video(sdp: &str) -> bool {
    let mut video = false;
    sdp.lines().any(|line| {
        if let Some(media) = line.strip_prefix("m=") {
            video = media.starts_with("video");
        }
        video && matches!(line.trim(), "a=sendonly" | "a=sendrecv")
    })
}

/// A receiving section's subscription, as the SUBSCRIBE names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscription {
    pub mid: String,
    pub attendee: String,
    pub kind: VideoKind,
}

/// What a SUBSCRIBE asks to receive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Placed {
    /// One per video section of the offer, in m-line order: the stream it
    /// receives, 0 for a section sending or spare.
    pub receive: Vec<u32>,
    pub subscriptions: Vec<Subscription>,
}

/// The remote videos wanted and the receiving section each one is on.
#[derive(Debug, Default)]
pub(crate) struct Subscriptions {
    limit: usize,
    wanted: Vec<Source>,
    /// Receiving section's mid to the source on it.
    placed: BTreeMap<String, Source>,
}

impl Subscriptions {
    /// At most `limit` videos received at once (JOIN_ACK's `video_subscription_limit`).
    pub(crate) fn new(limit: usize) -> Self {
        Subscriptions { limit, ..Self::default() }
    }

    /// A new list of sources: `None` when it changes nothing, else the
    /// sections freed of a source gone.
    pub(crate) fn index(&mut self, mut sources: Vec<Source>) -> Option<Vec<String>> {
        sources.truncate(self.limit);
        if sources == self.wanted {
            return None;
        }
        let freed: Vec<String> =
            self.placed.iter().filter(|(_, s)| !sources.contains(s)).map(|(mid, _)| mid.clone()).collect();
        for mid in &freed {
            self.placed.remove(mid);
        }
        self.wanted = sources;
        Some(freed)
    }

    /// How many sections must receive.
    pub(crate) fn receiving(&self) -> usize {
        self.wanted.len()
    }

    /// Each wanted source on a receiving section of this offer, those placed
    /// before staying where they are.
    pub(crate) fn place(&mut self, offer: &str) -> Placed {
        let sections = video_sections(offer);
        self.placed.retain(|mid, _| sections.iter().any(|(m, receives)| m == mid && *receives));
        let mut free: Vec<&String> = sections
            .iter()
            .filter(|(mid, receives)| *receives && !self.placed.contains_key(mid))
            .map(|(mid, _)| mid)
            .collect();
        free.reverse();
        for source in &self.wanted {
            if self.placed.values().any(|s| s == source) {
                continue;
            }
            let Some(mid) = free.pop() else { break };
            self.placed.insert(mid.clone(), source.clone());
        }
        let mut placed = Placed::default();
        for (mid, _) in &sections {
            match self.placed.get(mid) {
                Some(source) => {
                    placed.receive.push(source.stream);
                    placed.subscriptions.push(Subscription {
                        mid: mid.clone(),
                        attendee: source.attendee.clone(),
                        kind: source.kind,
                    });
                }
                None => placed.receive.push(0),
            }
        }
        if placed.receive.is_empty() {
            placed.receive.push(0);
        }
        placed
    }

    /// The remote videos received, each by the stream id the server's answer
    /// gives its section (`a=msid:v_<mid> track_<mid>`).
    pub(crate) fn remote(&self) -> Vec<RemoteVideo> {
        self.placed
            .iter()
            .map(|(mid, s)| RemoteVideo { track: format!("v_{mid}"), member: s.member.clone(), kind: s.kind })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto::Writer;
    use super::*;

    fn source(stream: u64, attendee: &str, external: &str, layer: Option<u64>) -> Writer {
        let w = Writer::default()
            .uint(1, stream)
            .uint(2, 15)
            .string(4, "AmazonChimeExpressVideo")
            .uint(6, 2)
            .string(8, attendee)
            .uint(9, 2)
            .string(10, external);
        match layer {
            Some(base) => w.uint(13, 0).uint(14, 0).uint(15, base),
            None => w,
        }
    }

    fn index(sources: Vec<Writer>) -> Vec<u8> {
        sources.into_iter().fold(Writer::default(), |w, s| w.message(2, s)).uint(4, 3).finish()
    }

    #[test]
    fn an_index_lists_cameras_and_screens_without_this_client_or_their_layers() {
        let bytes = index(vec![
            source(11, "other", "T-R-UOTHER", None),
            source(71, "other#content", "T-R-UOTHER", Some(71)),
            source(72, "other#content", "T-R-UOTHER", Some(71)),
            source(131, "me#content", "T-R-UME", Some(131)),
            Writer::default().uint(1, 1).string(8, "third").uint(9, 1),
        ]);
        let sources = index_sources(&Message::parse(&bytes).unwrap(), "me");
        assert_eq!(
            sources,
            vec![
                Source { attendee: "other".into(), stream: 11, member: "T-R-UOTHER".into(), kind: VideoKind::Camera },
                Source {
                    attendee: "other#content".into(),
                    stream: 71,
                    member: "T-R-UOTHER".into(),
                    kind: VideoKind::Screen
                },
            ]
        );
        assert!(index_sources(&Message::parse(&index(vec![])).unwrap(), "me").is_empty());
    }

    fn offer(sections: &[(&str, &str)]) -> String {
        let mut sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\n".to_owned();
        for (mid, direction) in sections {
            sdp.push_str(&format!("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:{mid}\r\na={direction}\r\n"));
        }
        sdp
    }

    fn camera(attendee: &str, stream: u32) -> Source {
        Source { attendee: attendee.into(), stream, member: format!("T-R-{attendee}"), kind: VideoKind::Camera }
    }

    #[test]
    fn receive_ids_follow_the_video_sections_in_order() {
        let mut subs = Subscriptions::new(25);
        assert_eq!(subs.place(&offer(&[])).receive, vec![0]);
        assert_eq!(subs.index(vec![camera("a", 11)]), Some(vec![]));
        assert_eq!(subs.index(vec![camera("a", 11)]), None);
        assert_eq!(subs.receiving(), 1);

        let placed = subs.place(&offer(&[("1", "sendonly"), ("2", "recvonly")]));
        assert_eq!(placed.receive, vec![0, 11]);
        assert_eq!(
            placed.subscriptions,
            vec![Subscription { mid: "2".into(), attendee: "a".into(), kind: VideoKind::Camera }]
        );
        assert_eq!(
            subs.remote(),
            vec![RemoteVideo { track: "v_2".into(), member: "T-R-a".into(), kind: VideoKind::Camera }]
        );

        subs.index(vec![camera("b", 21), camera("a", 11)]);
        let placed = subs.place(&offer(&[("1", "sendonly"), ("2", "recvonly"), ("3", "recvonly")]));
        assert_eq!(placed.receive, vec![0, 11, 21]);

        assert_eq!(subs.index(vec![camera("b", 21)]), Some(vec!["2".to_owned()]));
        let placed = subs.place(&offer(&[("1", "sendonly"), ("2", "recvonly"), ("3", "recvonly")]));
        assert_eq!(placed.receive, vec![0, 0, 21]);
        assert_eq!(subs.remote().len(), 1);
    }

    #[test]
    fn videos_past_the_limit_or_the_sections_wait() {
        let mut subs = Subscriptions::new(1);
        subs.index(vec![camera("a", 11), camera("b", 21)]);
        assert_eq!(subs.receiving(), 1);
        let mut subs = Subscriptions::new(25);
        subs.index(vec![camera("a", 11), camera("b", 21)]);
        assert_eq!(subs.place(&offer(&[("1", "recvonly")])).receive, vec![11]);
        assert_eq!(subs.place(&offer(&[("1", "recvonly"), ("2", "recvonly")])).receive, vec![11, 21]);
    }

    #[test]
    fn an_offer_sends_video_from_a_sending_video_section_only() {
        assert!(!sends_video(&offer(&[("1", "inactive"), ("2", "recvonly")])));
        assert!(sends_video(&offer(&[("1", "sendonly")])));
        assert!(!sends_video("v=0\r\nm=audio 9 X 111\r\na=sendrecv\r\n"));
    }
}
