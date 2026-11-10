//! What the session plays, through WASAPI loopback: the mix the audio engine
//! sends to the default playback device. The engine converts it to our
//! format. While nothing plays, loopback delivers nothing.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use remothing_core::platform::{AudioSource, AUDIO_CHANNELS, AUDIO_RATE};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

/// Capture buffer of the audio engine, in 100 ns units: room for what plays
/// while the thread is busy.
const BUFFER_HNS: i64 = 2_000_000;
/// Loopback has no usable event: the engine's packets (10 ms each) are polled.
const POLL: Duration = Duration::from_millis(5);
const WAIT: Duration = Duration::from_millis(100);
/// The user may pick another playback device; this one would stay silent.
const DEVICE_CHECK: Duration = Duration::from_secs(2);

/// COM on the calling thread for as long as this lives.
struct Com(bool);

impl Com {
    fn new() -> Self {
        // SAFETY: no preconditions. An error means the thread already has
        // COM in another mode, which works as well and is not ours to undo.
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx of this thread.
            unsafe { CoUninitialize() };
        }
    }
}

pub struct Loopback {
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    device_id: String,
    name: String,
    checked: Instant,
    /// Dropped after the interfaces above.
    _com: Com,
}

// SAFETY: the interfaces live in the multithreaded apartment and are only
// used by the audio thread, which also opened them.
unsafe impl Send for Loopback {}

fn endpoint_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: GetId returns a string allocated for the caller to free.
    unsafe {
        let id = device.GetId()?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.0.cast()));
        Ok(text?)
    }
}

impl Loopback {
    pub fn open() -> Result<Self> {
        let com = Com::new();
        // SAFETY: COM calls on interfaces just obtained; `format` outlives Initialize.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator")?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("no playback device")?;
            let device_id = endpoint_id(&device)?;
            let name = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .map_or_else(|_| device_id.clone(), |name| name.to_string());
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("open the playback device")?;
            let block = (AUDIO_CHANNELS * size_of::<i16>()) as u16;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: AUDIO_CHANNELS as u16,
                nSamplesPerSec: AUDIO_RATE,
                nAvgBytesPerSec: AUDIO_RATE * u32::from(block),
                nBlockAlign: block,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &format, None)
                .context("start loopback capture")?;
            let capture: IAudioCaptureClient = client.GetService().context("loopback capture client")?;
            client.Start().context("start loopback capture")?;
            Ok(Self { enumerator, client, capture, device_id, name, checked: Instant::now(), _com: com })
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: a started client.
        let _ = unsafe { self.client.Stop() };
    }
}

impl AudioSource for Loopback {
    fn name(&self) -> String {
        format!("WASAPI loopback of {}", self.name)
    }

    fn read(&mut self, out: &mut Vec<i16>) -> Result<()> {
        let before = out.len();
        let deadline = Instant::now() + WAIT;
        loop {
            // SAFETY: GetBuffer lends `frames` frames of our format at `data` until ReleaseBuffer.
            unsafe {
                while self.capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let samples = frames as usize * AUDIO_CHANNELS;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        out.resize(out.len() + samples, 0);
                    } else {
                        let bytes = std::slice::from_raw_parts(data, samples * size_of::<i16>());
                        out.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                    }
                    self.capture.ReleaseBuffer(frames)?;
                }
            }
            if out.len() > before || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        if self.checked.elapsed() >= DEVICE_CHECK {
            self.checked = Instant::now();
            // SAFETY: a COM call on our enumerator.
            let current = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.context("no playback device")?;
            if endpoint_id(&current)? != self.device_id {
                bail!("the default playback device changed");
            }
        }
        Ok(())
    }
}
