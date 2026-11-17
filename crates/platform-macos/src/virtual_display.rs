//! Virtual displays through CGVirtualDisplay. macOS has no public API for
//! them; CoreGraphics' private classes are what Chromium's display tests,
//! DeskPad and BetterDisplay use, looked up by name so a macOS without them
//! only loses this feature. A display lives as long as its object: the
//! process ending removes it. The objects stay on one thread of their own
//! because nothing says they may be used from several.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2::rc::{autoreleasepool, Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDisplaySetDisplayMode, CGError,
};
use objc2_foundation::{NSArray, NSString};
use remothing_core::platform::{
    DisplayInfo, DisplayKind, DisplayMode, VirtualCapabilities, VirtualDisplays, VirtualSetup,
};

use crate::display::{self, Geometry};

const CLASSES: [&str; 4] = ["CGVirtualDisplay", "CGVirtualDisplayDescriptor", "CGVirtualDisplayMode", "CGVirtualDisplaySettings"];
/// The framebuffer limit given when a display is made: a later mode cannot
/// be larger.
const MAX_WIDTH: u32 = 6144;
const MAX_HEIGHT: u32 = 3456;
const REFRESH_RATES: [u32; 3] = [30, 60, 120];
/// macOS 14 refuses a display with vendor 0 (Chromium's finding); any other
/// value works. Displays tell apart by serial number.
const VENDOR_ID: u32 = 0x7274;
const PRODUCT_ID: u32 = 0x0001;
/// Size the display claims to have, for the dpi macOS derives from it.
const PIXELS_PER_INCH: f64 = 110.0;
/// How long macOS gets to bring a display online, change it or remove it.
const SETTLE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);

enum Command {
    Create(u32, DisplayMode, Sender<Result<CGDirectDisplayID>>),
    Set(CGDirectDisplayID, DisplayMode, Sender<Result<()>>),
    Destroy(CGDirectDisplayID, Sender<Result<()>>),
}

pub(crate) struct MacVirtualDisplays {
    /// None where the private classes are missing; then `unavailable` says so.
    commands: Option<Mutex<Sender<Command>>>,
    unavailable: String,
    /// Display id → its mode and serial number, for the display list.
    displays: Mutex<HashMap<CGDirectDisplayID, (DisplayMode, u32)>>,
    next_serial: Mutex<u32>,
}

impl MacVirtualDisplays {
    pub fn new() -> Self {
        let missing: Vec<&str> = CLASSES.into_iter().filter(|c| class(c).is_err()).collect();
        let (commands, unavailable) = if missing.is_empty() {
            let (tx, rx) = channel();
            thread::Builder::new()
                .name("virtual-displays".into())
                .spawn(move || {
                    let mut owned: HashMap<CGDirectDisplayID, Retained<AnyObject>> = HashMap::new();
                    for command in rx {
                        autoreleasepool(|_| run(command, &mut owned));
                    }
                })
                .expect("spawning the virtual display thread");
            (Some(Mutex::new(tx)), String::new())
        } else {
            (None, format!("this macOS lacks the private CoreGraphics classes {}", missing.join(", ")))
        };
        Self { commands, unavailable, displays: Mutex::new(HashMap::new()), next_serial: Mutex::new(1) }
    }

    /// Marks `info` as virtual if it is one of these displays.
    pub fn annotate(&self, info: &mut DisplayInfo) {
        let Ok(id) = display::parse_id(&info.id) else { return };
        if let Some((mode, serial)) = self.displays.lock().unwrap().get(&id) {
            info.kind = DisplayKind::Virtual;
            info.mode = Some(*mode);
            info.name = format!("Virtual display {serial}");
            // The rate it was given: this process may not see its mode (`Geometry::of`).
            info.refresh_hz = f64::from(mode.refresh_hz);
        }
    }

    fn call<T>(&self, command: impl FnOnce(Sender<Result<T>>) -> Command) -> Result<T> {
        let commands = self.commands.as_ref().ok_or_else(|| anyhow!("{}", self.unavailable))?;
        let (tx, rx) = channel();
        commands.lock().unwrap().send(command(tx)).map_err(|_| anyhow!("the virtual display thread ended"))?;
        rx.recv().map_err(|_| anyhow!("the virtual display thread ended"))?
    }

    fn owned_id(&self, id: &str) -> Result<CGDirectDisplayID> {
        let display = display::parse_id(id)?;
        if !self.displays.lock().unwrap().contains_key(&display) {
            bail!("{id} is not a virtual display of this server");
        }
        Ok(display)
    }

