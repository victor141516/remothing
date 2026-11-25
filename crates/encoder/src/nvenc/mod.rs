//! NVENC driven through NVIDIA's Video Codec SDK directly, with the inputs of the FFmpeg path
//! (D3D11 texture on Windows, CUDA surface on Linux): the same low-latency setup and rate
//! control as FFmpeg's NVENC (`ffmpeg.rs`), plus what FFmpeg cannot do without restarting the
//! encoder: a new bitrate cap without a keyframe (`nvEncReconfigureEncoder`), a loss predicted
//! past instead of answered with a keyframe (reference invalidation and long-term anchors,
//! `refs.rs`), and areas the client does not show coded at the coarsest quantizer (QP delta
//! map).

mod api;
mod refs;
mod syntax;
mod sys;

use std::ffi::c_void;
use std::ptr;

use anyhow::{anyhow, bail, Result};
use remothing_core::encode::{Codec, Encoded, VideoEncoder};
use remothing_core::geom::Rect;
use remothing_core::platform::GpuSurface;

use self::api::{check, nv, Api};
use self::sys::*;
use crate::ffmpeg::{self, mark_no_reorder, MAX_BITRATE, RC_FPS};
use crate::Backend;

/// Where the encoder reads its input from.
enum Input {
    /// The capture device's texture, copied GPU-side (scaled and converted to NV12 by the
    /// video processor where needed).
    #[cfg(windows)]
    D3D11(crate::d3d11::D3D11Input),
    /// A CUDA copy of the display updated from system memory; when the encoded size is
    /// smaller, a system-memory copy scaled and uploaded whole each frame.
    #[cfg(target_os = "linux")]
    Cuda(crate::cuda::CudaSurface, Option<ffmpeg::HostInput>),
}

/// What keeps the input's CUDA context current around driver calls (FFmpeg makes it current
/// around each of its NVENC calls too); nothing for D3D11.
#[cfg(target_os = "linux")]
type Current = crate::cuda::Current;
#[cfg(windows)]
struct Current;

