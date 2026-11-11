//! A call carried natively for a provider other than LiveKit: rv-call speaks the
//! provider's signalling (Jitsi Meet, Chime), this drives one libwebrtc peer
//! connection with it, and the audio goes through the same chain as LiveKit's
//! (audio.rs: devices, echo canceller, noise remover, per-person mix). Audio only.

use std::collections::HashMap;
use std::time::Duration;

use livekit::webrtc::ice_candidate::IceCandidate;
use livekit::webrtc::media_stream_track::MediaStreamTrack;
use livekit::webrtc::peer_connection::{
    AnswerOptions, IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState,
};
use livekit::webrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use livekit::webrtc::peer_connection_factory::{IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration};
use livekit::webrtc::session_description::{SdpType, SessionDescription};
use livekit::webrtc::stats::RtcStats;
use rv_call::{CallSignaling, Candidate, Event as CallEvent, Member as CallMember};
use rv_voice_protocol::{Command, ConnectionState as State, Event, Participant};
use tokio::sync::mpsc;

use crate::{audio, diagnose, emit, error, level, report};

enum Peer {
    Candidate(IceCandidate),
    State(PeerConnectionState),
    Gathered,
    Track { stream: String, id: String, track: livekit::webrtc::audio_track::RtcAudioTrack },
}

/// How long an offer waits for ICE gathering before it goes with what was found.
const GATHERING: Duration = Duration::from_secs(5);
/// How long it waits once a candidate exists: gathering may never say it is done,
/// and one relay candidate is all a relayed call needs.
const FIRST_CANDIDATE: Duration = Duration::from_millis(500);

/// The loudness, as the provider reports it, above which a member speaks.
const SPEAKING: f32 = 0.15;

/// How often a lost call is joined again before it is given up, and the pause
/// before each try.
const REJOIN_DELAYS: [u64; 5] = [1, 2, 4, 8, 15];

/// How one joined session stopped.
enum Stop {
    /// This side hung up or the app went away: the call is over.
    Left,
    /// The connection, the media or the conference went away: worth joining again.
    Lost(String),
    /// The provider ended it for good: joining again would not bring it back.
    Over(String),
}

/// Runs one call until it ends, then reports `Disconnected`. A call lost on the
/// way (the signalling socket dropped, the media failed, the focus ended or
/// replaced the session) is joined again with a new peer connection while the
/// window shows `Reconnecting`, keeping the microphone and deafen choices; it is
/// reported ended only when every rejoin failed, with the last reason.
pub async fn run<S, J, F>(join: J, fake: bool, commands: &mut mpsc::UnboundedReceiver<Result<Command, ()>>)
where
    S: CallSignaling,
    J: Fn() -> F,
    F: std::future::Future<Output = Result<S, rv_call::Error>>,
{
    emit(Event::State { state: State::Connecting });
    let mut session = match join().await {
        Ok(session) => session,
        Err(e) => {
            diagnose(&format!("call: join failed: {e}"));
            emit(Event::State { state: State::Disconnected });
            emit(Event::Disconnected { reason: "connect_failed".into() });
            return;
        }
    };
    let mix = audio::Mix::new();
    let (mut sound, input, output) = audio::Audio::start(mix.clone(), fake, "", "");
    report((input, output));
    let (mut factory, mut microphone) = media(&sound);
    let mut call = Call {
        mix,
        pc: None,
        ice: Vec::new(),
        roster: Vec::new(),
        heard: HashMap::new(),
        levels: HashMap::new(),
        channel: None,
        offering: None,
        answering: None,
        gathered: Vec::new(),
        relay_only: false,
        stream: None,
        microphone: true,
        deafened: false,
        connected: false,
        input_level: 0.0,
        last: None,
    };
    let mut tries = 0;
    let reason = loop {
        let stop = joined(&mut session, &mut factory, &mut microphone, &mut sound, &mut call, commands).await;
        call.reset();
        let reason = match stop {
            Stop::Left => break "client_initiated".to_owned(),
            Stop::Over(reason) => break reason,
            Stop::Lost(reason) if session.rejoinable() => reason,
            Stop::Lost(reason) => break reason,
        };
        (factory, microphone) = media(&sound);
        diagnose(&format!("call: lost ({reason}), joining again"));
        if call.connected {
            tries = 0;
        }
        call.connected = false;
        let Some(delay) = REJOIN_DELAYS.get(tries) else { break reason };
        tries += 1;
        emit(Event::State { state: State::Reconnecting });
        if !wait(Duration::from_secs(*delay), commands, &mut call, &microphone, &mut sound).await {
            break "client_initiated".to_owned();
        }
        match join().await {
            Ok(next) => {
                session = next;
                if !call.microphone {
                    let _ = session.set_muted(true).await;
                }
            }
            Err(e) => {
                diagnose(&format!("call: rejoin failed: {e}"));
                if tries >= REJOIN_DELAYS.len() {
                    break reason;
                }
            }
        }
    };
    drop(sound);
    diagnose(&format!("call: ended ({reason})"));
    emit(Event::State { state: State::Disconnected });
    emit(Event::Disconnected { reason });
}

