//! A call carried natively for a provider other than LiveKit: rv-call speaks the
//! provider's signalling (Jitsi Meet, Chime), this drives one libwebrtc peer
//! connection with it, and the audio goes through the same chain as LiveKit's
//! (audio.rs: devices, echo canceller, noise remover, per-person mix). Video goes
//! through video.rs as LiveKit's does: this side's camera and screen captures as
//! tracks of the peer connection, the remote ones to the app's frame stream.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use livekit::webrtc::ice_candidate::IceCandidate;
use livekit::webrtc::media_stream_track::MediaStreamTrack;
use livekit::webrtc::peer_connection::{
    AnswerOptions, IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState, SignalingState,
};
use livekit::webrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use livekit::webrtc::peer_connection_factory::{IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration};
use livekit::webrtc::rtp_sender::RtpSender;
use livekit::webrtc::session_description::{SdpType, SessionDescription};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_source::VideoResolution;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_track::RtcVideoTrack;
use rv_call::{CallSignaling, Candidate, Event as CallEvent, LocalVideo, Member as CallMember, RemoteVideo, VideoKind};
use rv_voice_protocol::frames::Source;
use rv_voice_protocol::{Command, ConnectionState as State, Event, Participant, ScreenQuality};
use tokio::sync::mpsc;

use crate::pcm_video::{self, PcmVideo};
use crate::video::{self, Capture, Frames, Internal};
use crate::{audio, diagnose, emit, error, level, report};

/// Where a call's video goes: the app's frame stream, once it said, and
/// whether captures are the test pattern (`RV_VOICE_FAKE_VIDEO=pattern`).
#[derive(Clone)]
pub struct VideoOut {
    pub frames: Option<Arc<Frames>>,
    pub fake: bool,
}

