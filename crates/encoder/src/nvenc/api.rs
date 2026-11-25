//! The NVENC entry points, loaded from the driver at runtime as FFmpeg does
//! (`nvEncodeAPI64.dll`, `libnvidia-encode.so.1`): nothing NVIDIA's is linked, so the binary
//! runs on machines without the driver and the backend just reports why it is missing.

use std::ffi::{c_void, CStr};
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Result};

use super::sys::*;

/// The version word of an API struct (`NVENCAPI_STRUCT_VERSION` in the header).
const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | ver << 16 | 0x7 << 28
}

pub(super) const CAPS_PARAM_VER: u32 = struct_version(1);
pub(super) const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub(super) const CONFIG_VER: u32 = struct_version(9) | 1 << 31;
pub(super) const INITIALIZE_PARAMS_VER: u32 = struct_version(7) | 1 << 31;
pub(super) const RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | 1 << 31;
pub(super) const PRESET_CONFIG_VER: u32 = struct_version(5) | 1 << 31;
pub(super) const PIC_PARAMS_VER: u32 = struct_version(7) | 1 << 31;
pub(super) const LOCK_BITSTREAM_VER: u32 = struct_version(2) | 1 << 31;
pub(super) const MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub(super) const REGISTER_RESOURCE_VER: u32 = struct_version(5);
pub(super) const OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
const FUNCTION_LIST_VER: u32 = struct_version(2);

/// `gopLength` / `idrPeriod` without periodic keyframes.
pub(super) const INFINITE_GOP: u32 = 0xffff_ffff;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}

pub(super) const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
pub(super) const CODEC_HEVC: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub(super) const CODEC_AV1: GUID = guid(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);
pub(super) const PROFILE_H264_HIGH: GUID = guid(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
pub(super) const PROFILE_HEVC_MAIN: GUID = guid(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
pub(super) const PROFILE_AV1_MAIN: GUID = guid(0x5f2a39f5, 0xf14e, 0x4f95, [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97]);
/// Presets P1 (fastest) to P3, the ones a 165 Hz stream can afford.
pub(super) const PRESETS: [GUID; 3] = [
    guid(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]),
    guid(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]),
    guid(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]),
];

/// The oldest API whose structs these bindings lay out (`NVENCAPI_VERSION` of the header).
const NEEDS: (u32, u32) = (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);

/// The driver's function table.
pub(super) struct Api {
    pub(super) f: NV_ENCODE_API_FUNCTION_LIST,
}

// SAFETY: a table of function pointers into the driver, written once and only read after.
unsafe impl Send for Api {}
// SAFETY: see Send.
unsafe impl Sync for Api {}

/// Calls an NVENC entry point; `api()` made sure each one called is in the table.
macro_rules! nv {
    ($api:expr, $f:ident($($arg:expr),* $(,)?)) => {
        ($api.f.$f.expect(concat!(stringify!($f), " checked at load")))($($arg),*)
    };
}
pub(super) use nv;

/// The driver's table, loaded once per process.
pub(super) fn api() -> Result<&'static Api> {
    static API: LazyLock<Result<Api, String>> = LazyLock::new(|| load().map_err(|e| format!("{e:#}")));
    API.as_ref().map_err(|e| anyhow!("{e}"))
}

type CreateInstance = unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

