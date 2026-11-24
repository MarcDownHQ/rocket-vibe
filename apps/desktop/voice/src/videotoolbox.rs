//! H.264 decoding on macOS, where libwebrtc's FFmpeg carries no H.264 decoder:
//! VideoToolbox, fed the Annex-B access units `h264::Decoder` takes elsewhere.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use livekit::webrtc::video_frame::I420Buffer;

type Ref = *const c_void;
type OsStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type OutputCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, OsStatus, u32, Ref, CmTime, CmTime);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[repr(C)]
struct Opaque([u8; 0]);

const NUMBER_SINT32: isize = 3;
const BLOCK_ASSURE_MEMORY_NOW: u32 = 1;
const LOCK_READ_ONLY: u64 = 1;
const FORMAT_NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const FORMAT_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const FORMAT_I420: u32 = u32::from_be_bytes(*b"y420");

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFRelease(object: Ref);
    fn CFNumberCreate(allocator: Ref, kind: isize, value: *const c_void) -> Ref;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> Ref;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: Ref,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_length_size: i32,
        out: *mut Ref,
    ) -> OsStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: Ref,
        memory: *mut c_void,
        length: usize,
        block_allocator: Ref,
        source: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut Ref,
    ) -> OsStatus;
    fn CMBlockBufferReplaceDataBytes(source: *const c_void, buffer: Ref, offset: usize, length: usize) -> OsStatus;
    fn CMSampleBufferCreateReady(
        allocator: Ref,
        data: Ref,
        format: Ref,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes_count: isize,
        sizes: *const usize,
        out: *mut Ref,
    ) -> OsStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: Ref;
    fn CVPixelBufferLockBaseAddress(buffer: Ref, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: Ref, flags: u64) -> i32;
    fn CVPixelBufferGetPixelFormatType(buffer: Ref) -> u32;
    fn CVPixelBufferGetWidth(buffer: Ref) -> usize;
    fn CVPixelBufferGetHeight(buffer: Ref) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: Ref, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: Ref, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: Ref,
        format: Ref,
        decoder_specification: Ref,
        image_attributes: Ref,
        callback: *const OutputCallbackRecord,
        out: *mut Ref,
    ) -> OsStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: Ref,
        sample: Ref,
        flags: u32,
        frame_refcon: *mut c_void,
        info: *mut u32,
    ) -> OsStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: Ref) -> OsStatus;
    fn VTDecompressionSessionInvalidate(session: Ref);
}

#[derive(Default)]
struct Output {
    pictures: Vec<I420Buffer>,
    failed: bool,
}

/// VideoToolbox's H.264 decoder: access units in, I420 pictures out.
pub struct Decoder {
    sps: Vec<u8>,
    pps: Vec<u8>,
    format: Ref,
    session: Ref,
    output: Box<Mutex<Output>>,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Option<Self> {
        Some(Decoder {
            sps: Vec::new(),
            pps: Vec::new(),
            format: ptr::null(),
            session: ptr::null(),
            output: Box::default(),
        })
    }

