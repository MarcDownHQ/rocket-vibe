//! The XMPP connection and the Jingle session of a joined conference.

use std::collections::{BTreeMap, HashMap};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::Config;
use super::sdp::{self, Answer, JINGLE, LocalStream, Offer};
use crate::xml::Element;
use crate::{
    CallSignaling, Candidate, Error, Event, IceServer, LocalVideo, Member, RemoteVideo, VideoKind, random_hex,
};

const CLIENT: &str = "jabber:client";
const FRAMING: &str = "urn:ietf:params:xml:ns:xmpp-framing";
const STREAMS: &str = "http://etherx.jabber.org/streams";
const SASL: &str = "urn:ietf:params:xml:ns:xmpp-sasl";
const BIND: &str = "urn:ietf:params:xml:ns:xmpp-bind";
const MUC: &str = "http://jabber.org/protocol/muc";
const MUC_USER: &str = "http://jabber.org/protocol/muc#user";
const NICK: &str = "http://jabber.org/protocol/nick";
const DISCO_INFO: &str = "http://jabber.org/protocol/disco#info";
const PING: &str = "urn:xmpp:ping";
const EXTDISCO: &str = "urn:xmpp:extdisco:2";
const FOCUS: &str = "http://jitsi.org/protocol/focus";
const SCTP: &str = "urn:xmpp:jingle:transports:dtls-sctp:1";

/// How often the server is pinged, and how long without a frame from it means
/// the socket is gone.
const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(25);
const SILENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// `RV_CALL_DEBUG=1` prints each Jingle action and why a session ended, to stderr.
fn debug(what: &str) {
    if matches!(std::env::var("RV_CALL_DEBUG").as_deref(), Ok("1" | "2")) {
        eprintln!("rv-call: {what}");
    }
}

/// `RV_CALL_DEBUG=2` prints the Jingle stanzas themselves too.
fn trace(what: &str) {
    if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("2") {
        eprintln!("rv-call: {what}");
    }
}

/// What this client can do, as Jicofo reads it (disco#info): audio and video
/// over ICE and DTLS-SRTP, bundled and multiplexed, with source names and a
/// camera and a screen per participant, transport-wide congestion control (the
/// bridge forwards no video to an endpoint it cannot estimate) and
/// retransmissions, and SCTP: the client adds the bridge channel's section
/// itself (the bridge then forwards no video until asked over it).
const FEATURES: [&str; 16] = [
    DISCO_INFO,
    "urn:xmpp:jingle:1",
    "urn:xmpp:jingle:apps:rtp:1",
    "urn:xmpp:jingle:transports:ice-udp:1",
    "urn:xmpp:jingle:apps:dtls:0",
    "urn:xmpp:jingle:apps:rtp:audio",
    "urn:xmpp:jingle:apps:rtp:video",
    "http://jitsi.org/receive-multiple-video-streams",
    "http://jitsi.org/tcc",
    "urn:ietf:rfc:4588",
    "urn:xmpp:jingle:transports:dtls-sctp:1",
    "urn:xmpp:jingle:apps:rtp:rtp-hdrext:0",
    "urn:xmpp:jingle:apps:grouping:0",
    "urn:ietf:rfc:5761",
    "urn:ietf:rfc:5888",
    "http://jitsi.org/source-name",
];

enum Command {
    Answer(String),
    Candidate(Candidate),
    Muted(bool),
    Video(LocalVideo),
    Leave(oneshot::Sender<()>),
}

/// The height asked of the bridge for a camera, and for a screen.
const CAMERA_HEIGHT: u32 = 720;
const SCREEN_HEIGHT: u32 = 1080;

/// A joined Jitsi conference.
pub struct Session {
    events: mpsc::UnboundedReceiver<Event>,
    commands: mpsc::UnboundedSender<Command>,
    endpoint: String,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The bridge's `ReceiverVideoConstraints`: every camera at most
/// [`CAMERA_HEIGHT`], the screens named at [`SCREEN_HEIGHT`] (the bridge's
/// default is a 180-line thumbnail).
fn constraints(screens: &[String]) -> String {
    let screens: serde_json::Map<String, serde_json::Value> =
        screens.iter().map(|name| (name.clone(), serde_json::json!({ "maxHeight": SCREEN_HEIGHT }))).collect();
    serde_json::json!({
        "colibriClass": "ReceiverVideoConstraints",
        "lastN": -1,
        "defaultConstraints": { "maxHeight": CAMERA_HEIGHT },
        "constraints": screens,
    })
    .to_string()
}

/// The bridge channel over its WebSocket, on a task of its own: what is sent
/// goes out (the latest constraints first on opening), what the bridge says is
/// read and dropped. It closes with the sender.
fn bridge_channel(url: String) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = tokio_tungstenite::connect_async(url.as_str()).await else {
            debug("bridge channel: not opened");
            return;
        };
        debug("bridge channel open");
        let _ = socket.send(Message::text(constraints(&[]))).await;
        loop {
            tokio::select! {
                message = rx.recv() => match message {
                    Some(text) => {
                        if socket.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = socket.close(None).await;
                        break;
                    }
                },
                frame = socket.next() => match frame {
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
        debug("bridge channel closed");
    });
    tx
}

/// Connects, logs in, asks the focus for the conference and joins its room.
pub async fn join(config: Config) -> Result<Session, Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut request = config.socket_url().into_client_request().map_err(|e| Error::new(format!("url: {e}")))?;
    request.headers_mut().insert("Sec-WebSocket-Protocol", "xmpp".parse().expect("static header"));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(request).await.map_err(|e| Error::new(format!("connect: {e}")))?;

