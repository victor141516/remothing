//! Drives remothing's virtual display driver as client requests do: installs
//! it where needed (elevated), creates a display, captures it, changes its
//! mode, tries HDR and removes it. Run by .github/workflows/windows-vdd.yml.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, ensure, Context};
    use remothing_core::platform::{DisplayMode, DisplayProvider, VirtualSetup};
    use remothing_platform_windows::WindowsDisplays;

    let displays = WindowsDisplays::new();
    let ids = |d: &WindowsDisplays| -> anyhow::Result<Vec<String>> { Ok(d.list()?.into_iter().map(|i| i.id).collect()) };
    let virtuals = displays.virtual_displays().context("no virtual displays on Windows")?;
    println!("displays: {:?}", ids(&displays)?);
    match virtuals.setup() {
        VirtualSetup::Ready => println!("driver ready"),
        VirtualSetup::Install { notice } => {
            println!("installing: {notice}");
            virtuals.install()?;
        }
        VirtualSetup::Unavailable { reason } => bail!("unavailable: {reason}"),
    }
    ensure!(matches!(virtuals.setup(), VirtualSetup::Ready), "driver not ready after the installation");

    let mode = |width, height, refresh_hz, hdr| DisplayMode { width, height, refresh_hz, hdr };
    let created = virtuals.create(mode(1280, 720, 60, false))?;
    println!("created {} {}x{} @ {} Hz", created.id, created.width, created.height, created.refresh_hz);
    capture(&displays, &created.id, (1280, 720))?;

    let changed = virtuals.set_mode(&created.id, mode(1600, 900, 120, false))?;
    ensure!(changed.id == created.id, "the id changed from {} to {}", created.id, changed.id);
    println!("changed to {}x{} @ {} Hz", changed.width, changed.height, changed.refresh_hz);
    capture(&displays, &created.id, (1600, 900))?;

    match virtuals.set_mode(&created.id, mode(1600, 900, 60, true)) {
        Ok(_) => {
            let opened = displays.open(&created.id, true)?;
            println!("HDR on: picture {:?}", opened.capture.picture());
        }
        Err(e) => {
            println!("HDR not available here: {e:#}");
            ensure!(ids(&displays)?.contains(&created.id), "{} was lost by the refused change", created.id);
        }
    }

    virtuals.destroy(&created.id)?;
    let left = ids(&displays)?;
    ensure!(!left.contains(&created.id), "{} still listed: {left:?}", created.id);
    println!("removed; displays: {left:?}");
    Ok(())
}

/// Opens display `id` and checks it captures at `size`.
#[cfg(windows)]
fn capture(displays: &remothing_platform_windows::WindowsDisplays, id: &str, size: (u32, u32)) -> anyhow::Result<()> {
    use remothing_core::platform::DisplayProvider;
    let opened = displays.open(id, false)?;
    let mut session = opened.capture;
    anyhow::ensure!(session.size() == size, "{id} captures at {:?}, not {size:?}", session.size());
    let frame = session.next_frame(std::time::Duration::from_secs(3))?;
    println!("{id}: {:?}, first frame {}", session.size(), if frame.is_some() { "arrived" } else { "did not arrive in 3 s" });
    Ok(())
}
