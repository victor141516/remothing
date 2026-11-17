//! Virtual displays through remothing's own indirect display driver
//! (drivers/windows-vdd: SudoVDA with its own hardware id, interface and
//! signing certificate). The signed package is embedded; the first use
//! installs it after the user agreed (`VirtualSetup::Install`), which only
//! an elevated process can do: the Windows service's server, which runs as
//! SYSTEM. Displays are added and removed with IOCTLs on the driver's
//! control device; the driver removes them all when nobody pings it for a
//! few seconds, so they go away with the server. Windows names a display by
//! its target (adapter LUID, target id); `vdd:N` ids stay the same while
//! DXGI's `dxgi:a:o` shift as displays come and go.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, mem, thread};

use anyhow::{anyhow, bail, Context, Result};
use remothing_core::platform::{
    DisplayInfo, DisplayKind, DisplayMode, VirtualCapabilities, VirtualDisplays, VirtualSetup,
};
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW,
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, INSTALLFLAG_FORCE,
    SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_HARDWAREID, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
};
use windows::Win32::Security::Cryptography::{
    CertAddEncodedCertificateToStore, CertCloseStore, CertOpenStore, CERT_OPEN_STORE_FLAGS,
    CERT_QUERY_ENCODING_TYPE, CERT_STORE_ADD_REPLACE_EXISTING, CERT_STORE_PROV_SYSTEM_W,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::IO::DeviceIoControl;

use crate::dxgi::{self, Output};

/// drivers/windows-vdd/vdd-ioctl.h.
const HARDWARE_ID: &str = r"Root\remothing\vdd";
const INTERFACE: GUID = GUID::from_u128(0xc66e256f_0f7e_4d04_b1d5_9136bed1fbf0);
/// The Display device class.
const DISPLAY_CLASS: GUID = GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);
/// CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS).
const IOCTL_ADD: u32 = (0x22 << 16) | (0x800 << 2);
const IOCTL_REMOVE: u32 = (0x22 << 16) | (0x801 << 2);
const IOCTL_PING: u32 = (0x22 << 16) | (0x888 << 2);

/// The driver package built and signed by .github/workflows/windows-vdd.yml.
const PACKAGE: [(&str, &[u8]); 3] = [
    ("remothing-vdd.inf", include_bytes!("../../../drivers/windows-vdd/package/remothing-vdd.inf")),
    ("remothing-vdd.dll", include_bytes!("../../../drivers/windows-vdd/package/remothing-vdd.dll")),
    ("remothing-vdd.cat", include_bytes!("../../../drivers/windows-vdd/package/remothing-vdd.cat")),
];
/// The certificate the package is signed with.
const CERTIFICATE: &[u8] = include_bytes!("../../../drivers/windows-vdd/package/remothing-vdd.cer");

const NOTICE: &str = "Virtual displays on Windows need remothing's display driver (an indirect display \
    driver, the kind Apollo and Parsec use). Installing it adds a \"remothing Virtual Display Adapter\" \
    device and remothing's own driver signing certificate to this computer's trusted root and trusted \
    publisher stores. Both stay until removed in Device Manager and certlm.msc.";
const NOT_ELEVATED: &str = "the display driver is not installed, and only remothing running as the Windows \
    service (or as administrator) can install it";
const NO_HDR: &str = "Windows offers no HDR on the virtual display (the driver's HDR needs Windows 11 24H2)";

const MAX_WIDTH: u32 = 7680;
const MAX_HEIGHT: u32 = 4320;
/// The rates the driver's monitor description lists.
const REFRESH_RATES: [u32; 7] = [60, 75, 90, 120, 144, 165, 240];
/// The driver's watchdog removes every display after 3 s without a request.
const PING_INTERVAL: Duration = Duration::from_secs(1);
/// How long Windows gets to install the driver, or to show, change or
/// remove a display.
const INSTALL_WAIT: Duration = Duration::from_secs(30);
const SETTLE: Duration = Duration::from_secs(10);

#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    refresh_rate: u32,
    monitor: GUID,
    device_name: [u8; 14],
    serial_number: [u8; 14],
}

