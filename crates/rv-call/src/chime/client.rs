//! The Chime signalling connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::proto::{self, Message, frame, kind};
use super::video::{self, Subscriptions};
use super::{Config, Roster, Status};
use crate::{CallSignaling, Candidate, Error, Event, LocalVideo, VideoKind};

/// How often the server is pinged, and how long without a frame from it means
/// the socket is gone (it sends levels ten times a second).
const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
const SILENCE: std::time::Duration = std::time::Duration::from_secs(30);
/// At most this many remote videos at once, below the server's own limit.
const VIDEO_LIMIT: usize = 16;

/// `RV_CALL_DEBUG=1` prints the frames' types and why a session ended, to stderr.
fn debug(what: &str) {
    if matches!(std::env::var("RV_CALL_DEBUG").as_deref(), Ok("1" | "2")) {
        eprintln!("rv-call: chime: {what}");
    }
}

enum End {
    Lost(String),
    Over(String),
}

enum Command {
    Offer(String),
    Muted(bool),
    Video(LocalVideo),
    Leave(oneshot::Sender<()>),
}

/// A joined Chime meeting.
pub struct Session {
    events: mpsc::UnboundedReceiver<Event>,
    commands: mpsc::UnboundedSender<Command>,
    attendee: String,
    /// How many remote videos the offers must receive, and whether any ever did.
    receiving: Arc<AtomicUsize>,
    received: Arc<AtomicBool>,
    /// The attendee this one's screen joins as; `None` for that attendee itself.
    content: Option<Config>,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Connects with the attendee's join token and joins: the TURN servers come
/// back, then the engine is asked for its offer.
pub async fn join(config: Config) -> Result<Session, Error> {
    let (socket, servers, video_limit) = connect(&config).await?;
    let (event_tx, events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let attendee = config.attendee_id.clone();
    let content = Some(config.content());
    let receiving = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(AtomicBool::new(false));
    let mut state = State::new(config, video_limit, receiving.clone(), received.clone(), event_tx);
    state.started(servers);
    tokio::spawn(async move { state.run(socket, command_rx).await });
    Ok(Session { events, commands, attendee, receiving, received, content })
}

/// The socket joined: its TURN servers and the most videos it may receive.
async fn connect(config: &Config) -> Result<(Socket, Vec<crate::IceServer>, usize), Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = config.socket_url().ok_or_else(|| Error::new("not a Chime signalling URL"))?;
    let mut request = url.into_client_request().map_err(|e| Error::new(format!("url: {e}")))?;
    let protocols = format!("_aws_wt_session, {}", config.join_token);
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", protocols.parse().map_err(|_| Error::new("join token not a header"))?);
    if let Ok(origin) = config.origin.parse() {
        request.headers_mut().insert("Origin", origin);
    }
    let (mut socket, _) =
        tokio_tungstenite::connect_async(request).await.map_err(|e| Error::new(format!("connect: {e}")))?;

    let mut audio_session = [0u8; 4];
    let _ = getrandom::fill(&mut audio_session);
    send(&mut socket, super::join(now_ms(), u32::from_le_bytes(audio_session), config.is_content())).await?;
    loop {
        let incoming = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .map_err(|_| Error::new("no join acknowledgement"))?;
        let Some(Ok(incoming)) = incoming else { return Err(Error::new("connection lost before joining")) };
        let Frame::Binary(bytes) = incoming else {
            if let Frame::Close(close) = incoming {
                return Err(Error::new(format!(
                    "refused: {}",
                    close.map(|c| c.reason.to_string()).unwrap_or_default()
                )));
            }
            continue;
        };
        let Some((kind, message)) = proto::received(&bytes) else { continue };
        if let Some(error) = message.message(frame::ERROR) {
            return Err(Error::new(format!(
                "join refused: {} {}",
                error.int(1).unwrap_or(0),
                error.string(2).unwrap_or_default()
            )));
        }
        match kind {
            kind::JOIN_ACK => {
                let ack = message.message(frame::JOIN_ACK).unwrap_or(Message::empty());
                let limit = ack.int(2).unwrap_or(VIDEO_LIMIT as u64) as usize;
                return Ok((socket, super::turn_servers(&ack), limit));
            }
            kind::PING_PONG => pong(&mut socket, &message).await?,
            _ => {}
        }
    }
}

impl CallSignaling for Session {
    async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    async fn answer(&mut self, _sdp: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn offer(&mut self, sdp: &str) -> Result<(), Error> {
        self.commands.send(Command::Offer(sdp.to_owned())).map_err(|_| Error::new("session ended"))
    }

    async fn local_candidate(&mut self, _candidate: Candidate) -> Result<(), Error> {
        // The offer goes out once gathering completed, its candidates in it.
        Ok(())
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
        &self.attendee
    }

    fn can_send(&self, kind: VideoKind) -> bool {
        kind == VideoKind::Camera || self.content.is_some()
    }

    fn offers_to_receive_video(&self) -> bool {
        self.received.load(Ordering::Relaxed)
    }

    fn receive_videos(&self) -> usize {
        self.receiving.load(Ordering::Relaxed)
    }

    async fn set_video(&mut self, video: LocalVideo) -> Result<(), Error> {
        self.commands.send(Command::Video(video)).map_err(|_| Error::new("session ended"))
    }

    fn screen_apart(&self) -> bool {
        self.content.is_some()
    }

    fn screen_session(&self) -> Option<Self> {
        let config = self.content.clone()?;
        let (event_tx, events) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let attendee = config.attendee_id.clone();
        let (receiving, received) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (receiving_state, received_state) = (receiving.clone(), received.clone());
        tokio::spawn(async move {
            match connect(&config).await {
                Ok((socket, servers, limit)) => {
                    let mut state = State::new(config, limit, receiving_state, received_state, event_tx);
                    state.started(servers);
                    state.run(socket, command_rx).await;
                }
                Err(e) => {
                    debug(&format!("content: {e}"));
                    let _ = event_tx.send(Event::Ended(e.0));
                }
            }
        });
        Some(Session { events, commands, attendee, receiving, received, content: None })
    }
}

struct State {
    config: Config,
    roster: Roster,
    muted: bool,
    camera: bool,
    /// Whether this side's screen was told sent, from its own attendee.
    screen: bool,
    last_answer: String,
    videos: Subscriptions,
    receiving: Arc<AtomicUsize>,
    received: Arc<AtomicBool>,
    /// A SUBSCRIBE waits for its answer, and another must follow it.
    subscribing: bool,
    resubscribe: bool,
    events: mpsc::UnboundedSender<Event>,
}

impl State {
    fn new(
        config: Config,
        video_limit: usize,
        receiving: Arc<AtomicUsize>,
        received: Arc<AtomicBool>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        State {
            roster: Roster::new(&config.attendee_id),
            config,
            muted: false,
            camera: false,
            screen: false,
            last_answer: String::new(),
            videos: Subscriptions::new(video_limit.min(VIDEO_LIMIT)),
            receiving,
            received,
            subscribing: true,
            resubscribe: false,
            events,
        }
    }

