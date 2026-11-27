//! The video of the call carried here. The app opens a second connection to the
//! call's socket, `<token> video` as its first line, and both sides then send
//! messages of one kind byte, a little-endian `u32` length and the payload:
//!
//! - from the app: [`UP_PICTURE`] (`u32` 90 kHz timestamp, then one H.264
//!   Annex-B access unit), [`UP_SENDING`] (one byte, 1 while the app sends its
//!   camera or screen), [`UP_KEYFRAME`] (the app's decoder wants a keyframe);
//! - to the app: [`DOWN_PICTURE`] (keyframe byte, orientation byte in quarter
//!   turns, `u32` timestamp, the peer's access unit), [`DOWN_PEER`] (one byte, 1
//!   while the peer sends), [`DOWN_KEYFRAME`] (send a keyframe), [`DOWN_REFUSED`]
//!   (the peer refused or let lapse the switch to video the app asked for).
//!
//! The library carries H.264 only; the app encodes and decodes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use whatsapp_rust::async_channel;
use whatsapp_rust::voip::{
    CallHandle, KeyframeUrgency, TimedVideoFrame, VideoFrame, VideoSource, VideoState,
    VideoUpgradeToken,
};

pub const UP_PICTURE: u8 = 1;
pub const UP_SENDING: u8 = 2;
pub const UP_KEYFRAME: u8 = 3;
pub const DOWN_PICTURE: u8 = 1;
pub const DOWN_PEER: u8 = 2;
pub const DOWN_KEYFRAME: u8 = 3;
pub const DOWN_REFUSED: u8 = 4;
/// Larger than any access unit a 720p call sends.
const MAX_MESSAGE: usize = 4 << 20;

#[derive(Debug, PartialEq)]
pub enum Up {
    Picture { timestamp: u32, data: Vec<u8> },
    Sending(bool),
    Keyframe,
}

#[derive(Debug, PartialEq)]
pub enum Down {
    Picture {
        keyframe: bool,
        orientation: u8,
        timestamp: u32,
        data: Vec<u8>,
    },
    Peer(bool),
    Keyframe,
    Refused,
}

/// What the call's events say of video, for the task that drives it.
pub enum News {
    Peer {
        state: VideoState,
        token: Option<VideoUpgradeToken>,
    },
    KeyframeNeeded,
}

