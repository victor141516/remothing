//! While clients are connected: the display must not sleep (a sleeping
//! display gives ScreenCaptureKit nothing to capture), and the process must
//! not be throttled as background work (timer coalescing, App Nap).

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_core_foundation::CFString;
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
use objc2_io_kit::{IOPMAssertionCreateWithName, IOPMAssertionRelease};

/// kIOPMAssertionLevelOn.
const ASSERTION_ON: u32 = 255;

pub(crate) struct Awake {
    assertion: Option<u32>,
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

// SAFETY: the activity token is only handed back to NSProcessInfo, which
// may be called from any thread.
unsafe impl Send for Awake {}

impl Awake {
    /// Starts both; returns what was done and what failed.
    pub fn start(report: &mut Vec<String>) -> Self {
        let kind = CFString::from_static_str("PreventUserIdleDisplaySleep");
        let name = CFString::from_static_str("remothing is streaming this display");
        let mut id = 0u32;
        // SAFETY: valid strings and out pointer.
        let ret = unsafe { IOPMAssertionCreateWithName(Some(&kind), ASSERTION_ON, Some(&name), &mut id) };
        let assertion = if ret == 0 {
            report.push("display kept awake".into());
            Some(id)
        } else {
            report.push(format!("keeping the display awake failed ({ret:#x})"));
            None
        };
        let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiated | NSActivityOptions::LatencyCritical,
            &NSString::from_str("streaming"),
        );
        report.push("latency-critical activity".into());
        Self { assertion, activity }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(id) = self.assertion {
            IOPMAssertionRelease(id);
        }
        // SAFETY: the token beginActivity returned.
        unsafe { NSProcessInfo::processInfo().endActivity(&self.activity) };
    }
}
