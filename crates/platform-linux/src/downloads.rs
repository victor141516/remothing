//! The Downloads folder of the session's user, where files a client sends
//! are saved: the one `~/.config/user-dirs.dirs` names (a localized name,
//! such as `~/Descargas`), else `~/Downloads`, created when missing. When
//! the server runs as root (the container images) the desktop usually runs
//! as another user: the folder is that user's, and what is saved there is
//! given to them.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Regular users' ids start here on every distribution; `nobody` is not one.
const FIRST_REGULAR_UID: u32 = 1000;
const NOBODY_UID: u32 = 65534;

pub fn downloads_dir() -> Result<PathBuf> {
    let home = session_home();
    let dir = xdg_download_dir(&home).unwrap_or_else(|| home.join("Downloads"));
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        give(&dir, &home)?;
    }
    Ok(dir)
}

/// A saved file becomes the owner of the folder it is in.
pub fn hand_over(path: &Path) -> Result<()> {
    give(path, path.parent().context("a saved file has a folder")?)
}

/// Gives `path` to the owner of `like` when that is another user (only root may).
fn give(path: &Path, like: &Path) -> Result<()> {
    let owner = std::fs::metadata(like).with_context(|| format!("read {}", like.display()))?;
    // SAFETY: a plain query of this process's identity.
    let root = unsafe { libc::geteuid() } == 0;
    if root && owner.uid() != 0 {
        std::os::unix::fs::chown(path, Some(owner.uid()), Some(owner.gid())).with_context(|| format!("give {} to its user", path.display()))?;
    }
    Ok(())
}

/// Our own home, or as root the home of the regular user (uid 1000 and up)
/// running the most processes: the desktop's. Reading what a process of
/// another user was started with would need CAP_SYS_PTRACE, which a
/// container's root does not have; who owns it is always readable.
fn session_home() -> PathBuf {
    let own = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"));
    // SAFETY: a plain query of this process's identity.
    if unsafe { libc::geteuid() } != 0 {
        return own;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else { return own };
    let mut running: HashMap<u32, usize> = HashMap::new();
    for p in procs.flatten().filter(|p| p.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())) {
        if let Ok(meta) = p.metadata() {
            if (FIRST_REGULAR_UID..NOBODY_UID).contains(&meta.uid()) {
                *running.entry(meta.uid()).or_default() += 1;
            }
        }
    }
    let Some((uid, _)) = running.into_iter().max_by_key(|&(_, n)| n) else { return own };
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 6 && fields[2].parse() == Ok(uid))
        .map(|fields| PathBuf::from(fields[5]))
        .filter(|home| home.is_dir())
        .unwrap_or(own)
}

/// `XDG_DOWNLOAD_DIR="$HOME/..."` of the user's `user-dirs.dirs`.
fn xdg_download_dir(home: &Path) -> Option<PathBuf> {
    let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).ok()?;
    let value = dirs.lines().find_map(|l| l.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?.trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // `$HOME/` alone means "no such folder" in that file.
    (path != home).then_some(path)
}
