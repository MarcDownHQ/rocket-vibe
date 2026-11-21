//! H.264 for a call whose media this side encodes itself (WhatsApp's carries
//! access units, not a peer connection). libwebrtc already links OpenH264's
//! encoder and FFmpeg's H.264 decoder on every desktop platform; this calls
//! them directly. The layouts are OpenH264 2.6's `codec_app_def.h` and
//! FFmpeg's `AVPacket` / `AVFrame` as libwebrtc ships them.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;

use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer};

#[repr(C)]
struct EncParamBase {
    usage: i32,
    width: i32,
    height: i32,
    bitrate: i32,
    rate_control: i32,
    max_fps: f32,
}

#[repr(C)]
struct SourcePicture {
    color_format: i32,
    stride: [i32; 4],
    data: [*mut u8; 4],
    width: i32,
    height: i32,
    timestamp_ms: i64,
    psnr: [bool; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LayerInfo {
    temporal_id: u8,
    spatial_id: u8,
    quality_id: u8,
    frame_type: i32,
    layer_type: u8,
    sub_seq_id: i32,
    nal_count: i32,
    nal_lengths: *mut i32,
    buffer: *mut u8,
    psnr: [f32; 3],
}

const MAX_LAYERS: usize = 128;

#[repr(C)]
struct FrameInfo {
    layer_count: i32,
    layers: [LayerInfo; MAX_LAYERS],
    frame_type: i32,
    size: i32,
    timestamp: i64,
}

const _: () = assert!(size_of::<EncParamBase>() == 24);
const _: () = assert!(size_of::<SourcePicture>() == 80);
const _: () = assert!(size_of::<LayerInfo>() == 56);
const _: () = assert!(size_of::<FrameInfo>() == 7192);

type Encoder_ = *const EncoderVtbl;

#[repr(C)]
struct EncoderVtbl {
    initialize: unsafe extern "C" fn(*mut Encoder_, *const EncParamBase) -> i32,
    initialize_ext: *const c_void,
    get_default_params: *const c_void,
    uninitialize: unsafe extern "C" fn(*mut Encoder_) -> i32,
    encode_frame: unsafe extern "C" fn(*mut Encoder_, *const SourcePicture, *mut FrameInfo) -> i32,
    encode_parameter_sets: *const c_void,
    force_intra_frame: unsafe extern "C" fn(*mut Encoder_, bool, i32) -> i32,
    set_option: unsafe extern "C" fn(*mut Encoder_, i32, *mut c_void) -> i32,
    get_option: *const c_void,
}

const CAMERA_VIDEO_REAL_TIME: i32 = 0;
const SCREEN_CONTENT_REAL_TIME: i32 = 1;
const RC_BITRATE_MODE: i32 = 1;
const VIDEO_FORMAT_I420: i32 = 23;
const FRAME_TYPE_SKIP: i32 = 4;
const OPTION_DATAFORMAT: i32 = 0;
const OPTION_IDR_INTERVAL: i32 = 1;
const OPTION_TRACE_LEVEL: i32 = 25;
const WELS_LOG_QUIET: i32 = 0;

const AV_CODEC_ID_H264: i32 = 27;
const AV_PIX_FMT_YUV420P: i32 = 0;
const AV_PIX_FMT_YUVJ420P: i32 = 12;
const AV_LOG_QUIET: i32 = -8;
const PACKET_DATA: usize = 24;
const PACKET_SIZE: usize = 32;
const FRAME_LINESIZE: usize = 64;
const FRAME_WIDTH: usize = 104;
const FRAME_HEIGHT: usize = 108;
const FRAME_FORMAT: usize = 116;

unsafe extern "C" {
    fn WelsCreateSVCEncoder(encoder: *mut *mut Encoder_) -> i32;
    fn WelsDestroySVCEncoder(encoder: *mut Encoder_);
    fn avcodec_find_decoder(id: i32) -> *const c_void;
    fn avcodec_alloc_context3(codec: *const c_void) -> *mut c_void;
    fn avcodec_open2(context: *mut c_void, codec: *const c_void, options: *mut *mut c_void) -> i32;
    fn avcodec_free_context(context: *mut *mut c_void);
    fn avcodec_send_packet(context: *mut c_void, packet: *const u8) -> i32;
    fn avcodec_receive_frame(context: *mut c_void, frame: *mut u8) -> i32;
    fn av_packet_alloc() -> *mut u8;
    fn av_packet_free(packet: *mut *mut u8);
    fn av_frame_alloc() -> *mut u8;
    fn av_frame_free(frame: *mut *mut u8);
    fn av_log_set_level(level: i32);
}

/// A Constrained Baseline encoder of one size, the profile WhatsApp sends.
pub struct Encoder {
    raw: *mut Encoder_,
    width: u32,
    height: u32,
    screen: bool,
}

// The encoder is used from one thread at a time; OpenH264 keeps no thread affinity.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `fps` paces its rate control; a keyframe every `keyframe_every` pictures.
    pub fn new(width: u32, height: u32, screen: bool, bitrate: u32, fps: f32, keyframe_every: u32) -> Option<Self> {
        let mut raw: *mut Encoder_ = ptr::null_mut();
        unsafe {
            if WelsCreateSVCEncoder(&mut raw) != 0 || raw.is_null() {
                return None;
            }
            let vtbl = &**raw;
            let mut quiet = WELS_LOG_QUIET;
            (vtbl.set_option)(raw, OPTION_TRACE_LEVEL, (&raw mut quiet).cast());
            let params = EncParamBase {
                usage: if screen { SCREEN_CONTENT_REAL_TIME } else { CAMERA_VIDEO_REAL_TIME },
                width: width as i32,
                height: height as i32,
                bitrate: bitrate as i32,
                rate_control: RC_BITRATE_MODE,
                max_fps: fps,
            };
            if (vtbl.initialize)(raw, &params) != 0 {
                WelsDestroySVCEncoder(raw);
                return None;
            }
            let mut format = VIDEO_FORMAT_I420;
            (vtbl.set_option)(raw, OPTION_DATAFORMAT, (&raw mut format).cast());
            let mut interval = keyframe_every as i32;
            (vtbl.set_option)(raw, OPTION_IDR_INTERVAL, (&raw mut interval).cast());
        }
        Some(Encoder { raw, width, height, screen })
    }

    pub fn fits(&self, width: u32, height: u32, screen: bool) -> bool {
        (self.width, self.height, self.screen) == (width, height, screen)
    }

    pub fn force_keyframe(&mut self) {
        unsafe { ((**self.raw).force_intra_frame)(self.raw, true, -1) };
    }

    /// One picture of the encoder's size as an Annex-B access unit, start codes
    /// included; `None` when the rate control skipped it.
    pub fn encode(&mut self, picture: &I420Buffer, timestamp_ms: i64) -> Option<Vec<u8>> {
        if (picture.width(), picture.height()) != (self.width, self.height) {
            return None;
        }
        let (y, u, v) = picture.data();
        let (sy, su, sv) = picture.strides();
        let source = SourcePicture {
            color_format: VIDEO_FORMAT_I420,
            stride: [sy as i32, su as i32, sv as i32, 0],
            data: [y.as_ptr().cast_mut(), u.as_ptr().cast_mut(), v.as_ptr().cast_mut(), ptr::null_mut()],
            width: self.width as i32,
            height: self.height as i32,
            timestamp_ms,
            psnr: [false; 3],
        };
        let mut info: Box<FrameInfo> = Box::new(unsafe { std::mem::zeroed() });
        let status = unsafe { ((**self.raw).encode_frame)(self.raw, &source, &mut *info) };
        if status != 0 || info.frame_type == FRAME_TYPE_SKIP {
            return None;
        }
        let mut unit = Vec::with_capacity(info.size.max(0) as usize);
        for layer in &info.layers[..(info.layer_count.clamp(0, MAX_LAYERS as i32) as usize)] {
            if layer.buffer.is_null() || layer.nal_lengths.is_null() {
                continue;
            }
            let lengths = unsafe { std::slice::from_raw_parts(layer.nal_lengths, layer.nal_count.max(0) as usize) };
            let total: usize = lengths.iter().map(|&n| n.max(0) as usize).sum();
            unit.extend_from_slice(unsafe { std::slice::from_raw_parts(layer.buffer, total) });
        }
        (!unit.is_empty()).then_some(unit)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ((**self.raw).uninitialize)(self.raw);
            WelsDestroySVCEncoder(self.raw);
        }
    }
}