    open(&mut socket, &config.domain).await?;
    let features = expect(&mut socket, |e| e.is("features", STREAMS)).await?;
    let mechanisms: Vec<String> = features
        .find("mechanisms", SASL)
        .map(|m| m.find_all("mechanism", SASL).map(|e| e.content()).collect())
        .unwrap_or_default();
    if !mechanisms.iter().any(|m| m == "ANONYMOUS") {
        return Err(Error::new("no anonymous login"));
    }
    send(&mut socket, &Element::new("auth", SASL).attr("mechanism", "ANONYMOUS")).await?;
    let outcome = expect(&mut socket, |e| e.ns == SASL).await?;
    if outcome.name != "success" {
        return Err(Error::new("login refused"));
    }
    open(&mut socket, &config.domain).await?;
    expect(&mut socket, |e| e.is("features", STREAMS)).await?;
    let bind = iq("set", &config.domain).child(Element::new("bind", BIND));
    let bound = request_iq(&mut socket, bind).await?;
    let jid = bound
        .find("bind", BIND)
        .and_then(|b| b.find("jid", BIND))
        .map(Element::content)
        .ok_or_else(|| Error::new("no jid bound"))?;

    let ice = external_services(&mut socket, &config.domain).await;

    // Jicofo may take a moment to set the conference up: it says `ready='false'` until then.
    for attempt in 0..10 {
        let conference = iq("set", &config.focus).child(
            Element::new("conference", FOCUS)
                .attr("room", config.room_jid())
                .attr("machine-uid", random_hex(16))
                .child(Element::new("property", FOCUS).attr("name", "rtcpmux").attr("value", "true"))
                .child(Element::new("property", FOCUS).attr("name", "bundle").attr("value", "true"))
                .child(Element::new("property", FOCUS).attr("name", "startAudioMuted").attr("value", "9"))
                .child(Element::new("property", FOCUS).attr("name", "startVideoMuted").attr("value", "9")),
        );
        let answer = request_iq(&mut socket, conference).await?;
        if answer.find("conference", FOCUS).and_then(|c| c.get("ready")) != Some("false") {
            break;
        }
        if attempt == 9 {
            return Err(Error::new("conference not ready"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let endpoint = random_hex(4);
    let (event_tx, events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let _ = event_tx.send(Event::IceServers(ice));
    let mut state = State {
        config,
        jid,
        endpoint: endpoint.clone(),
        muted: false,
        offer: None,
        initiator: None,
        answer: None,
        pending: Vec::new(),
        candidates_sent: 0,
        members: BTreeMap::new(),
        sources: HashMap::new(),
        video: LocalVideo::default(),
        announced: Vec::new(),
        bridge: None,
        videos: Vec::new(),
        events: event_tx,
    };
    send(&mut socket, &state.presence(None)).await?;
    tokio::spawn(async move { state.run(socket, command_rx).await });
    Ok(Session { events, commands, endpoint })
}

impl CallSignaling for Session {
    async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    async fn answer(&mut self, sdp: &str) -> Result<(), Error> {
        self.commands.send(Command::Answer(sdp.to_owned())).map_err(|_| Error::new("session ended"))
    }

    async fn local_candidate(&mut self, candidate: Candidate) -> Result<(), Error> {
        self.commands.send(Command::Candidate(candidate)).map_err(|_| Error::new("session ended"))
    }

    async fn set_muted(&mut self, muted: bool) -> Result<(), Error> {
        self.commands.send(Command::Muted(muted)).map_err(|_| Error::new("session ended"))
    }

    async fn leave(&mut self) {
        let (done, wait) = oneshot::channel();
        if self.commands.send(Command::Leave(done)).is_ok() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), wait).await;
        }
    }

    fn local_id(&self) -> &str {
        &self.endpoint
    }

    fn can_send(&self, _kind: VideoKind) -> bool {
        true
    }

    fn data_channel(&self) -> Option<&'static str> {
        Some("JVB data channel")
    }

    fn data_protocol(&self) -> &'static str {
        "http://jitsi.org/protocols/colibri"
    }

    fn video_stream(&self, kind: VideoKind) -> String {
        video_stream(&self.endpoint, kind)
    }

    async fn set_video(&mut self, video: LocalVideo) -> Result<(), Error> {
        self.commands.send(Command::Video(video)).map_err(|_| Error::new("session ended"))
    }
}

