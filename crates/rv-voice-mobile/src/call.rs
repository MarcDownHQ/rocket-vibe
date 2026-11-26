//! A native call's signalling for the Android voice engine (rv-call, as the
//! desktop sidecar runs it), over JNI. `com.rocketvibe.voice.JitsiCall` starts one
//! from a meeting URL and its host's `config.js` (Jitsi Meet) or an attendee's
//! credentials (Chime, a Slack huddle), reads the session's events as JSON on a
//! thread of its own, and hands back libwebrtc's offer or answer, candidates and
//! the microphone's state. The peer connection itself stays on the Kotlin side.
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jlong, jstring};
use rv_call::jitsi::Config;
use rv_call::{CallSignaling, Candidate, Event, LocalVideo, VideoKind};
use serde_json::{Value, json};
use tokio::sync::mpsc as async_mpsc;

enum Command {
    Offer(String),
    Answer(String),
    Candidate(Candidate),
    Muted(bool),
    Video(LocalVideo),
    RemoteVideo { track: String, stream: String },
    /// This side's screen capture is ready (a session of its own joins) or stopped (it is left).
    Share(bool),
    /// That session's offer, its candidates in it.
    ShareOffer(String),
    Leave,
}

/// The calls the Kotlin side holds, by handle: a late call after `free` finds nothing.
static CALLS: LazyLock<Mutex<HashMap<jlong, Arc<Call>>>> = LazyLock::new(Mutex::default);
static NEXT: AtomicI64 = AtomicI64::new(1);

fn call(handle: jlong) -> Option<Arc<Call>> {
    CALLS.lock().ok()?.get(&handle).cloned()
}

pub struct Call {
    runtime: tokio::runtime::Runtime,
    commands: async_mpsc::UnboundedSender<Command>,
    events: Mutex<mpsc::Receiver<String>>,
}

fn kind_name(kind: VideoKind) -> &'static str {
    match kind {
        VideoKind::Camera => "camera",
        VideoKind::Screen => "screen",
    }
}

/// What the Kotlin side needs of the session before any media: its stream id,
/// the data channel to open, what it may send and on which streams.
pub fn local_json<S: CallSignaling>(session: &S) -> Value {
    json!({
        "type": "local",
        "id": session.local_id(),
        "dataChannel": session.data_channel(),
        "dataProtocol": session.data_protocol(),
        "canCamera": session.can_send(VideoKind::Camera),
        "canScreen": session.can_send(VideoKind::Screen),
        "cameraStream": session.video_stream(VideoKind::Camera),
        "screenStream": session.video_stream(VideoKind::Screen),
        "offersLocalSections": session.offers_local_sections(),
        "screenApart": session.screen_apart(),
    })
}

/// An event of the session that carries this side's screen, as the Kotlin side
/// reads it apart from the call's own: its end ends the share, not the call.
pub fn share_json(event: Value) -> Value {
    json!({"type": "share", "event": event})
}

/// The next event of the screen's session, if there is one; never otherwise.
async fn share_event<S: CallSignaling>(share: &mut Option<S>) -> Option<Event> {
    match share {
        Some(share) => share.next_event().await,
        None => std::future::pending().await,
    }
}

/// Whose and what a remote video track is, as its own ids say; `None` when
/// they say nothing.
pub fn named_json(track: &str, named: Option<(String, VideoKind)>) -> Option<Value> {
    let (member, kind) = named?;
    Some(json!({"type": "named", "track": track, "member": member, "kind": kind_name(kind)}))
}

