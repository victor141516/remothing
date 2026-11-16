//! The general pasteboard. macOS has no change notification for it: its
//! change count is polled, as every clipboard tool on the Mac does.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use remothing_core::platform::{Clipboard, ClipboardSink};

const POLL: Duration = Duration::from_millis(250);

pub(crate) struct Pasteboard {
    /// Change count right after our own last write: not reported back.
    own: Arc<AtomicIsize>,
    stop: Arc<AtomicBool>,
}

impl Pasteboard {
    pub fn new() -> Self {
        Self { own: Arc::new(AtomicIsize::new(-1)), stop: Arc::new(AtomicBool::new(false)) }
    }
}

impl Clipboard for Pasteboard {
    fn set_text(&self, text: &str) -> Result<()> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            // SAFETY: framework constant.
            if !pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString }) {
                bail!("the pasteboard refused the text");
            }
            self.own.store(pb.changeCount(), Ordering::Relaxed);
            Ok(())
        })
    }

    fn watch(&self, on_change: ClipboardSink) {
        let (own, stop) = (self.own.clone(), self.stop.clone());
        let spawned = std::thread::Builder::new().name("clipboard".into()).spawn(move || {
            let mut seen = autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount());
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
                let text = autoreleasepool(|_| {
                    let pb = NSPasteboard::generalPasteboard();
                    let count = pb.changeCount();
                    if count == seen {
                        return None;
                    }
                    seen = count;
                    if count == own.load(Ordering::Relaxed) {
                        return None;
                    }
                    // SAFETY: framework constant.
                    pb.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
                });
                if let Some(text) = text {
                    on_change(text);
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("clipboard: cannot watch the pasteboard: {e}");
        }
    }
}

impl Drop for Pasteboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