    /// Joined: the engine gets the TURN servers, then is asked for its offer.
    fn started(&self, servers: Vec<crate::IceServer>) {
        let _ = self.events.send(Event::IceServers(servers));
        let _ = self.events.send(Event::OfferWanted { relay_only: true });
    }

    async fn run(&mut self, mut socket: Socket, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut ping_id = 0u64;
        let mut heard = tokio::time::Instant::now();
        let end = loop {
            tokio::select! {
                incoming = socket.next() => {
                    let Some(Ok(incoming)) = incoming else { break End::Lost("connection_lost".to_owned()) };
                    heard = tokio::time::Instant::now();
                    match incoming {
                        Frame::Binary(bytes) => {
                            if let Some(end) = self.frame(&mut socket, &bytes).await {
                                break end;
                            }
                        }
                        Frame::Close(close) => {
                            debug(&format!("closed: {close:?}"));
                            break End::Lost("connection_closed".to_owned());
                        }
                        _ => {}
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { break End::Lost("client_gone".to_owned()) };
                    match command {
                        Command::Offer(sdp) => {
                            let placed = self.videos.place(&sdp);
                            let _ = self.events.send(Event::Videos(self.videos.remote()));
                            let video = if self.config.is_content() {
                                Some(VideoKind::Screen)
                            } else {
                                (self.camera && video::sends_video(&sdp)).then_some(VideoKind::Camera)
                            };
                            let deflated = miniz_oxide::deflate::compress_to_vec(sdp.as_bytes(), 6);
                            let subscribe = super::subscribe(now_ms(), &self.config, &deflated, self.muted, video, &placed);
                            self.subscribing = true;
                            if send(&mut socket, subscribe).await.is_err() {
                                break End::Lost("connection_lost".to_owned());
                            }
                        }
                        Command::Video(video) => {
                            if video.screen && !self.screen {
                                for frame in super::share_announced(now_ms(), &self.config.attendee_id) {
                                    let _ = send(&mut socket, frame).await;
                                }
                            }
                            self.screen = video.screen;
                            if video.camera != self.camera {
                                self.camera = video.camera;
                                self.subscribe_again();
                            }
                        }
                        Command::Muted(muted) => {
                            self.muted = muted;
                            let _ = send(&mut socket, super::audio_control(now_ms(), muted)).await;
                        }
                        Command::Leave(done) => {
                            let _ = send(&mut socket, super::leave(now_ms())).await;
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                                while let Some(Ok(incoming)) = socket.next().await {
                                    if let Frame::Binary(bytes) = incoming
                                        && proto::received(&bytes).is_some_and(|(k, _)| k == kind::LEAVE_ACK)
                                    {
                                        return;
                                    }
                                }
                            })
                            .await;
                            let _ = socket.close(None).await;
                            let _ = done.send(());
                            break End::Lost("client_initiated".to_owned());
                        }
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE {
                        break End::Lost("connection_lost".to_owned());
                    }
                    ping_id += 1;
                    let _ = send(&mut socket, super::ping_pong(now_ms(), false, ping_id)).await;
                }
            }
        };
        let event = match end {
            End::Lost(reason) => Event::Ended(reason),
            End::Over(reason) => Event::Over(reason),
        };
        debug(&format!("ended: {event:?}"));
        let _ = self.events.send(event);
    }

