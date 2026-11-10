//! A client's microphone as the session's microphone. The browser sends what
//! it records (Opus, or plain samples where it cannot encode), and it is
//! played into the platform's virtual microphone, which programs of the
//! session record from.
//!
//! One client talks at a time, and the server decides who: the first
//! request to arrive. The others see the microphone as taken until it is
//! released. Best effort like the sound going the other way: the talker is
//! told what was tried when there is no way, and what would have to be
//! installed when that is all that is missing.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::encode::AudioDecoder;
use crate::platform::{AudioSink, MicUnavailable, Platform, AUDIO_CHANNELS, AUDIO_RATE};
use crate::proto;
use crate::telemetry::Telemetry;

/// The microphone gets one piece of this length per tick of the server's
/// clock, the client's sound when there is some and silence otherwise: the
/// client's clock runs a little faster or slower, and a device fed evenly
/// never runs dry.
const TICK: Duration = Duration::from_millis(10);
const TICK_SAMPLES: usize = AUDIO_RATE as usize / 100 * AUDIO_CHANNELS;
/// Queued before the client's sound starts or resumes: room for packets that arrive unevenly.
const LEAD_SAMPLES: usize = 6 * TICK_SAMPLES;
/// More than this queued came in a burst after a stall (or the client's
/// clock is faster): the oldest is dropped rather than heard late for good.
const MAX_SAMPLES: usize = 25 * TICK_SAMPLES;
/// A stalled thread does not play everything it missed at once.
const MAX_BEHIND: Duration = Duration::from_millis(100);

/// What the talker's connection passes on.
pub enum Command {
    /// A C_MIC message: tag byte, then one packet.
    Packet(Vec<u8>),
    /// The user agreed to install the missing component.
    Install,
    /// The user agreed to restart the computer.
    Restart,
}

/// How the client sends its microphone (C_MIC_INFO).
#[derive(Deserialize)]
pub struct Format {
    /// "opus", or "pcm": signed 16-bit little-endian samples.
    pub codec: String,
    pub rate: u32,
    pub channels: usize,
}

pub struct MicHub {
    platform: Arc<dyn Platform>,
    telemetry: Arc<Telemetry>,
    /// The client that has the microphone.
    talker: watch::Sender<Option<u64>>,
    /// For the admin UI.
    status: Mutex<String>,
}

impl MicHub {
    pub fn new(platform: Arc<dyn Platform>, telemetry: Arc<Telemetry>) -> Arc<Self> {
        Arc::new(Self { platform, telemetry, talker: watch::channel(None).0, status: Mutex::new("free".into()) })
    }

    /// Who talks; every client follows it to show the microphone as free or taken.
    pub fn talker(&self) -> watch::Receiver<Option<u64>> {
        self.talker.subscribe()
    }