/// Waits `delay` between two tries, still taking the window's commands: false
/// when the user hung up meanwhile.
async fn wait(
    delay: Duration,
    commands: &mut mpsc::UnboundedReceiver<Result<Command, ()>>,
    call: &mut Call,
    microphone: &livekit::webrtc::audio_track::RtcAudioTrack,
    sound: &mut audio::Audio,
) -> bool {
    let deadline = tokio::time::sleep(delay);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return true,
            command = commands.recv() => match command {
                None | Some(Ok(Command::Disconnect)) => return false,
                Some(command) => local(command, call, microphone, sound),
            },
        }
    }
}

/// A window command that needs no session: devices, volumes, deafen, the
/// microphone's own state (told to the room on the next join).
fn local(
    command: Result<Command, ()>,
    call: &mut Call,
    microphone: &livekit::webrtc::audio_track::RtcAudioTrack,
    sound: &mut audio::Audio,
) {
    match command {
        Err(()) => error("invalid_command"),
        Ok(Command::SetMicrophone { enabled }) => {
            call.microphone = enabled;
            call.apply(microphone);
        }
        Ok(Command::SetDeafened { deafened }) => {
            call.deafened = deafened;
            call.mix.set_deafened(deafened);
            call.apply(microphone);
        }
        Ok(Command::ListDevices) => {
            let (inputs, outputs) = audio::devices();
            emit(Event::Devices { inputs, outputs });
        }
        Ok(Command::SetInput { device }) => report((sound.set_input(&device), audio::Opened::Chosen)),
        Ok(Command::SetOutput { device }) => report((audio::Opened::Chosen, sound.set_output(&device))),
        Ok(Command::SetParticipantVolume { identity, volume, muted }) => call.mix.set_gain(&identity, volume, muted),
        Ok(Command::SetInputVolume { volume }) => call.mix.set_input_volume(volume),
        Ok(Command::SetOutputVolume { volume }) => call.mix.set_output_volume(volume),
        Ok(Command::SetNoiseSuppression { enabled }) => call.mix.set_denoise(enabled),
        Ok(_) => error("unsupported"),
    }
}

