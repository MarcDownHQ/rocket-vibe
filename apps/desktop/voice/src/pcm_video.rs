//! The video of a call carried as raw media by another process (a WhatsApp
//! call, `Command::ConnectPcm`): its second connection, `<token> video`, takes
//! this side's camera or screen as H.264 access units and brings the peer's.
//! Messages are one kind byte, a little-endian `u32` length and the payload
//! (`crates/rv-whatsapp/engine/src/call_video.rs`). One picture goes out at a
//! time: the screen while it is shared, else the camera.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer};
use rv_voice_protocol::frames::{LOCAL_PREVIEW, Source};
use rv_voice_protocol::{Command, ScreenQuality};
use tokio::sync::mpsc::UnboundedSender;

use crate::call::VideoOut;
use crate::h264::{Decoder, Encoder, rotated};
use crate::video::{self, Capture, Internal};
use crate::{diagnose, error};

const UP_PICTURE: u8 = 1;
const UP_SENDING: u8 = 2;
const UP_KEYFRAME: u8 = 3;
const DOWN_PICTURE: u8 = 1;
const DOWN_PEER: u8 = 2;
const DOWN_KEYFRAME: u8 = 3;
const DOWN_REFUSED: u8 = 4;
const MAX_MESSAGE: usize = 4 << 20;

/// What goes out: at most 720p at 15 pictures a second, as WhatsApp's own
/// clients send, with a keyframe every three seconds.
const SENT_FIT: (u32, u32) = (1280, 720);
const SENT_INTERVAL: Duration = Duration::from_millis(66);
const SENT_FPS: f32 = 15.0;
const KEYFRAME_EVERY: u32 = 45;
const CAMERA_BITRATE: u32 = 800_000;
const SCREEN_BITRATE: u32 = 1_200_000;
/// How often the peer is asked for a keyframe while the decoder fails.
const KEYFRAME_ASK: Duration = Duration::from_secs(1);

const NOTHING: u8 = 0;
const CAMERA: u8 = 1;
const SCREEN: u8 = 2;

/// What the call's participants show, read by its publisher.
#[derive(Default)]
pub struct Flags {
    pub camera: AtomicBool,
    pub screen: AtomicBool,
    pub peer: AtomicBool,
}

/// Between the main loop and the threads.
struct Shared {
    wanted: AtomicU8,
    keyframe: AtomicBool,
    peer: Mutex<String>,
    flags: Arc<Flags>,
}

/// News of the video connection for the main loop.
pub enum News {
    /// The peer refused the switch to video, or let it lapse.
    Refused,
}

pub struct PcmVideo {
    out: VideoOut,
    internal: UnboundedSender<Internal>,
    camera: Option<Capture>,
    screen: Option<Capture>,
    writer: Sender<Vec<u8>>,
    pictures: SyncSender<(Source, I420Buffer)>,
    shared: Arc<Shared>,
    socket: TcpStream,
}

fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(5 + payload.len());
    message.push(kind);
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.extend_from_slice(payload);
    message
}

fn picture_message(timestamp: u32, unit: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + unit.len());
    payload.extend_from_slice(&timestamp.to_le_bytes());
    payload.extend_from_slice(unit);
    message(UP_PICTURE, &payload)
}

