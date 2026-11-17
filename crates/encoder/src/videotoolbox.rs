//! VideoToolbox, the Mac's video encoder, driven directly. FFmpeg's wrapper
//! hands each frame out only when the next one is sent: a frame of delay,
//! and the last frame of a desktop that went still would wait for the next
//! change. Here every frame is completed before `encode` returns.
//!
//! Input is the capture's IOSurface frame as it is: VideoToolbox converts it
//! to YUV and scales it to the encoded size on the GPU. Without one, a pixel
//! buffer filled from memory takes its place. The bitrate cap changes in
//! place, without a keyframe.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use gpu_interop::macos::{PixelBuffer, PixelBufferFrame};
use objc2_core_foundation::{kCFBooleanTrue, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    CMSampleBuffer, CMTime,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferIOSurfacePropertiesKey, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_MinAllowedFrameQP,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession, VTEncodeInfoFlags, VTSession,
    VTSessionCopyProperty, VTSessionSetProperty,
};
use remothing_core::encode::{Codec, Encoded, VideoEncoder};
use remothing_core::geom::Rect;
use remothing_core::platform::GpuSurface;

use crate::ffmpeg::{mark_no_reorder, MAX_BITRATE, MIN_BITRATE};

/// The quality floor of the other backends (QP 20): bits the cap allows
/// beyond it buy nothing visible on screen content.
const MIN_QP: i32 = 20;

/// What the output callback leaves for `encode`.
struct Output {
    codec: Codec,
    result: Option<Result<Encoded, String>>,
}

enum Input {
    /// The capture's frame of the moment (`load_gpu`); none yet right after
    /// a display switch, and then black of the display's `size`, as the
    /// server's copy of the screen is until the first capture.
    Surface { latest: Option<PixelBuffer>, size: (u32, u32) },
    /// A BGRA buffer of the display's size, updated from memory (`load_cpu`).
    Memory(CFRetained<CVPixelBuffer>),
}

pub(crate) struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Boxed: its address is the callback's context for the session's life.
    output: Box<Mutex<Output>>,
    input: Input,
    codec: Codec,
    hardware: bool,
    bitrate: u64,
    fps: u32,
    pts: i64,
    started: Instant,
}

// SAFETY: the session is used by one client thread at a time; VideoToolbox
// sessions may be driven from any thread.
unsafe impl Send for VtEncoder {}

fn number_i32(v: i32) -> CFRetained<CFNumber> {
    CFNumber::new_i32(v)
}

fn set(session: &VTSession, key: &CFString, value: &CFType) -> Result<()> {
    // SAFETY: a valid session, key and value.
    match unsafe { VTSessionSetProperty(session, key, Some(value)) } {
        0 => Ok(()),
        status => bail!("{key}: {status}"),
    }
}

/// Settings that not every encoder or macOS release has: a refusal is fine.
fn set_optional(session: &VTSession, key: &CFString, value: &CFType) {
    let _ = set(session, key, value);
}

fn true_value() -> &'static CFBoolean {
    // SAFETY: framework constant, never null.
    unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue")
}

/// The callback of every frame: turns the encoded sample into Annex B.
unsafe extern "C-unwind" fn on_output(
    context: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the context is the encoder's boxed Output, alive while the session is.
    let output = unsafe { &*(context as *const Mutex<Output>) };
    let mut out = output.lock().unwrap();
    let result = if status != 0 {
        Err(format!("VideoToolbox encode failed ({status})"))
    } else {
        // SAFETY: null or a valid sample buffer for the callback's duration.
        match unsafe { sample.as_ref() } {
            Some(s) => annex_b(s, out.codec).map_err(|e| format!("{e:#}")),
            None => Err(format!("VideoToolbox dropped the frame (flags {:#x})", flags.0)),
        }
    };
    out.result = Some(result);
}

fn is_key(sample: &CMSampleBuffer) -> bool {
    // SAFETY: plain getter; an array of CFDictionary when present.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    if attachments.count() == 0 {
        return true;
    }
    // SAFETY: index 0 exists; its value is a CFDictionary.
    let dict = unsafe { &*(attachments.value_at_index(0) as *const CFDictionary) };
    // SAFETY: framework constants; the value is a CFBoolean or absent.
    let not_sync = unsafe { dict.value((kCMSampleAttachmentKey_NotSync as *const CFString).cast()) };
    !(not_sync == (true_value() as *const CFBoolean).cast())
}

