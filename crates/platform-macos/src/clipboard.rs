//! The general pasteboard: text, pictures, and the files the Finder copies
//! (read only). macOS has no change notification for it: its change count
//! is polled, as every clipboard tool on the Mac does. Pictures cross as
//! PNG; what Mac programs copy is often TIFF only (screenshots, Preview),
//! converted with NSBitmapImageRep, and a picture set here goes as PNG and
//! TIFF, the format older Mac programs paste.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};
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

    fn set_image(&self, png: &[u8]) -> Result<()> {
        autoreleasepool(|_| {
            let png = NSData::with_bytes(png);
            let tiff = NSBitmapImageRep::imageRepWithData(&png)
                .and_then(|picture| picture.TIFFRepresentation())
                .context("the picture cannot be decoded")?;
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            // SAFETY: framework constants.
            let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
            if !pb.setData_forType(Some(&png), png_type) || !pb.setData_forType(Some(&tiff), tiff_type) {
                bail!("the pasteboard refused the picture");
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

/// Files (the Finder's copy: one file URL per item) when there are, else
/// text, else a picture (PNG as it is, TIFF converted).
fn read(pb: &NSPasteboard) -> Option<Copied> {
    // SAFETY: framework constants.
    let (file_url, string, png, tiff) =
        unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    let files: Vec<PathBuf> = pb
        .pasteboardItems()
        .map(|items| {
            items.iter().filter_map(|item| item.stringForType(file_url)).filter_map(|url| file_uri_path(&url.to_string())).collect()
        })
        .unwrap_or_default();
    if !files.is_empty() {
        return Some(Copied::Files(files));
    }
    if let Some(text) = pb.stringForType(string) {
        return Some(Copied::Text(text.to_string()));
    }
    if let Some(data) = pb.dataForType(png) {
        return Some(Copied::Image(data.to_vec()));
    }
    let tiff_data = pb.dataForType(tiff)?;
    let picture = NSBitmapImageRep::imageRepWithData(&tiff_data)?;
    // SAFETY: an empty dictionary of the property type asked for.
    let data = unsafe { picture.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
    match data {
        Some(data) => Some(Copied::Image(data.to_vec())),
        None => {
            eprintln!("clipboard: the copied TIFF picture cannot be converted to PNG");
            None
        }
    }
}

impl Drop for Pasteboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