fn read_message(read: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    read.read_exact(&mut head)?;
    let length = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > MAX_MESSAGE {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut payload = vec![0u8; length];
    read.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

impl PcmVideo {
    /// Opens the call's video connection at `host` with `token`; `None` when
    /// the other process carries no video.
    pub fn connect(
        host: &str,
        token: &str,
        out: VideoOut,
        internal: UnboundedSender<Internal>,
        news: UnboundedSender<News>,
        flags: Arc<Flags>,
    ) -> Option<Self> {
        let socket = TcpStream::connect(host)
            .and_then(|mut socket| {
                socket.set_nodelay(true)?;
                socket.write_all(format!("{token} video\n").as_bytes())?;
                Ok(socket)
            })
            .map_err(|e| diagnose(&format!("call: video socket: {e}")))
            .ok()?;
        let (writer, outbox) = std::sync::mpsc::channel::<Vec<u8>>();
        let (pictures, captured) = std::sync::mpsc::sync_channel::<(Source, I420Buffer)>(2);
        let shared = Arc::new(Shared {
            wanted: AtomicU8::new(NOTHING),
            keyframe: AtomicBool::new(false),
            peer: Mutex::new("peer".to_owned()),
            flags,
        });
        let mut write = socket.try_clone().ok()?;
        std::thread::spawn(move || {
            while let Ok(message) = outbox.recv() {
                if write.write_all(&message).is_err() {
                    return;
                }
            }
        });
        let read = socket.try_clone().ok()?;
        let (reader_shared, reader_writer, frames) = (shared.clone(), writer.clone(), out.frames.clone());
        std::thread::spawn(move || receive(read, reader_shared, reader_writer, frames, news));
        let (encoder_shared, encoder_writer) = (shared.clone(), writer.clone());
        std::thread::spawn(move || encode(captured, encoder_shared, encoder_writer));
        Some(PcmVideo { out, internal, camera: None, screen: None, writer, pictures, shared, socket })
    }

    /// The person on the other end, whose pictures are shown under their id.
    pub fn set_peer(&self, id: &str) {
        *self.shared.peer.lock().unwrap_or_else(|e| e.into_inner()) = id.to_owned();
    }

    /// The app's camera and screen commands.
    pub fn command(&mut self, command: Command) {
        match command {
            Command::ListScreens => video::list_screens(self.out.fake, self.out.frames.clone()),
            Command::SetCamera { enabled } => self.want(Source::Camera, enabled, None, None),
            Command::StartScreenShare { source, quality, .. } => self.want(Source::Screen, true, source, quality),
            Command::StopScreenShare => self.want(Source::Screen, false, None, None),
            _ => {}
        }
    }

    fn want(&mut self, source: Source, on: bool, chosen: Option<String>, quality: Option<ScreenQuality>) {
        let slot = match source {
            Source::Camera => &mut self.camera,
            Source::Screen => &mut self.screen,
        };
        if !on {
            *slot = None;
        } else if slot.is_none() {
            let (fake, frames, internal) = (self.out.fake, self.out.frames.clone(), self.internal.clone());
            let identity = LOCAL_PREVIEW.to_owned();
            let capture = match source {
                Source::Camera => video::camera(fake, frames, identity, internal),
                Source::Screen => {
                    let quality = quality.unwrap_or(ScreenQuality::DEFAULT).clamped();
                    video::screen(fake, chosen, quality, frames, identity, internal)
                }
            };
            capture.tap(self.pictures.clone());
            *slot = Some(capture);
        }
        self.sync();
    }

    /// A capture stopped on its own.
    pub fn news(&mut self, news: Internal) {
        if let Internal::Failed(source, code) = news {
            let slot = match source {
                Source::Camera => &mut self.camera,
                Source::Screen => &mut self.screen,
            };
            if slot.take().is_some() {
                error(code);
                self.sync();
            }
        }
    }

    /// The peer would not switch to video: nothing goes out.
    pub fn refused(&mut self) {
        if self.camera.is_some() || self.screen.is_some() {
            self.camera = None;
            self.screen = None;
            self.sync();
            error("video_declined");
        }
    }

    /// What goes out follows the captures, and the other process is told
    /// when this side starts or stops sending.
    fn sync(&mut self) {
        let wanted = if self.screen.is_some() {
            SCREEN
        } else if self.camera.is_some() {
            CAMERA
        } else {
            NOTHING
        };
        let before = self.shared.wanted.swap(wanted, Ordering::SeqCst);
        self.shared.flags.camera.store(self.camera.is_some(), Ordering::SeqCst);
        self.shared.flags.screen.store(self.screen.is_some(), Ordering::SeqCst);
        if before == wanted {
            return;
        }
        self.shared.keyframe.store(true, Ordering::SeqCst);
        if (before == NOTHING) != (wanted == NOTHING) {
            let _ = self.writer.send(message(UP_SENDING, &[u8::from(wanted != NOTHING)]));
        }
    }
}

impl Drop for PcmVideo {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn code(source: Source) -> u8 {
    match source {
        Source::Camera => CAMERA,
        Source::Screen => SCREEN,
    }
}

/// The encoder thread: the wanted capture's pictures, fitted and paced, as access units.
fn encode(captured: Receiver<(Source, I420Buffer)>, shared: Arc<Shared>, writer: Sender<Vec<u8>>) {
    let started = Instant::now();
    let mut encoder: Option<Encoder> = None;
    let mut last: Option<Instant> = None;
    while let Ok((source, picture)) = captured.recv() {
        if shared.wanted.load(Ordering::SeqCst) != code(source) || last.is_some_and(|at| at.elapsed() < SENT_INTERVAL) {
            continue;
        }
        last = Some(Instant::now());
        let (width, height) = video::fitted(picture.width(), picture.height(), SENT_FIT);
        let mut picture = picture;
        let picture = if (width, height) == (picture.width(), picture.height()) {
            picture
        } else {
            picture.scale(width as i32, height as i32)
        };
        let screen = source == Source::Screen;
        if !encoder.as_ref().is_some_and(|e| e.fits(width, height, screen)) {
            let bitrate = if screen { SCREEN_BITRATE } else { CAMERA_BITRATE };
            encoder = Encoder::new(width, height, screen, bitrate, SENT_FPS, KEYFRAME_EVERY);
            if encoder.is_none() {
                diagnose("call: no H.264 encoder");
                continue;
            }
            shared.keyframe.store(false, Ordering::SeqCst);
        }
        let Some(encoder) = encoder.as_mut() else { continue };
        if shared.keyframe.swap(false, Ordering::SeqCst) {
            encoder.force_keyframe();
        }
        let ms = started.elapsed().as_millis() as i64;
        if let Some(unit) = encoder.encode(&picture, ms)
            && writer.send(picture_message((ms as u64 * 90) as u32, &unit)).is_err()
        {
            return;
        }
    }
}

/// The reader thread: the peer's pictures to the app, and the other process's news.
fn receive(
    mut read: TcpStream,
    shared: Arc<Shared>,
    writer: Sender<Vec<u8>>,
    frames: Option<Arc<video::Frames>>,
    news: UnboundedSender<News>,
) {
    let mut decoder: Option<Decoder> = None;
    let mut asked: Option<Instant> = None;
    let peer = || shared.peer.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut shown: Option<String> = None;
    while let Ok((kind, payload)) = read_message(&mut read) {
        match kind {
            DOWN_PICTURE if payload.len() > 6 => {
                let Some(frames) = &frames else { continue };
                if decoder.is_none() {
                    decoder = Decoder::new();
                }
                let Some(decoder) = decoder.as_mut() else { continue };
                match decoder.decode(&payload[6..]) {
                    Ok(pictures) => {
                        let who = peer();
                        for picture in pictures {
                            match payload[1] % 4 {
                                0 => frames.show(Source::Camera, &who, &picture),
                                turns => frames.show(Source::Camera, &who, &rotated(&picture, turns)),
                            }
                            shown = Some(who.clone());
                            shared.flags.peer.store(true, Ordering::SeqCst);
                        }
                    }
                    Err(()) if asked.is_none_or(|at| at.elapsed() >= KEYFRAME_ASK) => {
                        asked = Some(Instant::now());
                        let _ = writer.send(message(UP_KEYFRAME, &[]));
                    }
                    Err(()) => {}
                }
            }
            DOWN_PEER if payload.first() == Some(&0) => {
                shared.flags.peer.store(false, Ordering::SeqCst);
                if let (Some(frames), Some(who)) = (&frames, shown.take()) {
                    frames.end(Source::Camera, &who);
                }
            }
            DOWN_KEYFRAME => shared.keyframe.store(true, Ordering::SeqCst),
            DOWN_REFUSED => {
                let _ = news.send(News::Refused);
            }
            _ => {}
        }
    }
    shared.flags.peer.store(false, Ordering::SeqCst);
    if let (Some(frames), Some(who)) = (&frames, shown) {
        frames.end(Source::Camera, &who);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_goes_out_as_the_engine_reads_it() {
        let sent = picture_message(0x0102_0304, &[0, 0, 0, 1, 0x65]);
        assert_eq!(sent, [UP_PICTURE, 9, 0, 0, 0, 4, 3, 2, 1, 0, 0, 0, 1, 0x65]);
        let mut read = &sent[..];
        assert_eq!(read_message(&mut read).unwrap(), (UP_PICTURE, sent[5..].to_vec()));
        let mut huge = vec![DOWN_PICTURE];
        huge.extend_from_slice(&(MAX_MESSAGE as u32 + 1).to_le_bytes());
        assert!(read_message(&mut &huge[..]).is_err());
    }

    #[test]
    fn the_encoder_thread_sends_the_wanted_capture_only_and_paces_it() {
        let (pictures, captured) = std::sync::mpsc::sync_channel(8);
        let (writer, outbox) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            wanted: AtomicU8::new(SCREEN),
            keyframe: AtomicBool::new(false),
            peer: Mutex::new("peer".into()),
            flags: Arc::default(),
        });
        let thread = std::thread::spawn({
            let shared = shared.clone();
            move || encode(captured, shared, writer)
        });
        let gray = || {
            let mut buffer = I420Buffer::new(64, 48);
            let (y, u, v) = buffer.data_mut();
            y.fill(90);
            u.fill(128);
            v.fill(128);
            buffer
        };
        pictures.send((Source::Camera, gray())).unwrap();
        pictures.send((Source::Screen, gray())).unwrap();
        pictures.send((Source::Screen, gray())).unwrap();
        drop(pictures);
        thread.join().unwrap();
        let sent: Vec<Vec<u8>> = outbox.try_iter().collect();
        assert_eq!(sent.len(), 1, "the camera is not wanted, the second screen picture comes too soon");
        assert_eq!(sent[0][0], UP_PICTURE);
        assert_eq!(&sent[0][9..13], &[0, 0, 0, 1], "an access unit after the timestamp");
    }
}