    /// The pictures one access unit completes; `Err` when the decoder refused
    /// it, so the sender should be asked for a keyframe.
    pub fn decode(&mut self, unit: &[u8]) -> Result<Vec<I420Buffer>, ()> {
        let mut sample = Vec::with_capacity(unit.len() + 16);
        let (mut sps, mut pps) = (None, None);
        for nal in nal_units(unit) {
            match nal[0] & 0x1f {
                7 => sps = Some(nal),
                8 => pps = Some(nal),
                9 => {}
                _ => {
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        if let (Some(sps), Some(pps)) = (sps, pps)
            && (sps != self.sps || pps != self.pps || self.session.is_null())
        {
            self.release();
            self.sps = sps.to_vec();
            self.pps = pps.to_vec();
            self.open()?;
        }
        if sample.is_empty() {
            return Ok(Vec::new());
        }
        if self.session.is_null() {
            return Err(());
        }
        unsafe { self.decode_sample(&sample) }
    }

    fn open(&mut self) -> Result<(), ()> {
        unsafe {
            let pointers = [self.sps.as_ptr(), self.pps.as_ptr()];
            let sizes = [self.sps.len(), self.pps.len()];
            let mut format = ptr::null();
            if CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                2,
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut format,
            ) != 0
            {
                return Err(());
            }
            self.format = format;
            let pixel_format = FORMAT_NV12_VIDEO as i32;
            let number = CFNumberCreate(ptr::null(), NUMBER_SINT32, (&raw const pixel_format).cast());
            let keys = [kCVPixelBufferPixelFormatTypeKey];
            let values = [number];
            let attributes = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            CFRelease(number);
            let record =
                OutputCallbackRecord { callback: decoded, refcon: (&raw const *self.output).cast_mut().cast() };
            let mut session = ptr::null();
            let status =
                VTDecompressionSessionCreate(ptr::null(), format, ptr::null(), attributes, &record, &mut session);
            CFRelease(attributes);
            if status != 0 {
                return Err(());
            }
            self.session = session;
        }
        Ok(())
    }

    unsafe fn decode_sample(&mut self, sample: &[u8]) -> Result<Vec<I420Buffer>, ()> {
        unsafe {
            let mut block = ptr::null();
            if CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                sample.len(),
                ptr::null(),
                ptr::null(),
                0,
                sample.len(),
                BLOCK_ASSURE_MEMORY_NOW,
                &mut block,
            ) != 0
            {
                return Err(());
            }
            if CMBlockBufferReplaceDataBytes(sample.as_ptr().cast(), block, 0, sample.len()) != 0 {
                CFRelease(block);
                return Err(());
            }
            let size = sample.len();
            let mut buffer = ptr::null();
            let status =
                CMSampleBufferCreateReady(ptr::null(), block, self.format, 1, 0, ptr::null(), 1, &size, &mut buffer);
            CFRelease(block);
            if status != 0 {
                return Err(());
            }
            let mut info = 0;
            let status = VTDecompressionSessionDecodeFrame(self.session, buffer, 0, ptr::null_mut(), &mut info);
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            CFRelease(buffer);
            let output = std::mem::take(&mut *self.output.lock().unwrap_or_else(|e| e.into_inner()));
            if status != 0 || (output.failed && output.pictures.is_empty()) {
                return Err(());
            }
            Ok(output.pictures)
        }
    }

    fn release(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
                self.format = ptr::null();
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.release();
    }
}

unsafe extern "C" fn decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OsStatus,
    _info: u32,
    image: Ref,
    _presented: CmTime,
    _duration: CmTime,
) {
    let output = unsafe { &*refcon.cast::<Mutex<Output>>() };
    let picture = if status == 0 && !image.is_null() { unsafe { picture(image) } } else { None };
    let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
    match picture {
        Some(picture) => output.pictures.push(picture),
        None => output.failed = true,
    }
}

unsafe fn picture(image: Ref) -> Option<I420Buffer> {
    unsafe {
        if CVPixelBufferLockBaseAddress(image, LOCK_READ_ONLY) != 0 {
            return None;
        }
        let picture = copy_planes(image);
        CVPixelBufferUnlockBaseAddress(image, LOCK_READ_ONLY);
        picture
    }
}

unsafe fn copy_planes(image: Ref) -> Option<I420Buffer> {
    unsafe {
        let format = CVPixelBufferGetPixelFormatType(image);
        let (width, height) = (CVPixelBufferGetWidth(image) as u32 & !1, CVPixelBufferGetHeight(image) as u32 & !1);
        if width == 0 || height == 0 {
            return None;
        }
        let plane = |i| (CVPixelBufferGetBaseAddressOfPlane(image, i), CVPixelBufferGetBytesPerRowOfPlane(image, i));
        let mut buffer = I420Buffer::new(width, height);
        let (dy, du, dv) = buffer.strides();
        let (y, u, v) = buffer.data_mut();
        let (w, h, half_w, half_h) = (width as usize, height as usize, width as usize / 2, height as usize / 2);
        let (luma, luma_stride) = plane(0);
        if luma.is_null() {
            return None;
        }
        for row in 0..h {
            let from = std::slice::from_raw_parts(luma.add(row * luma_stride), w);
            y[row * dy as usize..][..w].copy_from_slice(from);
        }
        match format {
            FORMAT_NV12_VIDEO | FORMAT_NV12_FULL => {
                let (chroma, stride) = plane(1);
                if chroma.is_null() {
                    return None;
                }
                for row in 0..half_h {
                    let from = std::slice::from_raw_parts(chroma.add(row * stride), half_w * 2);
                    for col in 0..half_w {
                        u[row * du as usize + col] = from[2 * col];
                        v[row * dv as usize + col] = from[2 * col + 1];
                    }
                }
            }
            FORMAT_I420 => {
                for (i, dst, dst_stride) in [(1, &mut *u, du), (2, &mut *v, dv)] {
                    let (src, stride) = plane(i);
                    if src.is_null() {
                        return None;
                    }
                    for row in 0..half_h {
                        let from = std::slice::from_raw_parts(src.add(row * stride), half_w);
                        dst[row * dst_stride as usize..][..half_w].copy_from_slice(from);
                    }
                }
            }
            _ => return None,
        }
        Some(buffer)
    }
}

/// The NAL units of an Annex-B access unit, start codes stripped.
fn nal_units(unit: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= unit.len() {
        if unit[i] == 0 && unit[i + 1] == 0 && unit[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(unit.len(), |&(at, _)| at);
        while end > begin && unit[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            units.push(&unit[begin..end]);
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_annex_b_unit_splits_on_three_and_four_byte_start_codes() {
        let unit = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        assert_eq!(nal_units(&unit), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }
}
