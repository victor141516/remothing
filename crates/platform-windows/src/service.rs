//! Windows service (SYSTEM) supervising the server in the active console session.
//! Interface owned by this module; implementation in progress.

use std::path::Path;

use anyhow::{bail, Result};

/// `remothing service <sub>`: install | uninstall | start | stop | run.
pub fn command(sub: &str, _config: &Path) -> Result<()> {
    bail!("service {sub}: not implemented yet")
}

/// Secure attention sequence (Ctrl+Alt+Del) through sas.dll.
pub fn send_sas() -> Result<()> {
    bail!("SendSAS not implemented yet")
}
