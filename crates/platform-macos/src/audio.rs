//! What the Mac plays, through ScreenCaptureKit's audio capture (macOS 13
//! and later): every program's sound mixed, without an audio driver. It needs
//! the same Screen Recording permission as the picture.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use objc2::AllocAnyThread;
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioStreamBasicDescription};
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime};
use objc2_foundation::NSArray;
use objc2_screen_capture_kit::{SCContentFilter, SCStreamConfiguration, SCStreamOutputType};
use remothing_core::platform::{AudioSource, AUDIO_CHANNELS, AUDIO_RATE};

use crate::sck::{self, SampleHandler, StopHandler, Stream};

/// About a second of sound: more means the reader stopped reading.
const MAX_QUEUED: usize = AUDIO_RATE as usize * AUDIO_CHANNELS;

#[derive(Default)]
struct Queue {
    pcm: Vec<i16>,
    stopped: Option<String>,
}

pub(crate) struct SckAudio {
    _stream: Stream,
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

impl SckAudio {
    pub fn open(tried: &mut Vec<String>) -> Result<Self> {
        match Self::start() {
            Ok(s) => {
                tried.push("ScreenCaptureKit system sound: ok".into());
                Ok(s)
            }
            Err(e) => {
                tried.push(format!("ScreenCaptureKit system sound: {e:#}"));
                Err(e)
            }
        }
    }

    fn start() -> Result<Self> {
        let content = sck::shareable_content()?;
        // The sound is the whole system's; a display is needed only to make a filter.
        let display = sck::find_display(&content, objc2_core_graphics::CGMainDisplayID())?;
        // SAFETY: plain object creation and setters.
        let (filter, config) = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(AUDIO_RATE as isize);
            config.setChannelCount(AUDIO_CHANNELS as isize);
            config.setExcludesCurrentProcessAudio(true);
            // The video of this stream is not used: as little of it as allowed.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            (filter, config)
        };
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let on_sample = {
            let queue = queue.clone();
            Box::new(move |sample: &CMSampleBuffer| {
                let mut q = queue.0.lock().unwrap();
                if let Err(e) = append(sample, &mut q.pcm) {
                    q.stopped.get_or_insert_with(|| format!("{e:#}"));
                }
                if q.pcm.len() > MAX_QUEUED {
                    let extra = q.pcm.len() - MAX_QUEUED;
                    q.pcm.drain(..extra);
                }
                drop(q);
                queue.1.notify_one();
            }) as SampleHandler
        };
        let on_stop = {
            let queue = queue.clone();
            Box::new(move |why: String| {
                queue.0.lock().unwrap().stopped = Some(why);
                queue.1.notify_one();
            }) as StopHandler
        };
        let stream = Stream::start(&filter, &config, SCStreamOutputType::Audio, "remothing.sound", on_sample, on_stop)?;
        Ok(Self { _stream: stream, queue })
    }
}

/// Appends the samples of one audio sample buffer as interleaved i16.
fn append(sample: &CMSampleBuffer, out: &mut Vec<i16>) -> Result<()> {
    // SAFETY: plain getters of a valid sample buffer.
    let (desc, data, frames) = unsafe { (sample.format_description(), sample.data_buffer(), sample.num_samples()) };
    let desc = desc.context("sound sample without format")?;
    // SAFETY: an audio format description; the pointer lives as long as `desc`.
    let asbd: AudioStreamBasicDescription =
        unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&desc).as_ref() }.copied().context("not an audio format")?;
    let data = data.context("sound sample without data")?;
    // SAFETY: plain getter.
    let len = unsafe { data.data_length() };
    let mut bytes = vec![0u8; len];
    // SAFETY: `bytes` holds `len` bytes.
    let status = unsafe { data.copy_data_bytes(0, len, std::ptr::NonNull::new(bytes.as_mut_ptr().cast()).unwrap()) };
    if status != 0 {
        bail!("CMBlockBufferCopyDataBytes failed ({status})");
    }
    if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
        bail!("unexpected sound format (flags {:#x}, {} bits)", asbd.mFormatFlags, asbd.mBitsPerChannel);
    }
    if asbd.mSampleRate as u32 != AUDIO_RATE {
        bail!("sound at {} Hz instead of {AUDIO_RATE}", asbd.mSampleRate);
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let frames = frames.max(0) as usize;
    let floats: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])).collect();
    if floats.len() < frames * channels {
        bail!("short sound sample");
    }
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let at = |frame: usize, ch: usize| {
        let ch = ch.min(channels - 1);
        if planar {
            floats[ch * frames + frame]
        } else {
            floats[frame * channels + ch]
        }
    };
    out.reserve(frames * AUDIO_CHANNELS);
    for f in 0..frames {
        for ch in 0..AUDIO_CHANNELS {
            out.push((at(f, ch).clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
    Ok(())
}

impl AudioSource for SckAudio {
    fn name(&self) -> String {
        "ScreenCaptureKit system sound".into()
    }

    fn read(&mut self, out: &mut Vec<i16>) -> Result<()> {
        let (lock, ready) = &*self.queue;
        let q = lock.lock().unwrap();
        let (mut q, _) = ready.wait_timeout_while(q, Duration::from_millis(100), |q| q.pcm.is_empty() && q.stopped.is_none()).unwrap();
        if let Some(why) = q.stopped.take() {
            bail!("{why}");
        }
        out.append(&mut q.pcm);
        Ok(())
    }
}
