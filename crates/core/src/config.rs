//! Service configuration file (TOML). Created with random tokens on first run;
//! admin changes are written back.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::shared::Tuning;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: ServerSettings,
    pub tuning: Tuning,
    pub encoder: EncoderSettings,
    pub input: InputSettings,
    pub auth: AuthSettings,
    pub tls: TlsSettings,
    /// Platform-specific options (e.g. `[platform] session_command = "..."` on Linux).
    pub platform: toml::Table,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    /// Streaming client (HTTP + WebSocket).
    pub listen: String,
    /// Admin UI + API, separate port.
    pub admin_listen: String,
    pub tile: u32,
    /// Display streamed at startup (last selection); empty = primary / first.
    pub display: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderSettings {
    pub codec: String,
    /// "auto" or a backend name ("nvenc", "amf", "qsv", "x264").
    pub preferred: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    /// Applied where the platform supports it (Linux X displays).
    pub keyboard_layout: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// Bearer token for the admin API / login.
    pub admin_token: String,
    /// Optional admin password as "salt$sha256hex(salt + password)".
    pub admin_password: String,
    /// Required by streaming clients (`?token=`); empty disables stream auth.
    pub stream_token: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsSettings {
    /// PEM paths; when both are set every listener uses HTTPS/WSS.
    pub cert: String,
    pub key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: ServerSettings::default(),
            tuning: Tuning { bandwidth_mbps: 200.0, max_lossless_frame_kb: 96 },
            encoder: EncoderSettings::default(),
            input: InputSettings::default(),
            auth: AuthSettings::default(),
            tls: TlsSettings::default(),
            platform: toml::Table::new(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8080".into(), admin_listen: "127.0.0.1:8081".into(), tile: 64, display: String::new() }
    }
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { codec: "h264".into(), preferred: "auto".into() }
    }
}

impl Default for InputSettings {
    fn default() -> Self {
        Self { keyboard_layout: "us".into() }
    }
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self { admin_token: random_token(), admin_password: String::new(), stream_token: random_token() }
    }
}

pub fn random_token() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), 32)
}

pub fn hash_password(password: &str) -> String {
    let salt = Alphanumeric.sample_string(&mut rand::rng(), 16);
    format!("{salt}${}", sha_hex(&salt, password))
}

pub fn verify_password(stored: &str, password: &str) -> bool {
    match stored.split_once('$') {
        Some((salt, hash)) => constant_time_eq(sha_hex(salt, password).as_bytes(), hash.as_bytes()),
        None => false,
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn sha_hex(salt: &str, password: &str) -> String {
    let digest = Sha256::digest(format!("{salt}{password}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Config file bound to its path.
pub struct ConfigFile {
    pub path: PathBuf,
}

impl ConfigFile {
    /// Loads the file, creating it with defaults (fresh random tokens) when missing.
    pub fn load_or_create(path: &Path) -> Result<(Self, Settings)> {
        let file = Self { path: path.to_path_buf() };
        if path.exists() {
            let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
            let settings: Settings = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            Ok((file, settings))
        } else {
            let settings = Settings::default();
            file.save(&settings)?;
            eprintln!("created {} (admin token: {})", path.display(), settings.auth.admin_token);
            Ok((file, settings))
        }
    }

    pub fn save(&self, settings: &Settings) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(settings)?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("replace {}", self.path.display()))?;
        Ok(())
    }
}
