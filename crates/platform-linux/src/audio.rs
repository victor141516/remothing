//! Sound of the session, best effort. Linux has no one place where "what
//! plays" can be read, so every path is tried in turn, and what each one
//! answered is kept for the log:
//!
//! 1. A sound server that speaks the PulseAudio protocol (PulseAudio, or
//!    PipeWire through pipewire-pulse), ours or the user's: `parec` records
//!    the monitor of its default sink.
//! 2. PipeWire itself: `pw-record` records its default sink.
//! 3. An ALSA loopback card (snd-aloop): `arecord` records its other end.
//!
//! Where no sound server runs at all (a container, a bare X server), one is
//! started with the platform: PulseAudio with a null sink, or PipeWire with
//! WirePlumber and pipewire-pulse where only those are installed. The
//! sessions started in simulated displays get its address (`PULSE_SERVER`),
//! so their programs play into it.
//!
//! The recorders are helper programs writing raw samples to a pipe: each is
//! a few lines here, they fail with a message worth logging, and none of
//! the sound libraries has to be linked.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use remothing_core::platform::{AudioSource, AUDIO_CHANNELS, AUDIO_RATE};

use crate::xvfb::die_with_parent;

/// A recorder that is still running after this long without having
/// written anything is waiting for sound, not failing.
const START: Duration = Duration::from_millis(1500);
const SERVER_START: Duration = Duration::from_secs(5);
const WAIT: Duration = Duration::from_millis(100);
/// What is kept of a helper's error output.
const STDERR_KEPT: usize = 600;

/// The sound server started here, if any, and what was found instead.
pub struct SoundServer {
    /// `PULSE_SERVER` of our own server.
    address: Option<String>,
    /// Runtime directory of our own PipeWire (`XDG_RUNTIME_DIR` for its tools).
    pipewire_dir: Option<PathBuf>,
    /// What was found and started, for the log.
    notes: Vec<String>,
    daemons: Vec<Child>,
}