fn load() -> Result<Api> {
    // SAFETY: the symbols have these signatures (nvEncodeAPI.h); the library stays loaded for
    // the life of the process, so the table never dangles.
    unsafe {
        let (create, max) = symbols()?;
        let mut version = 0;
        if max(&mut version) != NVENCSTATUS::NV_ENC_SUCCESS {
            bail!("NvEncodeAPIGetMaxSupportedVersion failed");
        }
        let driver = (version >> 4, version & 0xf);
        if driver < NEEDS {
            bail!("the driver offers NVENC API {}.{}, this build needs {}.{} (a newer driver)", driver.0, driver.1, NEEDS.0, NEEDS.1);
        }
        let mut f = NV_ENCODE_API_FUNCTION_LIST { version: FUNCTION_LIST_VER, ..Default::default() };
        if create(&mut f) != NVENCSTATUS::NV_ENC_SUCCESS {
            bail!("NvEncodeAPICreateInstance failed");
        }
        let used = [
            f.nvEncOpenEncodeSessionEx.is_some(),
            f.nvEncGetEncodeCaps.is_some(),
            f.nvEncGetEncodePresetConfigEx.is_some(),
            f.nvEncInitializeEncoder.is_some(),
            f.nvEncReconfigureEncoder.is_some(),
            f.nvEncCreateBitstreamBuffer.is_some(),
            f.nvEncDestroyBitstreamBuffer.is_some(),
            f.nvEncRegisterResource.is_some(),
            f.nvEncUnregisterResource.is_some(),
            f.nvEncMapInputResource.is_some(),
            f.nvEncUnmapInputResource.is_some(),
            f.nvEncEncodePicture.is_some(),
            f.nvEncLockBitstream.is_some(),
            f.nvEncUnlockBitstream.is_some(),
            f.nvEncInvalidateRefFrames.is_some(),
            f.nvEncGetLastErrorString.is_some(),
            f.nvEncDestroyEncoder.is_some(),
        ];
        if used.contains(&false) {
            bail!("the driver's NVENC table lacks entry points this build uses");
        }
        Ok(Api { f })
    }
}

/// The two exported entry points.
#[cfg(windows)]
unsafe fn symbols() -> Result<(CreateInstance, MaxVersion)> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    // System32 only: the driver installs it there, and no other directory may stand in for it.
    let lib = LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
        .map_err(|e| anyhow!("nvEncodeAPI64.dll (NVIDIA driver): {e}"))?;
    let create = GetProcAddress(lib, s!("NvEncodeAPICreateInstance")).ok_or_else(|| anyhow!("no NvEncodeAPICreateInstance"))?;
    let max = GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
        .ok_or_else(|| anyhow!("no NvEncodeAPIGetMaxSupportedVersion"))?;
    type Proc = unsafe extern "system" fn() -> isize;
    Ok((std::mem::transmute::<Proc, CreateInstance>(create), std::mem::transmute::<Proc, MaxVersion>(max)))
}

/// The two exported entry points.
#[cfg(target_os = "linux")]
unsafe fn symbols() -> Result<(CreateInstance, MaxVersion)> {
    let lib = libc::dlopen(c"libnvidia-encode.so.1".as_ptr(), libc::RTLD_NOW);
    if lib.is_null() {
        bail!("libnvidia-encode.so.1 (NVIDIA driver) not found");
    }
    let sym = |name: &CStr| -> Result<*mut c_void> {
        let p = libc::dlsym(lib, name.as_ptr());
        if p.is_null() {
            bail!("no {name:?} in libnvidia-encode.so.1");
        }
        Ok(p)
    };
    let create = sym(c"NvEncodeAPICreateInstance")?;
    let max = sym(c"NvEncodeAPIGetMaxSupportedVersion")?;
    Ok((std::mem::transmute::<*mut c_void, CreateInstance>(create), std::mem::transmute::<*mut c_void, MaxVersion>(max)))
}

/// An error for `status` from `what`, with the driver's own explanation.
pub(super) fn error(api: &Api, session: *mut c_void, status: NVENCSTATUS, what: &str) -> anyhow::Error {
    // SAFETY: the string belongs to the session and is read before any other call on it.
    let detail = unsafe {
        let p = nv!(api, nvEncGetLastErrorString(session));
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    anyhow!("{what} failed (NVENCSTATUS {}){}{detail}", status.0, if detail.is_empty() { "" } else { ": " })
}

/// Ok for success, otherwise `error`.
pub(super) fn check(api: &Api, session: *mut c_void, status: NVENCSTATUS, what: &str) -> Result<()> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(error(api, session, status, what))
    }
}