/// The event as the Kotlin side reads it.
pub fn event_json(event: &Event) -> Value {
    match event {
        Event::IceServers(servers) => json!({
            "type": "ice",
            "servers": servers.iter().map(|s| json!({"urls": s.urls, "username": s.username, "credential": s.credential})).collect::<Vec<_>>(),
        }),
        Event::Offer { sdp } => json!({"type": "offer", "sdp": sdp}),
        Event::OfferWanted { relay_only } => {
            json!({"type": "offerWanted", "relayOnly": relay_only})
        }
        Event::Answer { sdp } => json!({"type": "answer", "sdp": sdp}),
        Event::Levels(levels) => json!({
            "type": "levels",
            "levels": levels.iter().map(|(id, level)| json!({"id": id, "level": level})).collect::<Vec<_>>(),
        }),
        Event::RemoteCandidate(c) => json!({"type": "candidate", "mid": c.mid, "line": c.line}),
        Event::Roster(members) => json!({
            "type": "roster",
            "members": members.iter().map(|m| json!({"id": m.id, "name": m.name, "muted": m.muted})).collect::<Vec<_>>(),
        }),
        Event::Ended(reason) => json!({"type": "ended", "reason": reason}),
        Event::Over(reason) => json!({"type": "over", "reason": reason}),
        Event::MediaEnded => json!({"type": "media_ended"}),
        Event::Videos(videos) => {
            let videos: Vec<Value> =
                videos.iter().map(|v| json!({"track": v.track, "member": v.member, "kind": kind_name(v.kind)})).collect();
            json!({"type": "videos", "videos": videos})
        }
        Event::DataMessage(text) => json!({"type": "data", "text": text}),
    }
}

/// The event as the Kotlin side reads it, an offer wanted with the receive-only
/// video sections it must carry and whether it receives video (Chime).
pub fn session_event_json<S: CallSignaling>(session: &S, event: &Event) -> Value {
    let mut value = event_json(event);
    if matches!(event, Event::OfferWanted { .. }) {
        value["receiveVideos"] = json!(session.receive_videos());
        value["receiveVideo"] = json!(session.offers_to_receive_video());
    }
    value
}

impl Call {
    /// Joins the conference; `None` when the meeting is not a Jitsi one or the join fails.
    pub fn start(
        meeting: &str,
        config_js: &str,
        display_name: &str,
        token: Option<String>,
    ) -> Option<Call> {
        if !Config::is_jitsi(config_js) {
            return None;
        }
        let config = Config::from_meeting(meeting, config_js, display_name, token)?;
        let runtime = Self::runtime()?;
        let session = runtime.block_on(rv_call::jitsi::join(config)).ok()?;
        Some(Self::driven(runtime, session))
    }

    /// Joins a Chime meeting with an attendee's credentials; `None` when it fails.
    pub fn start_chime(config: rv_call::chime::Config) -> Option<Call> {
        let runtime = Self::runtime()?;
        let session = runtime
            .block_on(rv_call::chime::client::join(config))
            .ok()?;
        Some(Self::driven(runtime, session))
    }

    /// Joins a channel's Mattermost call (the Calls plugin); `None` when it fails.
    /// `ice_servers` is the plugin's servers as JSON (`[{urls, username, credential}]`).
    pub fn start_mattermost(config: rv_call::mattermost::Config, ice_servers: &str) -> Option<Call> {
        let runtime = Self::runtime()?;
        let servers = ice_servers_of(ice_servers);
        let session = runtime.block_on(rv_call::mattermost::join(config, servers)).ok()?;
        Some(Self::driven(runtime, session))
    }

    fn runtime() -> Option<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()
    }

    fn driven<S: CallSignaling + Send + 'static>(
        runtime: tokio::runtime::Runtime,
        session: S,
    ) -> Call {
        let (commands, command_rx) = async_mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::channel();
        let _ = event_tx.send(local_json(&session).to_string());
        runtime.spawn(drive(session, command_rx, event_tx));
        Call {
            runtime,
            commands,
            events: Mutex::new(events),
        }
    }

    /// The next event, waiting up to `timeout`; `None` on timeout, `"{\"type\":\"gone\"}"` once over.
    pub fn next(&self, timeout: Duration) -> Option<String> {
        match self.events.lock().ok()?.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(json!({"type": "gone"}).to_string()),
        }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

