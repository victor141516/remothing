//! The Downloads folder of the user at the session, where files a client
//! sends are saved. As the service, the session process runs as SYSTEM in
//! the user's session: the folder is the logged-in user's, found with their
//! token, not SYSTEM's own profile.

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::core::Owned;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_CREATE};

pub fn downloads_dir() -> Result<PathBuf> {
    let token = if crate::service::is_local_system()? { Some(user_token()?) } else { None };
    // SAFETY: a known folder id and a token handle (or none: this process's
    // user) alive for the call; the returned string is freed once copied.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_CREATE, token.as_deref().copied())
            .context("find the Downloads folder")?;
        let text = path.to_string();
        CoTaskMemFree(Some(path.0 as _));
        Ok(PathBuf::from(text.context("Downloads folder path")?))
    }
}

/// The token of the user logged in to this process's session.
fn user_token() -> Result<Owned<HANDLE>> {
    let mut session = 0;
    // SAFETY: plain out-parameter calls; the token handle is owned from here.
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session).context("find this session")?;
        let mut token = HANDLE::default();
        WTSQueryUserToken(session, &mut token).context("nobody is logged in to the session")?;
        Ok(Owned::new(token))
    }
}
