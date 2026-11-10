//! The session's virtual microphone: a source that programs record from,
//! fed with what a client says. Helper programs again, as for capture.
//!
//! With a PulseAudio-protocol server (PulseAudio or pipewire-pulse): a null
//! sink `remothing-mic-sink` and, over its monitor, the source
//! `remothing-mic` (a remapped source: many programs leave monitors out of
//! their microphone list); `pacat` plays into the sink. With PipeWire
//! alone: `pw-loopback` makes the same pair, a sink whose sound comes out
//! of a source, and `pw-cat` plays into the sink. (A single virtual source
//! node does not do: the session manager links players to sinks only.)
//!
//! In our own sound server the microphone is the default source, so
//! programs find it without being told; through the PulseAudio protocol it
//! is there from the start. In somebody else's server it is created when a
//! client starts talking and removed when they stop; through the
//! PulseAudio protocol it is also the default source meanwhile, and the
//! default goes back to what it was afterwards.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use remothing_core::platform::{AudioSink, MicUnavailable, AUDIO_CHANNELS, AUDIO_RATE};

use crate::audio::{connects, pulse_servers, runtime_dirs, SoundServer, OWN_SINK};
use crate::xvfb::die_with_parent;

const MIC_SINK: &str = "remothing-mic-sink";
const MIC_SOURCE: &str = "remothing-mic";
/// A player that is still running after this long was accepted by the server.
const START: Duration = Duration::from_millis(300);
/// How long `pw-loopback` may take to make its pair.
const PAIR_START: Duration = Duration::from_secs(2);

/// A PulseAudio-protocol server, as `pactl` and `pacat` are pointed at it.
struct Pulse {
    address: String,
    /// Another user's server wants that user's cookie.
    cookie: Option<PathBuf>,
}

impl Pulse {
    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("PULSE_SERVER", &self.address);
        if let Some(cookie) = &self.cookie {
            cmd.env("PULSE_COOKIE", cookie);
        }
        cmd
    }

    fn ctl(&self, args: &[&str]) -> Result<String> {
        let out = self.command("pactl").args(args).output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!("pactl is not installed (pulseaudio-utils)"),
            _ => anyhow!("pactl: {e}"),
        })?;
        if !out.status.success() {
            bail!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or_default());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Creates the sink and the source unless they are there; the modules loaded.
    fn create_microphone(&self) -> Result<Vec<String>> {
        let sources = self.ctl(&["list", "short", "sources"])?;
        if sources.lines().any(|l| l.split('\t').nth(1) == Some(MIC_SOURCE)) {
            return Ok(Vec::new());
        }
        let sink = self.ctl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={MIC_SINK}"),
            // Our rate: nothing to resample between the client and the programs.
            &format!("rate={AUDIO_RATE}"),
            "sink_properties=device.description=remothing-microphone-feed",
        ])?;
        let source = self.ctl(&[
            "load-module",
            "module-remap-source",
            &format!("master={MIC_SINK}.monitor"),
            &format!("source_name={MIC_SOURCE}"),
            "source_properties=device.description=remothing-microphone",
        ]);
        match source {
            Ok(source) => Ok(vec![source, sink]),
            Err(e) => {
                let _ = self.ctl(&["unload-module", &sink]);
                Err(e)
            }
        }
    }
}

/// Puts somebody else's sound server back as it was.
struct Undo {
    pulse: Pulse,
    default_source: String,
    modules: Vec<String>,
}

impl Drop for Undo {
    fn drop(&mut self) {
        let _ = self.pulse.ctl(&["set-default-source", &self.default_source]);
        for module in &self.modules {
            let _ = self.pulse.ctl(&["unload-module", module]);
        }
    }
}

/// The microphone of our own sound server, made with it: programs started
/// later find it as the default source. `sink`: where the session's sound
/// must keep going (a second sink is no reason to move it).
pub(crate) fn prepare(address: &str, sink: &str) -> Result<()> {
    let pulse = Pulse { address: address.to_string(), cookie: None };
    pulse.create_microphone()?;
    pulse.ctl(&["set-default-source", MIC_SOURCE])?;
    pulse.ctl(&["set-default-sink", sink])?;
    Ok(())
}