async fn drive<S: CallSignaling + Send + 'static>(
    mut session: S,
    mut commands: async_mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<String>,
) {
    let mut share: Option<S> = None;
    loop {
        tokio::select! {
            event = session.next_event() => match event {
                Some(event) => {
                    let ended = matches!(event, Event::Ended(_) | Event::Over(_));
                    if events.send(session_event_json(&session, &event).to_string()).is_err() || ended {
                        break;
                    }
                }
                None => break,
            },
            event = share_event(&mut share) => {
                let over = matches!(event, None | Some(Event::Ended(_) | Event::Over(_)));
                let event = event.map_or_else(|| json!({"type": "ended", "reason": "gone"}), |e| event_json(&e));
                if over {
                    share = None;
                }
                if events.send(share_json(event).to_string()).is_err() {
                    break;
                }
            }
            command = commands.recv() => match command {
                Some(Command::Offer(sdp)) => {
                    if let Err(e) = session.offer(&sdp).await {
                        let _ = events.send(json!({"type": "error", "message": e.to_string()}).to_string());
                    }
                }
                Some(Command::Answer(sdp)) => {
                    if let Err(e) = session.answer(&sdp).await {
                        let _ = events.send(json!({"type": "error", "message": e.to_string()}).to_string());
                    }
                }
                Some(Command::Candidate(c)) => {
                    let _ = session.local_candidate(c).await;
                }
                Some(Command::Muted(muted)) => {
                    let _ = session.set_muted(muted).await;
                }
                Some(Command::Video(video)) => {
                    if let Err(e) = session.set_video(video).await {
                        let _ = events.send(json!({"type": "error", "message": e.to_string()}).to_string());
                    }
                }
                Some(Command::RemoteVideo { track, stream }) => {
                    if let Some(named) = named_json(&track, session.video_of(&track, &stream)) {
                        let _ = events.send(named.to_string());
                    }
                }
                Some(Command::Share(true)) => {
                    if share.is_none() {
                        share = session.screen_session();
                        let event = share.as_ref().map_or_else(|| json!({"type": "ended", "reason": "not_apart"}), local_json);
                        let _ = events.send(share_json(event).to_string());
                    }
                }
                Some(Command::Share(false)) => {
                    if let Some(mut stopped) = share.take() {
                        tokio::spawn(async move { stopped.leave().await });
                    }
                }
                Some(Command::ShareOffer(sdp)) => {
                    if let Some(current) = &mut share
                        && let Err(e) = current.offer(&sdp).await
                    {
                        share = None;
                        let _ = events.send(share_json(json!({"type": "ended", "reason": e.to_string()})).to_string());
                    }
                }
                Some(Command::Leave) | None => {
                    session.leave().await;
                    break;
                }
            },
        }
    }
    if let Some(mut left) = share.take() {
        left.leave().await;
    }
}