/// One joined session, until it stops.
async fn joined<S: CallSignaling>(
    session: &mut S,
    factory: &mut PeerConnectionFactory,
    microphone: &mut livekit::webrtc::audio_track::RtcAudioTrack,
    sound: &mut audio::Audio,
    call: &mut Call,
    commands: &mut mpsc::UnboundedReceiver<Result<Command, ()>>,
) -> Stop {
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let mut activity = tokio::time::interval(Duration::from_millis(100));
    activity.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(Ok(Command::Disconnect)) => {
                    session.leave().await;
                    return Stop::Left;
                }
                Some(Ok(Command::SetMicrophone { enabled })) => {
                    local(Ok(Command::SetMicrophone { enabled }), call, microphone, sound);
                    let _ = session.set_muted(!enabled).await;
                }
                Some(Ok(Command::RemoteOffer { sdp })) => {
                    if !call.ensure_pc(factory, false, &peer_tx, microphone, session.local_id(), session.data_channel()) {
                        session.leave().await;
                        return Stop::Over("connect_failed".to_owned());
                    }
                    let Some(pc) = &call.pc else { continue };
                    match negotiate(pc, &sdp).await {
                        Ok(answer) => {
                            call.answering = Some((answer, tokio::time::Instant::now()));
                            // A new offer on a running call gathers nothing more.
                            if call.connected || pc.ice_gathering_state() == IceGatheringState::Complete {
                                send_answer(call);
                            }
                        }
                        Err(e) => {
                            diagnose(&format!("call: remote offer: {e}"));
                            if !call.connected {
                                session.leave().await;
                                return Stop::Over("connect_failed".to_owned());
                            }
                        }
                    }
                }
                Some(Ok(Command::RemoteAnswer { sdp })) => {
                    if let Err(e) = set_answer(call, &sdp).await {
                        diagnose(&format!("call: answer: {e}"));
                        session.leave().await;
                        return Stop::Over("connect_failed".to_owned());
                    }
                }
                Some(Ok(Command::RemoteRoster { members })) => {
                    call.roster = members
                        .into_iter()
                        .map(|m| CallMember { id: m.id, name: m.name, muted: m.muted })
                        .collect();
                    call.publish(session.local_id());
                }
                Some(Ok(Command::RemoteEnded { reason })) => {
                    session.leave().await;
                    return Stop::Over(reason);
                }
                Some(command) => local(command, call, microphone, sound),
            },
            event = session.next_event() => match event {
                None => return Stop::Lost("other".to_owned()),
                Some(CallEvent::Ended(reason)) => return Stop::Lost(reason),
                Some(CallEvent::Over(reason)) => return Stop::Over(reason),
                Some(CallEvent::IceServers(servers)) => {
                    call.ice = servers
                        .into_iter()
                        .map(|s| IceServer { urls: s.urls, username: s.username, password: s.credential })
                        .collect();
                }
                Some(CallEvent::OfferWanted { relay_only }) => {
                    call.relay_only = relay_only;
                    if !call.ensure_pc(factory, relay_only, &peer_tx, microphone, session.local_id(), session.data_channel()) {
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                    let Some(pc) = &call.pc else { continue };
                    match offer(pc).await {
                        Ok(sdp) => {
                            call.offering = Some((sdp, tokio::time::Instant::now()));
                            if pc.ice_gathering_state() == IceGatheringState::Complete {
                                let _ = peer_tx.send(Peer::Gathered);
                            }
                        }
                        Err(e) => {
                            diagnose(&format!("call: offer: {e}"));
                            session.leave().await;
                            return Stop::Lost("connect_failed".to_owned());
                        }
                    }
                }
                Some(CallEvent::Answer { sdp }) => {
                    if let Err(e) = set_answer(call, &sdp).await {
                        diagnose(&format!("call: answer: {e}"));
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                }
                Some(CallEvent::Levels(levels)) => {
                    call.levels = levels.into_iter().collect();
                }
                Some(CallEvent::Offer { sdp }) => {
                    if !call.ensure_pc(factory, false, &peer_tx, microphone, session.local_id(), session.data_channel()) {
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                    let Some(pc) = &call.pc else { continue };
                    match negotiate(pc, &sdp).await {
                        Ok(answer) => {
                            if session.answer(&answer).await.is_err() {
                                return Stop::Lost("other".to_owned());
                            }
                        }
                        Err(e) => diagnose(&format!("call: negotiation: {e}")),
                    }
                }
                Some(CallEvent::RemoteCandidate(c)) => {
                    if let (Some(pc), Ok(candidate)) = (&call.pc, IceCandidate::parse(&c.mid, 0, &c.line)) {
                        let _ = pc.add_ice_candidate(candidate).await;
                    }
                }
                Some(CallEvent::Roster(members)) => {
                    call.roster = members;
                    call.publish(session.local_id());
                }
                Some(CallEvent::MediaEnded) => {
                    diagnose("call: media session ended by the server, still in the call");
                    call.drop_media();
                    (*factory, *microphone) = media(sound);
                    call.apply(microphone);
                }
            },
            Some(peer) = peer_rx.recv() => match peer {
                Peer::Candidate(c) => {
                    let candidate = Candidate { mid: c.sdp_mid(), line: c.candidate() };
                    call.gathered.push(candidate.clone());
                    let _ = session.local_candidate(candidate).await;
                }
                Peer::Gathered => {
                    send_answer(call);
                    if send_offer(session, call).await.is_err() {
                        return Stop::Lost("other".to_owned());
                    }
                }
                Peer::State(state) => {
                    debug(&format!("peer connection {state:?}"));
                    match state {
                        PeerConnectionState::Connected if !call.connected => {
                            call.connected = true;
                            emit(Event::State { state: State::Connected });
                        }
                        PeerConnectionState::Failed => {
                            session.leave().await;
                            return Stop::Lost("connection_failed".to_owned());
                        }
                        _ => {}
                    }
                }
                Peer::Track { stream, id, track } => {
                    debug(&format!("track {id} of {stream}"));
                    call.stream = Some(stream.clone());
                    let task = sound.listen_rtc(id.clone(), stream, true, track);
                    if let Some(old) = call.heard.insert(id, task) {
                        old.abort();
                    }
                }
            },
            _ = stats.tick(), if debugging() => {
                if let Some(pc) = &call.pc {
                    print_stats(pc).await;
                }
            }
            _ = activity.tick() => {
                if call.answering.as_ref().is_some_and(|(_, at)| at.elapsed() > GATHERING) {
                    send_answer(call);
                }
                if call
                    .offering
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() > GATHERING || (call.relay_only && !call.gathered.is_empty() && at.elapsed() > FIRST_CANDIDATE))
                    && send_offer(session, call).await.is_err()
                {
                    return Stop::Lost("other".to_owned());
                }
                call.tick(session.local_id());
            }
        }
    }
}

