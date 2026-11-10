//! The session's virtual microphone on Windows. Windows has no way to make
//! a microphone without a kernel driver, and one only loads with
//! Microsoft's signature, so an existing virtual cable is used: VB-CABLE.
//! What is played into its "CABLE Input" comes out of its "CABLE Output",
//! which programs record from as a microphone.
//!
//! VB-CABLE is VB-Audio's (www.vb-cable.com), donationware, not ours. Its
//! licence lets other applications install its package as it is when the
//! user is told where it comes from. It is installed only when a client
//! asks for it: the package is downloaded from VB-Audio, checked against a
//! known hash and run with its own silent switch, and Windows asks whoever
//! is at the server for administrator consent.
//!
//! While a client talks the cable is the default recording device, so
//! programs that record "the default microphone" hear the client; the
//! previous default comes back afterwards.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use remothing_core::platform::{AudioSink, MicComponent, MicUnavailable, AUDIO_CHANNELS, AUDIO_RATE};
use sha2::{Digest, Sha256};
use windows::core::{w, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IAudioClient, IAudioRenderClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

use crate::audio::{endpoint_id, friendly_name, wave_format, Com};

pub const COMPONENT: MicComponent = MicComponent {
    name: "VB-CABLE",
    notice: "VB-CABLE is a virtual audio cable by VB-Audio (www.vb-cable.com). It is donationware: free to use, \
             and its author welcomes a donation if you find it useful. It is downloaded from VB-Audio and installed as it is.",
    restart: true,
};

/// The package and what it must hash to: nothing else is ever run elevated.
const PACKAGE_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
const PACKAGE_SHA256: &str = "bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90";
const SETUP: &str = "VBCABLE_Setup_x64.exe";
/// Every end of the cable carries this in its name; its two-channel playing end starts with the other.
const CABLE: &str = "VB-Audio Virtual Cable";
const CABLE_INPUT: &str = "CABLE Input";
/// The driver's service: there once installed, even before the restart that makes the cable appear.
const DRIVER_SERVICE: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\VBAudioVACMME";

/// The cable's buffer, in 100 ns units: room for the cushion and late ticks.
const BUFFER_HNS: i64 = 2_000_000;
/// Silence the cable gets when it is empty, to play from while the pieces
/// arrive unevenly (the microphone hub's ticks follow the system timer).
const CUSHION_FRAMES: u32 = AUDIO_RATE * 30 / 1000;
const SETUP_TIMEOUT_MS: u32 = 5 * 60 * 1000;
/// After its setup the cable takes a moment to show up.
const APPEAR: Duration = Duration::from_secs(20);
/// Helper programs must not flash a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// The undocumented interface behind the Sound control panel's "Set
/// Default" (stable since Windows 7). Only `set_default_endpoint` is
/// used; the entries before it keep its place in the table (in the
/// system's headers: GetMixFormat, GetDeviceFormat, ResetDeviceFormat,
/// SetDeviceFormat, Get/SetProcessingPeriod, Get/SetShareMode,
/// Get/SetPropertyValue, then SetDefaultEndpoint).
#[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: windows::core::IUnknown {
    fn get_mix_format(&self) -> HRESULT;
    fn get_device_format(&self) -> HRESULT;
    fn reset_device_format(&self) -> HRESULT;
    fn set_device_format(&self) -> HRESULT;
    fn get_processing_period(&self) -> HRESULT;
    fn set_processing_period(&self) -> HRESULT;
    fn get_share_mode(&self) -> HRESULT;
    fn set_share_mode(&self) -> HRESULT;
    fn get_property_value(&self) -> HRESULT;
    fn set_property_value(&self) -> HRESULT;
    fn set_default_endpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
}

const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: plain COM activation.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("audio device enumerator")
}

