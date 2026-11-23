//! Teams' SDP dialect against plain WebRTC's (brain/architecture/native-calls.md):
//! the offers and answers the calling stack carries, and what libwebrtc reads.
//!
//! Teams names a section by its `a=label` (`main-audio`, `main-video` for the
//! cameras, `applicationsharing-video` for a shared screen, `data`) and renumbers
//! the `a=mid`s from one offer to the next, while libwebrtc keeps every section
//! it negotiated in its place under its own mid. A [`Layout`] holds a call's plain
//! sections across its negotiations: the audio stays in its section whatever mid
//! Teams gives it, a video section under a mid the call has not seen becomes a
//! section of its own (the other side's packets name that mid), and a section
//! Teams no longer lists is refused.
//!
//! On Teams' side: the profile is written `RTP/SAVP` and extension URIs with
//! backslashes, each section carries its label and the range of the source it
//! sends, the media line names a real address taken from a candidate (port 9 and
//! `0.0.0.0` read as disabled media, `DisabledMediaNotSupported`), and only UDP
//! candidates, without the extras Teams does not parse, go either way.

use std::collections::HashMap;

/// What a Teams section carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Audio,
    Camera,
    Screen,
}

impl Label {
    fn name(self) -> &'static str {
        match self {
            Label::Audio => "main-audio",
            Label::Camera => "main-video",
            Label::Screen => "applicationsharing-video",
        }
    }

    fn of(name: &str) -> Option<Label> {
        [Label::Audio, Label::Camera, Label::Screen].into_iter().find(|l| l.name() == name)
    }
}

/// What this side sends now: a section of its own camera or screen that sends
/// nothing is told to Teams as not sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sending {
    pub camera: bool,
    pub screen: bool,
}

/// The stream id the sidecar gives this side's camera and screen tracks.
const CAMERA_STREAM: &str = "-camera";
const SCREEN_STREAM: &str = "-screen";

/// A call's sections across its negotiations.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// The plain SDP libwebrtc last took from the other side, offer or answer:
    /// its sections are the call's, in their order.
    remote: String,
    /// This side's plain offer, waiting for Teams' answer.
    offered: Option<String>,
    /// What each plain video section carries, by its mid.
    labels: HashMap<String, Label>,
    /// The plain mid of each section of Teams' last offer, by Teams' mid.
    mids: HashMap<String, String>,
}

impl Layout {
    /// Whether an offer of this side's waits for Teams' answer.
    pub fn offering(&self) -> bool {
        self.offered.is_some()
    }

    /// Forgets this side's offer: Teams offered meanwhile, and its answer, should
    /// it come, answers an offer the sidecar rolled back.
    pub fn withdraw(&mut self) {
        self.offered = None;
    }
}

struct Section<'a> {
    lines: Vec<&'a str>,
}

const DIRECTIONS: [&str; 4] = ["sendrecv", "sendonly", "recvonly", "inactive"];

impl<'a> Section<'a> {
    fn field(&self, at: usize) -> &'a str {
        self.lines[0].strip_prefix("m=").unwrap_or("").split(' ').nth(at).unwrap_or("")
    }

    fn kind(&self) -> &'a str {
        self.field(0)
    }

    fn open(&self) -> bool {
        self.field(1) != "0"
    }

    /// The payload types, after the profile.
    fn formats(&self) -> &'a str {
        self.lines[0].splitn(4, ' ').nth(3).unwrap_or("")
    }

    fn mid(&self) -> Option<&'a str> {
        self.lines.iter().find_map(|l| l.strip_prefix("a=mid:"))
    }

    fn direction(&self) -> &'a str {
        self.lines
            .iter()
            .find_map(|l| DIRECTIONS.into_iter().find(|d| l.strip_prefix("a=") == Some(*d)))
            .unwrap_or("sendrecv")
    }

    fn label(&self) -> Option<Label> {
        self.lines.iter().find_map(|l| Label::of(l.strip_prefix("a=label:")?))
    }

    fn stream(&self) -> Option<&'a str> {
        self.lines.iter().find_map(|l| l.strip_prefix("a=msid:")?.split(' ').next())
    }

    /// This side's own camera or screen, by the stream id the sidecar gives it.
    fn own(&self) -> Option<Label> {
        let stream = self.stream()?;
        if stream.ends_with(CAMERA_STREAM) {
            Some(Label::Camera)
        } else if stream.ends_with(SCREEN_STREAM) {
            Some(Label::Screen)
        } else {
            None
        }
    }

    fn ssrc(&self) -> Option<&'a str> {
        self.lines.iter().find_map(|l| l.strip_prefix("a=ssrc:")?.split(' ').next())
    }
}