/// The remote answer set on the peer connection.
async fn set_answer(call: &Call, sdp: &str) -> Result<(), String> {
    let Some(pc) = &call.pc else { return Ok(()) };
    let answer = SessionDescription::parse(sdp, SdpType::Answer).map_err(|e| format!("{e:?}"))?;
    pc.set_remote_description(answer).await.map_err(|e| format!("{e:?}"))
}

/// The signalling of a call the app speaks itself: this side only offers, and the
/// app answers through the window's commands. It cannot be joined again from here.
pub struct External {
    identity: String,
    answering: bool,
    started: u8,
}

impl External {
    pub fn new(identity: String, answering: bool) -> Self {
        External { identity, answering, started: 0 }
    }
}

impl CallSignaling for External {
    /// The engine's loop drops and calls this again on everything else it does,
    /// so it may be called any number of times: each step once.
    async fn next_event(&mut self) -> Option<CallEvent> {
        match self.started {
            0 => {
                self.started = 1;
                Some(CallEvent::IceServers(Vec::new()))
            }
            1 if !self.answering => {
                self.started = 2;
                Some(CallEvent::OfferWanted { relay_only: false })
            }
            _ => std::future::pending().await,
        }
    }

    async fn answer(&mut self, _sdp: &str) -> Result<(), rv_call::Error> {
        Ok(())
    }

    async fn offer(&mut self, sdp: &str) -> Result<(), rv_call::Error> {
        emit(Event::LocalOffer { sdp: sdp.to_owned() });
        Ok(())
    }

    async fn local_candidate(&mut self, _candidate: Candidate) -> Result<(), rv_call::Error> {
        Ok(())
    }

    async fn set_muted(&mut self, _muted: bool) -> Result<(), rv_call::Error> {
        Ok(())
    }

    async fn leave(&mut self) {}

    fn local_id(&self) -> &str {
        &self.identity
    }

    fn rejoinable(&self) -> bool {
        false
    }
}

/// The answer waiting for gathering, with the candidates found so far.
fn send_answer(call: &mut Call) {
    let Some((sdp, _)) = call.answering.take() else { return };
    emit(Event::LocalAnswer { sdp: rv_call::sdp_with_candidates(&sdp, &call.gathered) });
}

/// The offer waiting for gathering, with the candidates found so far.
async fn send_offer<S: CallSignaling>(session: &mut S, call: &mut Call) -> Result<(), rv_call::Error> {
    let Some((sdp, _)) = call.offering.take() else { return Ok(()) };
    session.offer(&rv_call::sdp_with_candidates(&sdp, &call.gathered)).await
}

/// `RV_CALL_DEBUG=1` prints the peer connection's states, to stderr.
fn debug(what: &str) {
    if debugging() {
        eprintln!("rv-voice: {what}");
    }
}

fn debugging() -> bool {
    std::env::var("RV_CALL_DEBUG").as_deref() == Ok("1")
}

/// `RV_CALL_DEBUG=1`: what the media is doing, to tell a lossy path from a local stall.
async fn print_stats(pc: &PeerConnection) {
    let Ok(stats) = pc.get_stats().await else { return };
    for stat in stats {
        match stat {
            RtcStats::OutboundRtp(s) => debug(&format!("stats sent {} packets", s.sent.packets_sent)),
            RtcStats::RemoteInboundRtp(s) => debug(&format!(
                "stats sent, seen by the far side: lost {}, jitter {:.3}, rtt {:.3}",
                s.received.packets_lost, s.received.jitter, s.remote_inbound.round_trip_time,
            )),
            RtcStats::InboundRtp(s) => debug(&format!(
                "stats received {} packets, lost {}, jitter {:.3}",
                s.received.packets_received, s.received.packets_lost, s.received.jitter,
            )),
            RtcStats::LocalCandidate(s) => debug(&format!("stats local candidate {:?}", s.local_candidate)),
            _ => {}
        }
    }
}