    fn info(&self, display: CGDirectDisplayID) -> Result<DisplayInfo> {
        let mut info = Geometry::of(display)?.info();
        self.annotate(&mut info);
        Ok(info)
    }
}

impl VirtualDisplays for MacVirtualDisplays {
    fn capabilities(&self) -> VirtualCapabilities {
        VirtualCapabilities {
            max_width: MAX_WIDTH,
            max_height: MAX_HEIGHT,
            refresh_rates: REFRESH_RATES.to_vec(),
            hdr: false,
            resize: true,
        }
    }

    fn setup(&self) -> VirtualSetup {
        match self.commands {
            Some(_) => VirtualSetup::Ready,
            None => VirtualSetup::Unavailable { reason: self.unavailable.clone() },
        }
    }

    fn create(&self, mode: DisplayMode) -> Result<DisplayInfo> {
        let serial = {
            let mut next = self.next_serial.lock().unwrap();
            *next += 1;
            *next - 1
        };
        let display = self.call(|tx| Command::Create(serial, mode, tx))?;
        self.displays.lock().unwrap().insert(display, (mode, serial));
        self.info(display)
    }

    fn set_mode(&self, id: &str, mode: DisplayMode) -> Result<DisplayInfo> {
        let display = self.owned_id(id)?;
        self.call(|tx| Command::Set(display, mode, tx))?;
        if let Some(entry) = self.displays.lock().unwrap().get_mut(&display) {
            entry.0 = mode;
        }
        self.info(display)
    }

    fn destroy(&self, id: &str) -> Result<()> {
        let display = self.owned_id(id)?;
        self.call(|tx| Command::Destroy(display, tx))?;
        self.displays.lock().unwrap().remove(&display);
        Ok(())
    }
}

/// Carries out one command on the thread that owns the display objects.
fn run(command: Command, owned: &mut HashMap<CGDirectDisplayID, Retained<AnyObject>>) {
    match command {
        Command::Create(serial, mode, reply) => {
            let made = create(serial, mode).and_then(|object| {
                // SAFETY: displayID is a property of CGVirtualDisplay (unsigned int).
                let display: u32 = unsafe { msg_send![&*object, displayID] };
                if display == 0 {
                    bail!("macOS gave the virtual display no id");
                }
                // Kept before waiting: dropping it would remove the display.
                owned.insert(display, object);
                let ready = wait_for(display, true).and_then(|()| ensure_mode(display, mode));
                if let Err(e) = ready {
                    owned.remove(&display);
                    return Err(e);
                }
                Ok(display)
            });
            let _ = reply.send(made);
        }
        Command::Set(display, mode, reply) => {
            let done = owned
                .get(&display)
                .context("no such virtual display")
                .and_then(|object| apply(object, mode))
                .and_then(|()| ensure_mode(display, mode));
            let _ = reply.send(done);
        }
        Command::Destroy(display, reply) => {
            let done = match owned.remove(&display) {
                // Released here, which removes it; the wait lets the capture
                // loop see it gone before the client's list is sent.
                Some(object) => {
                    drop(object);
                    wait_for(display, false)
                }
                None => Err(anyhow!("no such virtual display")),
            };
            let _ = reply.send(done);
        }
    }
}

fn class(name: &str) -> Result<&'static AnyClass> {
    let name = std::ffi::CString::new(name)?;
    AnyClass::get(&name).with_context(|| format!("class {} is missing", name.to_string_lossy()))
}