/// The stream id of this client's camera or screen track (`msid`), by which
/// its section is found in libwebrtc's answer.
fn video_stream(endpoint: &str, kind: VideoKind) -> String {
    match kind {
        VideoKind::Camera => format!("{endpoint}-video-0"),
        VideoKind::Screen => format!("{endpoint}-video-1"),
    }
}

/// Jitsi's name for this client's camera or screen source, and its `videoType`.
fn source_name(endpoint: &str, kind: VideoKind) -> (String, &'static str) {
    match kind {
        VideoKind::Camera => (format!("{endpoint}-v0"), "camera"),
        VideoKind::Screen => (format!("{endpoint}-v1"), "desktop"),
    }
}

/// What a participant's presence says of their sources (`SourceInfo`): each
/// source name's muted state and video type.
fn source_info(presence: &Element) -> HashMap<String, (bool, String)> {
    let Some(info) = presence.find("SourceInfo", CLIENT) else { return HashMap::new() };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(info.content().trim()) else {
        return HashMap::new();
    };
    map.into_iter()
        .map(|(name, v)| {
            let muted = v.get("muted").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let kind = v.get("videoType").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            (name, (muted, kind))
        })
        .collect()
}

struct State {
    config: Config,
    jid: String,
    endpoint: String,
    muted: bool,
    offer: Option<Offer>,
    /// The focus occupant who offered, and the Jingle initiator.
    initiator: Option<(String, String)>,
    answer: Option<Answer>,
    /// Local candidates gathered before the answer went out.
    pending: Vec<Candidate>,
    candidates_sent: usize,
    members: BTreeMap<String, Member>,
    /// Each member's sources as their presence describes them.
    sources: HashMap<String, HashMap<String, (bool, String)>>,
    /// What this client sends, and the sources the focus was told of.
    video: LocalVideo,
    announced: Vec<(VideoKind, LocalStream)>,
    /// The bridge channel's messages to send, while it is open.
    bridge: Option<mpsc::UnboundedSender<String>>,
    /// The remote videos last reported.
    videos: Vec<RemoteVideo>,
    events: mpsc::UnboundedSender<Event>,
}