fn open(
    factory: &PeerConnectionFactory,
    ice_servers: Vec<IceServer>,
    relay_only: bool,
    events: mpsc::UnboundedSender<Peer>,
) -> Result<PeerConnection, String> {
    let mut config = RtcConfiguration::default();
    config.ice_servers = ice_servers;
    if relay_only {
        config.ice_transport_type = IceTransportsType::Relay;
    }
    let pc = factory.create_peer_connection(config).map_err(|e| format!("{e:?}"))?;
    let tx = events.clone();
    pc.on_ice_gathering_state_change(Some(Box::new(move |state| {
        if state == IceGatheringState::Complete {
            let _ = tx.send(Peer::Gathered);
        }
    })));
    let tx = events.clone();
    pc.on_ice_candidate(Some(Box::new(move |c| {
        let _ = tx.send(Peer::Candidate(c));
    })));
    let tx = events.clone();
    pc.on_connection_state_change(Some(Box::new(move |state| {
        let _ = tx.send(Peer::State(state));
    })));
    pc.on_track(Some(Box::new(move |event| {
        if let MediaStreamTrack::Audio(track) = event.track {
            let stream = event.streams.first().map(|s| s.id()).unwrap_or_default();
            let id = track.id();
            let _ = events.send(Peer::Track { stream, id, track });
        }
    })));
    Ok(pc)
}

/// The local offer made and set: its SDP, which goes out once gathering completed.
async fn offer(pc: &PeerConnection) -> Result<String, String> {
    let options = OfferOptions { offer_to_receive_audio: true, ..OfferOptions::default() };
    let offer = pc.create_offer(options).await.map_err(|e| format!("create: {e:?}"))?;
    let sdp = offer.to_string();
    pc.set_local_description(offer).await.map_err(|e| format!("local: {e:?}"))?;
    Ok(sdp)
}

/// A peer connection factory and the microphone track made from it, for one
/// media session: a second peer connection on a factory whose first one closed
/// receives the remote tracks but never plays them (seen on meet.barrut.me).
fn media(sound: &audio::Audio) -> (PeerConnectionFactory, livekit::webrtc::audio_track::RtcAudioTrack) {
    let factory = PeerConnectionFactory::default();
    let microphone = factory.create_audio_track("microphone", sound.source.clone());
    (factory, microphone)
}

/// The remote offer set, and the local answer made and set: its SDP.
async fn negotiate(pc: &PeerConnection, offer: &str) -> Result<String, String> {
    let offer = SessionDescription::parse(offer, SdpType::Offer).map_err(|e| format!("offer: {e:?}"))?;
    pc.set_remote_description(offer).await.map_err(|e| format!("remote: {e:?}"))?;
    let answer = pc.create_answer(AnswerOptions::default()).await.map_err(|e| format!("answer: {e:?}"))?;
    let sdp = answer.to_string();
    pc.set_local_description(answer).await.map_err(|e| format!("local: {e:?}"))?;
    Ok(sdp)
}

struct Call {
    mix: std::sync::Arc<audio::Mix>,
    pc: Option<PeerConnection>,
    ice: Vec<IceServer>,
    roster: Vec<CallMember>,
    heard: HashMap<String, tokio::task::JoinHandle<()>>,
    /// Each member's loudness when the provider says it (Chime mixes the audio).
    levels: HashMap<String, f32>,
    /// The data channel the provider wants open, kept for the call's life.
    channel: Option<livekit::webrtc::data_channel::DataChannel>,
    /// An offer set, waiting for gathering to complete before it goes out, and since when.
    offering: Option<(String, tokio::time::Instant)>,
    /// The same for an answer to an offer from the app.
    answering: Option<(String, tokio::time::Instant)>,
    /// The local candidates so far, which an offer that waited carries.
    gathered: Vec<Candidate>,
    relay_only: bool,
    /// The stream of the last remote track.
    stream: Option<String>,
    microphone: bool,
    deafened: bool,
    connected: bool,
    input_level: f32,
    last: Option<Vec<Participant>>,
}