/// An SDP's session lines and its sections.
fn split(sdp: &str) -> (Vec<&str>, Vec<Section<'_>>) {
    let mut head = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            sections.push(Section { lines: vec![line] });
        } else if let Some(section) = sections.last_mut() {
            section.lines.push(line);
        } else {
            head.push(line);
        }
    }
    (head, sections)
}

/// Whether the side that wrote this direction sends.
fn sends(direction: &str) -> bool {
    matches!(direction, "sendrecv" | "sendonly")
}

/// The same direction without this side sending.
fn quiet(direction: &str) -> &'static str {
    match direction {
        "sendrecv" | "recvonly" => "recvonly",
        _ => "inactive",
    }
}

/// The stream id a video section the other side sends gets on this side.
fn remote_stream(mid: &str) -> String {
    format!("teams-{mid}")
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

/// The port and address a local SDP's media lines name: its first UDP IPv4
/// candidate's.
fn media_address(sdp: &str) -> (String, String) {
    let found = sdp.lines().filter_map(candidate_fields).find(|c| c.udp && !c.address.contains(':'));
    match found {
        Some(c) => (c.port.to_owned(), c.address.to_owned()),
        None => ("9".to_owned(), "0.0.0.0".to_owned()),
    }
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

/// A section of a local SDP in Teams' dialect, as `label` under Teams' `mid`;
/// `direction` replaces its own.
fn teams_section(
    section: &Section,
    label: Label,
    mid: &str,
    (port, address): (&str, &str),
    direction: Option<&str>,
) -> String {
    let mut out = String::new();
    let mut push = |line: &str| {
        out.push_str(line);
        out.push_str("\r\n");
    };
    push(&format!("m={} {port} RTP/SAVP {}", section.kind(), section.formats()));
    for line in &section.lines[1..] {
        if line.starts_with("c=") {
            push(&format!("c=IN IP4 {address}"));
        } else if line.starts_with("a=rtcp:") {
            push(&format!("a=rtcp:{port}"));
        } else if line.starts_with("a=extmap:") {
            push(&line.replace('/', "\\"));
        } else if *line == "a=rtcp-rsize" && label == Label::Audio {
        } else if line.starts_with("a=candidate:") {
            let Some(c) = candidate_fields(line).filter(|c| c.udp) else { continue };
            push(&format!("a=candidate:{}", c.plain));
        } else if line.starts_with("a=mid:") {
            push(&format!("a=mid:{mid}"));
            push(&format!("a=label:{}", label.name()));
            push(if label == Label::Audio {
                "a=x-signaling-fb:* x-message app recv:dsh"
            } else {
                "a=x-signaling-fb:* x-message app send:src recv:src,vc"
            });
            if let Some(ssrc) = section.ssrc() {
                push(&format!("a=x-ssrc-range:{ssrc}-{ssrc}"));
            }
        } else if let Some(direction) =
            direction.filter(|_| DIRECTIONS.iter().any(|d| line.strip_prefix("a=") == Some(*d)))
        {
            push(&format!("a={direction}"));
        } else {
            push(line);
        }
    }
    out
}

/// What this side tells Teams of a section of its own camera or screen that
/// sends nothing now.
fn told_direction(section: &Section, sending: Sending) -> Option<&'static str> {
    let quiet_now = match section.own()? {
        Label::Camera => !sending.camera,
        Label::Screen => !sending.screen,
        Label::Audio => false,
    };
    quiet_now.then(|| quiet(section.direction()))
}

/// A section of Teams' in plain WebRTC under `mid`: the profile and extension URIs
/// libwebrtc reads (an extension `keep` refuses left out), UDP candidates without
/// their extras, nothing of Teams' own; a video section the other side sends
/// carries `stream`.
fn plain_section(section: &Section, mid: &str, stream: Option<&str>, keep: &dyn Fn(&str) -> bool) -> String {
    let video = section.kind() == "video";
    let mut out = String::new();
    let mut push = |line: &str| {
        out.push_str(line);
        out.push_str("\r\n");
    };
    push(&format!("m={} {} UDP/TLS/RTP/SAVPF {}", section.kind(), section.field(1), section.formats()));
    for line in &section.lines[1..] {
        if line.starts_with("a=x-") || line.starts_with("a=label:") || line.starts_with("a=ptime:") {
            continue;
        }
        if line.starts_with("a=mid:") {
            push(&format!("a=mid:{mid}"));
            if let Some(stream) = stream {
                push(&format!("a=msid:{stream} {stream}"));
            }
            continue;
        }
        if video
            && (line.starts_with("a=msid:")
                || line.strip_prefix("a=ssrc:").and_then(|r| r.split_once(' ')).is_some_and(|(_, attribute)| {
                    ["msid:", "mslabel:", "label:"].iter().any(|a| attribute.starts_with(a))
                }))
        {
            continue;
        }
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            let fields: Vec<&str> = candidate.split(' ').collect();
            if fields.get(2).is_none_or(|p| !p.eq_ignore_ascii_case("udp")) {
                continue;
            }
            let end = fields.iter().position(|f| *f == "MTURNID").unwrap_or(fields.len());
            push(&format!("a=candidate:{}", fields[..end].join(" ")));
            continue;
        }
        if line.starts_with("a=extmap:") {
            let line = line.replace('\\', "/");
            if keep(&line) {
                push(&line);
            }
            continue;
        }
        push(line);
    }
    out
}