    /// Client `id` has the microphone right now.
    pub fn talks(&self, id: u64) -> bool {
        *self.talker.borrow() == Some(id)
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    /// Gives client `id` the microphone if nobody has it. Its state
    /// (S_MIC_INFO) goes to `reply`; it is released when the returned
    /// sender is dropped. None: somebody else talks.
    pub fn start(self: &Arc<Self>, id: u64, format: Format, reply: UnboundedSender<Vec<u8>>) -> Option<mpsc::Sender<Command>> {
        let taken = self.talker.send_if_modified(|talker| {
            let free = talker.is_none();
            if free {
                *talker = Some(id);
            }
            free
        });
        if !taken {
            return None;
        }
        let (tx, commands) = mpsc::channel();
        let hub = self.clone();
        std::thread::Builder::new()
            .name("microphone".into())
            .spawn(move || {
                hub.run(id, &format, &commands, &reply);
                *hub.status.lock().unwrap() = "free".into();
                hub.talker.send_replace(None);
            })
            .expect("spawn microphone thread");
        Some(tx)
    }

    fn run(&self, id: u64, format: &Format, commands: &mpsc::Receiver<Command>, reply: &UnboundedSender<Vec<u8>>) {
        let say = |state: serde_json::Value| {
            let _ = reply.send(proto::text(proto::S_MIC_INFO, &state.to_string()));
        };
        let mut decoder = match Decoder::new(self.platform.as_ref(), format) {
            Ok(decoder) => decoder,
            Err(e) => {
                say(json!({ "state": "unavailable", "reason": format!("{e:#}") }));
                return;
            }
        };
        let Ok(mut sink) = self.open(id, &say) else { return };
        let mut queue: Vec<i16> = Vec::new();
        let mut playing = false;
        let mut next = Instant::now() + TICK;
        loop {
            match commands.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Command::Packet(packet)) => {
                    // Until the microphone is open there is nowhere to put it.
                    if sink.is_some() {
                        if let Err(e) = decoder.decode(&packet[1..], &mut queue) {
                            say(json!({ "state": "unavailable", "reason": format!("cannot decode the microphone: {e:#}") }));
                            return;
                        }
                    }
                }
                Ok(Command::Install) => {
                    say(json!({ "state": "installing" }));
                    if let Err(e) = self.platform.install_microphone() {
                        self.telemetry.error("microphone install", &e);
                        say(json!({ "state": "unavailable", "reason": format!("the installation failed: {e:#}") }));
                        return;
                    }
                    match self.open(id, &say) {
                        Ok(opened) => sink = opened,
                        Err(()) => return,
                    }
                }
                Ok(Command::Restart) => {
                    self.telemetry.event(id, "restart of the computer requested (microphone component)");
                    if let Err(e) = self.platform.restart() {
                        self.telemetry.error("restart", e);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if now > next + MAX_BEHIND {
                next = now;
            }
            while next <= now {
                next += TICK;
                let Some(sink) = &mut sink else { continue };
                if queue.len() > MAX_SAMPLES {
                    queue.drain(..queue.len() - LEAD_SAMPLES);
                }
                playing = if playing { queue.len() >= TICK_SAMPLES } else { queue.len() >= LEAD_SAMPLES };
                let written = match playing {
                    true => sink.write(&queue[..TICK_SAMPLES]),
                    false => sink.write(&[0; TICK_SAMPLES]),
                };
                if let Err(e) = written {
                    self.telemetry.error("microphone", &e);
                    say(json!({ "state": "unavailable", "reason": format!("{} stopped: {e:#}", sink.name()) }));
                    return;
                }
                if playing {
                    queue.drain(..TICK_SAMPLES);
                }
            }
        }
    }

    /// Opens the virtual microphone and tells the talker how it went.
    /// Ok(None): something has to be installed or restarted first, and the
    /// talker was asked. Err: there is no way.
    fn open(&self, id: u64, say: &dyn Fn(serde_json::Value)) -> Result<Option<Box<dyn AudioSink>>, ()> {
        let mut tried = Vec::new();
        match self.platform.open_microphone(&mut tried) {
            Ok(sink) => {
                let status = format!("client {id} -> {}", sink.name());
                eprintln!("microphone: {status} (tried: {})", tried.join("; "));
                *self.status.lock().unwrap() = status;
                say(json!({ "state": "active", "sink": sink.name() }));
                Ok(Some(sink))
            }
            Err(MicUnavailable::Install(component)) => {
                *self.status.lock().unwrap() = format!("client {id} asked: needs {}", component.name);
                say(json!({ "state": "install", "component": component }));
                Ok(None)
            }
            Err(MicUnavailable::Restart) => {
                *self.status.lock().unwrap() = format!("client {id} asked: needs a restart");
                say(json!({ "state": "restart" }));
                Ok(None)
            }
            Err(MicUnavailable::Failed) => {
                self.telemetry.error("microphone unavailable", tried.join("; "));
                say(json!({ "state": "unavailable", "reason": "the server found no way to offer a microphone", "tried": tried }));
                Err(())
            }
        }
    }
}

/// Turns the client's packets into the platform's audio format.
enum Decoder {
    Opus(Box<dyn AudioDecoder>),
    /// Plain samples at a rate that divides ours: `step` output samples per
    /// input sample, on a line from the previous one (`last`, per channel).
    Pcm { channels: usize, step: i32, last: [i16; AUDIO_CHANNELS] },
}

impl Decoder {
    fn new(platform: &dyn Platform, format: &Format) -> Result<Self> {
        if !(1..=AUDIO_CHANNELS).contains(&format.channels) {
            bail!("a microphone with {} channels is not supported", format.channels);
        }
        match format.codec.as_str() {
            "opus" => Ok(Self::Opus(platform.encoders().opus_decoder(format.channels)?)),
            "pcm" => {
                if format.rate < 8000 || !AUDIO_RATE.is_multiple_of(format.rate) {
                    bail!("plain samples at {} Hz are not supported", format.rate);
                }
                Ok(Self::Pcm { channels: format.channels, step: (AUDIO_RATE / format.rate) as i32, last: [0; AUDIO_CHANNELS] })
            }
            other => bail!("microphone codec {other:?} is not supported"),
        }
    }

    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<()> {
        match self {
            Self::Opus(decoder) => decoder.decode(packet, out),
            Self::Pcm { channels, step, last } => {
                let samples: Vec<i16> = packet.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                for frame in samples.chunks_exact(*channels) {
                    // One channel goes to both sides.
                    let now = [frame[0], frame[*channels - 1]];
                    for k in 1..=*step {
                        for side in 0..AUDIO_CHANNELS {
                            let from = i32::from(last[side]);
                            out.push((from + (i32::from(now[side]) - from) * k / *step) as i16);
                        }
                    }
                    *last = now;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_samples_are_interpolated_to_our_rate() {
        let mut decoder = Decoder::Pcm { channels: 1, step: 3, last: [0; AUDIO_CHANNELS] };
        let mut out = Vec::new();
        let packet: Vec<u8> = [300i16, -300].iter().flat_map(|s| s.to_le_bytes()).collect();
        decoder.decode(&packet, &mut out).unwrap();
        // Three stereo frames per input sample, on a line between them.
        assert_eq!(out, [100, 100, 200, 200, 300, 300, 100, 100, -100, -100, -300, -300]);
    }
}