#[repr(C)]
#[derive(Default)]
struct AddOut {
    adapter: LUID,
    target_id: u32,
}

#[repr(C)]
struct RemoveParams {
    monitor: GUID,
}

/// A path target: adapter and target id.
type Target = (LUID, u32);

/// The driver's control device.
struct Device(HANDLE);

// SAFETY: a kernel file handle; DeviceIoControl may be called on it from any
// thread, and it is closed only on drop.
unsafe impl Send for Device {}
// SAFETY: as above; the requests are synchronous and independent.
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: opened by CreateFileW and closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl Device {
    /// None where the driver's interface is not there (not installed).
    fn open() -> Result<Option<Self>> {
        let Some(path) = interface_path()? else { return Ok(None) };
        // SAFETY: NUL-terminated path; plain open of an existing device.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .context("opening the virtual display driver (administrators only)")?;
        Ok(Some(Self(handle)))
    }

    fn call<I, O>(&self, code: u32, input: Option<&I>, output: Option<&mut O>) -> windows::core::Result<()> {
        let (in_ptr, in_len) = input.map_or((None, 0), |i| (Some(i as *const I as *const c_void), mem::size_of::<I>()));
        let (out_ptr, out_len) = output.map_or((None, 0), |o| (Some(o as *mut O as *mut c_void), mem::size_of::<O>()));
        let mut returned = 0u32;
        // SAFETY: the buffers are live for the call and their sizes are theirs.
        unsafe {
            DeviceIoControl(self.0, code, in_ptr, in_len as u32, out_ptr, out_len as u32, Some(&mut returned), None)
        }
    }
}

/// A SetupAPI device information set, destroyed on drop.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        // SAFETY: created by SetupDiGetClassDevsW/SetupDiCreateDeviceInfoList.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

struct Virtual {
    number: u32,
    monitor: GUID,
    target: Target,
    mode: DisplayMode,
}

#[derive(Default)]
struct Table {
    displays: Vec<Virtual>,
    next: u32,
}

#[derive(Default)]
pub(crate) struct VirtualDisplayDriver {
    /// Opened on first use; a thread pings it from then on.
    device: Mutex<Option<Arc<Device>>>,
    /// Installing, adding and removing take seconds and go one at a time;
    /// listing displays only takes `table`.
    busy: Mutex<()>,
    table: Mutex<Table>,
}

impl VirtualDisplayDriver {
    fn device(&self) -> Result<Arc<Device>> {
        let mut slot = self.device.lock().unwrap();
        if let Some(d) = slot.as_ref() {
            return Ok(d.clone());
        }
        let device = Arc::new(Device::open()?.context("the virtual display driver is not installed")?);
        let pinged = device.clone();
        thread::Builder::new()
            .name("vdd-ping".into())
            .spawn(move || {
                let mut failing = false;
                loop {
                    match pinged.call::<(), ()>(IOCTL_PING, None, None) {
                        Ok(()) => failing = false,
                        Err(e) if !failing => {
                            eprintln!("virtual display driver ping failed: {e}");
                            failing = true;
                        }
                        Err(_) => {}
                    }
                    thread::sleep(PING_INTERVAL);
                }
            })
            .context("spawning the driver ping thread")?;
        *slot = Some(device.clone());
        Ok(device)
    }

    /// Gives the virtual displays among `outputs` their `vdd:N` id, kind and mode.
    pub fn annotate(&self, outputs: &mut [Output]) {
        let table = self.table.lock().unwrap();
        if table.displays.is_empty() {
            return;
        }
        let names = gdi_names();
        for v in &table.displays {
            let Some((_, gdi)) = names.iter().find(|(t, _)| *t == v.target) else { continue };
            if let Some(out) = outputs.iter_mut().find(|o| o.gdi == gdi[..gdi.len() - 1]) {
                out.info.name = format!("Virtual display {} ({})", v.number, String::from_utf16_lossy(&out.gdi));
                out.info.id = format!("vdd:{}", v.number);
                out.info.kind = DisplayKind::Virtual;
                out.info.mode = Some(v.mode);
            }
        }
    }