/// The cable's ends for `flow`: where we can play (render) or what
/// programs record (capture). The package brings two playing ends, two
/// channels and sixteen, into the same cable: the plain one comes first.
fn cable_ends(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Vec<(String, IMMDevice)> {
    // SAFETY: COM calls on the enumerator and its collection.
    let mut ends: Vec<(String, IMMDevice)> = unsafe {
        let Ok(devices) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        (0..devices.GetCount().unwrap_or(0))
            .filter_map(|i| devices.Item(i).ok())
            .filter_map(|device| Some((friendly_name(&device)?, device)))
            .filter(|(name, _)| name.contains(CABLE))
            .collect()
    };
    ends.sort_by_key(|(name, _)| !name.starts_with(CABLE_INPUT));
    ends
}

/// Ids of the default devices for `flow`, one per role (None: there is none).
fn defaults(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> [Option<String>; 3] {
    // SAFETY: a COM call on the enumerator.
    ROLES.map(|role| unsafe { enumerator.GetDefaultAudioEndpoint(flow, role) }.ok().and_then(|device| endpoint_id(&device).ok()))
}

fn set_defaults(ids: &[Option<String>; 3]) -> Result<()> {
    // SAFETY: COM activation, then calls with a NUL-terminated id that outlives them.
    unsafe {
        let policy: IPolicyConfig = CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        for (id, role) in ids.iter().zip(ROLES) {
            let Some(id) = id else { continue };
            let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
            policy.set_default_endpoint(PCWSTR(wide.as_ptr()), role).ok()?;
        }
    }
    Ok(())
}

/// A helper program without a window, waited for; its error output on failure.
fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program).args(args).creation_flags(CREATE_NO_WINDOW).output().with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        bail!("{program} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or_default());
    }
    Ok(())
}

pub struct CableSink {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
    name: String,
    /// The recording defaults to put back, when they were changed.
    previous: Option<[Option<String>; 3]>,
    /// Dropped after the interfaces above.
    _com: Com,
}

// SAFETY: the interfaces live in the multithreaded apartment and are only
// used by the microphone thread, which also opened them.
unsafe impl Send for CableSink {}

impl CableSink {
    pub fn open(tried: &mut Vec<String>) -> Result<Self, MicUnavailable> {
        let com = Com::new();
        let failed = |tried: &mut Vec<String>, e: anyhow::Error| {
            tried.push(format!("VB-CABLE: {e:#}"));
            MicUnavailable::Failed
        };
        let enumerator = enumerator().map_err(|e| failed(tried, e))?;
        let inputs = cable_ends(&enumerator, eRender);
        let Some((_, output)) = cable_ends(&enumerator, eCapture).into_iter().next().filter(|_| !inputs.is_empty()) else {
            let installed = Command::new("reg").args(["query", DRIVER_SERVICE]).creation_flags(CREATE_NO_WINDOW).output();
            return Err(match installed {
                Ok(out) if out.status.success() => {
                    tried.push("VB-CABLE: installed, but the cable only appears after a restart".into());
                    MicUnavailable::Restart
                }
                _ => {
                    tried.push("VB-CABLE: not installed".into());
                    MicUnavailable::Install(COMPONENT)
                }
            });
        };
        // One end can refuse (seen right after installing, before any
        // restart: 0x8889000A, "in use") while the other plays.
        let opened = inputs.into_iter().find_map(|(name, input)| {
            // SAFETY: COM calls on interfaces just obtained; `format` outlives Initialize.
            let opened = unsafe {
                (|| {
                    let client: IAudioClient = input.Activate(CLSCTX_ALL, None).context("open it")?;
                    let format = wave_format();
                    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &format, None).context("start playing into it")?;
                    let frames = client.GetBufferSize()?;
                    let render: IAudioRenderClient = client.GetService()?;
                    client.Start().context("start playing into it")?;
                    anyhow::Ok((client, render, frames))
                })()
            };
            match opened {
                Ok((client, render, frames)) => Some((name, client, render, frames)),
                Err(e) => {
                    tried.push(format!("{name}: {e:#}"));
                    None
                }
            }
        });
        let Some((name, client, render, frames)) = opened else { return Err(MicUnavailable::Failed) };
        // Programs recording the default microphone hear the client from now on.
        let before = defaults(&enumerator, eCapture);
        let cable = endpoint_id(&output).ok();
        let previous = match set_defaults(&[cable.clone(), cable.clone(), cable]) {
            Ok(()) => {
                tried.push(format!("{name}: ok, its output is the default microphone while the client talks"));
                Some(before)
            }
            Err(e) => {
                tried.push(format!("{name}: ok, but its output could not be made the default microphone ({e:#}): choose it in the program"));
                None
            }
        };
        Ok(Self { client, render, frames, name, previous, _com: com })
    }
}