impl Input {
    /// The device the session runs on.
    fn device(&self) -> (NV_ENC_DEVICE_TYPE, *mut c_void) {
        match self {
            #[cfg(windows)]
            Input::D3D11(i) => {
                use windows::core::Interface;
                (NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX, i.device().as_raw())
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(s, _) => (NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA, s.context()),
        }
    }

    /// The picture to register: type, resource, pitch, subresource.
    fn resource(&self) -> (NV_ENC_INPUT_RESOURCE_TYPE, *mut c_void, u32, u32) {
        match self {
            #[cfg(windows)]
            Input::D3D11(i) => {
                use windows::core::Interface;
                let (texture, subresource) = i.texture();
                (NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX, texture.as_raw(), 0, subresource)
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(s, _) => {
                let (address, pitch) = s.device_ptr();
                (NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR, address as *mut c_void, pitch, 0)
            }
        }
    }

    #[cfg(windows)]
    fn current(&self) -> Result<Current> {
        Ok(Current)
    }

    #[cfg(target_os = "linux")]
    fn current(&self) -> Result<Current> {
        let Input::Cuda(s, _) = self;
        s.current()
    }

    /// Brings the picture up to date before it is coded (scaling, conversion, upload).
    fn prepare(&mut self) -> Result<()> {
        match self {
            #[cfg(windows)]
            Input::D3D11(i) => i.scale(),
            #[cfg(target_os = "linux")]
            Input::Cuda(s, Some(h)) => {
                let f = h.frame()?;
                let whole = Rect::new(0, 0, f.width(), f.height());
                s.upload(f.data(0), f.stride(0), 0, whole)
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(_, None) => Ok(()),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            #[cfg(windows)]
            Input::D3D11(_) => "d3d11",
            #[cfg(target_os = "linux")]
            Input::Cuda(..) => "cuda",
        }
    }
}

/// Short-term references kept besides the anchors, where the level's buffer has room: how far
/// back a loss is predicted past without an anchor.
const SHORT_REFS: u32 = 4;

/// Long-term references (anchors), where the device has them.
const ANCHORS: u32 = 2;

pub(crate) struct NvencEncoder {
    api: &'static Api,
    session: *mut c_void,
    bitstream: NV_ENC_OUTPUT_PTR,
    registered: NV_ENC_REGISTERED_PTR,
    format: NV_ENC_BUFFER_FORMAT,
    /// Boxed: `init.encodeConfig` points at it.
    config: Box<NV_ENC_CONFIG>,
    init: NV_ENC_INITIALIZE_PARAMS,
    input: Input,
    codec: Codec,
    /// Display (input) and encoded size.
    display: (u32, u32),
    size: (u32, u32),
    /// Rate-control bitrate now.
    bitrate: u64,
    /// The driver takes new rates without a reset.
    free_rates: bool,
    two_pass: bool,
    bt601: bool,
    preset: usize,
    refs: refs::References,
    unseen: UnseenMap,
    pub(crate) fallback: Option<String>,
}

// SAFETY: the session, its buffers and the input are used by the one thread that owns the
// encoder at a time; NVENC sessions may be driven from any thread.
unsafe impl Send for NvencEncoder {}

/// Per-block QP offsets that code what the client does not show at the coarsest quantizer.
struct UnseenMap {
    /// Block edge: H.264 macroblock, HEVC coding tree block, AV1 superblock.
    block: u32,
    /// Offset that takes any quantizer the rate control picks to the codec's coarsest.
    coarsest: i8,
    map: Vec<i8>,
    /// Once a picture names a map, every later one has to (zeros when nothing is unseen):
    /// the driver otherwise codes it with the last map it was given (selkies observed this).
    used: bool,
}

impl UnseenMap {
    fn new(codec: Codec, (w, h): (u32, u32)) -> Self {
        let (block, coarsest) = match codec {
            Codec::H264 => (16, 51),
            Codec::Hevc => (32, 51),
            // AV1's quantizer index runs to 255; the offset to the largest an i8 holds.
            Codec::Av1 => (64, i8::MAX),
        };
        let map = vec![0; (w.div_ceil(block) * h.div_ceil(block)) as usize];
        Self { block, coarsest, map, used: false }
    }

    /// Marks the blocks `rects` (encoded coordinates, inner-rounded) cover entirely.
    fn set(&mut self, rects: &[(u32, u32, u32, u32)], (w, h): (u32, u32)) {
        let b = self.block;
        let cols = w.div_ceil(b);
        self.map.fill(0);
        for &(x0, y0, x1, y1) in rects {
            // Blocks whose part inside the picture lies within the rect.
            let bx0 = x0.div_ceil(b);
            let by0 = y0.div_ceil(b);
            let bx1 = if x1 >= w { cols } else { x1 / b };
            let by1 = if y1 >= h { h.div_ceil(b) } else { y1 / b };
            for by in by0..by1 {
                self.map[(by * cols + bx0) as usize..(by * cols + bx1.max(bx0)) as usize].fill(self.coarsest);
            }
        }
        self.used |= self.map.iter().any(|&q| q != 0);
    }
}

/// The FFmpeg NVENC option overrides this backend understands: `preset` (p1-p3).
fn preset(overrides: &[(String, String)]) -> Result<usize> {
    let mut preset = 0;
    for (k, v) in overrides {
        match (k.as_str(), v.as_str()) {
            ("preset", "p1") => preset = 0,
            ("preset", "p2") => preset = 1,
            ("preset", "p3") => preset = 2,
            _ => bail!("native NVENC has no option {k}={v}"),
        }
    }
    Ok(preset)
}

impl NvencEncoder {
    /// A session reading the capture surface's copy (Windows: `surface`, scaled to `output`,
    /// NV12 in BT.709 when `bt709`) or a CUDA copy loaded from system memory (Linux; BT.601
    /// only), at rate-control bitrate `bitrate`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        codec: Codec,
        surface: Option<&GpuSurface>,
        input: (u32, u32),
        output: (u32, u32),
        bitrate: u64,
        two_pass: bool,
        bt709: bool,
        overrides: &[(String, String)],
    ) -> Result<Self> {
        let preset = preset(overrides)?;
        let api = api::api()?;
        #[cfg(windows)]
        let source = {
            let surface = surface.ok_or_else(|| anyhow!("no capture surface"))?;
            Input::D3D11(crate::d3d11::D3D11Input::new(surface, input, output, bt709)?)
        };
        #[cfg(target_os = "linux")]
        let source = {
            if bt709 {
                bail!("BT.709 needs NVENC's system-memory input here");
            }
            let _ = surface;
            let bgrz = ffmpeg_next::format::Pixel::BGRZ;
            let scaled = (input != output).then(|| ffmpeg::HostInput::new(input, output, bgrz)).transpose()?;
            Input::Cuda(crate::cuda::CudaSurface::new(output.0, output.1)?, scaled)
        };
        let (device_type, device) = source.device();
        let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: api::OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: device_type,
            device,
            apiVersion: NVENCAPI_VERSION,
            ..Default::default()
        };
        let _current = source.current()?;
        let mut session = ptr::null_mut();
        // SAFETY: valid parameters; the device outlives the session (`input` holds it).
        let status = unsafe { nv!(api, nvEncOpenEncodeSessionEx(&mut params, &mut session)) };
        if status != NVENCSTATUS::NV_ENC_SUCCESS {
            if !session.is_null() {
                // SAFETY: the half-opened session is destroyed once.
                unsafe { nv!(api, nvEncDestroyEncoder(session)) };
            }
            bail!("nvEncOpenEncodeSessionEx failed (NVENCSTATUS {})", status.0);
        }
        let bt601 = !bt709;
        let mut e = Self {
            api,
            session,
            bitstream: ptr::null_mut(),
            registered: ptr::null_mut(),
            format: if bt709 { NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 } else { NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB },
            config: Box::default(),
            init: NV_ENC_INITIALIZE_PARAMS::default(),
            input: source,
            codec,
            display: input,
            size: output,
            bitrate: 0,
            free_rates: false,
            two_pass: two_pass && ffmpeg::offers_two_pass(Backend::Nvenc, codec),
            bt601,
            preset,
            refs: refs::References::new(None),
            unseen: UnseenMap::new(codec, output),
            fallback: None,
        };
        e.initialize()?;
        e.set_rate(bitrate)?;
        e.buffers()?;
        Ok(e)
    }

    fn guid(&self) -> GUID {
        match self.codec {
            Codec::H264 => api::CODEC_H264,
            Codec::Hevc => api::CODEC_HEVC,
            Codec::Av1 => api::CODEC_AV1,
        }
    }

    /// A capability of the codec on this device (0 when the query fails).
    fn cap(&self, cap: NV_ENC_CAPS) -> i32 {
        let mut param = NV_ENC_CAPS_PARAM { version: api::CAPS_PARAM_VER, capsToQuery: cap, ..Default::default() };
        let mut value = 0;
        // SAFETY: valid session and parameters.
        let status = unsafe { nv!(self.api, nvEncGetEncodeCaps(self.session, self.guid(), &mut param, &mut value)) };
        if status == NVENCSTATUS::NV_ENC_SUCCESS {
            value
        } else {
            0
        }
    }

    /// Configures and initializes the session: FFmpeg's settings (`ffmpeg.rs`, NVENC arm),
    /// written out, plus the references and anchors this backend needs.
    ///
    /// The level is the driver's choice (as FFmpeg leaves it) for the highest cap,
    /// `MAX_BITRATE`: the session opens at that cap and gets its own right after (`set_rate`).
    /// A level chosen for a lower cap would have to rise with a later one, and a new level is a
    /// new sequence header, which only a keyframe carries.
    fn initialize(&mut self) -> Result<()> {
        let (w, h) = self.size;
        let invalidation = self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) == 1;
        let ltr = self.cap(NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_LTR_FRAMES).max(0) as u32;
        self.free_rates = self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE) == 1;
        let room = dpb_frames(self.codec, w, h);
        // Anchors only beside two short-term references at least.
        let anchors = if invalidation { ANCHORS.min(ltr).min(room.saturating_sub(2)) } else { 0 };
        let dpb = room.min(anchors + SHORT_REFS);
        self.refs = refs::References::new(invalidation.then_some(refs::Limits { short: (dpb - anchors) as u64, anchors: anchors as usize }));
        self.bitrate = MAX_BITRATE;