/// A new display with `mode` as its only mode.
fn create(serial: u32, mode: DisplayMode) -> Result<Retained<AnyObject>> {
    let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::UserInitiated));
    let queue_ptr: *mut AnyObject = DispatchRetained::as_ptr(&queue).as_ptr().cast();
    let name = NSString::from_str(&format!("remothing {serial}"));
    let millimeters = |pixels: u32| 25.4 * pixels as f64 / PIXELS_PER_INCH;
    // SAFETY: the setters and initialiser of CGVirtualDisplayDescriptor and
    // CGVirtualDisplay with the argument types of their interface (as
    // generated from CoreGraphics in Chromium's virtual_display_util_mac.mm).
    // The descriptor retains the queue, a dispatch object (an ObjC object).
    let display: Option<Retained<AnyObject>> = unsafe {
        let descriptor: Retained<AnyObject> = msg_send![class("CGVirtualDisplayDescriptor")?, new];
        let _: () = msg_send![&*descriptor, setQueue: queue_ptr];
        let _: () = msg_send![&*descriptor, setName: &*name];
        let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_WIDTH];
        let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_HEIGHT];
        let size = CGSize::new(millimeters(mode.width), millimeters(mode.height));
        let _: () = msg_send![&*descriptor, setSizeInMillimeters: size];
        let _: () = msg_send![&*descriptor, setSerialNum: serial];
        let _: () = msg_send![&*descriptor, setProductID: PRODUCT_ID];
        let _: () = msg_send![&*descriptor, setVendorID: VENDOR_ID];
        // sRGB primaries and D65, as a plain monitor reports them.
        let _: () = msg_send![&*descriptor, setRedPrimary: CGPoint::new(0.64, 0.33)];
        let _: () = msg_send![&*descriptor, setGreenPrimary: CGPoint::new(0.30, 0.60)];
        let _: () = msg_send![&*descriptor, setBluePrimary: CGPoint::new(0.15, 0.06)];
        let _: () = msg_send![&*descriptor, setWhitePoint: CGPoint::new(0.3127, 0.3290)];
        let allocated: Allocated<AnyObject> = msg_send![class("CGVirtualDisplay")?, alloc];
        msg_send![allocated, initWithDescriptor: &*descriptor]
    };
    let display = display.context("CGVirtualDisplay refused the descriptor")?;
    apply(&display, mode)?;
    Ok(display)
}

/// Gives the display `mode` as its only mode, 1 pixel per point.
fn apply(display: &AnyObject, mode: DisplayMode) -> Result<()> {
    // SAFETY: CGVirtualDisplayMode's initialiser and CGVirtualDisplaySettings'
    // setters, then applySettings: on a CGVirtualDisplay. Width and height
    // are passed as NSUInteger: headers disagree on unsigned int and
    // NSUInteger, and a 64-bit register holds either on arm64.
    let ok: Bool = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class("CGVirtualDisplayMode")?, alloc];
        let display_mode: Option<Retained<AnyObject>> = msg_send![
            allocated,
            initWithWidth: mode.width as usize,
            height: mode.height as usize,
            refreshRate: mode.refresh_hz as f64
        ];
        let display_mode = display_mode.context("CGVirtualDisplayMode refused the mode")?;
        let modes = NSArray::from_retained_slice(&[display_mode]);
        let settings: Retained<AnyObject> = msg_send![class("CGVirtualDisplaySettings")?, new];
        let _: () = msg_send![&*settings, setHiDPI: 0u32];
        let _: () = msg_send![&*settings, setModes: &*modes];
        msg_send![display, applySettings: &*settings]
    };
    if !ok.as_bool() {
        bail!("CGVirtualDisplay refused {}x{} at {} Hz", mode.width, mode.height, mode.refresh_hz);
    }
    Ok(())
}

/// Waits until `display` is online (`online`) or gone.
fn wait_for(display: CGDirectDisplayID, online: bool) -> Result<()> {
    let deadline = Instant::now() + SETTLE;
    while Geometry::of(display).is_ok() != online {
        if Instant::now() > deadline {
            let what = if online { "come online" } else { "go away" };
            bail!("display {display} did not {what} within {SETTLE:?}");
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// macOS may pick another mode than the one just given (a scaled one, or
/// the previous one); switches to it where this process knows the modes,
/// and waits until the picture has its size.
fn ensure_mode(display: CGDirectDisplayID, mode: DisplayMode) -> Result<()> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let g = Geometry::of(display)?;
        if (g.width, g.height) == (mode.width, mode.height) {
            return Ok(());
        }
        // SAFETY: plain query; no options.
        if let Some(modes) = unsafe { CGDisplayCopyAllDisplayModes(display, None) } {
            // SAFETY: CGDisplayCopyAllDisplayModes returns an array of CGDisplayModes.
            let modes = unsafe { modes.cast_unchecked::<CGDisplayMode>() };
            let wanted = modes.iter().find(|m| {
                CGDisplayMode::pixel_width(Some(m)) as u32 == mode.width
                    && CGDisplayMode::pixel_height(Some(m)) as u32 == mode.height
            });
            if let Some(wanted) = wanted {
                // SAFETY: a mode of this display; no options.
                let err = unsafe { CGDisplaySetDisplayMode(display, Some(&wanted), None) };
                if err != CGError::Success {
                    bail!("CGDisplaySetDisplayMode failed ({})", err.0);
                }
            }
        }
        if Instant::now() > deadline {
            bail!("display {display} shows {}x{}, not {}x{}", g.width, g.height, mode.width, mode.height);
        }
        thread::sleep(POLL);
    }
}
