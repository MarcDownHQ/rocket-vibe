//! WhatsApp 1:1 calls carried by the app, audio only. The library speaks the
//! call (signalling, relay, MLow); its audio is 16 kHz mono in 60 ms frames,
//! which this engine serves on a socket of its own on 127.0.0.1 so that the
//! app's audio engine (the voice sidecar, Android's recorder and player) can
//! carry it whichever process holds the engine. Each frame is 960 samples,
//! little-endian i16 (1920 bytes), both ways; the first line the client
//! sends is the token the `call` frame gave it.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use whatsapp_rust::async_channel;
use whatsapp_rust::voip::CallHandle;

use crate::engine::{Engine, Ringing};
use crate::wire::{Call, Command, Frame};

/// One frame of a call's audio: 60 ms at 16 kHz, mono.
pub const FRAME_SAMPLES: usize = 960;
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// How long the app may take to open the audio socket.
const AUDIO_WAIT: Duration = Duration::from_secs(20);

/// The call carried here, at most one.
pub(crate) struct Active {
    pub id: String,
    pub handle: CallHandle,
}

/// The audio socket of a call: where the app connects, and the library's ends.
struct Bridge {
    address: String,
    microphone: async_channel::Receiver<Vec<i16>>,
    speaker: async_channel::Sender<Vec<i16>>,
}

/// Listens for the app's audio on 127.0.0.1, then pumps frames until either
/// side closes: the app's microphone into the library, the library's playout out.
async fn bridge(stopped: tokio::sync::watch::Receiver<bool>) -> std::io::Result<Bridge> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = crate::engine::random_token();
    let address = format!("{}#{token}", listener.local_addr()?);
    let (mic_tx, microphone) = async_channel::bounded::<Vec<i16>>(8);
    let (speaker, spk_rx) = async_channel::bounded::<Vec<i16>>(8);
    tokio::spawn(async move {
        let Ok(Ok((socket, _))) = tokio::time::timeout(AUDIO_WAIT, listener.accept()).await else { return };
        drop(listener);
        let _ = socket.set_nodelay(true);
        let (read, mut write) = socket.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        let said = tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line)).await;
        if !matches!(said, Ok(Ok(_))) || line.trim_end() != token {
            return;
        }
        let mut stopped_in = stopped.clone();
        let inbound = async move {
            let mut bytes = vec![0u8; FRAME_BYTES];
            loop {
                tokio::select! {
                    read = read.read_exact(&mut bytes) => {
                        if read.is_err() { return; }
                        let frame = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
                        // A full queue drops the frame: the library is behind, not the app.
                        let _ = mic_tx.try_send(frame);
                    }
                    _ = stopped_in.changed() => return,
                }
            }
        };
        let mut stopped_out = stopped;
        let outbound = async move {
            loop {
                tokio::select! {
                    frame = spk_rx.recv() => {
                        let Ok(frame) = frame else { return };
                        let bytes: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
                        if write.write_all(&bytes).await.is_err() { return; }
                    }
                    _ = stopped_out.changed() => return,
                }
            }
        };
        tokio::join!(inbound, outbound);
    });
    Ok(Bridge { address, microphone, speaker })
}

impl Engine {
    fn call_frame(&self, id: &str, from: &str, state: &str, audio: &str, reason: &str) {
        let call = Call {
            id: id.to_owned(),
            from: from.to_owned(),
            group: String::new(),
            video: false,
            state: state.to_owned(),
            audio: audio.to_owned(),
            reason: reason.to_owned(),
        };
        self.emit(Frame::Call { call });
    }

    /// Answers the call ringing here (`cmd.id`), audio only: `connecting` with
    /// the audio socket, `active` once the media flows, `ended` when it stops.
    pub(crate) async fn accept_call(self: Arc<Self>, cmd: Command) {
        let ringing = self.calls.lock().expect("calls").remove(&cmd.id);
        let (Some(Ringing { creator, incoming, .. }), Some(client)) = (ringing, self.ready()) else {
            return self.call_frame(&cmd.id, "", "ended", "", "not_ringing");
        };
        let peer = self.canonical(&creator).await.to_string();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let Ok(bridge) = bridge(stopped).await else {
            return self.call_frame(&cmd.id, &peer, "ended", "", "audio_failed");
        };
        let started = client.voip().accept(&incoming).audio(bridge.microphone, bridge.speaker).start().await;
        match started {
            Ok(handle) => self.run_call(cmd.id, peer, handle, bridge.address, stop).await,
            Err(e) => {
                log::warn!("call not accepted: {e}");
                let _ = stop.send(true);
                self.call_frame(&cmd.id, &peer, "ended", "", "accept_failed");
            }
        }
    }

