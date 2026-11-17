//! remothing: one native binary per platform; providers chosen by `cfg(target_os)`.

const USAGE: &str = "\
usage:
  remothing [--config PATH] [--display ID] [--virtual WxH] [--session-command CMD]   run the server
  remothing [--config PATH] set-admin-password PASSWORD
  remothing [--config PATH] service install|uninstall|start|stop|run   (Windows; macOS: install|uninstall|start|stop)";

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use remothing_core::config::{hash_password, ConfigFile};
use remothing_core::encode::EncoderProvider;
use remothing_core::platform::Platform;

fn default_config_path() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        PathBuf::from(base).join("remothing").join("config.toml")
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return PathBuf::from("/etc/remothing/config.toml");
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("remothing").join("config.toml")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join("Library/Application Support/remothing/config.toml")
    }
}

fn main() -> Result<()> {
    let mut config_path = default_config_path();
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut command: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--config" => config_path = value()?.into(),
            "--virtual" => overrides.push(("virtual_on_start".into(), value()?)),
            "--session-command" => overrides.push(("session_command".into(), value()?)),
            "--display" => overrides.push(("display".into(), value()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if arg.starts_with("--") => bail!("unknown flag {arg}\n{USAGE}"),
            _ => {
                command.push(arg);
                command.extend(it.by_ref());
            }
        }
    }

    match command.first().map(String::as_str) {
        None => {}
        Some("set-admin-password") => {
            let pw = command.get(1).context("set-admin-password PASSWORD")?;
            let (file, mut settings) = ConfigFile::load_or_create(&config_path)?;
            settings.auth.admin_password = hash_password(pw);
            file.save(&settings)?;
            println!("admin password updated in {}", config_path.display());
            return Ok(());
        }
        #[cfg(windows)]
        Some("service") => {
            let sub = command.get(1).context("service install|uninstall|start|stop|run")?;
            return remothing_platform_windows::service::command(sub, &config_path);
        }
        #[cfg(target_os = "macos")]
        Some("service") => {
            let sub = command.get(1).context("service install|uninstall|start|stop")?;
            return remothing_platform_macos::service::command(sub, &config_path);
        }
        Some(other) => bail!("unknown command {other}"),
    }

    let (file, mut settings) = ConfigFile::load_or_create(&config_path)?;
    for (k, v) in overrides {
        if k == "display" {
            settings.server.display = v;
        } else {
            settings.platform.insert(k, toml::Value::String(v));
        }
    }
    eprintln!("config: {}", config_path.display());
    let encoders = Box::new(remothing_encoder::FfmpegEncoders::new()?);
    let platform: Arc<dyn Platform> = build_platform(&settings.platform, encoders)?;
    // CoreGraphics needs the main thread's run loop (`with_main_run_loop`).
    #[cfg(target_os = "macos")]
    return remothing_platform_macos::with_main_run_loop(move || remothing_core::app::run(platform, file, settings));
    #[cfg(not(target_os = "macos"))]
    remothing_core::app::run(platform, file, settings)
}

#[cfg(target_os = "linux")]
fn build_platform(table: &toml::Table, encoders: Box<dyn EncoderProvider>) -> Result<Arc<dyn Platform>> {
    use remothing_platform_linux::{LinuxOptions, LinuxPlatform};
    Ok(Arc::new(LinuxPlatform::new(LinuxOptions::from_table(table)?, encoders)?))
}

#[cfg(windows)]
fn build_platform(table: &toml::Table, encoders: Box<dyn EncoderProvider>) -> Result<Arc<dyn Platform>> {
    Ok(Arc::new(remothing_platform_windows::WindowsPlatform::new(table, encoders)?))
}

#[cfg(target_os = "macos")]
fn build_platform(_table: &toml::Table, encoders: Box<dyn EncoderProvider>) -> Result<Arc<dyn Platform>> {
    Ok(Arc::new(remothing_platform_macos::MacPlatform::new(encoders)?))
}