/// FFmpeg's H.264 decoder: access units in, I420 pictures out.
pub struct Decoder {
    context: *mut c_void,
    packet: *mut u8,
    frame: *mut u8,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Option<Self> {
        unsafe {
            av_log_set_level(AV_LOG_QUIET);
            let codec = avcodec_find_decoder(AV_CODEC_ID_H264);
            if codec.is_null() {
                return None;
            }
            let mut context = avcodec_alloc_context3(codec);
            if context.is_null() {
                return None;
            }
            if avcodec_open2(context, codec, ptr::null_mut()) < 0 {
                avcodec_free_context(&mut context);
                return None;
            }
            let (packet, frame) = (av_packet_alloc(), av_frame_alloc());
            let mut decoder = Decoder { context, packet, frame };
            if packet.is_null() || frame.is_null() {
                decoder.release();
                return None;
            }
            Some(decoder)
        }
    }

    /// The pictures one access unit completes; `Err` when the decoder refused
    /// it, so the sender should be asked for a keyframe.
    pub fn decode(&mut self, unit: &[u8]) -> Result<Vec<I420Buffer>, ()> {
        let mut pictures = Vec::new();
        unsafe {
            self.packet.add(PACKET_DATA).cast::<*const u8>().write_unaligned(unit.as_ptr());
            self.packet.add(PACKET_SIZE).cast::<i32>().write_unaligned(unit.len() as i32);
            let sent = avcodec_send_packet(self.context, self.packet);
            self.packet.add(PACKET_DATA).cast::<*const u8>().write_unaligned(ptr::null());
            self.packet.add(PACKET_SIZE).cast::<i32>().write_unaligned(0);
            if sent < 0 {
                return Err(());
            }
            while avcodec_receive_frame(self.context, self.frame) >= 0 {
                if let Some(picture) = self.picture() {
                    pictures.push(picture);
                }
            }
        }
        Ok(pictures)
    }