impl Drop for CableSink {
    fn drop(&mut self) {
        // SAFETY: a started client.
        let _ = unsafe { self.client.Stop() };
        if let Some(previous) = &self.previous {
            let _ = set_defaults(previous);
        }
    }
}

impl AudioSink for CableSink {
    fn name(&self) -> String {
        format!("{} (programs record its CABLE Output)", self.name)
    }

    fn write(&mut self, pcm: &[i16]) -> Result<()> {
        // SAFETY: GetBuffer lends room for `frames` frames of our format until ReleaseBuffer.
        unsafe {
            if self.client.GetCurrentPadding()? == 0 {
                self.render.GetBuffer(CUSHION_FRAMES)?;
                self.render.ReleaseBuffer(CUSHION_FRAMES, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            let free = self.frames - self.client.GetCurrentPadding()?;
            // What does not fit would only be heard late.
            let frames = ((pcm.len() / AUDIO_CHANNELS) as u32).min(free);
            if frames == 0 {
                return Ok(());
            }
            let samples = frames as usize * AUDIO_CHANNELS;
            let room = std::slice::from_raw_parts_mut(self.render.GetBuffer(frames)?, samples * size_of::<i16>());
            for (bytes, sample) in room.chunks_exact_mut(2).zip(&pcm[..samples]) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
            self.render.ReleaseBuffer(frames, 0)?;
        }
        Ok(())
    }
}

/// Downloads VB-CABLE's package and runs its setup, elevated.
pub fn install() -> Result<()> {
    let _com = Com::new();
    let dir = std::env::temp_dir().join("remothing-vbcable");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("temporary directory")?;
    let zip = dir.join("package.zip");
    let (zip_arg, dir_arg) = (zip.to_string_lossy(), dir.to_string_lossy());
    // Both tools come with Windows 10 and later.
    run("curl.exe", &["-fsSL", "--max-time", "120", "-o", &zip_arg, PACKAGE_URL]).context("download VB-CABLE")?;
    let hash: String = Sha256::digest(std::fs::read(&zip)?).iter().map(|b| format!("{b:02x}")).collect();
    if hash != PACKAGE_SHA256 {
        bail!("the VB-CABLE package from {PACKAGE_URL} is not the one this build knows (sha256 {hash}): not installed");
    }
    run("tar.exe", &["-xf", &zip_arg, "-C", &dir_arg]).context("unpack VB-CABLE")?;

    // A new sound device tends to become the default one: playback would
    // go into the cable, and so into the microphone.
    let enumerator = enumerator()?;
    let (playback, recording) = (defaults(&enumerator, eRender), defaults(&enumerator, eCapture));
    let setup = run_elevated(&dir.join(SETUP), "-i -h");
    let appeared = Instant::now();
    while setup.is_ok() && cable_ends(&enumerator, eRender).is_empty() && appeared.elapsed() < APPEAR {
        std::thread::sleep(Duration::from_millis(500));
    }
    if defaults(&enumerator, eRender) != playback || defaults(&enumerator, eCapture) != recording {
        set_defaults(&playback).and_then(|()| set_defaults(&recording)).context("put the default sound devices back")?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    setup
}

/// Runs a program as administrator and waits for it. Windows asks for
/// consent on the server's screen unless we are elevated already.
fn run_elevated(program: &Path, parameters: &str) -> Result<()> {
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (file, parameters) = (wide(&program.to_string_lossy()), wide(parameters));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        ..Default::default()
    };
    // SAFETY: `info` and the strings it points to outlive the call; the
    // process handle it returns is closed below.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                bail!("the administrator prompt on the server was declined");
            }
            return Err(anyhow!(e).context("start the VB-CABLE setup as administrator"));
        }
        let waited = WaitForSingleObject(info.hProcess, SETUP_TIMEOUT_MS);
        let mut code = 0;
        let exited = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if waited != WAIT_OBJECT_0 {
            bail!("the VB-CABLE setup did not finish in {} minutes", SETUP_TIMEOUT_MS / 60_000);
        }
        exited.context("VB-CABLE setup")?;
        if code != 0 {
            bail!("the VB-CABLE setup ended with code {code}");
        }
    }
    Ok(())
}

pub fn restart() -> Result<()> {
    run("shutdown.exe", &["/r", "/t", "5", "/c", "remothing: restart asked from a client, to finish installing VB-CABLE"])
}