pub async fn read_up<R: AsyncRead + Unpin>(read: &mut R) -> std::io::Result<Up> {
    let mut head = [0u8; 5];
    read.read_exact(&mut head).await?;
    let length = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > MAX_MESSAGE {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut payload = vec![0u8; length];
    read.read_exact(&mut payload).await?;
    match head[0] {
        UP_PICTURE if payload.len() > 4 => {
            let timestamp = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            payload.drain(..4);
            Ok(Up::Picture {
                timestamp,
                data: payload,
            })
        }
        UP_SENDING => Ok(Up::Sending(payload.first() == Some(&1))),
        UP_KEYFRAME => Ok(Up::Keyframe),
        _ => Err(std::io::ErrorKind::InvalidData.into()),
    }
}

pub fn encode_down(down: &Down) -> Vec<u8> {
    let (kind, payload) = match down {
        Down::Picture {
            keyframe,
            orientation,
            timestamp,
            data,
        } => {
            let mut payload = Vec::with_capacity(6 + data.len());
            payload.push(u8::from(*keyframe));
            payload.push(*orientation);
            payload.extend_from_slice(&timestamp.to_le_bytes());
            payload.extend_from_slice(data);
            (DOWN_PICTURE, payload)
        }
        Down::Peer(on) => (DOWN_PEER, vec![u8::from(*on)]),
        Down::Keyframe => (DOWN_KEYFRAME, Vec::new()),
        Down::Refused => (DOWN_REFUSED, Vec::new()),
    };
    let mut message = Vec::with_capacity(5 + payload.len());
    message.push(kind);
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.extend_from_slice(&payload);
    message
}

/// Reads the app's messages into `up` and writes `down` to it, until either closes.
pub async fn pump<R, W>(
    mut read: R,
    mut write: W,
    up: async_channel::Sender<Up>,
    down: async_channel::Receiver<Down>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let inbound = async move {
        while let Ok(message) = read_up(&mut read).await {
            let sent = match message {
                // A full queue drops the picture: the library is behind, not the app.
                picture @ Up::Picture { .. } => match up.try_send(picture) {
                    Err(async_channel::TrySendError::Closed(_)) => Err(()),
                    _ => Ok(()),
                },
                control => up.send(control).await.map_err(|_| ()),
            };
            if sent.is_err() {
                return;
            }
        }
    };
    let outbound = async move {
        while let Ok(message) = down.recv().await {
            if write.write_all(&encode_down(&message)).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        _ = inbound => {}
        _ = outbound => {}
    }
}

/// This side's camera or screen as the library takes it: capture-timestamped access units.
pub struct Picture {
    timed: async_channel::Receiver<TimedVideoFrame>,
}

impl VideoSource for Picture {
    fn frames(&self) -> async_channel::Receiver<Vec<u8>> {
        async_channel::bounded(1).1
    }

    fn timed_frames(&self) -> Option<async_channel::Receiver<TimedVideoFrame>> {
        Some(self.timed.clone())
    }
}

fn picture() -> (async_channel::Sender<TimedVideoFrame>, Picture) {
    let (tx, timed) = async_channel::bounded(4);
    (tx, Picture { timed })
}

/// Where the library plays the peer's pictures, made before the call is
/// answered so that a video call is accepted with it.
pub struct Link {
    playout: async_channel::Sender<VideoFrame>,
    peer: async_channel::Receiver<VideoFrame>,
}

impl Link {
    pub fn new() -> Self {
        let (playout, peer) = async_channel::bounded(8);
        Link { playout, peer }
    }

    /// A source that sends nothing yet, and the sink.
    pub fn endpoints(&self) -> (Picture, async_channel::Sender<VideoFrame>) {
        (picture().1, self.playout.clone())
    }
}

/// Drives the call's video for as long as the call lasts: the app's pictures
/// into the library, the peer's to the app. The peer's switch to video is
/// accepted without sending anything, so their picture shows here while this
/// side's camera stays off until the app turns it on.
pub async fn drive(
    link: Link,
    handle: CallHandle,
    up: async_channel::Receiver<Up>,
    down: async_channel::Sender<Down>,
    news: async_channel::Receiver<News>,
    video_call: bool,
) {
    let forward = {
        let (down, peer) = (down.clone(), link.peer.clone());
        tokio::spawn(async move {
            while let Ok(frame) = peer.recv().await {
                let picture = Down::Picture {
                    keyframe: frame.keyframe,
                    orientation: frame.orientation,
                    timestamp: frame.timestamp,
                    data: frame.data,
                };
                if let Err(async_channel::TrySendError::Closed(_)) = down.try_send(picture) {
                    return;
                }
            }
        })
    };
    let mut sending: Option<async_channel::Sender<TimedVideoFrame>> = None;
    if video_call {
        let _ = handle.stop_video().await;
    }
    loop {
        tokio::select! {
            message = up.recv() => match message {
                Err(_) => break,
                Ok(Up::Picture { timestamp, data }) => {
                    if let Some(tx) = &sending {
                        let _ = tx.try_send(TimedVideoFrame::builder().data(data).timestamp(timestamp).build());
                    }
                }
                Ok(Up::Sending(true)) if sending.is_none() => {
                    // Back on in a call that has video; otherwise the peer is asked to switch.
                    let (tx, source) = picture();
                    let started = match handle.resume_video(source, link.playout.clone()).await {
                        Ok(()) => Ok(tx),
                        Err(_) => {
                            let (tx, source) = picture();
                            handle.start_video(source, link.playout.clone()).await.map(|()| tx)
                        }
                    };
                    match started {
                        Ok(tx) => sending = Some(tx),
                        Err(e) => {
                            log::warn!("call video not started: {e}");
                            let _ = down.send(Down::Refused).await;
                        }
                    }
                }
                Ok(Up::Sending(false)) if sending.is_some() => {
                    sending = None;
                    let _ = handle.stop_video().await;
                }
                Ok(Up::Sending(_)) => {}
                Ok(Up::Keyframe) => handle.request_peer_keyframe(KeyframeUrgency::Coalesced),
            },
            news = news.recv() => match news {
                Err(_) => break,
                Ok(News::KeyframeNeeded) => {
                    let _ = down.try_send(Down::Keyframe);
                }
                Ok(News::Peer { state, token }) => match state {
                    VideoState::UpgradeRequest | VideoState::UpgradeRequestV2 => {
                        let Some(token) = token else { continue };
                        let (tx, source) = picture();
                        if let Err(e) = handle.accept_video(token, source, link.playout.clone()).await {
                            log::warn!("call video upgrade not accepted: {e}");
                        } else if sending.is_none() {
                            let _ = handle.stop_video().await;
                        } else {
                            sending = Some(tx);
                        }
                    }
                    VideoState::Enabled | VideoState::UpgradeAccept => {
                        let _ = down.try_send(Down::Peer(true));
                    }
                    VideoState::Disabled | VideoState::Paused | VideoState::Stopped => {
                        let _ = down.try_send(Down::Peer(false));
                    }
                    VideoState::UpgradeReject | VideoState::UpgradeRejectByTimeout if sending.is_some() => {
                        sending = None;
                        let _ = down.try_send(Down::Refused);
                    }
                    _ => {}
                },
            },
        }
    }
    forward.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up_message(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut message = vec![kind];
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(payload);
        message
    }

    #[tokio::test]
    async fn the_apps_messages_read_back_as_sent() {
        let mut bytes = up_message(UP_PICTURE, &[0x10, 0, 0, 0, 0, 0, 0, 1, 0x65]);
        bytes.extend(up_message(UP_SENDING, &[1]));
        bytes.extend(up_message(UP_KEYFRAME, &[]));
        let mut read = bytes.as_slice();
        assert_eq!(
            read_up(&mut read).await.unwrap(),
            Up::Picture {
                timestamp: 16,
                data: vec![0, 0, 0, 1, 0x65]
            }
        );
        assert_eq!(read_up(&mut read).await.unwrap(), Up::Sending(true));
        assert_eq!(read_up(&mut read).await.unwrap(), Up::Keyframe);
        assert!(read_up(&mut read).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_or_oversized_message_closes_the_link() {
        let mut read: &[u8] = &up_message(9, &[]);
        assert!(read_up(&mut read).await.is_err());
        let mut huge = vec![UP_PICTURE];
        huge.extend_from_slice(&(MAX_MESSAGE as u32 + 1).to_le_bytes());
        let mut read = huge.as_slice();
        assert!(read_up(&mut read).await.is_err());
    }

    #[test]
    fn a_peer_picture_carries_its_keyframe_orientation_and_time() {
        let message = encode_down(&Down::Picture {
            keyframe: true,
            orientation: 3,
            timestamp: 0x0102_0304,
            data: vec![0, 0, 0, 1, 0x67],
        });
        assert_eq!(
            message,
            [
                DOWN_PICTURE,
                11,
                0,
                0,
                0,
                1,
                3,
                4,
                3,
                2,
                1,
                0,
                0,
                0,
                1,
                0x67
            ]
        );
        assert_eq!(encode_down(&Down::Peer(false)), [DOWN_PEER, 1, 0, 0, 0, 0]);
        assert_eq!(encode_down(&Down::Refused), [DOWN_REFUSED, 0, 0, 0, 0]);
    }
}
