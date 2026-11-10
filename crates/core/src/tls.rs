//! TLS certificates: the user's PEM files, or a self-signed one generated
//! here so a browser can reach the server directly over HTTPS (WebCodecs and
//! the clipboard need a secure context, which plain HTTP only is on
//! localhost). The self-signed one is kept next to the config file: a browser
//! exception accepted once stays valid across restarts.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};

use crate::config::TlsSettings;

/// Validity of a generated certificate; browsers cap even accepted
/// certificates at roughly this. Renewed a month before it runs out.
const SELF_SIGNED_DAYS: i64 = 397;
const SELF_SIGNED_RENEW: Duration = Duration::from_secs(365 * 24 * 3600);

/// Certificate chain and key (PEM) to serve, or None for plain HTTP.
pub fn load(settings: &TlsSettings, config_dir: &Path) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    if !settings.cert.is_empty() && !settings.key.is_empty() {
        let cert = std::fs::read(&settings.cert).with_context(|| format!("read {}", settings.cert))?;
        let key = std::fs::read(&settings.key).with_context(|| format!("read {}", settings.key))?;
        return Ok(Some((cert, key)));
    }
    if !settings.self_signed {
        if !settings.listen.is_empty() {
            bail!("tls.listen needs tls.cert and tls.key, or tls.self_signed = true");
        }
        return Ok(None);
    }
    let dir = config_dir.join("tls");
    let cert_path = dir.join("self-signed-cert.pem");
    let key_path = dir.join("self-signed-key.pem");
    let age = std::fs::metadata(&cert_path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    if !key_path.exists() || age.is_none_or(|a| a > SELF_SIGNED_RENEW) {
        generate(&dir, &cert_path, &key_path)?;
    }
    let cert = std::fs::read(&cert_path).with_context(|| format!("read {}", cert_path.display()))?;
    let key = std::fs::read(&key_path).with_context(|| format!("read {}", key_path.display()))?;
    // The fingerprint lets a user check the certificate a browser warns about.
    let der = CertificateDer::from_pem_slice(&cert).context("parse self-signed certificate")?;
    let fingerprint: Vec<String> = Sha256::digest(&der).iter().map(|b| format!("{b:02X}")).collect();
    eprintln!("tls: self-signed certificate {} (SHA-256 {})", cert_path.display(), fingerprint.join(":"));
    Ok(Some((cert, key)))
}

/// Writes a new certificate for the names a browser may use for this machine:
/// localhost, the host name and every local address (a public address behind
/// NAT is unknown here; the browser then warns about the name too, which the
/// same exception covers).
fn generate(dir: &Path, cert_path: &Path, key_path: &Path) -> Result<()> {
    let mut names = vec!["localhost".to_string()];
    names.extend(sysinfo::System::host_name());
    names.extend(if_addrs::get_if_addrs().context("list interfaces")?.into_iter().map(|i| i.ip().to_string()));
    names.sort();
    names.dedup();
    let mut params = rcgen::CertificateParams::new(names)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "remothing");
    let now = time::OffsetDateTime::now_utc();
    // A day of slack for clients whose clock is behind.
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(SELF_SIGNED_DAYS);
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_private(key_path, key.serialize_pem().as_bytes())?;
    std::fs::write(cert_path, cert.pem()).with_context(|| format!("write {}", cert_path.display()))?;
    Ok(())
}

/// The key is readable by the owner only (the config next to it holds tokens
/// and is protected the same way by its directory on Windows).
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("write {}", path.display()))?;
    file.write_all(data).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