    fn info(&self, number: u32) -> Result<DisplayInfo> {
        let mut outputs = dxgi::outputs()?;
        self.annotate(&mut outputs);
        let id = format!("vdd:{number}");
        Ok(outputs.into_iter().find(|o| o.info.id == id).with_context(|| format!("{id} is not on the desktop"))?.info)
    }

    /// Adds the display to the driver and brings it up in `mode`.
    fn add(&self, device: &Device, number: u32, monitor: GUID, mode: DisplayMode) -> Result<Target> {
        let mut params = AddParams {
            width: mode.width,
            height: mode.height,
            refresh_rate: mode.refresh_hz,
            monitor,
            device_name: [0; 14],
            serial_number: [0; 14],
        };
        let name = b"remothing";
        params.device_name[..name.len()].copy_from_slice(name);
        let serial = format!("RMT{number:05}");
        params.serial_number[..serial.len()].copy_from_slice(serial.as_bytes());
        let mut out = AddOut::default();
        device.call(IOCTL_ADD, Some(&params), Some(&mut out)).context("adding the display to the driver")?;
        let target = (out.adapter, out.target_id);
        if let Err(e) = bring_up(target, mode) {
            let _ = device.call::<_, ()>(IOCTL_REMOVE, Some(&RemoveParams { monitor }), None);
            return Err(e);
        }
        Ok(target)
    }
}

impl VirtualDisplays for VirtualDisplayDriver {
    fn capabilities(&self) -> VirtualCapabilities {
        VirtualCapabilities {
            max_width: MAX_WIDTH,
            max_height: MAX_HEIGHT,
            refresh_rates: REFRESH_RATES.to_vec(),
            hdr: true,
            resize: true,
        }
    }

    fn setup(&self) -> VirtualSetup {
        if self.device.lock().unwrap().is_some() {
            return VirtualSetup::Ready;
        }
        match interface_path() {
            Ok(Some(_)) => match self.device() {
                Ok(_) => VirtualSetup::Ready,
                Err(e) => VirtualSetup::Unavailable { reason: format!("{e:#}") },
            },
            Ok(None) if elevated() => VirtualSetup::Install { notice: NOTICE.into() },
            Ok(None) => VirtualSetup::Unavailable { reason: NOT_ELEVATED.into() },
            Err(e) => VirtualSetup::Unavailable { reason: format!("{e:#}") },
        }
    }