enum Peer {
    Candidate(IceCandidate),
    State(PeerConnectionState),
    Gathered,
    Track { stream: String, id: String, track: livekit::webrtc::audio_track::RtcAudioTrack },
    Video { stream: String, id: String, track: RtcVideoTrack },
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
pub async fn run<S, J, F>(
    join: J,
    fake: bool,
    video_out: VideoOut,
    commands: &mut mpsc::UnboundedReceiver<Result<Command, ()>>,
) where
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
    let (internal, mut captures) = mpsc::unbounded_channel();
    let mut call = Call::new(mix);
    call.video = Some(Videos::new(video_out, internal));
    emit(Event::CallVideo { camera: session.can_send(VideoKind::Camera), screen: session.can_send(VideoKind::Screen) });
    let mut tries = 0;
    let reason = loop {
        let stop =
            joined(&mut session, &mut factory, &mut microphone, &mut sound, &mut call, commands, &mut captures).await;
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
        if !wait(Duration::from_secs(*delay), commands, &mut call, &microphone, &mut sound, &mut captures).await {
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
    call.video = None;
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
    captures: &mut mpsc::UnboundedReceiver<Internal>,
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
            Some(news) = captures.recv() => {
                if let Some(video) = &mut call.video {
                    video.news(news);
                }
            }
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
        Ok(Command::ListScreens) => {
            if let Some(video) = &call.video {
                video::list_screens(video.out.fake, video.out.frames.clone());
            }
        }
        Ok(Command::SetCamera { enabled }) => {
            if let Some(video) = &mut call.video {
                video.want(Source::Camera, enabled, None, None);
            }
        }
        Ok(Command::StartScreenShare { source, quality, .. }) => {
            if let Some(video) = &mut call.video {
                video.want(Source::Screen, true, source, quality);
            }
        }
        Ok(Command::StopScreenShare) => {
            if let Some(video) = &mut call.video {
                video.want(Source::Screen, false, None, None);
            }
        }
        Ok(Command::RemoteVideos { .. }) => {}
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
    captures: &mut mpsc::UnboundedReceiver<Internal>,
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
                Some(Ok(command @ (Command::SetCamera { .. } | Command::StartScreenShare { .. } | Command::StopScreenShare))) => {
                    let (kind, starting) = match &command {
                        Command::SetCamera { enabled } => (VideoKind::Camera, *enabled),
                        Command::StartScreenShare { .. } => (VideoKind::Screen, true),
                        _ => (VideoKind::Screen, false),
                    };
                    if starting && !session.can_send(kind) {
                        error(if kind == VideoKind::Camera { "camera_unavailable" } else { "screen_unavailable" });
                        continue;
                    }
                    local(Ok(command), call, microphone, sound);
                    sync_video(session, factory, call).await;
                }
                Some(Ok(Command::RemoteOffer { sdp })) => {
                    if !call.ensure_pc(factory, false, &peer_tx, microphone, &*session) {
                        session.leave().await;
                        return Stop::Over("connect_failed".to_owned());
                    }
                    let Some(pc) = &call.pc else { continue };
                    let yielded = pc.signaling_state() == SignalingState::HaveLocalOffer;
                    match negotiate(pc, &sdp).await {
                        Ok(answer) => {
                            call.answering = Some((answer, tokio::time::Instant::now()));
                            // A new offer on a running call gathers nothing more.
                            if call.connected || pc.ice_gathering_state() == IceGatheringState::Complete {
                                send_answer(call);
                            }
                            if yielded {
                                debug("offers crossed: this side's goes again");
                                call.offering = None;
                                call.deferred = true;
                            }
                            sync_video(session, factory, call).await;
                            if offer_deferred(session, call).await.is_err() {
                                return Stop::Lost("other".to_owned());
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
                        // A late answer to a new offer leaves the running call as it was.
                        if !call.connected {
                            session.leave().await;
                            return Stop::Over("connect_failed".to_owned());
                        }
                    }
                    sync_video(session, factory, call).await;
                    if offer_deferred(session, call).await.is_err() {
                        return Stop::Lost("other".to_owned());
                    }
                }
                Some(Ok(Command::RemoteVideos { videos })) => {
                    if let Some(video) = &mut call.video {
                        video.remote = videos
                            .into_iter()
                            .map(|v| RemoteVideo {
                                track: v.stream,
                                member: v.member,
                                kind: if v.screen { VideoKind::Screen } else { VideoKind::Camera },
                            })
                            .collect();
                        video.listed = true;
                        video.show();
                    }
                    call.publish(session.local_id());
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
                    if !call.ensure_pc(factory, relay_only, &peer_tx, microphone, &*session) {
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                    if session.offers_in_turn()
                        && call.pc.as_ref().is_some_and(|pc| pc.signaling_state() != SignalingState::Stable)
                    {
                        call.deferred = true;
                        continue;
                    }
                    call.add_receivers(session.receive_videos());
                    let Some(pc) = &call.pc else { continue };
                    match offer(pc, session.offers_to_receive_video()).await {
                        Ok(sdp) => {
                            call.offering = Some((sdp, tokio::time::Instant::now()));
                            // A renegotiation gathers nothing new: waiting would only
                            // leave room for the server's own offer to cross it.
                            if call.connected {
                                if send_offer(session, call).await.is_err() {
                                    return Stop::Lost("other".to_owned());
                                }
                            } else if pc.ice_gathering_state() == IceGatheringState::Complete {
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
                    // The answer to an offer rolled back when offers crossed comes too late.
                    if call.connected && call.pc.as_ref().is_some_and(|pc| pc.signaling_state() == SignalingState::Stable) {
                        debug("an answer to no offer, dropped");
                        continue;
                    }
                    if let Err(e) = set_answer(call, &sdp).await {
                        diagnose(&format!("call: answer: {e}"));
                        if std::env::var("RV_CALL_DEBUG").as_deref() == Ok("2") {
                            debug(&format!("offer {}\nanswer {}", media_lines(&call.sent), media_lines(&sdp)));
                        }
                        // An answer to an offer that crossed the server's: offering
                        // again settles it, the call goes on meanwhile.
                        if call.connected && call.reoffers < REOFFERS {
                            call.reoffers += 1;
                            if reoffer(session, call).await.is_err() {
                                return Stop::Lost("other".to_owned());
                            }
                            continue;
                        }
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                    call.reoffers = 0;
                    sync_video(session, factory, call).await;
                }
                Some(CallEvent::Levels(levels)) => {
                    call.levels = levels.into_iter().collect();
                }
                Some(CallEvent::Offer { sdp }) => {
                    if !call.ensure_pc(factory, false, &peer_tx, microphone, &*session) {
                        session.leave().await;
                        return Stop::Lost("connect_failed".to_owned());
                    }
                    let Some(pc) = &call.pc else { continue };
                    let yielded = pc.signaling_state() == SignalingState::HaveLocalOffer;
                    match negotiate(pc, &sdp).await {
                        Ok(answer) => {
                            if session.answer(&answer).await.is_err() {
                                return Stop::Lost("other".to_owned());
                            }
                            if yielded {
                                debug("offers crossed: this side's goes again");
                                call.offering = None;
                                if reoffer(session, call).await.is_err() {
                                    return Stop::Lost("other".to_owned());
                                }
                            }
                            sync_video(session, factory, call).await;
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
                Some(CallEvent::DataMessage(text)) => {
                    call.outbox.push(text);
                    call.flush();
                }
                Some(CallEvent::Videos(videos)) => {
                    if let Some(video) = &mut call.video {
                        video.remote = videos;
                        video.listed = true;
                        video.show();
                    }
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
                Peer::Video { stream, id, track } => {
                    debug(&format!("video track {id} of {stream}"));
                    if let Some(video) = &mut call.video {
                        if let Some((member, kind)) = session.video_of(&id, &stream) {
                            video.named.insert(id.clone(), RemoteVideo { track: id.clone(), member, kind });
                        }
                        video.tracks.insert(id, (stream, track));
                        video.show();
                    }
                    call.publish(session.local_id());
                }
            },
            Some(news) = captures.recv() => {
                if let Some(video) = &mut call.video {
                    video.news(news);
                }
                sync_video(session, factory, call).await;
            }
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

fn media_lines(sdp: &str) -> String {
    sdp.lines()
        .filter(|l| {
            l.starts_with("m=")
                || l.starts_with("a=mid")
                || matches!(*l, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive")
                || l.starts_with("a=msid")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// How many times in a row a refused answer is answered by offering again.
const REOFFERS: u8 = 3;

/// This side's offer anew, the one waiting rolled back first.
async fn reoffer<S: CallSignaling>(session: &mut S, call: &mut Call) -> Result<(), rv_call::Error> {
    call.add_receivers(session.receive_videos());
    let Some(pc) = &call.pc else { return Ok(()) };
    if pc.signaling_state() == SignalingState::HaveLocalOffer
        && let Ok(rollback) = SessionDescription::parse("", SdpType::Rollback)
    {
        let _ = pc.set_local_description(rollback).await;
    }
    match offer(pc, session.offers_to_receive_video()).await {
        Ok(sdp) => {
            call.offering = Some((sdp, tokio::time::Instant::now()));
            send_offer(session, call).await
        }
        Err(e) => {
            diagnose(&format!("call: offer: {e}"));
            Ok(())
        }
    }
}

/// The remote answer set on the peer connection.
async fn set_answer(call: &Call, sdp: &str) -> Result<(), String> {
    let Some(pc) = &call.pc else { return Ok(()) };
    let answer = SessionDescription::parse(sdp, SdpType::Answer).map_err(|e| format!("{e:?}"))?;
    pc.set_remote_description(answer).await.map_err(|e| format!("{e:?}"))
}

/// The signalling of a call the app speaks itself: this side offers, and the
/// app answers through the window's commands, or the app's offer comes first
/// (`answering`). With `video` the other side takes this side's camera and
/// screen, and each start or stop is offered anew. It cannot be joined again
/// from here.
pub struct External {
    identity: String,
    answering: bool,
    video: bool,
    started: u8,
    offer_wanted: bool,
}

impl External {
    pub fn new(identity: String, answering: bool, video: bool) -> Self {
        External { identity, answering, video, started: 0, offer_wanted: false }
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
            _ if self.offer_wanted => {
                self.offer_wanted = false;
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

    fn can_send(&self, _kind: VideoKind) -> bool {
        self.video
    }

    fn video_stream(&self, kind: VideoKind) -> String {
        match kind {
            VideoKind::Camera => format!("{}-camera", self.identity),
            VideoKind::Screen => format!("{}-screen", self.identity),
        }
    }

    fn reuses_section(&self, kind: VideoKind) -> bool {
        kind == VideoKind::Camera
    }

    fn offers_in_turn(&self) -> bool {
        true
    }

    fn offers_to_receive_video(&self) -> bool {
        self.video
    }

    async fn set_video(&mut self, _video: LocalVideo) -> Result<(), rv_call::Error> {
        self.offer_wanted = self.video;
        Ok(())
    }
}

/// The offer the provider wanted while the last one waited, once the
/// connection is stable again.
async fn offer_deferred<S: CallSignaling>(session: &mut S, call: &mut Call) -> Result<(), rv_call::Error> {
    if !call.deferred || call.pc.as_ref().is_none_or(|pc| pc.signaling_state() != SignalingState::Stable) {
        return Ok(());
    }
    call.deferred = false;
    reoffer(session, call).await
}

/// The answer waiting for gathering, with the candidates found so far.
fn send_answer(call: &mut Call) {
    let Some((sdp, _)) = call.answering.take() else { return };
    emit(Event::LocalAnswer { sdp: rv_call::sdp_with_candidates(&sdp, &call.gathered) });
}

/// The offer waiting for gathering, with the candidates found so far.
async fn send_offer<S: CallSignaling>(session: &mut S, call: &mut Call) -> Result<(), rv_call::Error> {
    let Some((sdp, _)) = call.offering.take() else { return Ok(()) };
    call.sent = sdp.clone();
    session.offer(&rv_call::sdp_with_candidates(&sdp, &call.gathered)).await
}

/// `RV_CALL_DEBUG=1` prints the peer connection's states, to stderr.
fn debug(what: &str) {
    if debugging() {
        eprintln!("rv-voice: {what}");
    }
}

fn debugging() -> bool {
    matches!(std::env::var("RV_CALL_DEBUG").as_deref(), Ok("1" | "2"))
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
        let stream = event.streams.first().map(|s| s.id()).unwrap_or_default();
        match event.track {
            MediaStreamTrack::Audio(track) => {
                let id = track.id();
                let _ = events.send(Peer::Track { stream, id, track });
            }
            MediaStreamTrack::Video(track) => {
                let id = track.id();
                let _ = events.send(Peer::Video { stream, id, track });
            }
        }
    })));
    Ok(pc)
}

/// The local offer made and set: its SDP, which goes out once gathering completed.
async fn offer(pc: &PeerConnection, video: bool) -> Result<String, String> {
    let options =
        OfferOptions { offer_to_receive_audio: true, offer_to_receive_video: video, ..OfferOptions::default() };
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

/// The remote offer set, and the local answer made and set: its SDP. An
/// offer of this side's own still waiting for its answer is rolled back first:
/// this side yields when both offer at once, and offers again once answered.
async fn negotiate(pc: &PeerConnection, offer: &str) -> Result<String, String> {
    if pc.signaling_state() == SignalingState::HaveLocalOffer {
        let rollback = SessionDescription::parse("", SdpType::Rollback).map_err(|e| format!("rollback: {e:?}"))?;
        pc.set_local_description(rollback).await.map_err(|e| format!("rollback: {e:?}"))?;
    }
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
    /// Answers refused in a row, each answered by offering again.
    reoffers: u8,
    /// The last offer sent, for the debug log.
    sent: String,
    /// The data channel the provider wants open, kept for the call's life,
    /// and the messages waiting for it to open.
    channel: Option<livekit::webrtc::data_channel::DataChannel>,
    outbox: Vec<String>,
    /// An offer set, waiting for gathering to complete before it goes out, and since when.
    offering: Option<(String, tokio::time::Instant)>,
    /// An offer the provider wanted while the last one waited for its answer.
    deferred: bool,
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
    /// The call's video, where the app shows any (not a call carried as PCM).
    video: Option<Videos>,
    /// The receive-only video sections added for [`CallSignaling::receive_videos`].
    receivers: usize,
    /// A call carried as PCM: what its video shows.
    pcm_video: Option<Arc<pcm_video::Flags>>,
}

impl Call {
    fn new(mix: std::sync::Arc<audio::Mix>) -> Self {
        Call {
            mix,
            pc: None,
            ice: Vec::new(),
            roster: Vec::new(),
            heard: HashMap::new(),
            levels: HashMap::new(),
            reoffers: 0,
            sent: String::new(),
            channel: None,
            outbox: Vec::new(),
            offering: None,
            deferred: false,
            answering: None,
            gathered: Vec::new(),
            relay_only: false,
            stream: None,
            microphone: true,
            deafened: false,
            connected: false,
            input_level: 0.0,
            last: None,
            video: None,
            receivers: 0,
            pcm_video: None,
        }
    }

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
        if let Some(video) = &mut self.video {
            video.drop_media();
        }
        self.levels.clear();
        self.stream = None;
        self.channel = None;
        self.outbox.clear();
        self.offering = None;
        self.deferred = false;
        self.answering = None;
        self.gathered.clear();
        self.receivers = 0;
    }

    /// Receive-only video sections on the peer connection until there are `wanted`.
    fn add_receivers(&mut self, wanted: usize) {
        let Some(pc) = &self.pc else { return };
        while self.receivers < wanted {
            let init = livekit::webrtc::rtp_transceiver::RtpTransceiverInit {
                direction: livekit::webrtc::rtp_transceiver::RtpTransceiverDirection::RecvOnly,
                stream_ids: Vec::new(),
                send_encodings: Vec::new(),
            };
            if let Err(e) = pc.add_transceiver_for_media(livekit::webrtc::MediaType::Video, init) {
                diagnose(&format!("call: video receiver: {e:?}"));
                return;
            }
            self.receivers += 1;
        }
    }

    /// The peer connection, opened with the microphone on first need; false
    /// when it cannot be.
    fn ensure_pc<S: CallSignaling>(
        &mut self,
        factory: &PeerConnectionFactory,
        relay_only: bool,
        events: &mpsc::UnboundedSender<Peer>,
        microphone: &livekit::webrtc::audio_track::RtcAudioTrack,
        session: &S,
    ) -> bool {
        if self.pc.is_some() {
            return true;
        }
        match open(factory, self.ice.clone(), relay_only, events.clone()) {
            Ok(pc) => {
                if pc.add_track(MediaStreamTrack::Audio(microphone.clone()), &[session.local_id()]).is_err() {
                    error("publish_failed");
                }
                if let Some(video) = &mut self.video {
                    video
                        .attach(&pc, factory, &|kind| session.video_stream(kind), &|kind| session.reuses_section(kind));
                }
                if let Some(label) = session.data_channel() {
                    let init = livekit::webrtc::data_channel::DataChannelInit {
                        protocol: session.data_protocol().to_owned(),
                        ..Default::default()
                    };
                    match pc.create_data_channel(label, init) {
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

    /// The provider's messages, once its data channel is open.
    fn flush(&mut self) {
        let Some(channel) = &self.channel else { return };
        if channel.state() != livekit::webrtc::data_channel::DataChannelState::Open {
            return;
        }
        for text in self.outbox.drain(..) {
            if let Err(e) = channel.send(text.as_bytes(), false) {
                diagnose(&format!("call: data channel: {e:?}"));
            }
        }
    }

    fn tick(&mut self, me: &str) {
        self.flush();
        let level_now = (self.mix.input_level() * 50.0).round() / 50.0;
        if level_now != self.input_level {
            self.input_level = level_now;
            emit(Event::InputLevel { level: level_now });
        }
        self.publish(me);
    }

    fn pcm_flag(&self, flag: impl Fn(&pcm_video::Flags) -> &std::sync::atomic::AtomicBool) -> bool {
        self.pcm_video.as_ref().is_some_and(|f| flag(f).load(std::sync::atomic::Ordering::SeqCst))
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
            camera: self.video.as_ref().is_some_and(|v| v.told.camera) || self.pcm_flag(|f| &f.camera),
            screen: self.video.as_ref().is_some_and(|v| v.told.screen) || self.pcm_flag(|f| &f.screen),
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
                camera: self.video.as_ref().is_some_and(|v| v.shows(&m.id, Source::Camera))
                    || self.pcm_flag(|f| &f.peer),
                screen: self.video.as_ref().is_some_and(|v| v.shows(&m.id, Source::Screen)),
                name: Some(m.name.clone()).filter(|n| !n.is_empty()),
            });
        }
        if self.last.as_ref() != Some(&participants) {
            self.last = Some(participants.clone());
            emit(Event::Participants { participants });
        }
    }
}

/// A local video track on the peer connection: on the first unused section
/// when `reuse` (the provider's offer carries this side's), else on one of its own.
fn add_video(
    pc: &PeerConnection,
    track: RtcVideoTrack,
    stream: String,
    reuse: bool,
) -> Result<RtpSender, livekit::webrtc::RtcError> {
    if reuse {
        return pc.add_track(MediaStreamTrack::Video(track), &[stream]);
    }
    let init = livekit::webrtc::rtp_transceiver::RtpTransceiverInit {
        direction: livekit::webrtc::rtp_transceiver::RtpTransceiverDirection::SendOnly,
        stream_ids: vec![stream],
        send_encodings: Vec::new(),
    };
    pc.add_transceiver(MediaStreamTrack::Video(track), init).map(|t| t.sender())
}

/// This side's camera and screen on the peer connection, then the provider
/// told what is sent; the participants show the change.
async fn sync_video<S: CallSignaling>(session: &mut S, factory: &PeerConnectionFactory, call: &mut Call) {
    let Call { video: Some(video), pc, .. } = call else { return };
    let Some(pc) = pc.as_ref() else { return };
    video.attach(pc, factory, &|kind| session.video_stream(kind), &|kind| session.reuses_section(kind));
    let now = video.sending();
    if now != video.told {
        video.told = now;
        if let Err(e) = session.set_video(now).await {
            diagnose(&format!("call: video: {e}"));
        }
    }
    call.publish(session.local_id());
}

/// One of this side's captures: its frames start, then its track goes on the
/// peer connection, made anew for each media session's factory.
struct Local {
    capture: Capture,
    ready: bool,
    track: Option<RtcVideoTrack>,
    attached: bool,
}

struct Videos {
    out: VideoOut,
    internal: mpsc::UnboundedSender<Internal>,
    camera: Option<Local>,
    screen: Option<Local>,
    /// The peer connection's senders of this side's video, kept across a
    /// capture stopped and started again, and whether each carries a track.
    senders: HashMap<Source, (RtpSender, bool)>,
    /// What the provider was last told is sent.
    told: LocalVideo,
    /// The provider's remote videos, once it named any, and the tracks it
    /// named by their own ids.
    remote: Vec<RemoteVideo>,
    listed: bool,
    named: HashMap<String, RemoteVideo>,
    /// Every remote video track received, by id: its stream and the track.
    tracks: HashMap<String, (String, RtcVideoTrack)>,
    /// The ones shown, by track id: the task, as what, and whose.
    watched: HashMap<String, (tokio::task::JoinHandle<()>, Source, String)>,
}

fn source_of(kind: VideoKind) -> Source {
    match kind {
        VideoKind::Camera => Source::Camera,
        VideoKind::Screen => Source::Screen,
    }
}

fn kind_of(source: Source) -> VideoKind {
    match source {
        Source::Camera => VideoKind::Camera,
        Source::Screen => VideoKind::Screen,
    }
}

impl Videos {
    fn new(out: VideoOut, internal: mpsc::UnboundedSender<Internal>) -> Self {
        Videos {
            out,
            internal,
            camera: None,
            screen: None,
            senders: HashMap::new(),
            told: LocalVideo::default(),
            remote: Vec::new(),
            listed: false,
            named: HashMap::new(),
            tracks: HashMap::new(),
            watched: HashMap::new(),
        }
    }

    fn slot(&mut self, source: Source) -> &mut Option<Local> {
        match source {
            Source::Camera => &mut self.camera,
            Source::Screen => &mut self.screen,
        }
    }

    /// Starts or stops a capture; its track follows on the next `attach`.
    /// The previews go under the local participant's identity.
    fn want(&mut self, source: Source, on: bool, chosen: Option<String>, quality: Option<ScreenQuality>) {
        if !on {
            *self.slot(source) = None;
            return;
        }
        if self.slot(source).is_some() {
            return;
        }
        let (fake, frames, internal) = (self.out.fake, self.out.frames.clone(), self.internal.clone());
        let identity = rv_voice_protocol::frames::LOCAL_PREVIEW.to_owned();
        let capture = match source {
            Source::Camera => video::camera(fake, frames, identity, internal),
            Source::Screen => {
                let quality = quality.unwrap_or(ScreenQuality::DEFAULT).clamped();
                video::screen(fake, chosen, quality, frames, identity, internal)
            }
        };
        *self.slot(source) = Some(Local { capture, ready: false, track: None, attached: false });
    }

    fn news(&mut self, news: Internal) {
        match news {
            Internal::CameraReady => {
                if let Some(local) = &mut self.camera {
                    local.ready = true;
                }
            }
            Internal::ScreenReady => {
                if let Some(local) = &mut self.screen {
                    local.ready = true;
                }
            }
            Internal::Failed(source, code) => {
                if self.slot(source).take().is_some() {
                    error(code);
                }
            }
        }
    }

    /// Each capture delivering frames on its sender, each stopped one off it.
    fn attach(
        &mut self,
        pc: &PeerConnection,
        factory: &PeerConnectionFactory,
        streams: &dyn Fn(VideoKind) -> String,
        reuse: &dyn Fn(VideoKind) -> bool,
    ) {
        for source in [Source::Camera, Source::Screen] {
            let label = if source == Source::Camera { "camera" } else { "screen" };
            let wanted = match self.slot(source) {
                Some(local) if local.ready && !local.attached => {
                    let capture = local.capture.source.clone();
                    let track = local.track.get_or_insert_with(|| factory.create_video_track(label, capture)).clone();
                    local.attached = true;
                    Some(track)
                }
                Some(_) => continue,
                None => None,
            };
            match (self.senders.get_mut(&source), wanted) {
                (Some((sender, live)), Some(track)) => {
                    if sender.set_track(Some(MediaStreamTrack::Video(track))).is_ok() {
                        *live = true;
                    }
                }
                (Some((sender, live)), None) if *live => {
                    // webrtc-sys dereferences the track it is given: None would crash.
                    let idle =
                        NativeVideoSource::new(VideoResolution { width: 16, height: 16 }, source == Source::Screen);
                    let _ = sender.set_track(Some(MediaStreamTrack::Video(factory.create_video_track(label, idle))));
                    *live = false;
                }
                (None, Some(track)) => match add_video(pc, track, streams(kind_of(source)), reuse(kind_of(source))) {
                    Ok(sender) => {
                        self.senders.insert(source, (sender, true));
                    }
                    Err(e) => {
                        diagnose(&format!("call: {label} track: {e:?}"));
                        *self.slot(source) = None;
                        error(if source == Source::Camera { "camera_unavailable" } else { "screen_unavailable" });
                    }
                },
                _ => {}
            }
        }
    }

    fn sending(&self) -> LocalVideo {
        let live = |source| self.senders.get(&source).is_some_and(|(_, live)| *live);
        LocalVideo { camera: live(Source::Camera), screen: live(Source::Screen) }
    }

    /// Whether someone's camera or screen is on screen here.
    fn shows(&self, member: &str, source: Source) -> bool {
        self.watched.values().any(|(_, s, m)| *s == source && m == member)
    }

    /// The remote tracks to the app as the provider names them: a track it
    /// does not name is shown as its stream's camera until it names any.
    fn show(&mut self) {
        let Some(frames) = self.out.frames.clone() else { return };
        let mut wanted: HashMap<String, (Source, String)> = HashMap::new();
        for (id, (stream, _)) in &self.tracks {
            let named = self
                .remote
                .iter()
                .find(|r| &r.track == id)
                .or_else(|| self.remote.iter().find(|r| &r.track == stream))
                .or_else(|| self.named.get(id));
            match named {
                Some(r) => {
                    wanted.insert(id.clone(), (source_of(r.kind), r.member.clone()));
                }
                None if !self.listed => {
                    wanted.insert(id.clone(), (Source::Camera, stream.clone()));
                }
                None => {}
            }
        }
        let stale: Vec<String> = self
            .watched
            .iter()
            .filter(|(id, (_, source, member))| wanted.get(*id) != Some(&(*source, member.clone())))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some((task, source, member)) = self.watched.remove(&id) {
                task.abort();
                frames.end(source, &member);
            }
        }
        for (id, (source, member)) in wanted {
            if self.watched.contains_key(&id) {
                continue;
            }
            let Some((_, track)) = self.tracks.get(&id) else { continue };
            let task = tokio::spawn(video::watch_rtc(track.clone(), source, member.clone(), frames.clone()));
            self.watched.insert(id, (task, source, member));
        }
    }

    /// A media session over: its tracks and senders go, the captures stay and
    /// go on the next peer connection.
    fn drop_media(&mut self) {
        for (_, (task, source, member)) in self.watched.drain() {
            task.abort();
            if let Some(frames) = &self.out.frames {
                frames.end(source, &member);
            }
        }
        self.tracks.clear();
        self.remote.clear();
        self.named.clear();
        self.listed = false;
        self.senders.clear();
        self.told = LocalVideo::default();
        for local in [&mut self.camera, &mut self.screen].into_iter().flatten() {
            local.track = None;
            local.attached = false;
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
        let mut session = External::new("me".into(), false, false);
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::IceServers(_)))));
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::OfferWanted { .. }))));
        for _ in 0..1000 {
            assert!(next(&mut session).await.is_none(), "nothing more, ever");
        }
    }

    #[tokio::test]
    async fn an_answering_call_never_offers() {
        let mut session = External::new("me".into(), true, false);
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::IceServers(_)))));
        for _ in 0..1000 {
            assert!(next(&mut session).await.is_none());
        }
    }

    #[tokio::test]
    async fn a_video_call_offers_again_for_each_change_of_what_it_sends() {
        let mut session = External::new("me".into(), true, true);
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::IceServers(_)))));
        assert!(next(&mut session).await.is_none());
        assert!(session.can_send(VideoKind::Camera) && session.can_send(VideoKind::Screen));
        assert!(session.reuses_section(VideoKind::Camera) && !session.reuses_section(VideoKind::Screen));
        assert!(session.offers_in_turn() && session.offers_to_receive_video());
        assert_eq!(session.video_stream(VideoKind::Screen), "me-screen");
        session.set_video(LocalVideo { camera: true, screen: false }).await.unwrap();
        assert!(matches!(next(&mut session).await, Some(Some(CallEvent::OfferWanted { relay_only: false }))));
        assert!(next(&mut session).await.is_none(), "once per change");
        let mut audio = External::new("me".into(), false, false);
        audio.set_video(LocalVideo { camera: true, screen: false }).await.unwrap();
        assert!(!audio.can_send(VideoKind::Camera) && !audio.offers_to_receive_video());
        assert!(matches!(next(&mut audio).await, Some(Some(CallEvent::IceServers(_)))));
        assert!(matches!(next(&mut audio).await, Some(Some(CallEvent::OfferWanted { .. }))));
        assert!(next(&mut audio).await.is_none(), "an audio call never offers again");
    }

    fn receiving(sdp: &str) -> Vec<&str> {
        let mut video = false;
        sdp.lines()
            .filter(|line| {
                if let Some(media) = line.strip_prefix("m=") {
                    video = media.starts_with("video");
                }
                video && matches!(*line, "a=recvonly" | "a=inactive" | "a=sendonly" | "a=sendrecv")
            })
            .collect()
    }

    #[tokio::test]
    async fn offers_carry_the_receiving_sections_asked_and_keep_them() {
        let factory = PeerConnectionFactory::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut call = Call::new(audio::Mix::new());
        call.pc = Some(open(&factory, Vec::new(), false, tx).unwrap());
        assert!(receiving(&offer(call.pc.as_ref().unwrap(), false).await.unwrap()).is_empty());

        call.add_receivers(2);
        assert_eq!(receiving(&offer(call.pc.as_ref().unwrap(), true).await.unwrap()), ["a=recvonly", "a=recvonly"]);
        call.add_receivers(1);
        assert_eq!(receiving(&offer(call.pc.as_ref().unwrap(), true).await.unwrap()), ["a=recvonly", "a=recvonly"]);
        call.add_receivers(3);
        assert_eq!(
            receiving(&offer(call.pc.as_ref().unwrap(), true).await.unwrap()),
            ["a=recvonly", "a=recvonly", "a=recvonly"]
        );
        assert_eq!(
            receiving(&offer(call.pc.as_ref().unwrap(), false).await.unwrap()),
            ["a=inactive", "a=inactive", "a=inactive"],
            "the legacy option takes receiving away from every video section"
        );
    }
}

/// A call carried as raw PCM by another process (`Command::ConnectPcm`, a WhatsApp
/// call): 16 kHz mono in 60 ms frames over TCP, resampled from and to this side's
/// 48 kHz processing, so the echo canceller, the noise remover, the devices and who
/// speaks all work as in any call. Its video goes on a second connection
/// (pcm_video.rs). It ends when the app says so, or the socket closes.
pub async fn run_pcm(
    address: String,
    fake: bool,
    video_out: VideoOut,
    commands: &mut mpsc::UnboundedReceiver<Result<Command, ()>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    emit(Event::State { state: State::Connecting });
    let Some((host, token)) = address.split_once('#') else {
        emit(Event::State { state: State::Disconnected });
        emit(Event::Disconnected { reason: "connect_failed".into() });
        return;
    };
    let socket = match tokio::net::TcpStream::connect(host).await {
        Ok(socket) => socket,
        Err(e) => {
            diagnose(&format!("call: audio socket: {e}"));
            emit(Event::State { state: State::Disconnected });
            emit(Event::Disconnected { reason: "connect_failed".into() });
            return;
        }
    };
    let _ = socket.set_nodelay(true);
    let (mut read, mut write) = socket.into_split();
    if write.write_all(format!("{token}\n").as_bytes()).await.is_err() {
        emit(Event::State { state: State::Disconnected });
        emit(Event::Disconnected { reason: "connect_failed".into() });
        return;
    }
    let mix = audio::Mix::new();
    let (mut sound, input, output) = audio::Audio::start(mix.clone(), fake, "", "");
    report((input, output));
    let (_factory, microphone) = media(&sound);
    let mut call = Call::new(mix.clone());
    call.apply(&microphone);
    let (internal, mut captures) = mpsc::unbounded_channel();
    let (news_tx, mut news) = mpsc::unbounded_channel();
    let flags = Arc::new(pcm_video::Flags::default());
    let mut video = PcmVideo::connect(host, token, video_out, internal, news_tx, flags.clone());
    if video.is_some() {
        call.pcm_video = Some(flags);
    }
    emit(Event::CallVideo { camera: video.is_some(), screen: video.is_some() });
    let (tap, mut tapped) = mpsc::unbounded_channel::<Vec<i16>>();
    mix.set_tap(Some(tap));
    // The far side's frames, played 10 ms at a time on this side's clock.
    let (heard_tx, mut heard) = mpsc::unbounded_channel::<Vec<i16>>();
    let reader = tokio::spawn(async move {
        let mut bytes = vec![0u8; PCM_FRAME * 2];
        while read.read_exact(&mut bytes).await.is_ok() {
            let frame: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
            if heard_tx.send(frame).is_err() {
                return;
            }
        }
    });
    call.connected = true;
    emit(Event::State { state: State::Connected });
    let mut queue: std::collections::VecDeque<Vec<i16>> = std::collections::VecDeque::new();
    let mut playing = false;
    let mut captured: Vec<i16> = Vec::with_capacity(PCM_FRAME);
    let mut send = tokio::time::interval(Duration::from_millis(60));
    let mut play = tokio::time::interval(Duration::from_millis(10));
    let mut activity = tokio::time::interval(Duration::from_millis(100));
    for tick in [&mut send, &mut play, &mut activity] {
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }
    let peer = |call: &Call| call.roster.first().map(|m| m.id.clone()).unwrap_or_else(|| "peer".to_owned());
    let reason = loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(Ok(Command::Disconnect)) => break "client_initiated".to_owned(),
                Some(Ok(Command::RemoteEnded { reason })) => break reason,
                Some(Ok(Command::RemoteRoster { members })) => {
                    call.roster = members.into_iter().map(|m| CallMember { id: m.id, name: m.name, muted: m.muted }).collect();
                    if let (Some(video), Some(first)) = (&video, call.roster.first()) {
                        video.set_peer(&first.id);
                    }
                    call.publish("me");
                }
                Some(Ok(command @ (Command::SetCamera { .. } | Command::StartScreenShare { .. } | Command::StopScreenShare | Command::ListScreens))) if video.is_some() => {
                    if let Some(video) = &mut video {
                        video.command(command);
                    }
                }
                Some(command) => local(command, &mut call, &microphone, &mut sound),
            },
            Some(capture) = captures.recv() => {
                if let Some(video) = &mut video {
                    video.news(capture);
                }
            }
            Some(pcm_video::News::Refused) = news.recv() => {
                if let Some(video) = &mut video {
                    video.refused();
                }
            }
            frame = heard.recv() => {
                let Some(frame) = frame else { break "remote_ended".to_owned() };
                queue.extend(upsampled(&frame));
                if queue.len() > PCM_BACKLOG {
                    queue.drain(..queue.len() - PCM_PRIME);
                }
            }
            Some(frame) = tapped.recv() => captured.extend(decimated(&frame)),
            _ = send.tick() => {
                // Silence while muted: the library wants a frame every 60 ms.
                captured.resize(captured.len().max(PCM_FRAME), 0);
                let frame: Vec<i16> = captured.drain(..PCM_FRAME).collect();
                if captured.len() > PCM_FRAME * 2 {
                    captured.clear();
                }
                let bytes: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
                if write.write_all(&bytes).await.is_err() {
                    break "remote_ended".to_owned();
                }
            }
            _ = play.tick() => {
                if !playing && queue.len() >= PCM_PRIME {
                    playing = true;
                }
                if playing {
                    match queue.pop_front() {
                        Some(frame) => mix.play(&peer(&call), &frame),
                        None => playing = false,
                    }
                }
            }
            _ = activity.tick() => call.tick("me"),
        }
    };
    reader.abort();
    drop(video);
    mix.set_tap(None);
    drop(sound);
    diagnose(&format!("call: ended ({reason})"));
    emit(Event::State { state: State::Disconnected });
    emit(Event::Disconnected { reason });
}