        let mut preset = NV_ENC_PRESET_CONFIG {
            version: api::PRESET_CONFIG_VER,
            presetCfg: NV_ENC_CONFIG { version: api::CONFIG_VER, ..Default::default() },
            ..Default::default()
        };
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        let preset_guid = api::PRESETS[self.preset];
        // SAFETY: valid session and parameters.
        let status = unsafe { nv!(self.api, nvEncGetEncodePresetConfigEx(self.session, self.guid(), preset_guid, tuning, &mut preset)) };
        check(self.api, self.session, status, "nvEncGetEncodePresetConfigEx")?;
        let c = &mut *self.config;
        *c = preset.presetCfg;
        c.version = api::CONFIG_VER;
        c.profileGUID = match self.codec {
            Codec::H264 => api::PROFILE_H264_HIGH,
            Codec::Hevc => api::PROFILE_HEVC_MAIN,
            Codec::Av1 => api::PROFILE_AV1_MAIN,
        };
        // Keyframes only on demand, no B-frames.
        c.gopLength = api::INFINITE_GOP;
        c.frameIntervalP = 1;
        // VBR capped at the link's share with a quality floor (`ffmpeg::min_qp`), as FFmpeg's
        // `rc=vbr`, `qmin`, `b`, `maxrate`, `bufsize` set it.
        let rc = &mut c.rcParams;
        rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
        rc.averageBitRate = self.bitrate as u32;
        rc.maxBitRate = self.bitrate as u32;
        rc.vbvBufferSize = ffmpeg::vbv_size(self.bitrate) as u32;
        let qmin = ffmpeg::min_qp(self.codec) as u32;
        rc.set_enableMinQP(1);
        rc.minQP = NV_ENC_QP { qpInterP: qmin, qpInterB: qmin, qpIntra: qmin };
        rc.set_enableInitialRCQP(1);
        rc.initialRCQP = NV_ENC_QP { qpInterP: qmin, qpInterB: qmin, qpIntra: qmin };
        rc.set_zeroReorderDelay(1);
        rc.set_enableLookahead(0);
        rc.multiPass = if self.two_pass {
            NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
        } else {
            NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED
        };
        rc.qpMapMode = NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA;