/// One access unit in Annex B, parameter sets first on a keyframe.
fn annex_b(sample: &CMSampleBuffer, codec: Codec) -> Result<Encoded> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let key = is_key(sample);
    // SAFETY: plain getters of a valid sample buffer.
    let (data, desc) = unsafe { (sample.data_buffer(), sample.format_description()) };
    let data = data.context("encoded sample without data")?;
    // SAFETY: plain getter.
    let len = unsafe { data.data_length() };
    let mut avcc = vec![0u8; len];
    // SAFETY: `avcc` holds `len` bytes.
    let status = unsafe { data.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast()).unwrap()) };
    if status != 0 {
        bail!("CMBlockBufferCopyDataBytes failed ({status})");
    }
    let mut out = Vec::with_capacity(len + 128);
    let mut length_size = 4usize;
    if let Some(desc) = desc {
        let mut count = 1usize;
        let mut i = 0;
        while i < count {
            let (mut ptr, mut size, mut header) = (std::ptr::null::<u8>(), 0usize, 0i32);
            // SAFETY: valid out pointers; the parameter set lives as long as `desc`.
            let status = unsafe {
                match codec {
                    Codec::Hevc => {
                        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(&desc, i, &mut ptr, &mut size, &mut count, &mut header)
                    }
                    _ => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&desc, i, &mut ptr, &mut size, &mut count, &mut header),
                }
            };
            if status != 0 {
                bail!("parameter set {i}: {status}");
            }
            length_size = header.clamp(1, 4) as usize;
            if key {
                out.extend_from_slice(&START);
                // SAFETY: `size` bytes at `ptr`, see above.
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
            }
            i += 1;
        }
    }
    // Length-prefixed NAL units (AVCC / HVCC) to start codes.
    let mut pos = 0;
    while pos + length_size <= avcc.len() {
        let n = avcc[pos..pos + length_size].iter().fold(0usize, |n, b| n << 8 | *b as usize);
        pos += length_size;
        let end = (pos + n).min(avcc.len());
        out.extend_from_slice(&START);
        out.extend_from_slice(&avcc[pos..end]);
        pos = end;
    }
    if key && codec == Codec::H264 {
        mark_no_reorder(&mut out);
    }
    Ok(Encoded { data: out, key })
}

impl VtEncoder {
    /// `input`: the display's size; `output`: the encoded size, which
    /// VideoToolbox scales to.
    pub(crate) fn open(
        codec: Codec,
        input: (u32, u32),
        output: (u32, u32),
        surface: Option<&GpuSurface>,
        bitrate: u64,
        fps: u32,
    ) -> Result<Self> {
        let codec_type = match codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::Hevc => kCMVideoCodecType_HEVC,
            Codec::Av1 => bail!("VideoToolbox has no AV1 encoder"),
        };
        let boxed = Box::new(Mutex::new(Output { codec, result: None }));
        let context = &*boxed as *const Mutex<Output> as *mut c_void;
        // Low-latency rate control where the encoder takes it with these
        // settings (each frame sized for the cap, the hardware's fast path);
        // the ordinary rate control otherwise.
        let mut session = None;
        let mut errors = Vec::new();
        for low_latency in [true, false] {
            let label = if low_latency { "low-latency" } else { "standard" };
            match create_session(codec_type, output, low_latency, context) {
                Ok(s) => match configure(&s, codec) {
                    Ok(()) => {
                        session = Some(s);
                        break;
                    }
                    Err(e) => {
                        // SAFETY: a valid session, dropped right after.
                        unsafe { s.invalidate() };
                        errors.push(format!("{label}: {e:#}"));
                    }
                },
                Err(e) => errors.push(format!("{label}: {e:#}")),
            }
        }
        let session = session.ok_or_else(|| anyhow!("VideoToolbox: {}", errors.join("; ")))?;
        let vt: &VTSession = &session;
        // SAFETY: framework constant.
        let hardware = read_bool(vt, unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder });
        let input = match surface.and_then(|s| s.downcast_ref::<PixelBufferFrame>()) {
            Some(_) => Input::Surface { latest: None, size: input },
            None => Input::Memory(memory_buffer(input)?),
        };
        let mut encoder = Self { session, output: boxed, input, codec, hardware, bitrate: 0, fps: 0, pts: -1, started: Instant::now() };
        encoder.apply_bitrate(bitrate, fps)?;
        Ok(encoder)
    }

    fn apply_bitrate(&mut self, bitrate: u64, fps: u32) -> Result<()> {
        let bitrate = bitrate.clamp(MIN_BITRATE, MAX_BITRATE);
        let fps = fps.max(1);
        if (bitrate, fps) == (self.bitrate, self.fps) {
            return Ok(());
        }
        let vt: &VTSession = &self.session;
        // SAFETY: framework constants.
        unsafe {
            set(vt, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(bitrate as i64))?;
            set_optional(vt, kVTCompressionPropertyKey_ExpectedFrameRate, &number_i32(fps as i32));
        }
        (self.bitrate, self.fps) = (bitrate, fps);
        Ok(())
    }
}