impl Drop for SoundServer {
    fn drop(&mut self) {
        for daemon in &mut self.daemons {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

impl SoundServer {
    /// Looks for a sound server and starts one when there is none.
    pub fn start() -> Self {
        let mut server = Self { address: None, pipewire_dir: None, notes: Vec::new(), daemons: Vec::new() };
        let found: Vec<String> = pulse_servers().into_iter().map(|(address, _)| address).collect();
        let pipewire = runtime_dirs().into_iter().filter(|d| connects(&d.join("pipewire-0"))).count();
        if !found.is_empty() || pipewire > 0 {
            server.notes.push(format!(
                "sound server found (PulseAudio protocol: {}; PipeWire sockets: {pipewire})",
                if found.is_empty() { "none".into() } else { found.join(", ") }
            ));
        } else if let Err(pulse) = server.start_pulseaudio() {
            server.notes.push(format!("own PulseAudio: {pulse:#}"));
            if let Err(pipewire) = server.start_pipewire() {
                server.notes.push(format!("own PipeWire: {pipewire:#}"));
                // Children of a failed start.
                for mut daemon in server.daemons.drain(..) {
                    let _ = daemon.kill();
                    let _ = daemon.wait();
                }
            }
        }
        for note in &server.notes {
            eprintln!("sound: {note}");
        }
        server
    }

    /// For the programs of the sessions we start.
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    fn start_pulseaudio(&mut self) -> Result<()> {
        let dir = own_dir()?;
        let socket = dir.join("native");
        let log = dir.join("pulseaudio.log");
        let mut cmd = Command::new("pulseaudio");
        // No default configuration (it looks for hardware and a session
        // bus): only the socket and a sink that goes nowhere.
        cmd.args(["-n", "--daemonize=no", "--exit-idle-time=-1", "--use-pid-file=no", "--disable-shm=yes"])
            .args(["--high-priority=no", "--realtime=no", "--log-target=stderr", "--log-level=error"])
            .arg("-L")
            .arg(format!("module-native-protocol-unix socket={} auth-anonymous=1", socket.display()))
            .args(["-L", "module-null-sink sink_name=remothing sink_properties=device.description=remothing"])
            .env("XDG_RUNTIME_DIR", &dir);
        self.daemons.push(spawn_daemon(cmd, &log)?);
        wait_for_socket(&socket, &log)?;
        self.notes.push(format!("no sound server found: started PulseAudio with a null sink at {}", socket.display()));
        self.address = Some(format!("unix:{}", socket.display()));
        Ok(())
    }

    fn start_pipewire(&mut self) -> Result<()> {
        let dir = own_dir()?;
        let log = dir.join("pipewire.log");
        // WirePlumber, which connects the programs to the sink, gives up
        // without a D-Bus session bus.
        let bus = match std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|a| !a.is_empty()) {
            Some(address) => address,
            None => {
                let socket = dir.join("bus");
                let address = format!("unix:path={}", socket.display());
                let mut cmd = Command::new("dbus-daemon");
                cmd.args(["--session", "--nofork", "--nopidfile", &format!("--address={address}")]);
                self.daemons.push(spawn_daemon(cmd, &log)?);
                wait_for_socket(&socket, &log)?;
                address
            }
        };
        for program in ["pipewire", "wireplumber", "pipewire-pulse"] {
            let mut cmd = Command::new(program);
            cmd.env("XDG_RUNTIME_DIR", &dir).env("DBUS_SESSION_BUS_ADDRESS", &bus);
            self.daemons.push(spawn_daemon(cmd, &log)?);
            if program == "pipewire" {
                wait_for_socket(&dir.join("pipewire-0"), &log)?;
            }
        }
        let socket = dir.join("pulse/native");
        wait_for_socket(&socket, &log)?;
        // Without hardware there is no sink: one that goes nowhere.
        let sink = "{ factory.name=support.null-audio-sink node.name=remothing node.description=remothing \
                    media.class=Audio/Sink object.linger=true audio.position=[FL FR] }";
        let created = Command::new("pw-cli").args(["create-node", "adapter", sink]).env("XDG_RUNTIME_DIR", &dir).output();
        match created {
            Ok(out) if out.status.success() => {}
            Ok(out) => bail!("pw-cli create-node: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => bail!("pw-cli: {e}"),
        }
        self.notes.push(format!("no sound server found: started PipeWire with a null sink in {}", dir.display()));
        self.address = Some(format!("unix:{}", socket.display()));
        self.pipewire_dir = Some(dir);
        Ok(())
    }

    /// Opens the first capture path that works; `tried` gets all of them.
    pub fn open(&self, tried: &mut Vec<String>) -> Result<Box<dyn AudioSource>> {
        tried.extend(self.notes.iter().cloned());
        let rate = AUDIO_RATE.to_string();
        let channels = AUDIO_CHANNELS.to_string();

        let servers = match &self.address {
            Some(address) => vec![(address.clone(), None)],
            None => pulse_servers(),
        };
        if servers.is_empty() {
            tried.push("PulseAudio protocol: no server".into());
        }
        for (address, cookie) in servers {
            let mut cmd = Command::new("parec");
            cmd.args(["--device=@DEFAULT_MONITOR@", "--format=s16le", "--latency-msec=20", "--client-name=remothing"])
                .args([format!("--rate={rate}"), format!("--channels={channels}")])
                .env("PULSE_SERVER", &address);
            if let Some(cookie) = cookie {
                cmd.env("PULSE_COOKIE", cookie);
            }
            if let Some(recorder) = Recorder::start(format!("monitor of the default sink of {address} (parec)"), cmd, tried) {
                return Ok(Box::new(recorder));
            }
        }

        let dirs = match &self.pipewire_dir {
            Some(dir) => vec![dir.clone()],
            None => runtime_dirs().into_iter().filter(|d| connects(&d.join("pipewire-0"))).collect(),
        };
        if dirs.is_empty() {
            tried.push("PipeWire: no server".into());
        }
        for dir in dirs {
            // PipeWire 1.0 writes plain samples to a pipe and knows no
            // `--raw`; where the option exists, it asks for just that.
            for option in [Some("--raw"), None] {
                let mut cmd = Command::new("pw-record");
                cmd.args(option)
                    .args(["--format=s16", "--latency=20ms", "-P", "{ stream.capture.sink=true }"])
                    .args([format!("--rate={rate}"), format!("--channels={channels}")])
                    .arg("-")
                    .env("XDG_RUNTIME_DIR", &dir);
                let tool = option.map_or("pw-record".into(), |o| format!("pw-record {o}"));
                let name = format!("default sink of PipeWire in {} ({tool})", dir.display());
                if let Some(recorder) = Recorder::start(name, cmd, tried) {
                    return Ok(Box::new(recorder));
                }
            }
        }

        if Path::new("/proc/asound/Loopback").exists() {
            let mut cmd = Command::new("arecord");
            cmd.args(["-q", "-D", "hw:Loopback,1,0", "-t", "raw", "-f", "S16_LE", "--buffer-time=40000"])
                .args(["-r", &rate, "-c", &channels]);
            if let Some(recorder) = Recorder::start("ALSA loopback card (arecord)".into(), cmd, tried) {
                return Ok(Box::new(recorder));
            }
        } else {
            tried.push("ALSA loopback: no snd-aloop card".into());
        }
        Err(anyhow!("no way to capture the session's sound"))
    }
}

/// A directory for our own sound server that every user can reach.
fn own_dir() -> Result<PathBuf> {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let candidates = [
        Some(PathBuf::from("/run/remothing/sound")),
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("remothing-sound")),
        Some(std::env::temp_dir().join(format!("remothing-sound-{uid}"))),
    ];
    for dir in candidates.into_iter().flatten() {
        if std::fs::create_dir_all(&dir).is_ok() {
            return Ok(dir);
        }
    }
    bail!("no writable directory for its socket")
}

/// Starts a daemon with its messages in `log`. Its sockets are for every
/// user: sessions may run as another one than the server.
fn spawn_daemon(mut cmd: Command, log: &Path) -> Result<Child> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(out);
    die_with_parent(&mut cmd);
    // SAFETY: umask is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!("{name} is not installed"),
        _ => anyhow!("start {name}: {e}"),
    })
}

