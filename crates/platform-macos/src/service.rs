//! `remothing service install|uninstall|start|stop` on macOS: a launch agent
//! of the logged-in user, so the server starts with the session and
//! restarts if it exits. It has to be an agent in the user's session, not a
//! system daemon: ScreenCaptureKit and event posting work only there.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const LABEL: &str = "com.remothing.server";

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// launchd's name for the agent in this user's GUI session.
fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = Command::new("launchctl").args(args).output().context("run launchctl")?;
    if !out.status.success() {
        bail!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn install(config: &Path) -> Result<()> {
    let config = std::path::absolute(config).context("resolve config path")?;
    let exe = std::env::current_exe().context("locate current executable")?;
    let log = config.parent().context("config path has no directory")?.join("remothing.log");
    std::fs::create_dir_all(log.parent().unwrap())?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>--config</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        escape(&exe.to_string_lossy()),
        escape(&config.to_string_lossy()),
        escape(&log.to_string_lossy()),
        escape(&log.to_string_lossy()),
    );
    let path = plist_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    // A reinstall replaces a running agent (new binary or config path).
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    std::fs::write(&path, plist).with_context(|| format!("write {}", path.display()))?;
    launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    println!("installed and started {LABEL}; log: {}", log.display());
    println!("grant Screen & System Audio Recording and Accessibility to {} when macOS asks", exe.display());
    Ok(())
}

pub fn command(sub: &str, config: &Path) -> Result<()> {
    let target = format!("{}/{LABEL}", domain());
    match sub {
        "install" => install(config),
        "uninstall" => {
            let _ = launchctl(&["bootout", &target]);
            let path = plist_path()?;
            if path.exists() {
                std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            }
            println!("removed {LABEL}");
            Ok(())
        }
        "start" => launchctl(&["bootstrap", &domain(), &plist_path()?.to_string_lossy()]),
        "stop" => launchctl(&["bootout", &target]),
        other => bail!("unknown service command {other}: install|uninstall|start|stop"),
    }
}
