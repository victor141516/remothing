//! Simulated displays: Xvfb instances started and stopped by this process.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use remothing_core::platform::{DisplayInfo, DisplayKind, VirtualDisplays};

const FIRST_DISPLAY: u32 = 10;

pub struct XvfbInstance {
    pub num: u32,
    pub width: u32,
    pub height: u32,
    pub shmid: i32,
    children: Mutex<Vec<Child>>,
}

impl XvfbInstance {
    pub fn display(&self) -> String {
        format!(":{}", self.num)
    }

    pub fn info(&self) -> DisplayInfo {
        DisplayInfo {
            id: format!("xvfb:{}", self.num),
            name: format!("Xvfb :{}", self.num),
            width: self.width,
            height: self.height,
            // No scanout: frames are captured as fast as the apps draw.
            refresh_hz: 0.0,
            x: 0,
            y: 0,
            primary: false,
            kind: DisplayKind::Virtual,
        }
    }

    fn kill(&self) {
        for mut c in self.children.lock().unwrap().drain(..).rev() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for XvfbInstance {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct XvfbManager {
    instances: Mutex<BTreeMap<u32, Arc<XvfbInstance>>>,
    /// Started inside each new display (window manager, app...), via `sh -c`.
    session_command: Option<String>,
}

/// Children die with the server instead of lingering.
fn die_with_parent(cmd: &mut Command) {
    // SAFETY: prctl is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

impl XvfbManager {
    pub fn new(session_command: Option<String>) -> Self {
        Self { instances: Mutex::new(BTreeMap::new()), session_command }
    }

    pub fn list(&self) -> Vec<Arc<XvfbInstance>> {
        self.instances.lock().unwrap().values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<XvfbInstance>> {
        let num: u32 = id.strip_prefix("xvfb:")?.parse().ok()?;
        self.instances.lock().unwrap().get(&num).cloned()
    }

    fn free_number(&self) -> u32 {
        let used = self.instances.lock().unwrap();
        (FIRST_DISPLAY..)
            .find(|n| !used.contains_key(n) && !std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists())
            .expect("display numbers exhausted")
    }

    fn start(&self, width: u32, height: u32) -> Result<Arc<XvfbInstance>> {
        let num = self.free_number();
        let mut cmd = Command::new("Xvfb");
        cmd.args([format!(":{num}"), "-screen".into(), "0".into(), format!("{width}x{height}x24")])
            .args(["-shmem", "-nolisten", "tcp", "-noreset"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        die_with_parent(&mut cmd);
        let mut xvfb = cmd.spawn().context("start Xvfb (is it installed?)")?;

        // Xvfb prints "screen 0 shmid N"; keep draining stderr afterwards.
        let stderr = xvfb.stderr.take().context("Xvfb stderr")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(id) = line.split("shmid ").nth(1).and_then(|s| s.trim().parse::<i32>().ok()) {
                    let _ = tx.send(id);
                }
            }
        });
        let shmid = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(id) => id,
            Err(_) => {
                let _ = xvfb.kill();
                let _ = xvfb.wait();
                bail!("Xvfb :{num} did not report its shmid");
            }
        };
        // From here on, dropping the instance stops Xvfb.
        let instance = Arc::new(XvfbInstance { num, width, height, shmid, children: Mutex::new(vec![xvfb]) });

        let deadline = Instant::now() + Duration::from_secs(10);
        while x11rb::connect(Some(&instance.display())).is_err() {
            if Instant::now() > deadline {
                bail!("Xvfb :{num} did not accept connections");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(command) = &self.session_command {
            let mut session = Command::new("sh");
            session.args(["-c", command]).env("DISPLAY", instance.display()).stdin(Stdio::null());
            die_with_parent(&mut session);
            let child = session.spawn().with_context(|| format!("start session command {command:?}"))?;
            instance.children.lock().unwrap().push(child);
        }
        eprintln!("started Xvfb :{num} ({width}x{height})");
        Ok(instance)
    }
}

impl VirtualDisplays for XvfbManager {
    fn create(&self, width: u32, height: u32) -> Result<DisplayInfo> {
        let instance = self.start(width, height)?;
        let info = instance.info();
        self.instances.lock().unwrap().insert(instance.num, instance);
        Ok(info)
    }

    fn destroy(&self, id: &str) -> Result<()> {
        let num: u32 = id.strip_prefix("xvfb:").and_then(|n| n.parse().ok()).context("not a virtual display id")?;
        let instance = self.instances.lock().unwrap().remove(&num).context("no such virtual display")?;
        // Kill now even if a capture session still holds a reference; it then
        // fails and the capture loop falls back to another display.
        instance.kill();
        Ok(())
    }
}