        // NVENC converts RGB input with BT.601 (measured); NV12 input comes in BT.709.
        let matrix = if self.bt601 {
            NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT470BG
        } else {
            NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709
        };
        let vui = |v: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS| {
            v.videoSignalTypePresentFlag = 1;
            v.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            v.videoFullRangeFlag = 0;
            v.colourDescriptionPresentFlag = 1;
            v.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_UNSPECIFIED;
            v.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_UNSPECIFIED;
            v.colourMatrix = matrix;
        };
        let eight = NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8;
        let one = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
        // SAFETY: the union arm of the session's codec.
        unsafe {
            match self.codec {
                Codec::H264 => {
                    let h = &mut c.encodeCodecConfig.h264Config;
                    h.level = NV_ENC_LEVEL::NV_ENC_LEVEL_AUTOSELECT.0;
                    h.idrPeriod = api::INFINITE_GOP;
                    h.set_repeatSPSPPS(1);
                    h.set_outputAUD(0);
                    h.sliceMode = 3;
                    h.sliceModeData = 1;
                    h.chromaFormatIDC = 1;
                    h.maxNumRefFrames = dpb;
                    h.numRefL0 = one;
                    if anchors > 0 {
                        h.set_enableLTR(1);
                        h.ltrTrustMode = 0;
                        h.ltrNumFrames = anchors;
                    }
                    vui(&mut h.h264VUIParameters);
                    h.h264VUIParameters.bitstreamRestrictionFlag = 1;
                    h.inputBitDepth = eight;
                    h.outputBitDepth = eight;
                }
                Codec::Hevc => {
                    let h = &mut c.encodeCodecConfig.hevcConfig;
                    h.level = NV_ENC_LEVEL::NV_ENC_LEVEL_AUTOSELECT.0;
                    // The QP delta map's unit (`UnseenMap`).
                    h.maxCUSize = NV_ENC_HEVC_CUSIZE::NV_ENC_HEVC_CUSIZE_32x32;
                    h.idrPeriod = api::INFINITE_GOP;
                    h.set_repeatSPSPPS(1);
                    h.set_outputAUD(0);
                    h.sliceMode = 3;
                    h.sliceModeData = 1;
                    h.set_chromaFormatIDC(1);
                    h.maxNumRefFramesInDPB = dpb;
                    h.numRefL0 = one;
                    if anchors > 0 {
                        h.set_enableLTR(1);
                        h.ltrTrustMode = 0;
                        h.ltrNumFrames = anchors;
                    }
                    vui(&mut h.hevcVUIParameters);
                    h.inputBitDepth = eight;
                    h.outputBitDepth = eight;
                }
                Codec::Av1 => {
                    let a = &mut c.encodeCodecConfig.av1Config;
                    a.level = NV_ENC_LEVEL::NV_ENC_LEVEL_AV1_AUTOSELECT.0;
                    a.idrPeriod = api::INFINITE_GOP;
                    a.set_repeatSeqHdr(1);
                    a.set_outputAnnexBFormat(0);
                    a.set_chromaFormatIDC(1);
                    a.maxNumRefFramesInDPB = dpb;
                    a.numFwdRefs = one;
                    a.numBwdRefs = one;
                    if anchors > 0 {
                        a.set_enableLTR(1);
                        a.ltrNumFrames = anchors;
                    }
                    a.matrixCoefficients = matrix;
                    a.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_UNSPECIFIED;
                    a.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_UNSPECIFIED;
                    a.colorRange = 0;
                    a.inputBitDepth = eight;
                    a.outputBitDepth = eight;
                }
            }
        }