/// Opens the first way to feed a virtual microphone; `tried` gets all of them.
pub(crate) fn open(server: &SoundServer, tried: &mut Vec<String>) -> Result<Box<dyn AudioSink>, MicUnavailable> {
    let rate = AUDIO_RATE.to_string();
    let channels = AUDIO_CHANNELS.to_string();

    let servers = match server.address() {
        Some(address) => vec![(address.to_string(), None, true)],
        None => pulse_servers().into_iter().map(|(address, cookie)| (address, cookie, false)).collect(),
    };
    if servers.is_empty() {
        tried.push("PulseAudio protocol: no server".into());
    }
    for (address, cookie, own) in servers {
        let name = format!("source {MIC_SOURCE} of {address} (pacat)");
        let pulse = Pulse { address, cookie };
        // Ours has the microphone already and keeps it.
        let undo = if own {
            pulse.create_microphone().map(|_| None)
        } else {
            pulse.ctl(&["get-default-source"]).and_then(|default_source| {
                let modules = pulse.create_microphone()?;
                let undo = Undo { pulse: Pulse { address: pulse.address.clone(), cookie: pulse.cookie.clone() }, default_source, modules };
                pulse.ctl(&["set-default-source", MIC_SOURCE])?;
                Ok(Some(undo))
            })
        };
        let undo = match undo {
            Ok(undo) => undo,
            Err(e) => {
                tried.push(format!("{name}: {e:#}"));
                continue;
            }
        };
        let mut cmd = pulse.command("pacat");
        cmd.args(["--playback", "--format=s16le", "--latency-msec=20", "--client-name=remothing-microphone"])
            .args([format!("--device={MIC_SINK}"), format!("--rate={rate}"), format!("--channels={channels}")]);
        if let Some(player) = Player::start(name, cmd, undo, tried) {
            return Ok(Box::new(player));
        }
    }

    let dirs = match server.pipewire_dir() {
        Some(dir) => vec![dir.to_path_buf()],
        None => runtime_dirs().into_iter().filter(|d| connects(&d.join("pipewire-0"))).collect(),
    };
    if dirs.is_empty() {
        tried.push("PipeWire: no server".into());
    }
    for dir in dirs {
        let place = format!("source {MIC_SOURCE} of PipeWire in {}", dir.display());
        let tool = |program: &str| {
            let mut cmd = Command::new(program);
            cmd.env("XDG_RUNTIME_DIR", &dir);
            cmd
        };
        let mut cmd = tool("pw-loopback");
        cmd.arg(format!("--capture-props=media.class=Audio/Sink node.name={MIC_SINK} node.description=remothing-microphone-feed audio.position=[FL FR]"))
            .arg(format!("--playback-props=media.class=Audio/Source node.name={MIC_SOURCE} node.description=remothing-microphone audio.position=[FL FR]"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let mut loopback = match cmd.spawn() {
            Ok(child) => Some(Helper(child)),
            Err(e) => {
                let why = if e.kind() == std::io::ErrorKind::NotFound { "is not installed".into() } else { e.to_string() };
                tried.push(format!("{place}: pw-loopback {why}"));
                continue;
            }
        };
        // The pair takes a moment to exist; a player that comes first plays elsewhere.
        let listed = || tool("pw-cli").args(["ls", "Node"]).output().is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{MIC_SINK}\"")));
        let deadline = Instant::now() + PAIR_START;
        while !listed() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if server.pipewire_dir().is_some() {
            // Ours: programs record the microphone, and keep playing where they did.
            for (key, node) in [("default.configured.audio.source", MIC_SOURCE), ("default.configured.audio.sink", OWN_SINK)] {
                let _ = tool("pw-metadata").args(["0", key, &format!("{{ \"name\": \"{node}\" }}")]).output();
            }
        }
        // As with pw-record: `--raw` where the option exists.
        for option in [Some("--raw"), None] {
            let mut cmd = tool("pw-cat");
            cmd.arg("--playback")
                .args(option)
                .args(["--format=s16", "--latency=20ms", "--target", MIC_SINK])
                .args([format!("--rate={rate}"), format!("--channels={channels}")])
                .arg("-");
            let name = format!("{place} ({})", option.map_or("pw-cat".into(), |o| format!("pw-cat {o}")));
            if let Some(mut player) = Player::start(name, cmd, None, tried) {
                player._loopback = loopback.take();
                return Ok(Box::new(player));
            }
        }
    }
    Err(MicUnavailable::Failed)
}

/// A helper program that lives as long as this.
struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A helper program playing the samples written to its input.
struct Player {
    name: String,
    child: Child,
    input: ChildStdin,
    stderr: Arc<Mutex<String>>,
    /// Dropped (and so undone) after the player is gone.
    _undo: Option<Undo>,
    /// The sink-and-source pair the player plays into, where we made it.
    _loopback: Option<Helper>,
}

impl Player {
    /// Starts the helper; None (with the reason in `tried`) when it does not play.
    fn start(name: String, mut cmd: Command, undo: Option<Undo>, tried: &mut Vec<String>) -> Option<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        die_with_parent(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let program = cmd.get_program().to_string_lossy().into_owned();
                let why = if e.kind() == std::io::ErrorKind::NotFound { "is not installed".into() } else { e.to_string() };
                tried.push(format!("{name}: {program} {why}"));
                return None;
            }
        };
        let (input, mut err) = (child.stdin.take()?, child.stderr.take()?);
        let stderr = Arc::new(Mutex::new(String::new()));
        let kept = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok(n @ 1..) = err.read(&mut buf) {
                let mut kept = kept.lock().unwrap();
                if kept.len() < 600 {
                    kept.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        let mut player = Self { name, child, input, stderr, _undo: undo, _loopback: None };
        std::thread::sleep(START);
        match player.ended() {
            Some(why) => {
                tried.push(format!("{}: {why}", player.name));
                None
            }
            None => {
                tried.push(format!("{}: ok", player.name));
                Some(player)
            }
        }
    }

    /// Why the helper ended, if it did.
    fn ended(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok()??;
        // A helper says what is wrong first; a usage text may follow.
        let said = self.stderr.lock().unwrap().lines().take(2).collect::<Vec<_>>().join(" | ");
        Some(format!("ended ({status}): {said}"))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AudioSink for Player {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn write(&mut self, pcm: &[i16]) -> Result<()> {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        if self.input.write_all(&bytes).is_err() {
            bail!("{}", self.ended().unwrap_or_else(|| "its input closed".into()));
        }
        Ok(())
    }
}