    unsafe fn picture(&self) -> Option<I420Buffer> {
        let frame = self.frame;
        let field = |offset: usize| unsafe { frame.add(offset).cast::<i32>().read_unaligned() };
        let format = field(FRAME_FORMAT);
        if format != AV_PIX_FMT_YUV420P && format != AV_PIX_FMT_YUVJ420P {
            return None;
        }
        let (width, height) = (field(FRAME_WIDTH).max(0) as u32 & !1, field(FRAME_HEIGHT).max(0) as u32 & !1);
        if width == 0 || height == 0 {
            return None;
        }
        let plane = |i: usize| unsafe { frame.cast::<*const u8>().add(i).read_unaligned() };
        let stride = |i: usize| field(FRAME_LINESIZE + 4 * i).max(0) as usize;
        let mut buffer = I420Buffer::new(width, height);
        let (dy, du, dv) = buffer.strides();
        let (y, u, v) = buffer.data_mut();
        let copy = |src: *const u8, src_stride: usize, dst: &mut [u8], dst_stride: u32, w: u32, h: u32| {
            for row in 0..h as usize {
                let from = unsafe { std::slice::from_raw_parts(src.add(row * src_stride), w as usize) };
                let at = row * dst_stride as usize;
                dst[at..at + w as usize].copy_from_slice(from);
            }
        };
        copy(plane(0), stride(0), y, dy, width, height);
        copy(plane(1), stride(1), u, du, width / 2, height / 2);
        copy(plane(2), stride(2), v, dv, width / 2, height / 2);
        Some(buffer)
    }

    fn release(&mut self) {
        unsafe {
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame);
            }
            if !self.context.is_null() {
                avcodec_free_context(&mut self.context);
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.release();
    }
}