    /// Calls `cmd.chat`, a person, audio only: `calling` with the audio socket,
    /// `active` once answered and the media flows, `ended` when it stops.
    pub(crate) async fn place_call(self: Arc<Self>, cmd: Command) {
        let Some(peer) = crate::content::valid_chat(&cmd.chat).filter(|j| !crate::content::is_group(&j.to_string()))
        else {
            return self.call_frame(&cmd.id, &cmd.chat, "ended", "", "not_callable");
        };
        let Some(client) = self.ready() else { return self.call_frame(&cmd.id, &cmd.chat, "ended", "", "not_connected") };
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let Ok(bridge) = bridge(stopped).await else {
            return self.call_frame(&cmd.id, &cmd.chat, "ended", "", "audio_failed");
        };
        let started = client.voip().call(&peer).audio(bridge.microphone, bridge.speaker).start().await;
        match started {
            Ok(handle) => {
                let id = handle.call_id().to_owned();
                // The app knows the call by the id it asked with until this frame names the real one.
                self.call_frame(&cmd.id, &cmd.chat, "placed", &id, "");
                self.run_call(id, cmd.chat, handle, bridge.address, stop).await;
            }
            Err(e) => {
                log::warn!("call not placed: {e}");
                let _ = stop.send(true);
                self.call_frame(&cmd.id, &cmd.chat, "ended", "", "place_failed");
            }
        }
    }

    async fn run_call(
        self: Arc<Self>,
        id: String,
        peer: String,
        handle: CallHandle,
        address: String,
        stop: tokio::sync::watch::Sender<bool>,
    ) {
        if let Some(previous) = self.active.lock().await.replace(Active { id: id.clone(), handle: handle.clone() }) {
            let _ = previous.handle.terminate().await;
        }
        self.call_frame(&id, &peer, "connecting", &address, "");
        let mut events = handle.take_events();
        let engine = Arc::clone(&self);
        let (watch_id, watch_peer) = (id.clone(), peer.clone());
        let watcher = tokio::spawn(async move {
            let Some(events) = events.as_mut() else { return };
            let mut active = false;
            while let Ok(event) = events.recv().await {
                use whatsapp_rust::voip::CallEvent;
                match event {
                    CallEvent::MediaConnected if !active => {
                        active = true;
                        engine.call_frame(&watch_id, &watch_peer, "active", "", "");
                    }
                    CallEvent::Closed(reason) => {
                        log::info!("call media closed: {reason:?}");
                        return;
                    }
                    _ => {}
                }
            }
        });
        handle.wait_ended().await;
        watcher.abort();
        let _ = stop.send(true);
        let mut active = self.active.lock().await;
        if active.as_ref().is_some_and(|a| a.id == id) {
            *active = None;
        }
        drop(active);
        self.call_frame(&id, &peer, "ended", "", "over");
    }

    /// Hangs up the call carried here.
    pub(crate) async fn hangup_call(self: Arc<Self>, cmd: Command) {
        let active = self.active.lock().await.take();
        if let Some(active) = active.filter(|a| cmd.id.is_empty() || a.id == cmd.id) {
            let _ = active.handle.terminate().await;
        }
    }

    /// Mutes (`text` "on") or unmutes the call carried here; the other side is told.
    pub(crate) async fn mute_call(self: Arc<Self>, cmd: Command) {
        let handle = self.active.lock().await.as_ref().map(|a| a.handle.clone());
        if let Some(handle) = handle {
            let _ = handle.set_muted(cmd.text == "on").await;
        }
    }

    /// The other side ended or refused the call carried here.
    pub(crate) async fn remote_ended(&self, id: &str) {
        let handle = self.active.lock().await.as_ref().filter(|a| a.id == id).map(|a| a.handle.clone());
        if let Some(handle) = handle {
            handle.hangup_local().await;
        }
    }
}