    fn install(&self) -> Result<()> {
        let _busy = self.busy.lock().unwrap();
        if !elevated() {
            bail!("{NOT_ELEVATED}");
        }
        trust_certificate()?;
        let dir = PathBuf::from(std::env::var_os("ProgramData").context("ProgramData is not set")?).join(r"remothing\vdd");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for (name, bytes) in PACKAGE {
            fs::write(dir.join(name), bytes).with_context(|| format!("writing {name}"))?;
        }
        if !device_node_exists()? {
            create_device_node()?;
        }
        let inf: Vec<u16> = dir.join(PACKAGE[0].0).as_os_str().encode_wide().chain([0]).collect();
        let hardware_id: Vec<u16> = HARDWARE_ID.encode_utf16().chain([0]).collect();
        let mut reboot = windows::core::BOOL::default();
        // SAFETY: NUL-terminated strings; `reboot` outlives the call.
        unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                None,
                PCWSTR(hardware_id.as_ptr()),
                PCWSTR(inf.as_ptr()),
                INSTALLFLAG_FORCE,
                Some(&mut reboot),
            )
        }
        .context("installing the driver (UpdateDriverForPlugAndPlayDevices)")?;
        wait(INSTALL_WAIT, || interface_path().ok().flatten())
            .context("the driver was installed but its device did not start (see Device Manager)")?;
        self.device().map(|_| ())
    }

    fn create(&self, mode: DisplayMode) -> Result<DisplayInfo> {
        let _busy = self.busy.lock().unwrap();
        let device = self.device()?;
        let monitor = GUID::new().context("CoCreateGuid")?;
        let number = {
            let mut table = self.table.lock().unwrap();
            table.next += 1;
            table.next
        };
        let target = self.add(&device, number, monitor, mode)?;
        self.table.lock().unwrap().displays.push(Virtual { number, monitor, target, mode });
        self.info(number)
    }

    /// The driver gives a display its modes when it is added, so a new mode
    /// is the same monitor (GUID, so Windows keeps its settings) added again.
    /// If it does not come back in the new mode it comes back in the old one.
    fn set_mode(&self, id: &str, mode: DisplayMode) -> Result<DisplayInfo> {
        let _busy = self.busy.lock().unwrap();
        let device = self.device()?;
        let number = parse_id(id)?;
        let (monitor, target, previous) = self.entry(number)?;
        if mode.hdr && !previous.hdr && !advanced_color(target)?.0 {
            bail!("{NO_HDR}");
        }
        remove(&device, monitor, target)?;
        let (result, now) = match self.add(&device, number, monitor, mode) {
            Ok(target) => (Ok(()), Some((target, mode))),
            Err(e) => match self.add(&device, number, monitor, previous) {
                Ok(target) => {
                    let kept = format!("{id} stays at {}x{}", previous.width, previous.height);
                    (Err(e.context(kept)), Some((target, previous)))
                }
                Err(_) => (Err(e.context(format!("{id} was removed and could not come back"))), None),
            },
        };
        let mut table = self.table.lock().unwrap();
        match now {
            Some((target, mode)) => {
                if let Some(v) = table.displays.iter_mut().find(|v| v.number == number) {
                    v.target = target;
                    v.mode = mode;
                }
            }
            None => table.displays.retain(|v| v.number != number),
        }
        drop(table);
        result?;
        self.info(number)
    }

    fn destroy(&self, id: &str) -> Result<()> {
        let _busy = self.busy.lock().unwrap();
        let device = self.device()?;
        let number = parse_id(id)?;
        let (monitor, target, _) = self.entry(number)?;
        remove(&device, monitor, target)?;
        self.table.lock().unwrap().displays.retain(|v| v.number != number);
        Ok(())
    }
}

impl VirtualDisplayDriver {
    /// Monitor GUID, current target and mode of display `number`.
    fn entry(&self, number: u32) -> Result<(GUID, Target, DisplayMode)> {
        let table = self.table.lock().unwrap();
        let v = table.displays.iter().find(|v| v.number == number).with_context(|| format!("no virtual display vdd:{number}"))?;
        Ok((v.monitor, v.target, v.mode))
    }
}

fn parse_id(id: &str) -> Result<u32> {
    id.strip_prefix("vdd:").and_then(|n| n.parse().ok()).with_context(|| format!("{id} is not a virtual display"))
}

/// Removes the display from the driver and waits until it left the desktop.
fn remove(device: &Device, monitor: GUID, target: Target) -> Result<()> {
    device.call::<_, ()>(IOCTL_REMOVE, Some(&RemoveParams { monitor }), None).context("removing the display")?;
    wait(SETTLE, || (!gdi_names().iter().any(|(t, _)| *t == target)).then_some(()))
        .context("the removed display stayed on the desktop")?;
    Ok(())
}

/// Waits for the new display on the desktop, sets its mode and HDR and waits
/// until desktop duplication sees it at that size.
fn bring_up(target: Target, mode: DisplayMode) -> Result<()> {
    let gdi = wait(SETTLE, || gdi_names().into_iter().find(|(t, _)| *t == target).map(|(_, g)| g))
        .context("Windows did not put the virtual display on the desktop (is the display mode \"Extend\"?)")?;
    set_resolution(&gdi, mode)?;
    set_hdr(target, mode.hdr)?;
    let name = &gdi[..gdi.len() - 1];
    wait(SETTLE, || {
        dxgi::outputs().ok()?.into_iter().find(|o| o.gdi == name && (o.info.width, o.info.height) == (mode.width, mode.height))
    })
    .with_context(|| format!("{} did not reach {}x{}", String::from_utf16_lossy(name), mode.width, mode.height))?;
    Ok(())
}