impl State {
    async fn run(&mut self, mut socket: Socket, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut heard = tokio::time::Instant::now();
        let reason = loop {
            tokio::select! {
                frame = socket.next() => {
                    let frame = match frame {
                        Some(Ok(frame)) => frame,
                        Some(Err(e)) => {
                            eprintln!("rv-call: connection lost: {e}");
                            break "connection_lost";
                        }
                        None => {
                            eprintln!("rv-call: connection closed by the server");
                            break "connection_lost";
                        }
                    };
                    heard = tokio::time::Instant::now();
                    let Message::Text(text) = frame else { continue };
                    let Some(stanza) = Element::parse(&text) else { continue };
                    if stanza.is("close", FRAMING) {
                        break "connection_closed";
                    }
                    for reply in self.stanza(stanza) {
                        if send(&mut socket, &reply).await.is_err() {
                            break;
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { break "client_gone" };
                    if let Command::Leave(done) = command {
                        for frame in self.leave_frames() {
                            let _ = send(&mut socket, &frame).await;
                        }
                        let _ = socket.close(None).await;
                        let _ = done.send(());
                        break "client_initiated";
                    }
                    for frame in self.command(command) {
                        if send(&mut socket, &frame).await.is_err() {
                            break;
                        }
                    }
                }
                _ = ping.tick() => {
                    // Each ping is answered: a socket silent this long is dead even if TCP has not said so.
                    if heard.elapsed() > SILENCE {
                        break "connection_lost";
                    }
                    let ping = iq("get", &self.config.domain).child(Element::new("ping", PING));
                    if send(&mut socket, &ping).await.is_err() {
                        break "connection_lost";
                    }
                }
            }
        };
        debug(&format!("session ended: {reason}"));
        let _ = self.events.send(Event::Ended(reason.to_owned()));
    }

    fn room_jid(&self) -> String {
        self.config.room_jid()
    }

    /// This client's MUC presence; `kind` `unavailable` to leave.
    fn presence(&self, kind: Option<&str>) -> Element {
        let mut p = Element::new("presence", CLIENT).attr("to", format!("{}/{}", self.room_jid(), self.endpoint));
        if let Some(kind) = kind {
            p.set_attr("type", kind);
            return p;
        }
        let muted = if self.muted { "true" } else { "false" };
        let mut info = format!("{{\"{}-a0\":{{\"muted\":{muted}}}", self.endpoint);
        for (kind, on) in [(VideoKind::Camera, self.video.camera), (VideoKind::Screen, self.video.screen)] {
            let (name, video_type) = source_name(&self.endpoint, kind);
            if on || kind == VideoKind::Camera {
                info.push_str(&format!(",\"{name}\":{{\"muted\":{},\"videoType\":\"{video_type}\"}}", !on));
            }
        }
        info.push('}');
        let video_muted = if self.video.camera { "false" } else { "true" };
        p.child(Element::new("x", MUC))
            .child(Element::new("stats-id", CLIENT).text(random_hex(4)))
            .child(Element::new("SourceInfo", CLIENT).text(info))
            .child(Element::new("audiomuted", CLIENT).text(muted))
            .child(Element::new("videomuted", CLIENT).text(video_muted))
            .child(Element::new("nick", NICK).text(&self.config.display_name))
    }

    /// Replies and frames to send after one incoming stanza.
    fn stanza(&mut self, stanza: Element) -> Vec<Element> {
        match stanza.name.as_str() {
            "presence" => {
                self.presence_in(&stanza);
                Vec::new()
            }
            "iq" => self.iq_in(&stanza),
            _ => Vec::new(),
        }
    }

    fn presence_in(&mut self, p: &Element) {
        let Some(from) = p.get("from") else { return };
        let Some((room, nick)) = from.split_once('/') else { return };
        if room != self.room_jid() {
            return;
        }
        if p.get("type") == Some("error") {
            let _ = self.events.send(Event::Ended("join_refused".to_owned()));
            return;
        }
        // The focus and this client are not people in the call.
        let is_focus = p
            .find("x", MUC_USER)
            .and_then(|x| x.find("item", MUC_USER))
            .and_then(|i| i.get("jid"))
            .is_some_and(|j| j.starts_with("focus@"));
        if nick == self.endpoint || nick == "focus" || is_focus {
            return;
        }
        if p.get("type") == Some("unavailable") {
            self.members.remove(nick);
            self.sources.remove(nick);
        } else {
            self.sources.insert(nick.to_owned(), source_info(p));
            let name = p.find("nick", NICK).map(Element::content).unwrap_or_default();
            let muted = p.find("audiomuted", CLIENT).map(|m| m.content().trim() == "true").unwrap_or_else(|| {
                p.find("SourceInfo", CLIENT).map(|s| s.content().contains("\"muted\":true")).unwrap_or(false)
            });
            self.members.insert(nick.to_owned(), Member { id: nick.to_owned(), name, muted });
        }
        let _ = self.events.send(Event::Roster(self.members.values().cloned().collect()));
        self.report_videos();
    }

    /// The remote cameras and screens sent now: the offer's video streams
    /// whose owner's presence does not say muted.
    fn report_videos(&mut self) {
        let Some(offer) = &self.offer else { return };
        let mut videos = Vec::new();
        let mut screens = Vec::new();
        for stream in offer.video_streams() {
            let (name, member) = (stream.name(), stream.endpoint());
            let info = self.sources.get(&member).and_then(|s| s.get(&name));
            if info.is_some_and(|(muted, _)| *muted) {
                continue;
            }
            let video_type = Some(stream.video_type()).filter(|t| !t.is_empty()).or(info.map(|(_, t)| t.as_str()));
            let kind = if video_type == Some("desktop") { VideoKind::Screen } else { VideoKind::Camera };
            if kind == VideoKind::Screen {
                screens.push(name.clone());
            }
            videos.push(RemoteVideo { track: name, member, kind });
        }
        if videos != self.videos {
            self.videos = videos.clone();
            let _ = self.events.send(Event::Videos(videos));
            self.constrain(&screens);
        }
    }

    /// The receiver constraints, on the bridge's WebSocket when it offered
    /// one, else on the data channel.
    fn constrain(&self, screens: &[String]) {
        match &self.bridge {
            Some(bridge) => {
                let _ = bridge.send(constraints(screens));
            }
            None => {
                let _ = self.events.send(Event::DataMessage(constraints(screens)));
            }
        }
    }

    fn iq_in(&mut self, iq_in: &Element) -> Vec<Element> {
        let from = iq_in.get("from").unwrap_or_default().to_owned();
        let id = iq_in.get("id").unwrap_or_default().to_owned();
        let kind = iq_in.get("type").unwrap_or_default();
        if kind == "result" || kind == "error" {
            return Vec::new();
        }
        let result = Element::new("iq", CLIENT).attr("type", "result").attr("to", &from).attr("id", &id);
        if kind == "get" {
            if iq_in.find("ping", PING).is_some() {
                return vec![result];
            }
            if iq_in.find("query", DISCO_INFO).is_some() {
                trace(&format!("disco#info asked: {}", iq_in.write()));
                let mut query = Element::new("query", DISCO_INFO).child(
                    Element::new("identity", DISCO_INFO)
                        .attr("category", "client")
                        .attr("type", "web")
                        .attr("name", "RocketVibe"),
                );
                for feature in FEATURES {
                    query.push(Element::new("feature", DISCO_INFO).attr("var", feature));
                }
                return vec![result.child(query)];
            }
            return vec![error_iq(&from, &id)];
        }
        let Some(jingle) = iq_in.find("jingle", JINGLE) else { return vec![error_iq(&from, &id)] };
        let mut out = vec![result];
        let from_focus = from.ends_with("/focus") || from.split('@').next() == Some("focus");
        debug(&format!(
            "jingle {} from {}",
            jingle.get("action").unwrap_or_default(),
            if from_focus { "focus" } else { "peer" }
        ));
        trace(&iq_in.write());
        match jingle.get("action").unwrap_or_default() {
            "session-initiate" if !from_focus => {
                // Another participant offering peer to peer: declined, the bridge carries the call.
                out.push(terminate(&from, jingle.get("sid").unwrap_or_default(), "decline"));
            }
            "session-initiate" => {
                if let Some(offer) = Offer::from_initiate(jingle) {
                    let initiator = jingle.get("initiator").unwrap_or(&from).to_owned();
                    self.initiator = Some((from, initiator));
                    self.answer = None;
                    self.announced.clear();
                    self.candidates_sent = 0;
                    let socket = offer.audio.bridge_socket.clone();
                    let sdp = offer.sdp();
                    let video = offer.video.is_some();
                    self.offer = Some(offer);
                    self.videos.clear();
                    self.bridge = socket.filter(|_| video).map(bridge_channel);
                    // Named before their tracks arrive: an unnamed track would show as a camera.
                    let _ = self.events.send(Event::Videos(Vec::new()));
                    if video {
                        self.constrain(&[]);
                    }
                    self.report_videos();
                    let _ = self.events.send(Event::Offer { sdp });
                }
            }
            "source-add" | "addsource" => {
                if let Some(offer) = &mut self.offer {
                    offer.add(
                        jingle
                            .find_all("content", JINGLE)
                            .filter_map(|c| c.find("description", sdp::RTP))
                            .map(|d| {
                                let media = d.get("media").unwrap_or("audio").to_owned();
                                (media, sdp::parse_sources(d), sdp::parse_groups(d))
                            })
                            .collect(),
                    );
                    let sdp = offer.sdp();
                    self.report_videos();
                    let _ = self.events.send(Event::Offer { sdp });
                }
            }
            "source-remove" | "removesource" => {
                if let Some(offer) = &mut self.offer {
                    let ssrcs: Vec<u32> = jingle
                        .find_all("content", JINGLE)
                        .filter_map(|c| c.find("description", sdp::RTP))
                        .flat_map(sdp::parse_sources)
                        .map(|s| s.ssrc)
                        .collect();
                    offer.remove(&ssrcs);
                    let sdp = offer.sdp();
                    self.report_videos();
                    let _ = self.events.send(Event::Offer { sdp });
                }
            }
            "transport-info" => {
                if let Some(offer) = &self.offer {
                    for content in jingle.find_all("content", JINGLE) {
                        if let Some(t) = content.find("transport", sdp::ICE_UDP) {
                            for c in t.find_all("candidate", sdp::ICE_UDP) {
                                let _ = self
                                    .events
                                    .send(Event::RemoteCandidate(sdp::remote_candidate(offer, sdp::candidate_line(c))));
                            }
                        }
                    }
                }
            }
            "transport-replace" if from_focus => {
                // The focus restarting ICE or moving the call to another bridge: the
                // session is joined again from scratch rather than patched.
                let _ = self.events.send(Event::Ended("transport_replaced".to_owned()));
            }
            "session-terminate" if self.offer.as_ref().is_some_and(|o| Some(o.sid.as_str()) == jingle.get("sid")) => {
                // The focus ends the session of a participant left alone (Jicofo's
                // single-participant timeout) and invites them again when someone
                // joins: the call goes on, in the room, without media meanwhile.
                self.offer = None;
                self.initiator = None;
                self.answer = None;
                self.announced.clear();
                self.bridge = None;
                self.videos.clear();
                let _ = self.events.send(Event::MediaEnded);
            }
            _ => {}
        }
        out
    }

    fn command(&mut self, command: Command) -> Vec<Element> {
        match command {
            Command::Answer(sdp) => {
                let (Some(offer), Some((to, initiator))) = (&self.offer, &self.initiator) else { return Vec::new() };
                if self.answer.is_some() {
                    // A renegotiation stays local, but for this client's own video sources.
                    let (to, initiator, sid) = (to.clone(), initiator.clone(), offer.sid.clone());
                    return self.announce(&sdp, &to, &initiator, &sid);
                }
                let mut answer = Answer::parse(&sdp, &offer.audio.name);
                for c in self.pending.drain(..) {
                    if !answer.transport.candidates.contains(&c.line) {
                        answer.transport.candidates.push(c.line);
                    }
                }
                self.candidates_sent = answer.transport.candidates.len();
                // The video content is accepted without sources: they follow as `source-add`.
                let video = offer.video.as_ref().map(|v| (v.name.clone(), Answer::parse(&sdp, &v.name)));
                let video = video.filter(|(_, a)| !a.payloads.is_empty());
                let mut group = Element::new("group", sdp::GROUPING)
                    .attr("semantics", "BUNDLE")
                    .child(Element::new("content", sdp::GROUPING).attr("name", &offer.audio.name));
                if let Some((name, _)) = &video {
                    group.push(Element::new("content", sdp::GROUPING).attr("name", name));
                }
                let data = video.is_some() && sdp::answers_data(&sdp);
                if data {
                    group.push(Element::new("content", sdp::GROUPING).attr("name", sdp::DATA_MID));
                }
                let mut accept = Element::new("jingle", JINGLE)
                    .attr("action", "session-accept")
                    .attr("initiator", initiator)
                    .attr("responder", &self.jid)
                    .attr("sid", &offer.sid)
                    .child(group)
                    .child(answer.content(&offer.audio.name, &self.endpoint));
                if let Some((name, video)) = &video {
                    accept.push(video.content(name, &self.endpoint));
                }
                if data {
                    let mut transport = answer.transport_element(false);
                    transport.push(
                        Element::new("sctpmap", SCTP)
                            .attr("number", sdp::SCTP_PORT.to_string())
                            .attr("protocol", "webrtc-datachannel")
                            .attr("streams", "0"),
                    );
                    accept.push(
                        Element::new("content", JINGLE)
                            .attr("creator", "responder")
                            .attr("name", sdp::DATA_MID)
                            .child(transport),
                    );
                }
                let (to, initiator, sid) = (to.clone(), initiator.clone(), offer.sid.clone());
                let mut frames = vec![iq("set", &to).child(accept)];
                self.answer = Some(answer);
                frames.extend(self.announce(&sdp, &to, &initiator, &sid));
                frames
            }
            Command::Candidate(candidate) => {
                let (Some(offer), Some((to, initiator)), Some(answer)) = (&self.offer, &self.initiator, &self.answer)
                else {
                    self.pending.push(candidate);
                    return Vec::new();
                };
                let Some(element) = sdp::candidate_element(&candidate.line, &format!("c{}", self.candidates_sent))
                else {
                    return Vec::new();
                };
                self.candidates_sent += 1;
                let mut transport = answer.transport_element(false);
                transport.push(element);
                let info = Element::new("jingle", JINGLE)
                    .attr("action", "transport-info")
                    .attr("initiator", initiator)
                    .attr("sid", &offer.sid)
                    .child(
                        Element::new("content", JINGLE)
                            .attr("creator", "initiator")
                            .attr("name", &offer.audio.name)
                            .child(transport),
                    );
                vec![iq("set", to).child(info)]
            }
            Command::Muted(muted) => {
                self.muted = muted;
                vec![self.presence(None)]
            }
            Command::Video(video) => {
                self.video = video;
                // The engine answers the same offer again with its tracks on or off,
                // and that answer says which sources to add or remove.
                if let (Some(offer), Some(_)) = (&mut self.offer, &self.answer) {
                    offer.version += 1;
                    let _ = self.events.send(Event::Offer { sdp: offer.sdp() });
                }
                vec![self.presence(None)]
            }
            Command::Leave(_) => Vec::new(),
        }
    }

    /// `source-add` and `source-remove` for this client's camera and screen,
    /// as libwebrtc's answer now sends them against what the focus knows.
    fn announce(&mut self, sdp: &str, to: &str, initiator: &str, sid: &str) -> Vec<Element> {
        let Some(name) = self.offer.as_ref().and_then(|o| o.video.as_ref()).map(|v| v.name.clone()) else {
            return Vec::new();
        };
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut now = Vec::new();
        for kind in [VideoKind::Camera, VideoKind::Screen] {
            let on = match kind {
                VideoKind::Camera => self.video.camera,
                VideoKind::Screen => self.video.screen,
            };
            let sent = sdp::local_stream(sdp, &video_stream(&self.endpoint, kind)).filter(|_| on);
            let known = self.announced.iter().find(|(k, _)| *k == kind).map(|(_, s)| s.clone());
            match (&known, &sent) {
                (Some(old), Some(new)) if old.ssrcs == new.ssrcs => {}
                (Some(old), _) => removed.push((kind, old.clone())),
                (None, _) => {}
            }
            if let Some(new) = &sent
                && known.as_ref().is_none_or(|old| old.ssrcs != new.ssrcs)
            {
                added.push((kind, new.clone()));
            }
            now.extend(sent.map(|s| (kind, s)));
        }
        self.announced = now;
        let endpoint = self.endpoint.clone();
        let jingle = |action: &str, list: &[(VideoKind, LocalStream)]| {
            let named: Vec<(String, &str, &LocalStream)> = list
                .iter()
                .map(|(kind, stream)| {
                    let (source, video_type) = source_name(&endpoint, *kind);
                    (source, video_type, stream)
                })
                .collect();
            let content = Element::new("content", JINGLE)
                .attr("creator", "initiator")
                .attr("name", &name)
                .attr("senders", "both")
                .child(sdp::video_sources(&named));
            iq("set", to).child(
                Element::new("jingle", JINGLE)
                    .attr("action", action)
                    .attr("initiator", initiator)
                    .attr("sid", sid)
                    .child(content),
            )
        };
        let mut frames = Vec::new();
        if !removed.is_empty() {
            debug(&format!("source-remove {} of ours", removed.len()));
            frames.push(jingle("source-remove", &removed));
        }
        if !added.is_empty() {
            debug(&format!("source-add {} of ours", added.len()));
            frames.push(jingle("source-add", &added));
        }
        frames
    }

    fn leave_frames(&mut self) -> Vec<Element> {
        let mut frames = Vec::new();
        if let (Some(offer), Some((to, _))) = (&self.offer, &self.initiator) {
            frames.push(terminate(to, &offer.sid, "success"));
        }
        frames.push(self.presence(Some("unavailable")));
        frames.push(Element::new("close", FRAMING));
        frames
    }
}

fn iq(kind: &str, to: &str) -> Element {
    Element::new("iq", CLIENT).attr("type", kind).attr("to", to).attr("id", random_hex(6))
}

fn error_iq(to: &str, id: &str) -> Element {
    Element::new("iq", CLIENT).attr("type", "error").attr("to", to).attr("id", id).child(
        Element::new("error", CLIENT)
            .attr("type", "cancel")
            .child(Element::new("feature-not-implemented", "urn:ietf:params:xml:ns:xmpp-stanzas")),
    )
}

fn terminate(to: &str, sid: &str, reason: &str) -> Element {
    iq("set", to).child(
        Element::new("jingle", JINGLE)
            .attr("action", "session-terminate")
            .attr("sid", sid)
            .child(Element::new("reason", JINGLE).child(Element::new(reason, JINGLE))),
    )
}

async fn open(socket: &mut Socket, domain: &str) -> Result<(), Error> {
    send(socket, &Element::new("open", FRAMING).attr("to", domain).attr("version", "1.0")).await?;
    expect(socket, |e| e.is("open", FRAMING)).await.map(|_| ())
}

async fn send(socket: &mut Socket, element: &Element) -> Result<(), Error> {
    if element.find("jingle", JINGLE).is_some() {
        trace(&format!("out: {}", element.write()));
    }
    socket.send(Message::text(element.write())).await.map_err(|e| Error::new(format!("send: {e}")))
}

/// The next stanza `wanted` accepts, within ten seconds; others in between are dropped.
async fn expect(socket: &mut Socket, wanted: impl Fn(&Element) -> bool) -> Result<Element, Error> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| Error::new("timed out"))?
            .ok_or_else(|| Error::new("connection closed"))?
            .map_err(|e| Error::new(format!("receive: {e}")))?;
        let Message::Text(text) = frame else { continue };
        let Some(stanza) = Element::parse(&text) else { continue };
        if stanza.is("close", FRAMING) {
            return Err(Error::new("server closed the stream"));
        }
        if wanted(&stanza) {
            return Ok(stanza);
        }
    }
}

