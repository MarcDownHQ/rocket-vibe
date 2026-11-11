//! The Calls plugin's signalling socket.

use std::collections::BTreeMap;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::{Config, Incoming, Person};
use crate::{CallSignaling, Candidate, Error, Event, IceServer};

/// How often the socket is pinged, and how long without a frame from the server
/// means it is gone.
const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
const SILENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// `RV_CALL_DEBUG=1` prints the events' names and why a session ended, to stderr.
fn debug(what: &str) {
    if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1") {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 100_000)
            .unwrap_or(0);
        eprintln!("rv-call: mattermost: {ms} {what}");
    }
}

enum Command {
    Description(&'static str, String),
    Candidate(Candidate),
    Muted(bool),
    Leave(oneshot::Sender<()>),
}

/// A joined Mattermost call.
pub struct Session {
    events: mpsc::UnboundedReceiver<Event>,
    commands: mpsc::UnboundedSender<Command>,
    connection: String,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the socket, authenticates and joins the channel's call (starting it when
/// none runs); the engine is then asked for its offer.
pub async fn join(config: Config, ice_servers: Vec<IceServer>) -> Result<Session, Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = config.socket_url().ok_or_else(|| Error::new("not a Mattermost server URL"))?;
    let mut request = url.into_client_request().map_err(|e| Error::new(format!("url: {e}")))?;
    if let Ok(bearer) = format!("Bearer {}", config.token).parse() {
        request.headers_mut().insert("Authorization", bearer);
    }
    let (mut socket, _) =
        tokio_tungstenite::connect_async(request).await.map_err(|e| Error::new(format!("connect: {e}")))?;
    let mut seq = 1u64;
    send(&mut socket, super::authentication(seq, &config.token)).await?;
    seq += 1;
    let mut connection = String::new();
    let mut people = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let incoming = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| Error::new("no join acknowledgement"))?;
        let Some(Ok(incoming)) = incoming else { return Err(Error::new("connection lost before joining")) };
        let Frame::Text(text) = incoming else { continue };
        match super::incoming(&text, &connection, &config.channel_id) {
            Incoming::Hello { connection: id } if connection.is_empty() => {
                connection = id;
                let join = serde_json::json!({"channelID": config.channel_id, "title": "", "threadID": ""});
                send(&mut socket, super::action("join", seq, join)).await?;
                seq += 1;
            }
            Incoming::Refused(why) => return Err(Error::new(format!("join refused: {why}"))),
            Incoming::Joined => break,
            Incoming::State(all) => people = all,
            _ => {}
        }
    }
    debug(&format!("joined as {connection}"));
    let (event_tx, events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let _ = event_tx.send(Event::IceServers(ice_servers));
    let _ = event_tx.send(Event::OfferWanted { relay_only: false });
    let mut state = State {
        connection: connection.clone(),
        people: people.into_iter().map(|p| (p.session.clone(), p)).collect(),
        voice: BTreeMap::new(),
        config,
        seq,
        offered: false,
        muted: false,
        events: event_tx,
    };
    state.publish();
    tokio::spawn(async move { state.run(socket, command_rx).await });
    Ok(Session { events, commands, connection })
}

impl CallSignaling for Session {
    async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    async fn answer(&mut self, sdp: &str) -> Result<(), Error> {
        self.commands.send(Command::Description("answer", sdp.to_owned())).map_err(|_| Error::new("session ended"))
    }

    async fn offer(&mut self, sdp: &str) -> Result<(), Error> {
        self.commands.send(Command::Description("offer", sdp.to_owned())).map_err(|_| Error::new("session ended"))
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
        &self.connection
    }

    fn data_channel(&self) -> Option<&'static str> {
        Some("calls-dc")
    }
}

struct State {
    connection: String,
    people: BTreeMap<String, Person>,
    /// Who the server says speaks, by session.
    voice: BTreeMap<String, bool>,
    config: Config,
    seq: u64,
    /// The first offer went out: candidates found later trickle.
    offered: bool,
    /// The microphone as this side wants it; the server starts every session muted.
    muted: bool,
    events: mpsc::UnboundedSender<Event>,
}