/// Switches GDI display `gdi` (NUL-terminated) to the mode, unless it has it.
fn set_resolution(gdi: &[u16], mode: DisplayMode) -> Result<()> {
    let mut current = DEVMODEW { dmSize: mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: NUL-terminated name; dmSize is set.
    let known = unsafe { EnumDisplaySettingsW(PCWSTR(gdi.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current) }.as_bool();
    if known
        && (current.dmPelsWidth, current.dmPelsHeight, current.dmDisplayFrequency)
            == (mode.width, mode.height, mode.refresh_hz)
    {
        return Ok(());
    }
    let wanted = DEVMODEW {
        dmSize: mem::size_of::<DEVMODEW>() as u16,
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: mode.refresh_hz,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        ..Default::default()
    };
    // SAFETY: NUL-terminated name; the mode is a filled DEVMODEW.
    let result = unsafe { ChangeDisplaySettingsExW(PCWSTR(gdi.as_ptr()), Some(&wanted), None, CDS_UPDATEREGISTRY, None) };
    if result != DISP_CHANGE_SUCCESSFUL {
        bail!("Windows refused {}x{} at {} Hz (ChangeDisplaySettingsEx {})", mode.width, mode.height, mode.refresh_hz, result.0);
    }
    Ok(())
}

/// Whether the target can show HDR (advanced colour), and whether it does.
fn advanced_color(target: Target) -> Result<(bool, bool)> {
    let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            size: mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
            adapterId: target.0,
            id: target.1,
        },
        ..Default::default()
    };
    // SAFETY: the packet carries its own type and size.
    let err = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
    if err != 0 {
        bail!("reading the display's HDR state failed ({err})");
    }
    // SAFETY: the union's bit-field view; bit 0 supported, bit 1 enabled.
    let bits = unsafe { info.Anonymous.value };
    Ok((bits & 1 != 0, bits & 2 != 0))
}

/// Turns the target's HDR (advanced colour) on or off and checks it took.
fn set_hdr(target: Target, on: bool) -> Result<()> {
    let (supported, enabled) = advanced_color(target)?;
    if enabled == on {
        return Ok(());
    }
    if on && !supported {
        bail!("{NO_HDR}");
    }
    let mut set = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
            size: mem::size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32,
            adapterId: target.0,
            id: target.1,
        },
        ..Default::default()
    };
    set.Anonymous.value = u32::from(on);
    // SAFETY: the packet carries its own type and size.
    let err = unsafe { DisplayConfigSetDeviceInfo(&set.header) };
    if err != 0 {
        bail!("switching HDR {} failed ({err})", if on { "on" } else { "off" });
    }
    wait(SETTLE, || advanced_color(target).ok().filter(|s| s.1 == on)).context("Windows did not switch the display's HDR")?;
    Ok(())
}

/// GDI names (`\\.\DISPLAYn`, NUL-terminated) of the active paths, by target.
fn gdi_names() -> Vec<(Target, Vec<u16>)> {
    let mut names = Vec::new();
    // SAFETY: buffers sized by GetDisplayConfigBufferSizes; request packets carry their own size.
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return names;
        }
        for path in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let name = &source.viewGdiDeviceName;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let mut gdi = name[..len].to_vec();
            gdi.push(0);
            names.push(((path.targetInfo.adapterId, path.targetInfo.id), gdi));
        }
    }
    names
}