fn create_session(
    codec_type: u32,
    (width, height): (u32, u32),
    low_latency: bool,
    context: *mut c_void,
) -> Result<CFRetained<VTCompressionSession>> {
    // SAFETY: framework constants.
    let (hw_key, ll_key) = unsafe {
        (kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder, kVTVideoEncoderSpecification_EnableLowLatencyRateControl)
    };
    let spec = if low_latency {
        CFDictionary::<CFString, CFBoolean>::from_slices(&[hw_key, ll_key], &[true_value(), true_value()])
    } else {
        CFDictionary::<CFString, CFBoolean>::from_slices(&[hw_key], &[true_value()])
    };
    let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
    // SAFETY: valid dictionary and out pointer; the callback context outlives
    // the session, which its owner invalidates before dropping the context.
    let status = unsafe {
        VTCompressionSession::create(
            None,
            width as i32,
            height as i32,
            codec_type,
            Some(spec.as_opaque()),
            None,
            None,
            Some(on_output),
            context,
            NonNull::from(&mut raw),
        )
    };
    match NonNull::new(raw) {
        // SAFETY: created with +1 retain count.
        Some(s) if status == 0 => Ok(unsafe { CFRetained::from_raw(s) }),
        _ => bail!("VTCompressionSessionCreate failed ({status})"),
    }
}

fn configure(vt: &VTSession, codec: Codec) -> Result<()> {
    // SAFETY: framework constants.
    unsafe {
        set(vt, kVTCompressionPropertyKey_RealTime, true_value())?;
        set(vt, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false))?;
        let profile = match codec {
            Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
            _ => kVTProfileLevel_H264_High_AutoLevel,
        };
        set(vt, kVTCompressionPropertyKey_ProfileLevel, profile)?;
        set(vt, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)?;
        set(vt, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)?;
        set(vt, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)?;
        // Keyframes only when asked for (a new client, a decoder error); an
        // encoder that refuses a limit this high places them itself.
        set_optional(vt, kVTCompressionPropertyKey_MaxKeyFrameInterval, &number_i32(100_000));
        set_optional(vt, kVTCompressionPropertyKey_MinAllowedFrameQP, &number_i32(MIN_QP));
    }
    Ok(())
}

fn read_bool(session: &VTSession, key: &CFString) -> bool {
    let mut value: *const CFBoolean = std::ptr::null();
    // SAFETY: the out value is a retained CFBoolean (or untouched on error).
    let status = unsafe { VTSessionCopyProperty(session, key, None, (&mut value as *mut *const CFBoolean).cast()) };
    if status != 0 || value.is_null() {
        return false;
    }
    // SAFETY: a +1 CFBoolean, released here.
    let value = unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFBoolean)) };
    value.as_bool()
}