        let ratio = gcd(w, h);
        self.init = NV_ENC_INITIALIZE_PARAMS {
            version: api::INITIALIZE_PARAMS_VER,
            encodeGUID: self.guid(),
            presetGUID: preset_guid,
            tuningInfo: tuning,
            encodeWidth: w,
            encodeHeight: h,
            darWidth: w / ratio,
            darHeight: h / ratio,
            frameRateNum: RC_FPS as u32,
            frameRateDen: 1,
            enablePTD: 1,
            enableEncodeAsync: 0,
            encodeConfig: &mut *self.config,
            maxEncodeWidth: w,
            maxEncodeHeight: h,
            ..Default::default()
        };
        // Split-frame encoding as FFmpeg's `split_encode_mode=forced` (see ffmpeg.rs).
        if self.codec != Codec::H264 {
            self.init.set_splitEncodeMode(NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE.0);
        }
        // SAFETY: valid session; `init.encodeConfig` points at the boxed config.
        let status = unsafe { nv!(self.api, nvEncInitializeEncoder(self.session, &mut self.init)) };
        check(self.api, self.session, status, "nvEncInitializeEncoder")
    }

    /// The output buffer and the registered input texture.
    fn buffers(&mut self) -> Result<()> {
        let mut out = NV_ENC_CREATE_BITSTREAM_BUFFER { version: api::CREATE_BITSTREAM_BUFFER_VER, ..Default::default() };
        // SAFETY: valid session and parameters.
        let status = unsafe { nv!(self.api, nvEncCreateBitstreamBuffer(self.session, &mut out)) };
        check(self.api, self.session, status, "nvEncCreateBitstreamBuffer")?;
        self.bitstream = out.bitstreamBuffer;
        let (resource_type, resource, pitch, subresource) = self.input.resource();
        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: api::REGISTER_RESOURCE_VER,
            resourceType: resource_type,
            width: self.size.0,
            height: self.size.1,
            pitch,
            subResourceIndex: subresource,
            resourceToRegister: resource,
            bufferFormat: self.format,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        // SAFETY: the picture lives as long as `self.input`, which outlives the registration.
        let status = unsafe { nv!(self.api, nvEncRegisterResource(self.session, &mut reg)) };
        check(self.api, self.session, status, "nvEncRegisterResource")?;
        self.registered = reg.registeredResource;
        Ok(())
    }

    /// Gives the rate control a new bitrate (rate-control units, see
    /// `ffmpeg::rate_control_bitrate`) without a reset or a keyframe; on refusal the
    /// session keeps its previous one.
    fn set_rate(&mut self, bitrate: u64) -> Result<()> {
        if bitrate == self.bitrate {
            return Ok(());
        }
        let previous = self.config.rcParams;
        let rc = &mut self.config.rcParams;
        rc.averageBitRate = bitrate as u32;
        rc.maxBitRate = bitrate as u32;
        rc.vbvBufferSize = ffmpeg::vbv_size(bitrate) as u32;
        self.init.encodeConfig = &mut *self.config;
        let mut params = NV_ENC_RECONFIGURE_PARAMS { version: api::RECONFIGURE_PARAMS_VER, reInitEncodeParams: self.init, ..Default::default() };
        // SAFETY: valid session; the config pointer is live for the call.
        let status = unsafe { nv!(self.api, nvEncReconfigureEncoder(self.session, &mut params)) };
        if let Err(e) = check(self.api, self.session, status, "nvEncReconfigureEncoder") {
            self.config.rcParams = previous;
            return Err(e);
        }
        self.bitrate = bitrate;
        Ok(())
    }

    /// Codes the mapped input; the encoded frame and whether it is an IDR.
    fn code(&mut self, mapped: NV_ENC_INPUT_PTR, mapped_format: NV_ENC_BUFFER_FORMAT, force_key: bool, plan: refs::Plan) -> Result<(Vec<u8>, bool)> {
        let mut pic = NV_ENC_PIC_PARAMS {
            version: api::PIC_PARAMS_VER,
            inputWidth: self.size.0,
            inputHeight: self.size.1,
            inputBuffer: mapped,
            outputBitstream: self.bitstream,
            bufferFmt: mapped_format,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            // What `nvEncInvalidateRefFrames` names a frame by.
            inputTimeStamp: self.refs.next(),
            encodePicFlags: if force_key { NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0 } else { 0 },
            ..Default::default()
        };
        if self.unseen.used {
            pic.qpDeltaMap = self.unseen.map.as_mut_ptr();
            pic.qpDeltaMapSize = self.unseen.map.len() as u32;
        }
        macro_rules! long_term {
            ($p:expr) => {{
                let p = $p;
                if let Some(slot) = plan.mark {
                    p.set_ltrMarkFrame(1);
                    p.ltrMarkFrameIdx = slot as u32;
                }
                if let Some(slot) = plan.use_slot {
                    p.set_ltrUseFrames(1);
                    p.ltrUseFrameBitmap = 1 << slot;
                }
            }};
        }
        // SAFETY: the union arm of the session's codec.
        unsafe {
            match self.codec {
                Codec::H264 => long_term!(&mut pic.codecPicParams.h264PicParams),
                Codec::Hevc => long_term!(&mut pic.codecPicParams.hevcPicParams),
                Codec::Av1 => long_term!(&mut pic.codecPicParams.av1PicParams),
            }
        }
        // SAFETY: valid session, mapped input and output buffer; the map lives for the call.
        let status = unsafe { nv!(self.api, nvEncEncodePicture(self.session, &mut pic)) };
        check(self.api, self.session, status, "nvEncEncodePicture")?;
        let mut lock = NV_ENC_LOCK_BITSTREAM { version: api::LOCK_BITSTREAM_VER, outputBitstream: self.bitstream, ..Default::default() };
        // SAFETY: synchronous session: the lock waits for this picture.
        let status = unsafe { nv!(self.api, nvEncLockBitstream(self.session, &mut lock)) };
        check(self.api, self.session, status, "nvEncLockBitstream")?;
        // SAFETY: the driver's buffer holds that many bytes until the unlock below.
        let data = unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize) }.to_vec();
        // SAFETY: locked just above.
        let status = unsafe { nv!(self.api, nvEncUnlockBitstream(self.session, self.bitstream)) };
        check(self.api, self.session, status, "nvEncUnlockBitstream")?;
        Ok((data, lock.pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR))
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

