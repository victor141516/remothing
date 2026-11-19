//! The general pasteboard: text, and the files the Finder copies (read
//! only). macOS has no change notification for it: its change count is
//! polled, as every clipboard tool on the Mac does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString};
use objc2_foundation::NSString;
use remothing_core::files::file_uri_path;
use remothing_core::platform::{Clipboard, ClipboardSink, Copied};

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
            // What is there now is the first report.
            let (mut seen, found) = autoreleasepool(|_| {
                let pb = NSPasteboard::generalPasteboard();
                (pb.changeCount(), read(&pb))
            });
            if let Some(copied) = found {
                on_change(copied, false);
            }
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
                let copied = autoreleasepool(|_| {
                    let pb = NSPasteboard::generalPasteboard();
                    let count = pb.changeCount();
                    if count == seen {
                        return None;
                    }
                    seen = count;
                    if count == own.load(Ordering::Relaxed) {
                        return None;
                    }
                    read(&pb)
                });
                if let Some(copied) = copied {
                    on_change(copied, true);
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("clipboard: cannot watch the pasteboard: {e}");
        }
    }
}

/// Files (the Finder's copy: one file URL per item) when there are, else text.
fn read(pb: &NSPasteboard) -> Option<Copied> {
    // SAFETY: framework constants.
    let (file_url, string) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeString) };
    let files: Vec<PathBuf> = pb
        .pasteboardItems()
        .map(|items| {
            items.iter().filter_map(|item| item.stringForType(file_url)).filter_map(|url| file_uri_path(&url.to_string())).collect()
        })
        .unwrap_or_default();
    if !files.is_empty() {
        return Some(Copied::Files(files));
    }
    pb.stringForType(string).map(|s| Copied::Text(s.to_string()))
}

impl Drop for Pasteboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
