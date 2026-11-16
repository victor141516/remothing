//! The two permissions the server needs, granted by whoever sits at the Mac
//! in System Settings > Privacy & Security: Screen & System Audio Recording
//! (picture and sound) and Accessibility (input). Without the first there is
//! nothing to stream; without the second macOS drops every injected event
//! silently. Each missing one is asked for once (macOS shows its dialog) and
//! reported in the log.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

/// Whether input reaches the Mac; with `prompt`, macOS asks for it if not.
pub(crate) fn accessibility(prompt: bool) -> bool {
    let options: Option<Retained<NSDictionary<NSString, AnyObject>>> = prompt.then(|| {
        // SAFETY: a framework constant, a CFString toll-free bridged with NSString.
        let key: &NSString = unsafe { &*(kAXTrustedCheckOptionPrompt as *const NSString) };
        let yes = NSNumber::new_bool(true);
        NSDictionary::from_slices(&[key], &[yes.as_ref() as &AnyObject])
    });
    let ptr = options.as_ref().map_or(std::ptr::null(), |d| Retained::as_ptr(d) as *const c_void);
    // SAFETY: null or a CFDictionary (toll-free bridged NSDictionary) alive for the call.
    unsafe { AXIsProcessTrustedWithOptions(ptr) != 0 }
}

/// Asks for both permissions that are missing and says what is missing.
pub(crate) fn check() {
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        eprintln!(
            "permission: no Screen Recording access: nothing can be captured until it is granted in System Settings > \
             Privacy & Security > Screen & System Audio Recording (for the terminal the server runs in, or for \
             remothing itself when it runs as a launch agent), then the server restarted"
        );
    }
    if !accessibility(true) {
        eprintln!(
            "permission: no Accessibility access: input is dropped by macOS until it is granted in System Settings > \
             Privacy & Security > Accessibility"
        );
    }
}