/// The device path of the driver's control interface, NUL-terminated, if
/// the driver runs.
fn interface_path() -> Result<Option<Vec<u16>>> {
    // SAFETY: SetupAPI enumeration; the detail buffer is sized by the first call
    // and 8-byte aligned, as SP_DEVICE_INTERFACE_DETAIL_DATA_W needs.
    unsafe {
        let set = DevInfo(
            SetupDiGetClassDevsW(Some(&INTERFACE), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
                .context("SetupDiGetClassDevs")?,
        );
        let mut data = SP_DEVICE_INTERFACE_DATA { cbSize: mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
        if SetupDiEnumDeviceInterfaces(set.0, None, &INTERFACE, 0, &mut data).is_err() {
            return Ok(None);
        }
        let mut size = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(set.0, &data, None, 0, Some(&mut size), None);
        let mut buf = vec![0u64; (size as usize).div_ceil(8).max(1)];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        SetupDiGetDeviceInterfaceDetailW(set.0, &data, Some(detail), size, None, None)
            .context("SetupDiGetDeviceInterfaceDetail")?;
        let path = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
        let chars = (size as usize - mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath)) / 2;
        let path = std::slice::from_raw_parts(path, chars);
        let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        let mut out = path[..len].to_vec();
        out.push(0);
        Ok(Some(out))
    }
}

/// A device node with the driver's hardware id exists (running or not).
fn device_node_exists() -> Result<bool> {
    // SAFETY: SetupAPI enumeration of the Display class; the property buffer
    // is passed with its length.
    unsafe {
        let set = DevInfo(
            SetupDiGetClassDevsW(Some(&DISPLAY_CLASS), PCWSTR::null(), None, SETUP_DI_GET_CLASS_DEVS_FLAGS(0))
                .context("SetupDiGetClassDevs(Display)")?,
        );
        for index in 0.. {
            let mut info = SP_DEVINFO_DATA { cbSize: mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set.0, index, &mut info).is_err() {
                break;
            }
            let mut buf = [0u8; 1024];
            if SetupDiGetDeviceRegistryPropertyW(set.0, &info, SPDRP_HARDWAREID, None, Some(&mut buf), None).is_err() {
                continue;
            }
            let wide: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let ids = String::from_utf16_lossy(&wide);
            if ids.split('\0').any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A root-enumerated device with the driver's hardware id, which
/// UpdateDriverForPlugAndPlayDevices then gives the driver (what `devcon
/// install` does).
fn create_device_node() -> Result<()> {
    let ids: Vec<u8> = HARDWARE_ID.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes).collect();
    // SAFETY: SetupAPI calls on a set created here; `info` is initialised
    // with its size and the property buffer is passed with its length.
    unsafe {
        let set = DevInfo(SetupDiCreateDeviceInfoList(Some(&DISPLAY_CLASS), None).context("SetupDiCreateDeviceInfoList")?);
        let mut info = SP_DEVINFO_DATA { cbSize: mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        SetupDiCreateDeviceInfoW(set.0, w!("Display"), &DISPLAY_CLASS, PCWSTR::null(), None, DICD_GENERATE_ID, Some(&mut info))
            .context("SetupDiCreateDeviceInfo")?;
        SetupDiSetDeviceRegistryPropertyW(set.0, &mut info, SPDRP_HARDWAREID, Some(&ids))
            .context("setting the hardware id")?;
        SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&info)).context("registering the device")?;
    }
    Ok(())
}

/// Puts the signing certificate into the machine's trusted root and trusted
/// publisher stores, which is what makes Windows accept the package.
fn trust_certificate() -> Result<()> {
    for store in [w!("Root"), w!("TrustedPublisher")] {
        // SAFETY: system store opened by name and closed below; the
        // certificate bytes outlive the call.
        unsafe {
            let handle = CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE),
                Some(store.as_ptr() as *const c_void),
            )
            .with_context(|| format!("opening the {} certificate store", store.display()))?;
            let added = CertAddEncodedCertificateToStore(
                Some(handle),
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                CERTIFICATE,
                CERT_STORE_ADD_REPLACE_EXISTING,
                None,
            );
            let _ = CertCloseStore(Some(handle), 0);
            added.with_context(|| format!("adding the certificate to {}", store.display()))?;
        }
    }
    Ok(())
}

/// This process may install drivers (elevated administrator or SYSTEM).
fn elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: pseudo handle of this process; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the size given.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: opened above.
    let _ = unsafe { CloseHandle(token) };
    ok && elevation.TokenIsElevated != 0
}

/// Polls `probe` until it gives a value or `limit` passes.
fn wait<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = probe() {
            return Ok(v);
        }
        if Instant::now() > deadline {
            return Err(anyhow!("gave up after {limit:?}"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}
