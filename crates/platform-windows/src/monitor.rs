//! GPU list from DXGI adapters; utilization from NVML when the NVIDIA driver
//! ships it (nvml.dll, loaded dynamically).

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

use remothing_core::platform::{GpuInfo, SystemMonitor};
use windows::core::{s, w, PCWSTR};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const VENDOR_NVIDIA: u32 = 0x10DE;

#[repr(C)]
struct NvmlUtilization {
    gpu: c_uint,
    memory: c_uint,
}

type InitFn = unsafe extern "C" fn() -> c_int;
type CountFn = unsafe extern "C" fn(*mut c_uint) -> c_int;
type ByIndexFn = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
type NameFn = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int;
type UtilizationFn = unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> c_int;
/// What GetProcAddress returns (FARPROC without the Option).
type RawProc = unsafe extern "system" fn() -> isize;

struct Nvml {
    device_count: CountFn,
    device_by_index: ByIndexFn,
    device_name: NameFn,
    utilization: UtilizationFn,
}

impl Nvml {
    fn load() -> Option<Self> {
        // Recent drivers install nvml.dll in System32; older ones only under NVSMI.
        let paths: [PCWSTR; 2] = [w!("nvml.dll"), w!(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll")];
        // SAFETY: NUL-terminated names; signatures from nvml.h. The library
        // stays loaded for the process lifetime (the pointers are kept).
        unsafe {
            let lib = paths.iter().find_map(|p| LoadLibraryW(*p).ok())?;
            let sym = |name| GetProcAddress(lib, name);
            let init = std::mem::transmute::<RawProc, InitFn>(sym(s!("nvmlInit_v2"))?);
            if init() != 0 {
                return None;
            }
            Some(Self {
                device_count: std::mem::transmute::<RawProc, CountFn>(sym(s!("nvmlDeviceGetCount_v2"))?),
                device_by_index: std::mem::transmute::<RawProc, ByIndexFn>(sym(s!("nvmlDeviceGetHandleByIndex_v2"))?),
                device_name: std::mem::transmute::<RawProc, NameFn>(sym(s!("nvmlDeviceGetName"))?),
                utilization: std::mem::transmute::<RawProc, UtilizationFn>(sym(s!("nvmlDeviceGetUtilizationRates"))?),
            })
        }
    }

    /// (name, utilization %) per NVIDIA device, in NVML order.
    fn devices(&self) -> Vec<(String, Option<f32>)> {
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
                if (self.device_name)(dev, name.as_mut_ptr(), name.len() as c_uint) != 0 {
                    continue;
                }
                let mut util = NvmlUtilization { gpu: 0, memory: 0 };
                let utilization = ((self.utilization)(dev, &mut util) == 0).then_some(util.gpu as f32);
                out.push((CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned(), utilization));
            }
        }
        out
    }
}

pub struct WindowsMonitor {
    nvml: Option<Nvml>,
}

impl WindowsMonitor {
    pub fn new() -> Self {
        Self { nvml: Nvml::load() }
    }
}

fn vendor_name(id: u32) -> String {
    match id {
        VENDOR_NVIDIA => "NVIDIA".into(),
        0x1002 => "AMD".into(),
        0x8086 => "Intel".into(),
        other => format!("{other:#06x}"),
    }
}

impl SystemMonitor for WindowsMonitor {
    fn gpus(&self) -> Vec<GpuInfo> {
        // NVML and DXGI share no adapter id; identical names are paired in
        // enumeration order, which both follow the PCI order.
        let mut nvml = self.nvml.as_ref().map(Nvml::devices).unwrap_or_default();
        let mut out = Vec::new();
        // SAFETY: DXGI enumeration; descriptors are plain data.
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
        for i in 0.. {
            // SAFETY: as above.
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
            // SAFETY: as above.
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            let utilization = if desc.VendorId == VENDOR_NVIDIA {
                nvml.iter().position(|(n, _)| *n == name).and_then(|p| nvml.remove(p).1)
            } else {
                None
            };
            out.push(GpuInfo { name, vendor: vendor_name(desc.VendorId), utilization });
        }
        out
    }
}