/// Sends an IQ and waits for its result; an error answer is an error.
async fn request_iq(socket: &mut Socket, request: Element) -> Result<Element, Error> {
    let id = request.get("id").unwrap_or_default().to_owned();
    send(socket, &request).await?;
    let answer = expect(socket, |e| e.name == "iq" && e.get("id") == Some(id.as_str())).await?;
    if answer.get("type") == Some("error") {
        return Err(Error::new(format!(
            "{} refused",
            request.elements().next().map(|e| e.name.as_str()).unwrap_or("iq")
        )));
    }
    Ok(answer)
}

/// TURN and STUN servers the deployment announces (XEP-0215); none is fine.
async fn external_services(socket: &mut Socket, domain: &str) -> Vec<IceServer> {
    let query = iq("get", domain).child(Element::new("services", EXTDISCO));
    let Ok(answer) = request_iq(socket, query).await else { return Vec::new() };
    let Some(services) = answer.find("services", EXTDISCO) else { return Vec::new() };
    services.find_all("service", EXTDISCO).filter_map(ice_server).collect()
}

fn ice_server(service: &Element) -> Option<IceServer> {
    let kind = service.get("type")?;
    if !matches!(kind, "stun" | "turn" | "turns") {
        return None;
    }
    let host = service.get("host")?;
    let mut url = format!("{kind}:{host}");
    if let Some(port) = service.get("port") {
        url.push(':');
        url.push_str(port);
    }
    if kind != "stun"
        && let Some(transport) = service.get("transport")
    {
        url.push_str("?transport=");
        url.push_str(transport);
    }
    Some(IceServer {
        urls: vec![url],
        username: service.get("username").unwrap_or_default().to_owned(),
        credential: service.get("password").unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_JS: &str = "var config = {\n    hosts: {\n        domain: 'meet.jitsi',\n        // focus: 'focus.meet.jitsi',\n        muc: 'muc.meet.jitsi'\n    },\n    bosh: 'https://meet.barrut.me/http-bind',\n    websocket: 'wss://meet.barrut.me/xmpp-websocket',\n};";

    #[test]
    fn external_services_become_ice_servers() {
        let s = Element::new("service", EXTDISCO)
            .attr("type", "turns")
            .attr("host", "turn.example")
            .attr("port", "443")
            .attr("transport", "tcp")
            .attr("username", "u")
            .attr("password", "p");
        let server = ice_server(&s).unwrap();
        assert_eq!(server.urls, vec!["turns:turn.example:443?transport=tcp"]);
        assert_eq!((server.username.as_str(), server.credential.as_str()), ("u", "p"));
    }

    fn state() -> State {
        let (events, _rx) = mpsc::unbounded_channel();
        State {
            config: Config::from_meeting("https://meet.barrut.me/r", CONFIG_JS, "Ana", None).unwrap(),
            jid: "x@meet.jitsi/y".into(),
            endpoint: "ep000001".into(),
            muted: false,
            offer: None,
            initiator: None,
            answer: None,
            pending: Vec::new(),
            candidates_sent: 0,
            members: BTreeMap::new(),
            sources: HashMap::new(),
            video: LocalVideo::default(),
            announced: Vec::new(),
            bridge: None,
            videos: Vec::new(),
            events,
        }
    }

    #[test]
    fn disco_lists_audio_and_video_and_presence_names_the_sources() {
        let mut s = state();
        let disco = Element::new("iq", CLIENT)
            .attr("type", "get")
            .attr("from", "focus@auth.meet.jitsi/focus")
            .attr("id", "d1")
            .child(Element::new("query", DISCO_INFO));
        let reply = s.stanza(disco).remove(0).write();
        assert!(reply.contains("urn:xmpp:jingle:apps:rtp:audio"));
        assert!(reply.contains("urn:xmpp:jingle:apps:rtp:video") && reply.contains("dtls-sctp:1"));
        let presence = s.presence(None);
        assert_eq!(presence.get("to"), Some("r@muc.meet.jitsi/ep000001"));
        assert_eq!(
            presence.find("SourceInfo", CLIENT).unwrap().content(),
            r#"{"ep000001-a0":{"muted":false},"ep000001-v0":{"muted":true,"videoType":"camera"}}"#
        );
        assert_eq!(presence.find("videomuted", CLIENT).unwrap().content(), "true");
        s.video = LocalVideo { camera: true, screen: true };
        let presence = s.presence(None);
        assert_eq!(
            presence.find("SourceInfo", CLIENT).unwrap().content(),
            r#"{"ep000001-a0":{"muted":false},"ep000001-v0":{"muted":false,"videoType":"camera"},"ep000001-v1":{"muted":false,"videoType":"desktop"}}"#
        );
        assert_eq!(presence.find("videomuted", CLIENT).unwrap().content(), "false");
    }

    /// The next event that is not the list of remote videos.
    fn next_offer(rx: &mut mpsc::UnboundedReceiver<Event>) -> Option<Event> {
        loop {
            match rx.try_recv().ok()? {
                Event::Videos(_) => continue,
                event => return Some(event),
            }
        }
    }

    #[test]
    fn a_peer_offer_is_declined() {
        let mut s = state();
        let iq = Element::new("iq", CLIENT)
            .attr("type", "set")
            .attr("from", "r@muc.meet.jitsi/abcd1234")
            .attr("id", "p1")
            .child(Element::new("jingle", JINGLE).attr("action", "session-initiate").attr("sid", "s1"));
        let out = s.stanza(iq);
        assert_eq!(out.len(), 2);
        assert!(out[1].write().contains("session-terminate"));
    }

    #[test]
    fn a_session_the_focus_terminates_leaves_the_call_waiting_for_the_next_offer() {
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut s = State { events, ..state() };
        let initiate = || Element::parse(sdp::tests::INITIATE).unwrap();
        s.stanza(initiate());
        assert!(matches!(next_offer(&mut rx), Some(Event::Offer { .. })));
        let terminate = Element::new("iq", CLIENT)
            .attr("type", "set")
            .attr("from", "room@muc.meet.jitsi/focus")
            .attr("id", "t2")
            .child(Element::new("jingle", JINGLE).attr("action", "session-terminate").attr("sid", "7a1b2c3d4e"));
        assert_eq!(s.stanza(terminate).len(), 1, "only the iq result");
        assert!(matches!(rx.try_recv(), Ok(Event::MediaEnded)));
        assert!(rx.try_recv().is_err(), "the call is not ended");
        s.stanza(initiate());
        assert!(matches!(next_offer(&mut rx), Some(Event::Offer { .. })), "a new session is taken");
    }

    #[test]
    fn a_transport_replace_from_the_focus_ends_the_session_to_be_joined_again() {
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut s = State { events, ..state() };
        let iq = Element::new("iq", CLIENT)
            .attr("type", "set")
            .attr("from", "r@muc.meet.jitsi/focus")
            .attr("id", "t1")
            .child(Element::new("jingle", JINGLE).attr("action", "transport-replace").attr("sid", "s1"));
        let out = s.stanza(iq);
        assert_eq!(out.len(), 1, "only the iq result");
        assert!(matches!(rx.try_recv(), Ok(Event::Ended(reason)) if reason == "transport_replaced"));
    }
}