/// `picture` turned by `quarter_turns` clockwise, as a peer's orientation asks.
pub fn rotated(picture: &I420Buffer, quarter_turns: u8) -> I420Buffer {
    let turns = quarter_turns % 4;
    if turns == 0 {
        return copy(picture);
    }
    let (width, height) = (picture.width(), picture.height());
    let (out_w, out_h) = if turns % 2 == 1 { (height, width) } else { (width, height) };
    let mut out = I420Buffer::new(out_w, out_h);
    let (sy, su, sv) = picture.strides();
    let (dy, du, dv) = out.strides();
    let (y, u, v) = picture.data();
    let (oy, ou, ov) = out.data_mut();
    let turn = |src: &[u8], src_stride: u32, dst: &mut [u8], dst_stride: u32, w: u32, h: u32| {
        for row in 0..h {
            for col in 0..w {
                let (x, y) = match turns {
                    1 => (h - 1 - row, col),
                    2 => (w - 1 - col, h - 1 - row),
                    _ => (row, w - 1 - col),
                };
                dst[(y * dst_stride + x) as usize] = src[(row * src_stride + col) as usize];
            }
        }
    };
    turn(y, sy, oy, dy, width, height);
    turn(u, su, ou, du, width / 2, height / 2);
    turn(v, sv, ov, dv, width / 2, height / 2);
    out
}

fn copy(picture: &I420Buffer) -> I420Buffer {
    let mut out = I420Buffer::new(picture.width(), picture.height());
    let (sy, su, sv) = picture.strides();
    let (dy, du, dv) = out.strides();
    let (y, u, v) = picture.data();
    let (oy, ou, ov) = out.data_mut();
    let rows = |src: &[u8], s: u32, dst: &mut [u8], d: u32, w: u32, h: u32| {
        for row in 0..h as usize {
            dst[row * d as usize..][..w as usize].copy_from_slice(&src[row * s as usize..][..w as usize]);
        }
    };
    rows(y, sy, oy, dy, picture.width(), picture.height());
    rows(u, su, ou, du, picture.width() / 2, picture.height() / 2);
    rows(v, sv, ov, dv, picture.width() / 2, picture.height() / 2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: u32, height: u32, shift: u32) -> I420Buffer {
        let mut buffer = I420Buffer::new(width, height);
        let (sy, _, _) = buffer.strides();
        let (y, u, v) = buffer.data_mut();
        for row in 0..height {
            for col in 0..width {
                y[(row * sy + col) as usize] = ((col + shift) % 256) as u8;
            }
        }
        u.fill(128);
        v.fill(128);
        buffer
    }

    #[test]
    fn what_the_encoder_makes_the_decoder_reads_back_at_its_size() {
        let mut encoder = Encoder::new(320, 240, false, 300_000, 15.0, 30).expect("encoder");
        let mut decoder = Decoder::new().expect("decoder");
        let mut shown = Vec::new();
        for i in 0..10 {
            let Some(unit) = encoder.encode(&pattern(320, 240, i * 4), i as i64 * 66) else { continue };
            assert_eq!(&unit[..4], &[0, 0, 0, 1], "Annex-B, start code first");
            shown.extend(decoder.decode(&unit).expect("decoded"));
        }
        assert!(!shown.is_empty(), "pictures come out");
        assert!(shown.iter().all(|p| (p.width(), p.height()) == (320, 240)));
        let (y, _, _) = shown.last().unwrap().data();
        assert!(y[10].abs_diff(y[200]) > 50, "the gradient survived");
    }

    #[test]
    fn a_wrong_size_is_not_encoded_and_garbage_is_refused_or_ignored() {
        let mut encoder = Encoder::new(320, 240, true, 300_000, 15.0, 30).expect("encoder");
        assert!(encoder.encode(&pattern(160, 120, 0), 0).is_none());
        let mut decoder = Decoder::new().expect("decoder");
        assert!(decoder.decode(&[0, 0, 0, 1, 0x65, 1, 2, 3]).map(|p| p.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_quarter_turn_swaps_the_sides_and_moves_the_corner() {
        let mut picture = I420Buffer::new(4, 2);
        let (sy, _, _) = picture.strides();
        picture.data_mut().0[sy as usize] = 200;
        let turned = rotated(&picture, 1);
        assert_eq!((turned.width(), turned.height()), (2, 4));
        let (ty, _, _) = turned.data();
        assert_eq!(ty[0], 200, "bottom-left goes to top-left after a clockwise turn");
        let back = rotated(&rotated(&rotated(&turned, 1), 1), 1);
        assert_eq!(back.data().0[(back.strides().0) as usize], 200);
    }
}