impl Call {
    /// Forgets one session's media: its peer connection, the tracks heard, the roster.
    fn reset(&mut self) {
        self.drop_media();
        self.roster.clear();
        self.last = None;
    }

    /// Forgets the peer connection and the tracks heard; the next offer opens a new one.
    fn drop_media(&mut self) {
        for (_, task) in self.heard.drain() {
            task.abort();
        }
        if let Some(pc) = self.pc.take() {
            pc.close();
        }
        self.levels.clear();
        self.stream = None;
        self.channel = None;
        self.offering = None;
        self.answering = None;
        self.gathered.clear();
    }

    /// The peer connection, opened with the microphone on first need; false
    /// when it cannot be.
    fn ensure_pc(
        &mut self,
        factory: &PeerConnectionFactory,
        relay_only: bool,
        events: &mpsc::UnboundedSender<Peer>,
        microphone: &livekit::webrtc::audio_track::RtcAudioTrack,
        stream: &str,
        channel: Option<&str>,
    ) -> bool {
        if self.pc.is_some() {
            return true;
        }
        match open(factory, self.ice.clone(), relay_only, events.clone()) {
            Ok(pc) => {
                if pc.add_track(MediaStreamTrack::Audio(microphone.clone()), &[stream]).is_err() {
                    error("publish_failed");
                }
                if let Some(label) = channel {
                    match pc.create_data_channel(label, livekit::webrtc::data_channel::DataChannelInit::default()) {
                        Ok(dc) => self.channel = Some(dc),
                        Err(e) => diagnose(&format!("call: data channel: {e:?}")),
                    }
                }
                self.pc = Some(pc);
                self.apply(microphone);
                true
            }
            Err(e) => {
                diagnose(&format!("call: peer connection: {e}"));
                false
            }
        }
    }

    fn apply(&self, track: &livekit::webrtc::audio_track::RtcAudioTrack) {
        let sending = self.microphone && !self.deafened;
        self.mix.set_sending(sending);
        track.set_enabled(sending);
    }

    fn tick(&mut self, me: &str) {
        let level_now = (self.mix.input_level() * 50.0).round() / 50.0;
        if level_now != self.input_level {
            self.input_level = level_now;
            emit(Event::InputLevel { level: level_now });
        }
        self.publish(me);
    }

    fn publish(&mut self, me: &str) {
        let (speaking, loud) = self.mix.activity("", true);
        let muted = !self.microphone || self.deafened;
        let mut participants = vec![Participant {
            identity: me.to_owned(),
            local: true,
            muted,
            deafened: self.deafened,
            speaking: speaking && !muted,
            level: if muted { 0.0 } else { level(loud) },
            camera: false,
            screen: false,
            name: None,
        }];
        for m in &self.roster {
            let (speaking, loud) = match self.levels.get(&m.id) {
                Some(level) => (*level > SPEAKING && !m.muted, *level),
                // One mixed stream and one person: its loudness is theirs.
                None => match (&self.stream, self.roster.len()) {
                    (Some(stream), 1) if self.mix.activity(&m.id, false) == (false, 0.0) => {
                        self.mix.activity(stream, false)
                    }
                    _ => self.mix.activity(&m.id, false),
                },
            };
            participants.push(Participant {
                identity: m.id.clone(),
                local: false,
                muted: m.muted,
                deafened: false,
                speaking,
                level: level(loud),
                camera: false,
                screen: false,
                name: Some(m.name.clone()).filter(|n| !n.is_empty()),
            });
        }
        if self.last.as_ref() != Some(&participants) {
            self.last = Some(participants.clone());
            emit(Event::Participants { participants });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn next(session: &mut External) -> Option<Option<CallEvent>> {
        tokio::time::timeout(Duration::ZERO, session.next_event()).await.ok()
    }

    #[tokio::test]
    async fn an_external_call_offers_once_however_often_it_is_asked() {
        let mut session = External::new("me".into(), false);
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::IceServers(_)))));
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::OfferWanted { .. }))));
        for _ in 0..1000 {
            assert!(next(&mut session).await.is_none(), "nothing more, ever");
        }
    }

    #[tokio::test]
    async fn an_answering_call_never_offers() {
        let mut session = External::new("me".into(), true);
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::IceServers(_)))));
        for _ in 0..1000 {
            assert!(next(&mut session).await.is_none());
        }
    }
}