fn text(env: &mut JNIEnv, value: &JString) -> Option<String> {
    if value.is_null() {
        return None;
    }
    env.get_string(value).ok().map(Into::into)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_start(
    mut env: JNIEnv,
    _class: JClass,
    meeting: JString,
    config_js: JString,
    display_name: JString,
    token: JString,
) -> jlong {
    let (Some(meeting), Some(config_js)) = (text(&mut env, &meeting), text(&mut env, &config_js))
    else {
        return 0;
    };
    let display_name = text(&mut env, &display_name).unwrap_or_default();
    let token = text(&mut env, &token).filter(|t| !t.is_empty());
    match Call::start(&meeting, &config_js, &display_name, token) {
        Some(call) => {
            let handle = NEXT.fetch_add(1, Ordering::Relaxed);
            CALLS
                .lock()
                .map(|mut calls| calls.insert(handle, Arc::new(call)))
                .map_or(0, |_| handle)
        }
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_startChime(
    mut env: JNIEnv,
    _class: JClass,
    signaling_url: JString,
    audio_host: JString,
    attendee_id: JString,
    join_token: JString,
    origin: JString,
) -> jlong {
    let (Some(signaling_url), Some(audio_host), Some(attendee_id), Some(join_token), Some(origin)) = (
        text(&mut env, &signaling_url),
        text(&mut env, &audio_host),
        text(&mut env, &attendee_id),
        text(&mut env, &join_token),
        text(&mut env, &origin),
    ) else {
        return 0;
    };
    let config = rv_call::chime::Config {
        signaling_url,
        audio_host,
        attendee_id,
        join_token,
        origin,
    };
    match Call::start_chime(config) {
        Some(call) => {
            let handle = NEXT.fetch_add(1, Ordering::Relaxed);
            CALLS
                .lock()
                .map(|mut calls| calls.insert(handle, Arc::new(call)))
                .map_or(0, |_| handle)
        }
        None => 0,
    }
}

fn ice_servers_of(json: &str) -> Vec<rv_call::IceServer> {
    let list: Vec<Value> = serde_json::from_str(json).unwrap_or_default();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    list.iter()
        .filter_map(|s| {
            let urls = s.get("urls")?.as_array()?.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            Some(rv_call::IceServer { urls, username: text(s, "username"), credential: text(s, "credential") })
        })
        .collect()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_startMattermost(
    mut env: JNIEnv,
    _class: JClass,
    server: JString,
    token: JString,
    channel_id: JString,
    user_id: JString,
    ice_servers: JString,
    screen: jboolean,
    camera: jboolean,
) -> jlong {
    let (Some(server), Some(token), Some(channel_id), Some(user_id), Some(ice_servers)) = (
        text(&mut env, &server),
        text(&mut env, &token),
        text(&mut env, &channel_id),
        text(&mut env, &user_id),
        text(&mut env, &ice_servers),
    ) else {
        return 0;
    };
    let config = rv_call::mattermost::Config { server, token, channel_id, user_id, screen: screen != 0, camera: camera != 0 };
    match Call::start_mattermost(config, &ice_servers) {
        Some(call) => {
            let handle = NEXT.fetch_add(1, Ordering::Relaxed);
            CALLS
                .lock()
                .map(|mut calls| calls.insert(handle, Arc::new(call)))
                .map_or(0, |_| handle)
        }
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_next(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    timeout_ms: jlong,
) -> jstring {
    let Some(call) = call(handle) else {
        return std::ptr::null_mut();
    };
    match call.next(Duration::from_millis(timeout_ms.max(0) as u64)) {
        Some(event) => env
            .new_string(event)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_offer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sdp: JString,
) {
    if let (Some(call), Some(sdp)) = (call(handle), text(&mut env, &sdp)) {
        call.send(Command::Offer(sdp));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_answer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sdp: JString,
) {
    if let (Some(call), Some(sdp)) = (call(handle), text(&mut env, &sdp)) {
        call.send(Command::Answer(sdp));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_candidate(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    mid: JString,
    line: JString,
) {
    if let (Some(call), Some(mid), Some(line)) =
        (call(handle), text(&mut env, &mid), text(&mut env, &line))
    {
        call.send(Command::Candidate(Candidate { mid, line }));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_muted(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    muted: jboolean,
) {
    if let Some(call) = call(handle) {
        call.send(Command::Muted(muted != 0));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_video(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    camera: jboolean,
    screen: jboolean,
) {
    if let Some(call) = call(handle) {
        call.send(Command::Video(LocalVideo { camera: camera != 0, screen: screen != 0 }));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_remoteVideo(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    track: JString,
    stream: JString,
) {
    if let (Some(call), Some(track)) = (call(handle), text(&mut env, &track)) {
        let stream = text(&mut env, &stream).unwrap_or_default();
        call.send(Command::RemoteVideo { track, stream });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_share(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    on: jboolean,
) {
    if let Some(call) = call(handle) {
        call.send(Command::Share(on != 0));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_shareOffer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    sdp: JString,
) {
    if let (Some(call), Some(sdp)) = (call(handle), text(&mut env, &sdp)) {
        call.send(Command::ShareOffer(sdp));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_leave(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if let Some(call) = call(handle) {
        call.send(Command::Leave);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rocketvibe_voice_JitsiCall_free(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let Some(call) = CALLS
        .lock()
        .ok()
        .and_then(|mut calls| calls.remove(&handle))
    else {
        return;
    };
    call.send(Command::Leave);
    // The last holder shuts the runtime down; a call still running a command finishes first.
    if let Ok(call) = Arc::try_unwrap(call) {
        call.runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_as_the_kotlin_side_expects() {
        let ice = event_json(&Event::IceServers(vec![rv_call::IceServer {
            urls: vec!["turns:t.example:443?transport=tcp".into()],
            username: "u".into(),
            credential: "c".into(),
        }]));
        assert_eq!(ice["type"], "ice");
        assert_eq!(
            ice["servers"][0]["urls"][0],
            "turns:t.example:443?transport=tcp"
        );
        let roster = event_json(&Event::Roster(vec![rv_call::Member {
            id: "e1".into(),
            name: "Ana".into(),
            muted: true,
        }]));
        assert_eq!(
            roster["members"][0],
            json!({"id": "e1", "name": "Ana", "muted": true})
        );
        let candidate = event_json(&Event::RemoteCandidate(Candidate {
            mid: "audio".into(),
            line: "candidate:1".into(),
        }));
        assert_eq!(
            candidate,
            json!({"type": "candidate", "mid": "audio", "line": "candidate:1"})
        );
        assert_eq!(
            event_json(&Event::OfferWanted { relay_only: true }),
            json!({"type": "offerWanted", "relayOnly": true})
        );
        assert_eq!(
            event_json(&Event::Answer { sdp: "v=0".into() }),
            json!({"type": "answer", "sdp": "v=0"})
        );
        assert_eq!(
            event_json(&Event::Levels(vec![("a".into(), 0.5)])),
            json!({"type": "levels", "levels": [{"id": "a", "level": 0.5}]})
        );
        assert_eq!(
            event_json(&Event::Over("meeting_ended".into())),
            json!({"type": "over", "reason": "meeting_ended"})
        );
        assert_eq!(
            event_json(&Event::Ended("terminated".into())),
            json!({"type": "ended", "reason": "terminated"})
        );
        assert_eq!(
            event_json(&Event::MediaEnded),
            json!({"type": "media_ended"})
        );
    }

    #[test]
    fn the_plugins_ice_servers_are_read_from_json() {
        let servers = ice_servers_of(r#"[{"urls":["stun:s.example:3478"],"username":"","credential":""},{"nope":1}]"#);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["stun:s.example:3478".to_owned()]);
        assert!(ice_servers_of("not json").is_empty());
    }

    /// A provider that takes cameras only, on streams of its own, in sections of its own.
    struct Fake;

    impl CallSignaling for Fake {
        async fn next_event(&mut self) -> Option<Event> {
            None
        }
        async fn answer(&mut self, _sdp: &str) -> Result<(), rv_call::Error> {
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
            "me"
        }
        fn data_channel(&self) -> Option<&'static str> {
            Some("dc")
        }
        fn data_protocol(&self) -> &'static str {
            "proto"
        }
        fn can_send(&self, kind: VideoKind) -> bool {
            kind == VideoKind::Camera
        }
        fn video_stream(&self, kind: VideoKind) -> String {
            format!("me-{}", kind_name(kind))
        }
        fn receive_videos(&self) -> usize {
            2
        }
        fn offers_to_receive_video(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_local_event_says_what_the_call_takes_and_on_which_streams() {
        assert_eq!(
            local_json(&Fake),
            json!({
                "type": "local", "id": "me", "dataChannel": "dc", "dataProtocol": "proto",
                "canCamera": true, "canScreen": false,
                "cameraStream": "me-camera", "screenStream": "me-screen",
                "offersLocalSections": false, "screenApart": false,
            })
        );
    }

    #[test]
    fn an_offer_wanted_says_how_many_sections_receive_video() {
        assert_eq!(
            session_event_json(&Fake, &Event::OfferWanted { relay_only: true }),
            json!({"type": "offerWanted", "relayOnly": true, "receiveVideos": 2, "receiveVideo": true})
        );
        assert_eq!(session_event_json(&Fake, &Event::MediaEnded), json!({"type": "media_ended"}));
    }

    #[test]
    fn a_track_named_by_its_ids_reads_whose_and_what() {
        assert_eq!(
            named_json("screen_s1_x", Some(("s1".into(), VideoKind::Screen))),
            Some(json!({"type": "named", "track": "screen_s1_x", "member": "s1", "kind": "screen"}))
        );
        assert_eq!(named_json("t", None), None);
        assert_eq!(
            named_json("video_s2_y", rv_call::mattermost::video_of("video_s2_y")),
            Some(json!({"type": "named", "track": "video_s2_y", "member": "s2", "kind": "camera"}))
        );
    }

    #[test]
    fn the_remote_videos_read_as_a_list() {
        let videos = event_json(&Event::Videos(vec![rv_call::RemoteVideo {
            track: "e1-v0".into(),
            member: "e1".into(),
            kind: VideoKind::Camera,
        }]));
        assert_eq!(videos, json!({"type": "videos", "videos": [{"track": "e1-v0", "member": "e1", "kind": "camera"}]}));
        assert_eq!(event_json(&Event::DataMessage("hi".into())), json!({"type": "data", "text": "hi"}));
    }

    #[test]
    fn a_page_that_is_not_jitsi_starts_nothing() {
        assert!(Call::start("https://example.com/room", "var config = {};", "Ana", None).is_none());
    }
}
