//! GPU detection/utilization: NVML when the NVIDIA driver is present,
//! otherwise DRM device names from sysfs.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

use remothing_core::platform::{GpuInfo, SystemMonitor};

#[repr(C)]
struct NvmlUtilization {
    gpu: c_uint,
    memory: c_uint,
}

struct Nvml {
    device_count: unsafe extern "C" fn(*mut c_uint) -> c_int,
    device_by_index: unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int,
    device_name: unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int,
    utilization: unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> c_int,
}

impl Nvml {
    fn load() -> Option<Self> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; signatures from nvml.h.
        unsafe {
            let lib = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let sym = |n: &CStr| {
                let p = libc::dlsym(lib, n.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let init: unsafe extern "C" fn() -> c_int = std::mem::transmute(sym(c"nvmlInit_v2")?);
            if init() != 0 {
                return None;
            }
            Some(Self {
                device_count: std::mem::transmute(sym(c"nvmlDeviceGetCount_v2")?),
                device_by_index: std::mem::transmute(sym(c"nvmlDeviceGetHandleByIndex_v2")?),
                device_name: std::mem::transmute(sym(c"nvmlDeviceGetName")?),
                utilization: std::mem::transmute(sym(c"nvmlDeviceGetUtilizationRates")?),
            })
        }
    }

    fn gpus(&self) -> Vec<GpuInfo> {
        let mut out = Vec::new();
        // SAFETY: NVML initialized in `load`; buffers sized as documented.
        unsafe {
            let mut n = 0;
            if (self.device_count)(&mut n) != 0 {
                return out;
            }
            for i in 0..n {
                let mut dev = std::ptr::null_mut();
                if (self.device_by_index)(i, &mut dev) != 0 {
                    continue;
                }
                let mut name = [0 as c_char; 96];
                (self.device_name)(dev, name.as_mut_ptr(), name.len() as c_uint);
                let mut util = NvmlUtilization { gpu: 0, memory: 0 };
                let utilization = ((self.utilization)(dev, &mut util) == 0).then_some(util.gpu as f32);
                out.push(GpuInfo {
                    name: CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned(),
                    vendor: "NVIDIA".into(),
                    utilization,
                });
            }
        }
        out
    }
}

pub struct LinuxMonitor {
    nvml: Option<Nvml>,
}

// NVML handles are process-global and thread-safe.
unsafe impl Send for LinuxMonitor {}
unsafe impl Sync for LinuxMonitor {}

impl LinuxMonitor {
    pub fn new() -> Self {
        Self { nvml: Nvml::load() }
    }
}

fn drm_gpus() -> Vec<GpuInfo> {
    let vendor_name = |id: &str| match id.trim() {
        "0x10de" => "NVIDIA",
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        _ => "unknown",
    };
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    dir.filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("card") && !e.file_name().to_string_lossy().contains('-'))
        .filter_map(|e| {
            let vendor = std::fs::read_to_string(e.path().join("device/vendor")).ok()?;
            let device = std::fs::read_to_string(e.path().join("device/device")).unwrap_or_default();
            Some(GpuInfo {
                name: format!("{} {}", e.file_name().to_string_lossy(), device.trim()),
                vendor: vendor_name(&vendor).into(),
                utilization: None,
            })
        })
        .collect()
}

impl SystemMonitor for LinuxMonitor {
    fn gpus(&self) -> Vec<GpuInfo> {
        match &self.nvml {
            Some(n) => n.gpus(),
            None => drm_gpus(),
        }
    }
}