impl State {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq - 1
    }

    fn publish(&self) {
        let people: Vec<Person> = self.people.values().cloned().collect();
        let _ = self.events.send(Event::Roster(super::members(&people, &self.config.user_id)));
    }

    async fn run(&mut self, mut socket: Socket, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut heard = tokio::time::Instant::now();
        let event = loop {
            tokio::select! {
                incoming = socket.next() => {
                    let Some(Ok(incoming)) = incoming else { break Event::Ended("connection_lost".to_owned()) };
                    heard = tokio::time::Instant::now();
                    match incoming {
                        Frame::Text(text) => {
                            if let Some(end) = self.frame(&text) {
                                break end;
                            }
                        }
                        Frame::Close(close) => {
                            debug(&format!("closed: {close:?}"));
                            break Event::Ended("connection_closed".to_owned());
                        }
                        _ => {}
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { break Event::Ended("client_gone".to_owned()) };
                    let seq = self.next_seq();
                    let sent = match command {
                        Command::Description(kind, sdp) => {
                            debug(&format!("sending {kind}"));
                            let first = !self.offered;
                            self.offered = true;
                            let sent = socket.send(Frame::Binary(super::sdp_frame(seq, kind, &sdp).into())).await;
                            if first && !self.muted && sent.is_ok() {
                                let seq = self.next_seq();
                                socket.send(Frame::text(super::action("unmute", seq, serde_json::json!({})))).await
                            } else {
                                sent
                            }
                        }
                        Command::Candidate(candidate) if self.offered => {
                            socket.send(Frame::text(super::ice(seq, &candidate))).await
                        }
                        Command::Candidate(_) => Ok(()),
                        Command::Muted(muted) => {
                            self.muted = muted;
                            let name = if muted { "mute" } else { "unmute" };
                            socket.send(Frame::text(super::action(name, seq, serde_json::json!({})))).await
                        }
                        Command::Leave(done) => {
                            let _ = socket.send(Frame::text(super::action("leave", seq, serde_json::json!({})))).await;
                            let _ = socket.close(None).await;
                            let _ = done.send(());
                            break Event::Ended("client_initiated".to_owned());
                        }
                    };
                    if sent.is_err() {
                        break Event::Ended("connection_lost".to_owned());
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE {
                        break Event::Ended("connection_lost".to_owned());
                    }
                    let seq = self.next_seq();
                    let ping = serde_json::json!({"action": "ping", "seq": seq}).to_string();
                    let _ = socket.send(Frame::text(ping)).await;
                }
            }
        };
        debug(&format!("ended: {event:?}"));
        let _ = self.events.send(event);
    }

    /// One frame from the server; `Some` when it ends the session.
    fn frame(&mut self, text: &str) -> Option<Event> {
        let incoming = super::incoming(text, &self.connection, &self.config.channel_id);
        if !matches!(incoming, Incoming::Other) {
            debug(&format!("{:?}", short(&incoming)));
        } else if let Ok(frame) = serde_json::from_str::<serde_json::Value>(text)
            && let Some(event) = frame.get("event").and_then(serde_json::Value::as_str)
        {
            debug(&format!("other event {event}"));
        }
        match incoming {
            Incoming::Offer(sdp) => {
                let _ = self.events.send(Event::Offer { sdp });
            }
            Incoming::Answer(sdp) => {
                let _ = self.events.send(Event::Answer { sdp });
            }
            Incoming::Candidate(candidate) => {
                let _ = self.events.send(Event::RemoteCandidate(candidate));
            }
            Incoming::State(all) => {
                self.people = all.into_iter().map(|p| (p.session.clone(), p)).collect();
                self.publish();
            }
            Incoming::Arrived(person) => {
                self.people.insert(person.session.clone(), person);
                self.publish();
            }
            Incoming::Left { session } => {
                if session == self.connection {
                    return Some(Event::Over("removed".to_owned()));
                }
                self.people.remove(&session);
                self.voice.remove(&session);
                self.publish();
            }
            Incoming::Muted { session, muted } => {
                if let Some(person) = self.people.get_mut(&session) {
                    person.muted = muted;
                    self.publish();
                }
            }
            Incoming::Voice { session, on } => {
                self.voice.insert(session, on);
                let levels = self.voice.iter().map(|(id, on)| (id.clone(), if *on { 1.0 } else { 0.0 })).collect();
                let _ = self.events.send(Event::Levels(levels));
            }
            Incoming::CallEnded => return Some(Event::Over("call_ended".to_owned())),
            Incoming::Removed => return Some(Event::Over("removed".to_owned())),
            Incoming::Refused(why) => return Some(Event::Ended(format!("refused: {why}"))),
            Incoming::Hello { .. } | Incoming::Joined | Incoming::Other => {}
        }
        None
    }
}

/// An event without its SDP, for the debug log.
fn short(incoming: &Incoming) -> String {
    match incoming {
        Incoming::Offer(_) => "offer".to_owned(),
        Incoming::Answer(_) => "answer".to_owned(),
        Incoming::Candidate(_) => "candidate".to_owned(),
        other => format!("{other:?}"),
    }
}

async fn send(socket: &mut Socket, text: String) -> Result<(), Error> {
    socket.send(Frame::text(text)).await.map_err(|e| Error::new(format!("send: {e}")))
}