/// A frame of a call carried as raw PCM: 60 ms at 16 kHz.
const PCM_FRAME: usize = 960;
/// 10 ms frames of the far side to hold before playing, and at most.
const PCM_PRIME: usize = 8;
const PCM_BACKLOG: usize = 30;

/// 10 ms of this side's 48 kHz mono as 16 kHz: each three samples averaged.
fn decimated(frame: &[i16]) -> Vec<i16> {
    frame.chunks(3).map(|c| (c.iter().map(|&s| s as i32).sum::<i32>() / c.len() as i32) as i16).collect()
}

/// 60 ms of the far side's 16 kHz mono as six 10 ms frames of 48 kHz stereo.
fn upsampled(frame: &[i16]) -> Vec<Vec<i16>> {
    let mut mono = Vec::with_capacity(frame.len() * 3);
    for (i, &s) in frame.iter().enumerate() {
        let next = frame.get(i + 1).copied().unwrap_or(s) as i32;
        let s = s as i32;
        mono.extend([s, s + (next - s) / 3, s + 2 * (next - s) / 3].map(|v| v as i16));
    }
    mono.chunks(480).map(|c| c.iter().flat_map(|&s| [s, s]).collect()).collect()
}

#[cfg(test)]
mod pcm_tests {
    use super::*;

    #[test]
    fn the_rates_meet_in_frames_of_the_right_size() {
        let ten_ms = vec![300i16; 480];
        assert_eq!(decimated(&ten_ms).len(), 160);
        assert!(decimated(&ten_ms).iter().all(|&s| s == 300));
        let sixty_ms: Vec<i16> = (0..960).map(|i| (i % 100) as i16).collect();
        let frames = upsampled(&sixty_ms);
        assert_eq!(frames.len(), 6);
        assert!(frames.iter().all(|f| f.len() == 960), "10 ms of stereo each");
        assert_eq!((frames[0][0], frames[0][1]), (0, 0), "the same sample on both channels");
    }
}