/// An IOSurface-backed BGRA buffer of `size`, which VideoToolbox reads without a copy.
fn memory_buffer((w, h): (u32, u32)) -> Result<CFRetained<CVPixelBuffer>> {
    let empty = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: framework constant.
    let key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attrs = CFDictionary::<CFString, CFType>::from_slices(&[key], &[&**empty.as_opaque()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid attributes and out pointer.
    let status = unsafe {
        CVPixelBufferCreate(None, w as usize, h as usize, u32::from_be_bytes(*b"BGRA"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
    };
    match NonNull::new(raw) {
        // SAFETY: created with +1 retain count.
        Some(b) if status == 0 => Ok(unsafe { CFRetained::from_raw(b) }),
        _ => bail!("CVPixelBufferCreate failed ({status})"),
    }
}

/// A black BGRA buffer of `size`.
fn black_buffer(size: (u32, u32)) -> Result<CFRetained<CVPixelBuffer>> {
    let buffer = memory_buffer(size)?;
    // SAFETY: a valid buffer, unlocked below with the same flags.
    if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) } != 0 {
        bail!("CVPixelBufferLockBaseAddress failed");
    }
    let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
    let len = CVPixelBufferGetBytesPerRow(&buffer) * size.1 as usize;
    // SAFETY: the locked buffer holds `size.1` rows of that stride.
    unsafe { std::ptr::write_bytes(base, 0, len) };
    // SAFETY: locked above.
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
    Ok(buffer)
}

impl VideoEncoder for VtEncoder {
    fn name(&self) -> String {
        let path = match self.input {
            Input::Surface { .. } => "iosurface",
            Input::Memory(_) => "system memory",
        };
        let soft = if self.hardware { "" } else { " software" };
        format!("videotoolbox {}{soft} ({path})", self.codec.name())
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn fallback_reason(&self) -> Option<String> {
        None
    }

    fn gpu_input(&self) -> bool {
        matches!(self.input, Input::Surface { .. })
    }

    fn load_cpu(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) -> Result<()> {
        let Input::Memory(buffer) = &self.input else { bail!("this encoder reads the capture surface") };
        // SAFETY: a valid buffer, unlocked below with the same flags.
        if unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0)) } != 0 {
            bail!("CVPixelBufferLockBaseAddress failed");
        }
        let base = CVPixelBufferGetBaseAddress(buffer) as *mut u8;
        let dst_stride = CVPixelBufferGetBytesPerRow(buffer);
        for r in rects {
            let (x, w) = (r.x as usize * 4, r.w as usize * 4);
            for row in r.y as usize..(r.y + r.h) as usize {
                let s = (row - src_y0 as usize) * stride + x;
                // SAFETY: rows and columns are within the display-sized buffer, locked.
                unsafe { std::ptr::copy_nonoverlapping(src[s..s + w].as_ptr(), base.add(row * dst_stride + x), w) };
            }
        }
        // SAFETY: locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        Ok(())
    }

    fn load_gpu(&mut self, surface: &GpuSurface, _rects: &[Rect]) -> Result<()> {
        let Input::Surface { latest, .. } = &mut self.input else { bail!("this encoder reads from memory") };
        let frame = surface.downcast_ref::<PixelBufferFrame>().context("capture surface is not a pixel buffer")?;
        // The whole frame is the input: VideoToolbox reads only what it needs.
        if let Some(frame) = frame.latest() {
            *latest = Some(frame);
        }
        Ok(())
    }

    fn encode(&mut self, force_key: bool) -> Result<Encoded> {
        let buffer = match &self.input {
            Input::Surface { latest: Some(b), .. } => b.0.clone(),
            Input::Surface { latest: None, size } => black_buffer(*size)?,
            Input::Memory(b) => b.clone(),
        };
        // Strictly increasing even if two frames share a microsecond.
        self.pts = (self.started.elapsed().as_micros() as i64).max(self.pts + 1);
        // SAFETY: plain constructor.
        let pts = unsafe { CMTime::new(self.pts, 1_000_000) };
        let options = force_key.then(|| {
            // SAFETY: framework constant.
            CFDictionary::<CFString, CFBoolean>::from_slices(&[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }], &[true_value()])
        });
        // SAFETY: a valid image buffer and options; the result arrives through
        // `on_output` before complete_frames returns.
        let status = unsafe {
            self.session.encode_frame(
                &buffer,
                pts,
                kCMTimeInvalid,
                options.as_deref().map(|o| o.as_opaque()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed ({status})");
        }
        // SAFETY: plain call on a valid session.
        let status = unsafe { self.session.complete_frames(pts) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames failed ({status})");
        }
        let result = self.output.lock().unwrap().result.take().context("VideoToolbox returned no frame")?;
        result.map_err(|e| anyhow!(e))
    }

    fn set_max_bitrate(&mut self, bits_per_sec: u64, fps: u32) -> bool {
        if let Err(e) = self.apply_bitrate(bits_per_sec, fps) {
            eprintln!("videotoolbox bitrate: {e:#}");
        }
        true
    }

    fn two_pass(&self) -> Option<bool> {
        None
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // Waits for frames in flight, so the callback context is not used afterwards.
        // SAFETY: a valid session, not used after this.
        unsafe { self.session.invalidate() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VideoToolbox scales the input to the encoded size by itself (motion
    /// mode's smaller video relies on it) and starts with a keyframe that
    /// carries its parameter sets.
    #[test]
    fn encodes_scaled_input() {
        for codec in [Codec::H264, Codec::Hevc] {
            let mut e = VtEncoder::open(codec, (512, 256), (256, 128), None, 2_000_000, 60).unwrap();
            let src = vec![0x80u8; 512 * 256 * 4];
            e.load_cpu(&src, 512 * 4, 0, &[Rect::new(0, 0, 512, 256)]).unwrap();
            let first = e.encode(false).unwrap();
            assert!(first.key, "{codec:?}: first frame is a keyframe");
            let nal_type = match codec {
                Codec::Hevc => (first.data[4] >> 1) & 0x3f,
                _ => first.data[4] & 0x1f,
            };
            // H.264 SPS = 7, HEVC VPS = 32.
            assert_eq!(nal_type, if codec == Codec::Hevc { 32 } else { 7 }, "{codec:?}: parameter sets first");
            let next = e.encode(false).unwrap();
            assert!(!next.key && !next.data.is_empty(), "{codec:?}: a P-frame follows");
            assert!(e.encode(true).unwrap().key, "{codec:?}: a keyframe when asked");
        }
    }
}