fn wait_for_socket(socket: &Path, log: &Path) -> Result<()> {
    let deadline = Instant::now() + SERVER_START;
    while !connects(socket) {
        if Instant::now() > deadline {
            let said = std::fs::read_to_string(log).unwrap_or_default();
            bail!("{} did not come up: {}", socket.display(), tail(&said));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn connects(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text.char_indices().rev().nth(STDERR_KEPT).map_or(0, |(i, _)| i);
    text[start..].replace('\n', " | ")
}

/// Runtime directories where a user's sound server keeps its sockets: ours
/// first, then every logged-in user's (a server run as root streams the
/// desktop of another user).
fn runtime_dirs() -> Vec<PathBuf> {
    let own = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = own.iter().cloned().collect();
    if let Ok(users) = std::fs::read_dir("/run/user") {
        dirs.extend(users.flatten().map(|u| u.path()).filter(|p| Some(p) != own.as_ref()));
    }
    dirs
}

/// PulseAudio-protocol servers that accept a connection: (`PULSE_SERVER`
/// address, authentication cookie when it is another user's).
fn pulse_servers() -> Vec<(String, Option<PathBuf>)> {
    let mut servers = Vec::new();
    if let Some(address) = std::env::var("PULSE_SERVER").ok().filter(|a| !a.is_empty()) {
        servers.push((address, None));
    }
    for dir in runtime_dirs() {
        let socket = dir.join("pulse/native");
        if !connects(&socket) {
            continue;
        }
        // /run/user/<uid>: that user's cookie is in their home directory.
        let cookie = dir.file_name().and_then(|uid| home_of(&uid.to_string_lossy())).map(|home| home.join(".config/pulse/cookie"));
        servers.push((format!("unix:{}", socket.display()), cookie.filter(|c| c.exists())));
    }
    servers
}

fn home_of(uid: &str) -> Option<PathBuf> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.get(2) == Some(&uid))?.get(5).map(PathBuf::from)
}

/// A helper program writing interleaved 16-bit samples to its output.
struct Recorder {
    name: String,
    child: Child,
    bytes: mpsc::Receiver<Vec<u8>>,
    /// The first byte of a sample whose second one has not arrived.
    half: Option<u8>,
    stderr: Arc<Mutex<String>>,
}

impl Recorder {
    /// Starts the helper; None (with the reason in `tried`) when it does not record.
    fn start(name: String, mut cmd: Command, tried: &mut Vec<String>) -> Option<Self> {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
        let (mut out, mut err) = (child.stdout.take()?, child.stderr.take()?);
        let (tx, bytes) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = out.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let kept = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok(n @ 1..) = err.read(&mut buf) {
                let mut kept = kept.lock().unwrap();
                if kept.len() < STDERR_KEPT {
                    kept.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        let mut recorder = Self { name, child, bytes, half: None, stderr };
        // Samples, or a helper that keeps running, mean it records.
        let mut first = Vec::new();
        match recorder.take(&mut first, START) {
            Ok(()) => {
                tried.push(format!("{}: ok", recorder.name));
                // What it recorded while being judged is old.
                Some(recorder)
            }
            Err(e) => {
                tried.push(format!("{}: {e:#}", recorder.name));
                None
            }
        }
    }

    /// Appends the samples that arrive within `wait` (none is fine); the
    /// helper having ended is the error.
    fn take(&mut self, out: &mut Vec<i16>, wait: Duration) -> Result<()> {
        let mut chunk = match self.bytes.recv_timeout(wait) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                let status = self.child.wait().map_or_else(|e| e.to_string(), |s| s.to_string());
                // A helper says what is wrong first; a usage text may follow.
                let said = self.stderr.lock().unwrap().lines().take(2).collect::<Vec<_>>().join(" | ");
                bail!("ended ({status}): {said}");
            }
        };
        loop {
            let mut bytes = self.half.take().into_iter().chain(chunk);
            while let Some(low) = bytes.next() {
                match bytes.next() {
                    Some(high) => out.push(i16::from_le_bytes([low, high])),
                    None => self.half = Some(low),
                }
            }
            match self.bytes.try_recv() {
                Ok(more) => chunk = more,
                Err(_) => return Ok(()),
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AudioSource for Recorder {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, out: &mut Vec<i16>) -> Result<()> {
        self.take(out, WAIT)
    }
}