    /// One frame from the server; `Some` when it ends the session.
    async fn frame(&mut self, socket: &mut Socket, bytes: &[u8]) -> Option<End> {
        let (kind, message) = proto::received(bytes)?;
        if kind != kind::AUDIO_METADATA {
            debug(&format!("frame {kind}"));
        }
        if let Some(error) = message.message(frame::ERROR) {
            debug(&format!("error {} {}", error.int(1).unwrap_or(0), error.string(2).unwrap_or_default()));
        }
        match kind {
            kind::SUBSCRIBE_ACK => {
                let ack = message.message(frame::SUBSCRIBE_ACK)?;
                let sdp = match ack.string(3).filter(|s| !s.is_empty()) {
                    Some(sdp) => sdp,
                    None => {
                        let inflated = super::inflate_answer(ack.bytes(5)?, self.last_answer.as_bytes())?;
                        String::from_utf8(inflated).ok()?
                    }
                };
                self.last_answer.clone_from(&sdp);
                let _ = self.events.send(Event::Answer { sdp });
                self.subscribing = false;
                if std::mem::take(&mut self.resubscribe) {
                    self.subscribe_again();
                }
            }
            kind::INDEX | kind::AUDIO_STREAM_ID_INFO | kind::AUDIO_METADATA if self.config.is_content() => {}
            kind::INDEX => {
                let sources = video::index_sources(&message.message(frame::INDEX)?, &self.config.attendee_id);
                if let Some(freed) = self.videos.index(sources) {
                    if !freed.is_empty() {
                        let _ = send(socket, super::remote_video_removed(now_ms(), &freed)).await;
                        let _ = self.events.send(Event::Videos(self.videos.remote()));
                    }
                    self.receiving.store(self.videos.receiving(), Ordering::Relaxed);
                    if self.videos.receiving() > 0 {
                        self.received.store(true, Ordering::Relaxed);
                    }
                    self.subscribe_again();
                }
            }
            kind::AUDIO_STREAM_ID_INFO => {
                if self.roster.stream_info(&message.message(frame::AUDIO_STREAM_ID_INFO)?) {
                    let _ = self.events.send(Event::Roster(self.roster.members()));
                }
            }
            kind::AUDIO_METADATA => {
                let (levels, changed) = self.roster.metadata(&message.message(frame::AUDIO_METADATA)?);
                if changed {
                    let _ = self.events.send(Event::Roster(self.roster.members()));
                }
                let _ = self.events.send(Event::Levels(levels));
            }
            kind::PING_PONG => {
                let _ = pong(socket, &message).await;
            }
            kind::AUDIO_STATUS => {
                let status = message.message(frame::AUDIO_STATUS)?.int(1).unwrap_or(0);
                match super::audio_status(status) {
                    Status::Fine => {}
                    Status::Lost(reason) => return Some(End::Lost(reason)),
                    Status::Over(reason) => return Some(End::Over(reason.to_owned())),
                }
            }
            kind::LEAVE_ACK => return Some(End::Lost("left".to_owned())),
            _ => {}
        }
        None
    }
}

impl State {
    /// Has the engine offer again, once the SUBSCRIBE in flight is answered.
    fn subscribe_again(&mut self) {
        if self.subscribing {
            self.resubscribe = true;
        } else {
            self.subscribing = true;
            let _ = self.events.send(Event::OfferWanted { relay_only: true });
        }
    }
}

/// Answers the server's PING with its PONG.
async fn pong(socket: &mut Socket, message: &Message<'_>) -> Result<(), Error> {
    let Some(ping) = message.message(frame::PING_PONG) else { return Ok(()) };
    if ping.int(1) != Some(1) {
        return Ok(());
    }
    send(socket, super::ping_pong(now_ms(), true, ping.int(2).unwrap_or(0))).await
}

async fn send(socket: &mut Socket, bytes: Vec<u8>) -> Result<(), Error> {
    socket.send(Frame::Binary(bytes.into())).await.map_err(|e| Error::new(format!("send: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_offer_deflates_raw_as_the_sdk_inflates_it() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\n".repeat(20);
        let deflated = miniz_oxide::deflate::compress_to_vec(sdp.as_bytes(), 6);
        assert!(deflated.len() < sdp.len());
        assert_eq!(miniz_oxide::inflate::decompress_to_vec(&deflated).unwrap(), sdp.as_bytes());
    }
}
