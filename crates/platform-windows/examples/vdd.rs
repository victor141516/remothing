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

    crossing(&displays, &created.id)?;

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

/// Captured-mouse moves from another display towards virtual display `id`
/// reach it, as a real mouse goes from one monitor to the next
/// (`InputProvider::motion_relative`), with the runner's real cursor.
#[cfg(windows)]
fn crossing(displays: &remothing_platform_windows::WindowsDisplays, id: &str) -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use remothing_core::platform::{DisplayInfo, DisplayProvider};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let list = displays.list()?;
    let to = list.iter().find(|d| d.id == id).context("the virtual display is not listed")?.clone();
    let from = list.iter().find(|d| d.id != id).context("no other display")?.clone();
    let input = displays.open(&from.id, false)?.input;
    let cursor = || -> anyhow::Result<(i32, i32)> {
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        unsafe { GetCursorPos(&mut p) }?;
        Ok((p.x, p.y))
    };
    let on = |d: &DisplayInfo, (x, y): (i32, i32)| {
        (d.x..d.x + d.width as i32).contains(&x) && (d.y..d.y + d.height as i32).contains(&y)
    };
    let centre = |d: &DisplayInfo| (d.x + d.width as i32 / 2, d.y + d.height as i32 / 2);
    let (from_centre, to_centre) = (centre(&from), centre(&to));
    let (dx, dy) = (to_centre.0 - from_centre.0, to_centre.1 - from_centre.1);
    // Straight across the shared edge: Windows lets the cursor over only
    // there, as between physical monitors (a diagonal ends in a corner).
    let step = if dx.abs() >= dy.abs() { (dx.signum() * 20, 0) } else { (0, dy.signum() * 20) };
    println!("{} at ({}, {}) {}x{}; {id} at ({}, {}) {}x{}", from.id, from.x, from.y, from.width, from.height, to.x, to.y, to.width, to.height);
    input.motion(from.width / 2, from.height / 2)?;
    for _ in 0..1000 {
        input.motion_relative(step.0, step.1)?;
        let p = cursor()?;
        if on(&to, p) {
            println!("towards {id}: reached it, cursor at {p:?}");
            return Ok(());
        }
        ensure!(on(&from, p), "the cursor went to {p:?}, on neither display");
    }
    anyhow::bail!("the cursor never reached {id}, it stayed at {:?}", cursor()?)
}