/// A plain section refused: port 0, nothing sent or received.
fn closed(section: &Section) -> String {
    let mut out = format!("m={} 0 {} {}\r\n", section.kind(), section.field(2), section.formats());
    for line in &section.lines[1..] {
        if line.starts_with("a=msid:") || line.starts_with("a=candidate:") || line.starts_with("a=ssrc") {
            continue;
        }
        let line = if DIRECTIONS.iter().any(|d| line.strip_prefix("a=") == Some(*d)) { "a=inactive" } else { line };
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// Teams' session lines in plain WebRTC, bundling `mids`.
fn plain_head(head: &[&str], mids: &[String]) -> String {
    let mut out = String::new();
    for line in head {
        if line.starts_with("a=group:") || line.starts_with("a=x-") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
        if line.starts_with("t=") {
            out.push_str(&format!("a=group:BUNDLE {}\r\n", mids.join(" ")));
        }
    }
    out
}

/// A local SDP's session lines for Teams, bundling `mids`.
fn teams_head(head: &[&str], mids: &[&str]) -> String {
    let mut out = String::new();
    for line in head {
        if line.starts_with("a=group:BUNDLE") {
            out.push_str(&format!("a=group:BUNDLE {}\r\n", mids.join(" ")));
        } else {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out
}

/// This side's plain offer in Teams' dialect: its audio, the camera section and
/// the screen section, a section of this side's own taking the label over from
/// one that only received; a refused section is left out. The offer waits in
/// `layout` for Teams' answer ([`plain_answer`]).
pub fn teams_offer(sdp: &str, sending: Sending, layout: &mut Layout) -> String {
    let (head, sections) = split(sdp);
    let (port, address) = media_address(sdp);
    let mut kept: Vec<(usize, Label)> = Vec::new();
    let mut audio = false;
    for (at, section) in sections.iter().enumerate() {
        if !section.open() {
            continue;
        }
        let label = match section.kind() {
            "audio" if !audio => {
                audio = true;
                Label::Audio
            }
            "video" => section
                .own()
                .or_else(|| section.mid().and_then(|m| layout.labels.get(m).copied()))
                .unwrap_or(Label::Camera),
            _ => continue,
        };
        match kept.iter().position(|(_, l)| *l == label) {
            Some(i) if section.own().is_some() => kept[i] = (at, label),
            Some(_) => {}
            None => kept.push((at, label)),
        }
    }
    let mut mids = Vec::new();
    let mut body = String::new();
    for (at, label) in &kept {
        let section = &sections[*at];
        let Some(mid) = section.mid() else { continue };
        if *label != Label::Audio {
            layout.labels.insert(mid.to_owned(), *label);
        }
        mids.push(mid);
        body.push_str(&teams_section(section, *label, mid, (&port, &address), told_direction(section, sending)));
    }
    layout.offered = Some(sdp.to_owned());
    teams_head(&head, &mids) + &body
}

/// Teams' answer to this side's offer waiting in `layout`, in plain WebRTC: a
/// section for each of the offer's, refused where Teams' answer has none. `None`
/// with no offer waiting, or no audio answered.
pub fn plain_answer(sdp: &str, layout: &mut Layout) -> Option<String> {
    let offer = layout.offered.take()?;
    let (head, answered) = split(sdp);
    let (_, offered) = split(&offer);
    let mut body = String::new();
    let mut bundle = Vec::new();
    let mut audio = false;
    for section in &offered {
        let mid = section.mid().unwrap_or("");
        let answer = answered
            .iter()
            .filter(|a| a.open() && section.open() && a.kind() == section.kind())
            .find(|a| a.mid() == Some(mid))
            .or_else(|| {
                answered.iter().find(|a| a.open() && section.open() && a.kind() == "audio" && section.kind() == "audio")
            })
            .filter(|_| section.kind() != "audio" || !audio);
        match answer {
            Some(a) if matches!(a.kind(), "audio" | "video") => {
                audio |= a.kind() == "audio";
                let stream = (a.kind() == "video" && sends(a.direction())).then(|| remote_stream(mid));
                body.push_str(&plain_section(a, mid, stream.as_deref(), &|_| true));
                bundle.push(mid.to_owned());
            }
            _ => body.push_str(&closed(section)),
        }
    }
    if !audio {
        return None;
    }
    let plain = plain_head(&head, &bundle) + &body;
    layout.remote = plain.clone();
    Some(plain)
}

/// An offer of Teams' in plain WebRTC, made to fit the call's sections: each the
/// call has keeps its place, the audio its mid whatever Teams numbers it, a video
/// section under a new mid comes after them, and one Teams no longer lists is
/// refused. libwebrtc also refuses an extension id given another URI (Teams' hold
/// offer moves id 3 from transport-wide-cc to its own feedback extension): such an
/// extension is left out, so the answer declines it. `None` without audio.
pub fn plain_offer(sdp: &str, layout: &mut Layout) -> Option<String> {
    let previous = std::mem::take(&mut layout.remote);
    let (_, slots) = split(&previous);
    let active = extensions(&previous);
    let keep = |line: &str| {
        extension(line)
            .is_none_or(|(id, uri)| !active.iter().any(|&(other_id, other_uri)| (other_id == id) != (other_uri == uri)))
    };
    let (head, sections) = split(sdp);
    let mut out: Vec<(String, String, bool)> =
        slots.iter().map(|s| (s.mid().unwrap_or("").to_owned(), closed(s), false)).collect();
    let audio_slot = slots.iter().position(|s| s.kind() == "audio");
    let mut audio = false;
    layout.mids.clear();
    for section in &sections {
        let Some(theirs) = section.mid() else { continue };
        if !section.open() {
            continue;
        }
        let (slot, mid, label) = match section.kind() {
            "audio" if !audio => {
                audio = true;
                match audio_slot {
                    Some(i) => (Some(i), out[i].0.clone(), Label::Audio),
                    None => (None, theirs.to_owned(), Label::Audio),
                }
            }
            "video" => {
                let label = section.label().unwrap_or(Label::Camera);
                match slots.iter().position(|s| s.mid() == Some(theirs)) {
                    Some(i) if slots[i].kind() == "video" => (Some(i), theirs.to_owned(), label),
                    Some(_) => (None, format!("{theirs}v"), label),
                    None => (None, theirs.to_owned(), label),
                }
            }
            _ => continue,
        };
        layout.mids.insert(theirs.to_owned(), mid.clone());
        if label != Label::Audio {
            layout.labels.insert(mid.clone(), label);
        }
        let stream = (section.kind() == "video" && sends(section.direction())).then(|| remote_stream(&mid));
        let text = plain_section(section, &mid, stream.as_deref(), &keep);
        match slot {
            Some(i) => out[i] = (mid, text, true),
            None => out.push((mid, text, true)),
        }
    }
    if !audio {
        return None;
    }
    let bundle: Vec<String> = out.iter().filter(|(_, _, open)| *open).map(|(mid, _, _)| mid.clone()).collect();
    let plain = plain_head(&head, &bundle) + &out.into_iter().map(|(_, text, _)| text).collect::<String>();
    layout.remote = plain.clone();
    layout.offered = None;
    Some(plain)
}

/// A local answer to Teams' `offer` (made plain by [`plain_offer`]) in Teams'
/// dialect: the offer's sections in its order under its mids, each with no plain
/// answer refused with port 0, as its own client does.
pub fn teams_answer(answer: &str, offer: &str, sending: Sending, layout: &Layout) -> String {
    let (head, answered) = split(answer);
    let (port, address) = media_address(answer);
    let (_, offered) = split(offer);
    let mut body = String::new();
    let mut bundle = Vec::new();
    for section in &offered {
        let theirs = section.mid().unwrap_or("");
        let plain = layout.mids.get(theirs);
        let found = plain.and_then(|mid| answered.iter().find(|a| a.open() && a.mid() == Some(mid.as_str())));
        match found {
            Some(a) => {
                let label = match a.kind() {
                    "audio" => Label::Audio,
                    _ => plain.and_then(|mid| layout.labels.get(mid)).copied().unwrap_or(Label::Camera),
                };
                bundle.push(theirs);
                body.push_str(&teams_section(a, label, theirs, (&port, &address), told_direction(a, sending)));
            }
            None => {
                let mut fields = section.lines[0].splitn(3, ' ');
                let (kind, _, rest) = (fields.next().unwrap_or(""), fields.next(), fields.next().unwrap_or(""));
                body.push_str(&format!("{kind} 0 {rest}\r\n"));
                if section.mid().is_some() {
                    body.push_str(&format!("a=mid:{theirs}\r\n"));
                }
            }
        }
    }
    teams_head(&head, &bundle) + &body
}

/// The videos the other side sends now: each section's stream id on this side
/// and what it carries.
pub fn remote_videos(layout: &Layout) -> Vec<(String, Label)> {
    split(&layout.remote)
        .1
        .iter()
        .filter(|s| s.kind() == "video" && s.open())
        .filter_map(|s| {
            let (stream, mid) = (s.stream()?, s.mid()?);
            Some((stream.to_owned(), layout.labels.get(mid).copied().unwrap_or(Label::Camera)))
        })
        .collect()
}

/// The call's modalities as Teams' requests name them.
pub fn modalities(layout: &Layout, sending: Sending) -> Vec<&'static str> {
    let remote = remote_videos(layout);
    let mut out = vec!["Audio"];
    if sending.camera || remote.iter().any(|(_, l)| *l == Label::Camera) {
        out.push("Video");
    }
    if sending.screen {
        out.push("ScreenSharer");
    } else if remote.iter().any(|(_, l)| *l == Label::Screen) {
        out.push("ScreenViewer");
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_mid(sdp: &str) -> Option<&str> {
        split(sdp).1.iter().find(|s| s.kind() == "audio").and_then(Section::mid)
    }

    fn section(sdp: &str, mid: &str) -> String {
        split(sdp).1.iter().find(|s| s.mid() == Some(mid)).map(|s| s.lines.join("\n")).unwrap_or_default()
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

    /// An offer of Teams' with the other side's camera on and its screen shared.
    const VIDEO_OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 3\r\n\
        m=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\na=sendrecv\r\na=label:main-audio\r\n\
        m=video 3480 RTP/SAVP 102 103\r\nc=IN IP4 52.1.2.3\r\na=x-signaling-fb:* x-message app send:src recv:src,vc\r\n\
        a=mid:1\r\na=msid:abc def\r\na=sendrecv\r\na=rtcp-rsize\r\na=rtpmap:102 H264/90000\r\na=rtpmap:103 rtx/90000\r\n\
        a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=ssrc:7 cname:c\r\na=ssrc:7 msid:abc def\r\na=label:main-video\r\n\
        m=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:2\r\na=sendonly\r\na=rtpmap:102 H264/90000\r\n\
        a=label:applicationsharing-video\r\n\
        m=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\na=sendrecv\r\na=label:data\r\n";

    /// libwebrtc's answer to that offer made plain.
    const VIDEO_ANSWER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\na=ssrc:42 cname:x\r\n\
        a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 102 103\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=msid:ep-camera cam\r\na=sendrecv\r\n\
        a=rtcp-rsize\r\na=ssrc:51 cname:x\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=recvonly\r\n";

    #[test]
    fn an_offer_speaks_teams_dialect() {
        let mut layout = Layout::default();
        let offer = teams_offer(OFFER, Sending::default(), &mut layout);
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
        assert!(offer.contains("a=group:BUNDLE 0\r\n"));
        assert!(layout.offering());
    }

    #[test]
    fn an_answer_keeps_plain_webrtc_audio() {
        let mut layout = Layout::default();
        assert_eq!(plain_answer(ANSWER, &mut layout), None, "no offer waits");
        teams_offer(OFFER, Sending::default(), &mut layout);
        let answer = plain_answer(ANSWER, &mut layout).unwrap();
        assert!(!layout.offering());
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
        teams_offer(OFFER, Sending::default(), &mut layout);
        assert_eq!(plain_answer("v=0\r\nm=video 0 RTP/SAVP 1\r\n", &mut layout), None);
    }

    #[test]
    fn a_new_offer_keeps_the_calls_audio_mid_and_its_answer_the_offers() {
        let offer = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 7 8\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\n\
            a=inactive\r\na=mid:7\r\nm=video 0 RTP/SAVP 34\r\na=mid:8\r\n";
        let mut layout = Layout {
            remote: "v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n".into(),
            ..Layout::default()
        };
        let plain = plain_offer(offer, &mut layout).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=group:BUNDLE 0\r\n") && plain.contains("a=inactive") && !plain.contains("m=video"));
        let local = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=inactive\r\na=mid:0\r\n";
        let answer = teams_answer(local, offer, Sending::default(), &layout);
        assert_eq!(audio_mid(&answer), Some("7"));
        assert!(answer.contains("a=group:BUNDLE 7\r\n") && answer.contains("m=video 0 ") && answer.contains("a=mid:8"));
        assert_eq!(sdp_sections(offer), "audio port 5 inactive mid 7; video port 0 mid 8");
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
        let mut layout = Layout { remote: negotiated.into(), ..Layout::default() };
        let plain = plain_offer(offer, &mut layout).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n"));
        assert!(plain.contains("a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n"));
        assert!(!plain.contains("a=extmap:3 ") && !plain.contains("a=extmap:5 "), "{plain}");
        let fresh = plain_offer(offer, &mut Layout::default()).unwrap();
        assert_eq!(fresh.matches("a=extmap:").count(), 4);
    }

    #[test]
    fn an_offer_from_teams_is_answered_in_its_order() {
        let offer = "v=0\r\nt=0 0\r\nm=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\n\
            m=video 0 RTP/SAVP 102\r\na=mid:1\r\nm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\n";
        let mut layout = Layout::default();
        assert!(plain_offer(offer, &mut layout).unwrap().contains("m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n"));
        let local = "v=0\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=mid:0\r\na=sendonly\r\na=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n";
        let answer = teams_answer(local, offer, Sending::default(), &layout);
        assert!(answer.contains("m=audio 222 RTP/SAVP 111\r\n"), "{answer}");
        assert!(answer.contains("a=sendonly\r\n"));
        assert!(answer.contains("m=video 0 RTP/SAVP 102\r\na=mid:1\r\n"), "{answer}");
        assert!(answer.contains("m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n"), "{answer}");
        assert!(answer.find("m=audio").unwrap() < answer.find("m=video").unwrap());
    }

    #[test]
    fn the_other_sides_camera_and_screen_are_received_under_streams_of_their_own() {
        let mut layout = Layout::default();
        let plain = plain_offer(VIDEO_OFFER, &mut layout).unwrap();
        assert!(plain.contains("a=group:BUNDLE 0 1 2\r\n"), "{plain}");
        assert!(plain.contains("m=video 3480 UDP/TLS/RTP/SAVPF 102 103\r\n"), "{plain}");
        let camera = section(&plain, "1");
        assert!(camera.contains("a=msid:teams-1 teams-1") && !camera.contains("abc"), "{camera}");
        assert!(camera.contains("a=ssrc:7 cname:c") && camera.contains("a=rtcp-rsize"), "{camera}");
        assert!(!plain.contains("x-data") && !plain.contains("a=label") && !plain.contains("a=x-"), "{plain}");
        assert_eq!(
            remote_videos(&layout),
            vec![("teams-1".to_owned(), Label::Camera), ("teams-2".to_owned(), Label::Screen)]
        );
        assert_eq!(modalities(&layout, Sending::default()), ["Audio", "Video", "ScreenViewer"]);

        let answer = teams_answer(VIDEO_ANSWER, VIDEO_OFFER, Sending::default(), &layout);
        assert!(answer.contains("a=group:BUNDLE 0 1 2\r\n"), "{answer}");
        let camera = section(&answer, "1");
        assert!(camera.starts_with("m=video 222 RTP/SAVP 102 103"), "{camera}");
        assert!(camera.contains("a=label:main-video") && camera.contains("a=recvonly"), "camera off: {camera}");
        assert!(camera.contains("a=x-ssrc-range:51-51") && camera.contains("a=rtcp-rsize"), "{camera}");
        assert!(section(&answer, "2").contains("a=label:applicationsharing-video"), "{answer}");
        assert!(answer.contains("m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n"), "{answer}");
        let on = teams_answer(VIDEO_ANSWER, VIDEO_OFFER, Sending { camera: true, screen: false }, &layout);
        assert!(section(&on, "1").contains("a=sendrecv"), "{on}");
    }

    #[test]
    fn a_video_section_teams_renumbers_becomes_a_new_one() {
        let mut layout = Layout::default();
        plain_offer(VIDEO_OFFER, &mut layout).unwrap();
        let moved = VIDEO_OFFER.replace("a=mid:2\r\n", "a=mid:5\r\n").replace("BUNDLE 0 1 2 3", "BUNDLE 0 1 5 3");
        let plain = plain_offer(&moved, &mut layout).unwrap();
        assert!(plain.contains("a=group:BUNDLE 0 1 5\r\n"), "{plain}");
        assert!(section(&plain, "2").starts_with("m=video 0 "), "the old one refused: {plain}");
        assert!(section(&plain, "2").contains("a=inactive"), "{plain}");
        assert!(plain.find("a=mid:2").unwrap() < plain.find("a=mid:5").unwrap(), "in its place: {plain}");
        assert_eq!(
            remote_videos(&layout),
            vec![("teams-1".to_owned(), Label::Camera), ("teams-5".to_owned(), Label::Screen)]
        );
        let answered = VIDEO_ANSWER.replace("BUNDLE 0 1 2", "BUNDLE 0 1 5").replace(
            "m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\n",
            "m=video 0 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=mid:5\r\n",
        );
        let answer = teams_answer(&answered, &moved, Sending::default(), &layout);
        assert!(section(&answer, "5").contains("a=label:applicationsharing-video"), "{answer}");
        assert!(!answer.contains("a=mid:2"), "{answer}");
    }

    #[test]
    fn this_sides_screen_takes_the_label_over_and_a_stopped_one_says_so() {
        let mut layout = Layout::default();
        plain_offer(VIDEO_OFFER, &mut layout).unwrap();
        let ours = "v=0\r\no=- 1 3 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 2 6\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendrecv\r\n\
            a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n\
            m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=msid:ep-camera cam\r\na=sendrecv\r\n\
            m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=recvonly\r\n\
            m=video 9 UDP/TLS/RTP/SAVPF 102\r\nc=IN IP4 0.0.0.0\r\na=mid:6\r\na=msid:ep-screen scr\r\na=sendonly\r\n\
            a=ssrc:61 cname:x\r\n";
        let sending = Sending { camera: false, screen: true };
        let offer = teams_offer(ours, sending, &mut layout);
        assert!(offer.contains("a=group:BUNDLE 0 1 6\r\n"), "{offer}");
        assert!(!offer.contains("a=mid:2"), "the screen it received gives way: {offer}");
        assert!(section(&offer, "6").contains("a=label:applicationsharing-video"), "{offer}");
        assert!(section(&offer, "6").contains("a=sendonly") && section(&offer, "6").contains("x-ssrc-range:61-61"));
        assert!(section(&offer, "1").contains("a=recvonly"), "a camera sending nothing: {offer}");
        assert_eq!(modalities(&layout, sending), ["Audio", "Video", "ScreenSharer"]);

        let answer = "v=0\r\no=- 9 9 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1 6\r\n\
            m=audio 3480 RTP/SAVP 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\na=sendrecv\r\n\
            m=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:1\r\na=sendonly\r\n\
            m=video 3480 RTP/SAVP 102\r\nc=IN IP4 52.1.2.3\r\na=mid:6\r\na=recvonly\r\n";
        let plain = plain_answer(answer, &mut layout).unwrap();
        assert!(plain.contains("a=group:BUNDLE 0 1 6\r\n"), "{plain}");
        assert!(section(&plain, "2").starts_with("m=video 0 "), "{plain}");
        assert!(section(&plain, "1").contains("a=msid:teams-1 teams-1"), "{plain}");
        assert!(!section(&plain, "6").contains("a=msid"), "{plain}");
        assert_eq!(remote_videos(&layout), vec![("teams-1".to_owned(), Label::Camera)]);
        let stopped = teams_offer(ours, Sending::default(), &mut layout);
        assert!(section(&stopped, "6").contains("a=inactive"), "{stopped}");
    }
}