/// Reference frames a decoder's picture buffer holds for this picture size: H.264 at level 5.1
/// or 5.2 (Table A-1 MaxDpbMbs; NVENC declares 5.0 for 1080p, 5.2 for 4K, both with room for
/// more than this backend keeps), HEVC at the 5.x levels (A.4.2, less one for the picture
/// being decoded), AV1's eight slots. A larger picture than the levels hold gets fewer
/// references than its level would allow, never more.
fn dpb_frames(codec: Codec, w: u32, h: u32) -> u32 {
    match codec {
        Codec::H264 => (184_320 / (w.div_ceil(16) * h.div_ceil(16)).max(1)).min(16),
        Codec::Hevc => {
            const MAX_LUMA_PS: u32 = 8_912_896;
            let size = match w * h {
                p if p <= MAX_LUMA_PS >> 2 => 16,
                p if p <= MAX_LUMA_PS >> 1 => 12,
                p if p <= 3 * (MAX_LUMA_PS >> 2) => 8,
                _ => 6,
            };
            size - 1
        }
        Codec::Av1 => 8,
    }
}

impl VideoEncoder for NvencEncoder {
    fn name(&self) -> String {
        let pass = if self.two_pass { " two-pass" } else { "" };
        let bt709 = if self.bt601 { "" } else { ", BT.709" };
        format!("nvenc {}{pass} ({}, sdk{bt709})", self.codec.name(), self.input.label())
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn fallback_reason(&self) -> Option<String> {
        self.fallback.clone()
    }

    fn gpu_input(&self) -> bool {
        cfg!(windows)
    }

    fn load_cpu(&mut self, src: &[u8], stride: usize, src_y0: u32, rects: &[Rect]) -> Result<()> {
        match &mut self.input {
            #[cfg(windows)]
            Input::D3D11(_) => {
                let _ = (src, stride, src_y0, rects);
                Err(anyhow!("D3D11 encoder input is fed from the capture surface"))
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(_, Some(h)) => {
                h.load(src, stride, src_y0, rects);
                Ok(())
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(s, None) => rects.iter().try_for_each(|r| s.upload(src, stride, src_y0, *r)),
        }
    }

    fn load_gpu(&mut self, surface: &GpuSurface, rects: &[Rect]) -> Result<()> {
        match &mut self.input {
            #[cfg(windows)]
            Input::D3D11(i) => {
                use anyhow::Context;
                let frame = surface.downcast_ref::<gpu_interop::d3d11::D3D11Frame>().context("capture surface is not D3D11")?;
                i.copy(frame, rects)
            }
            #[cfg(target_os = "linux")]
            Input::Cuda(..) => {
                let _ = (surface, rects);
                Err(anyhow!("encoder input is not GPU-based"))
            }
        }
    }

    fn encode(&mut self, force_key: bool) -> Result<Encoded> {
        self.input.prepare()?;
        let _current = self.input.current()?;
        let plan = self.refs.plan(force_key);
        let mut map = NV_ENC_MAP_INPUT_RESOURCE { version: api::MAP_INPUT_RESOURCE_VER, registeredResource: self.registered, ..Default::default() };
        // SAFETY: valid session and registered resource.
        let status = unsafe { nv!(self.api, nvEncMapInputResource(self.session, &mut map)) };
        check(self.api, self.session, status, "nvEncMapInputResource")?;
        let coded = self.code(map.mappedResource, map.mappedBufferFmt, force_key, plan);
        // SAFETY: mapped just above; unmapped once the picture is coded (or failed).
        let status = unsafe { nv!(self.api, nvEncUnmapInputResource(self.session, map.mappedResource)) };
        let (mut data, key) = coded?;
        check(self.api, self.session, status, "nvEncUnmapInputResource")?;
        let index = self.refs.next();
        self.refs.coded(plan, key);
        if key {
            self.refs.set_syntax(syntax::read(self.codec, &data));
            if self.codec == Codec::H264 {
                mark_no_reorder(&mut data);
            }
        }
        Ok(Encoded { data, key, index, recovers: if key { None } else { plan.recovers } })
    }

    fn set_max_bitrate(&mut self, bits_per_sec: u64, fps: u32) -> bool {
        let bitrate = ffmpeg::rate_control_bitrate(Backend::Nvenc, bits_per_sec, fps);
        let changed = self.input.current().and_then(|_current| self.set_rate(bitrate));
        if let Err(e) = changed {
            // The session goes on at its previous rate.
            eprintln!("nvenc {}: new bitrate {bitrate} refused: {e:#}", self.codec.name());
        }
        true
    }

    fn two_pass(&self) -> Option<bool> {
        ffmpeg::offers_two_pass(Backend::Nvenc, self.codec).then_some(self.two_pass)
    }

    fn bt601(&self) -> bool {
        self.bt601
    }

    fn invalidate_from(&mut self, first: u64) -> bool {
        let Some(forget) = self.refs.invalidate(first) else { return false };
        let forgot = self.input.current().and_then(|_current| {
            forget.into_iter().try_for_each(|index| {
                // SAFETY: valid session.
                let status = unsafe { nv!(self.api, nvEncInvalidateRefFrames(self.session, index)) };
                check(self.api, self.session, status, "nvEncInvalidateRefFrames")
            })
        });
        if let Err(e) = forgot {
            eprintln!("nvenc {}: {e:#}", self.codec.name());
            self.refs.abandon();
            return false;
        }
        true
    }

    fn acknowledge(&mut self, index: u64) {
        self.refs.acknowledge(index);
    }

    fn free_rate_changes(&self) -> bool {
        self.free_rates
    }

    fn set_unseen(&mut self, rects: &[Rect]) {
        let ((dw, dh), (w, h)) = (self.display, self.size);
        // Display to encoded coordinates, rounded inwards: a block is skipped only where it
        // lies wholly inside an unseen area.
        let inner: Vec<_> = rects
            .iter()
            .map(|r| {
                let x = |v: u32, up: bool| {
                    let s = v as u64 * w as u64;
                    (if up { s.div_ceil(dw as u64) } else { s / dw as u64 }) as u32
                };
                let y = |v: u32, up: bool| {
                    let s = v as u64 * h as u64;
                    (if up { s.div_ceil(dh as u64) } else { s / dh as u64 }) as u32
                };
                (x(r.x, true), y(r.y, true), x(r.x + r.w, false).min(w), y(r.y + r.h, false).min(h))
            })
            .filter(|&(x0, y0, x1, y1)| x0 < x1 && y0 < y1)
            .collect();
        self.unseen.set(&inner, (w, h));
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let _current = self.input.current();
        // SAFETY: each object was created on this session and is released once, the session last.
        unsafe {
            if !self.registered.is_null() {
                nv!(self.api, nvEncUnregisterResource(self.session, self.registered));
            }
            if !self.bitstream.is_null() {
                nv!(self.api, nvEncDestroyBitstreamBuffer(self.session, self.bitstream));
            }
            nv!(self.api, nvEncDestroyEncoder(self.session));
        }
    }
}
