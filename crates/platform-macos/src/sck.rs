//! ScreenCaptureKit: one SCStream per open display for its frames, another
//! for the system sound (`audio.rs`). Samples arrive on a dispatch queue;
//! frames are IOSurface-backed BGRA pixel buffers that come with the areas
//! that changed, and only when something changed.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use gpu_interop::macos::{PixelBuffer, PixelBufferFrame};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFDictionary, CFRetained, CGRect};
use objc2_core_graphics::{kCGColorSpaceSRGB, CGDirectDisplayID, CGRectMakeWithDictionaryRepresentation};
use objc2_core_media::{kCMTimeZero, CMSampleBuffer};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use remothing_core::geom::Rect;
use remothing_core::platform::{CaptureSession, CursorProvider, CursorState, FrameInfo, GpuSurface};

use crate::cursor::Cursor;
use crate::display::Geometry;

/// Handles one sample of the kind the stream was opened for.
pub(crate) type SampleHandler = Box<dyn Fn(&CMSampleBuffer) + Send + Sync>;
/// Told why the stream stopped on its own (display gone, permission revoked).
pub(crate) type StopHandler = Box<dyn Fn(String) + Send + Sync>;

pub(crate) struct OutputIvars {
    kind: SCStreamOutputType,
    on_sample: SampleHandler,
    on_stop: StopHandler,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "RemothingStreamOutput"]
    #[ivars = OutputIvars]
    pub(crate) struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == self.ivars().kind {
                autoreleasepool(|_| (self.ivars().on_sample)(sample));
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            (self.ivars().on_stop)(error.localizedDescription().to_string());
        }
    }
);

impl StreamOutput {
    fn new(ivars: OutputIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// An object that may be handed to another thread: ScreenCaptureKit's
/// results are immutable once delivered and retain counts are atomic.
struct Sendable<T>(T);
// SAFETY: see above; used only to carry completion handler results.
unsafe impl<T> Send for Sendable<T> {}

fn describe(error: *mut NSError) -> String {
    // SAFETY: the completion handler's error is null or a valid NSError.
    match unsafe { error.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "unknown error".into(),
    }
}

/// What can be captured now. Fails without the Screen Recording permission.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel::<Sendable<Result<Retained<SCShareableContent>, String>>>(1);
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: a valid object or null, retained for the receiver.
        let result = unsafe { Retained::retain(content) }.ok_or_else(|| describe(error));
        let _ = tx.send(Sendable(result));
    });
    // SAFETY: the block lives until the handler has run (it is copied by the callee).
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let Sendable(result) = rx.recv_timeout(Duration::from_secs(10)).context("ScreenCaptureKit did not answer")?;
    result.map_err(|e| anyhow!("ScreenCaptureKit: {e} (System Settings > Privacy & Security > Screen & System Audio Recording)"))
}

pub(crate) fn find_display(content: &SCShareableContent, id: CGDirectDisplayID) -> Result<Retained<SCDisplay>> {
    // SAFETY: plain getters of an immutable snapshot.
    let displays = unsafe { content.displays() };
    displays.iter().find(|d| unsafe { d.displayID() } == id).with_context(|| format!("ScreenCaptureKit does not list display {id}"))
}

/// A running SCStream; stopped when dropped.
pub(crate) struct Stream {
    stream: Retained<SCStream>,
    // The stream holds its output and delegate weakly.
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: SCStream is used only to stop it, which ScreenCaptureKit allows
// from any thread; the output's handlers are Send + Sync.
unsafe impl Send for Stream {}

impl Stream {
    pub fn start(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
        kind: SCStreamOutputType,
        label: &str,
        on_sample: SampleHandler,
        on_stop: StopHandler,
    ) -> Result<Self> {
        let output = StreamOutput::new(OutputIvars { kind, on_sample, on_stop });
        // SAFETY: valid filter and configuration; the delegate outlives the stream (kept in Self).
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        let queue = DispatchQueue::new(label, None);
        // SAFETY: the output and the serial queue are kept alive in Self.
        unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), kind, Some(&queue)) }
            .map_err(|e| anyhow!("SCStream output: {}", e.localizedDescription()))?;
        let (tx, rx) = mpsc::sync_channel::<Option<String>>(1);
        let block = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send((!error.is_null()).then(|| describe(error)));
        });
        // SAFETY: the block is copied by the callee.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
        match rx.recv_timeout(Duration::from_secs(10)).context("ScreenCaptureKit did not start the stream")? {
            None => Ok(Self { stream, _output: output, _queue: queue }),
            Some(e) => bail!("SCStream start: {e}"),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stopping a started stream; no completion handler.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// The frame info attached to a sample: its status and changed areas.
fn frame_info(sample: &CMSampleBuffer) -> Option<(SCFrameStatus, Option<Retained<NSArray>>)> {
    // SAFETY: plain getter; the array is a CFArray of CFDictionary, toll-free
    // bridged with NSArray / NSDictionary.
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    let array: &NSArray<NSDictionary<NSString, AnyObject>> =
        // SAFETY: toll-free bridging, see above.
        unsafe { &*(CFRetained::as_ptr(&attachments).as_ptr() as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let info = array.firstObject()?;
    // SAFETY: framework constants.
    let (status_key, dirty_key) = unsafe { (SCStreamFrameInfoStatus, SCStreamFrameInfoDirtyRects) };
    let status = info.objectForKey(status_key)?.downcast::<NSNumber>().ok()?.integerValue();
    let dirty = info.objectForKey(dirty_key).and_then(|d| d.downcast::<NSArray>().ok());
    Some((SCFrameStatus(status), dirty))
}

/// Rectangles of an SCStreamFrameInfoDirtyRects array, multiplied by `scale`.
fn dirty_rects(array: &NSArray, scale: f64, out: &mut Vec<Rect>) {
    for item in array.iter() {
        let mut r = CGRect::default();
        let dict = Retained::as_ptr(&item) as *const CFDictionary;
        // SAFETY: each entry is a CGRect dictionary representation (CFDictionary).
        if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&*dict), &mut r) } {
            continue;
        }
        let x0 = (r.origin.x * scale).floor().max(0.0);
        let y0 = (r.origin.y * scale).floor().max(0.0);
        let x1 = ((r.origin.x + r.size.width) * scale).ceil();
        let y1 = ((r.origin.y + r.size.height) * scale).ceil();
        if x1 > x0 && y1 > y0 {
            out.push(Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32));
        }
    }
}

/// More changed areas than this are sent as the whole display.
const MAX_RECTS: usize = 64;

#[derive(Default)]
struct Pending {
    buffer: Option<PixelBuffer>,
    dirty: Vec<Rect>,
    whole: bool,
    at_us: u64,
    stopped: Option<String>,
}

#[derive(Default)]
struct Inbox {
    pending: Mutex<Pending>,
    ready: Condvar,
}

/// A frame being read: its pixel buffer stays locked until the next one.
struct Locked {
    buffer: PixelBuffer,
    base: *const u8,
    stride: usize,
    len: usize,
}

impl Locked {
    fn new(buffer: PixelBuffer) -> Result<Self> {
        let pb: &CVPixelBuffer = &buffer.0;
        // SAFETY: a valid pixel buffer, unlocked in Drop with the same flags.
        if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            bail!("CVPixelBufferLockBaseAddress failed");
        }
        let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(pb);
        let len = stride * CVPixelBufferGetHeight(pb);
        Ok(Self { buffer, base, stride, len })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: locked read-only in `new`.
        unsafe { CVPixelBufferUnlockBaseAddress(&self.buffer.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// Capture of one display.
pub(crate) struct SckSession {
    _stream: Stream,
    inbox: Arc<Inbox>,
    geometry: Geometry,
    current: Option<Locked>,
    surface: Arc<PixelBufferFrame>,
    first: bool,
    cursor: Cursor,
}

// SAFETY: `current` points into a locked pixel buffer owned by the session,
// which moves between threads only as a whole (one capture thread at a time).
unsafe impl Send for SckSession {}

impl SckSession {
    pub fn open(geometry: Geometry) -> Result<Self> {
        let content = shareable_content()?;
        let display = find_display(&content, geometry.id)?;
        // A separate cursor where macOS still tells what it looks like (it
        // draws over the picture with no delay on the client); else drawn
        // into the picture by ScreenCaptureKit.
        let cursor = Cursor::new(geometry);
        // SAFETY: plain object creation and setters.
        let (filter, config) = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(geometry.width as usize);
            config.setHeight(geometry.height as usize);
            config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
            config.setColorSpaceName(kCGColorSpaceSRGB);
            config.setShowsCursor(!cursor.separate());
            // The display's own rate, whatever it is (ProMotion panels report none).
            config.setMinimumFrameInterval(kCMTimeZero);
            // Frames held by the session and the encoders, plus ScreenCaptureKit's own.
            config.setQueueDepth(6);
            (filter, config)
        };
        let inbox = Arc::new(Inbox::default());
        let (width, height) = (geometry.width, geometry.height);
        // Changed areas come in points or in pixels, depending on the macOS
        // release: scaled up as if points (over-reporting is harmless, the
        // tiles are compared anyway) until one reaches beyond the points.
        let points = (geometry.bounds.size.width, geometry.bounds.size.height);
        let scale = geometry.scale().0.max(1.0);
        let in_pixels = std::sync::atomic::AtomicBool::new(false);
        let on_sample = {
            let inbox = inbox.clone();
            Box::new(move |sample: &CMSampleBuffer| {
                let Some((status, dirty)) = frame_info(sample) else { return };
                if status != SCFrameStatus::Complete {
                    return;
                }
                // SAFETY: plain getter; a retained pixel buffer.
                let Some(image) = (unsafe { sample.image_buffer() }) else { return };
                let mut p = inbox.pending.lock().unwrap();
                p.buffer = Some(PixelBuffer(image));
                p.at_us = remothing_core::proto::now_us();
                match dirty {
                    Some(list) if !p.whole => {
                        let mut rects = Vec::new();
                        let pixels = in_pixels.load(std::sync::atomic::Ordering::Relaxed);
                        dirty_rects(&list, if pixels { 1.0 } else { scale }, &mut rects);
                        if !pixels {
                            let mut probe = Vec::new();
                            dirty_rects(&list, 1.0, &mut probe);
                            if probe.iter().any(|r| (r.x + r.w) as f64 > points.0 + 1.0 || (r.y + r.h) as f64 > points.1 + 1.0) {
                                in_pixels.store(true, std::sync::atomic::Ordering::Relaxed);
                                rects = probe;
                            }
                        }
                        p.dirty.extend(rects.into_iter().filter_map(|r| r.clip(width, height)));
                        if p.dirty.len() > MAX_RECTS {
                            p.whole = true;
                            p.dirty.clear();
                        }
                    }
                    Some(_) => {}
                    None => {
                        p.whole = true;
                        p.dirty.clear();
                    }
                }
                drop(p);
                inbox.ready.notify_one();
            }) as SampleHandler
        };
        let on_stop = {
            let inbox = inbox.clone();
            Box::new(move |why: String| {
                inbox.pending.lock().unwrap().stopped = Some(why);
                inbox.ready.notify_one();
            }) as StopHandler
        };
        let stream = Stream::start(&filter, &config, SCStreamOutputType::Screen, "remothing.capture", on_sample, on_stop)?;
        Ok(Self {
            _stream: stream,
            inbox,
            geometry,
            current: None,
            surface: Arc::new(PixelBufferFrame::default()),
            first: true,
            cursor,
        })
    }
}

impl CursorProvider for SckSession {
    fn cursor_change(&mut self) -> Result<Option<CursorState>> {
        Ok(self.cursor.change())
    }

    fn cursor_position(&mut self) -> Result<Option<(u32, u32)>> {
        Ok(self.cursor.position())
    }
}

impl CaptureSession for SckSession {
    fn size(&self) -> (u32, u32) {
        (self.geometry.width, self.geometry.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<FrameInfo>> {
        let p = self.inbox.pending.lock().unwrap();
        let (mut p, _) =
            self.inbox.ready.wait_timeout_while(p, timeout, |p| p.buffer.is_none() && p.stopped.is_none()).unwrap();
        if let Some(why) = p.stopped.take() {
            bail!("screen capture stopped: {why}");
        }
        let Some(buffer) = p.buffer.take() else { return Ok(None) };
        let whole = std::mem::take(&mut p.whole) || self.first;
        let mut dirty = std::mem::take(&mut p.dirty);
        let at_us = p.at_us;
        drop(p);
        let (w, h) = (CVPixelBufferGetWidth(&buffer.0) as u32, CVPixelBufferGetHeight(&buffer.0) as u32);
        if (w, h) != self.size() {
            bail!("display size changed to {w}x{h}");
        }
        self.current = None;
        self.current = Some(Locked::new(buffer.clone())?);
        self.surface.set(buffer);
        if whole {
            self.first = false;
            dirty = vec![Rect::new(0, 0, w, h)];
        }
        Ok(Some(FrameInfo { capture_us: at_us, dirty, moved: Vec::new() }))
    }

    fn read(&mut self, rects: &[Rect], dst: &mut [u8], dst_stride: usize) -> Result<()> {
        let (src, stride) = self.mapped().context("no frame captured yet")?;
        for r in rects {
            let (x, w) = (r.x as usize * 4, r.w as usize * 4);
            for row in r.y as usize..(r.y + r.h) as usize {
                dst[row * dst_stride + x..row * dst_stride + x + w].copy_from_slice(&src[row * stride + x..row * stride + x + w]);
            }
        }
        Ok(())
    }

    fn mapped(&self) -> Option<(&[u8], usize)> {
        let l = self.current.as_ref()?;
        // SAFETY: the buffer stays locked (and its memory mapped) while `l` lives.
        Some((unsafe { std::slice::from_raw_parts(l.base, l.len) }, l.stride))
    }

    fn gpu_surface(&self) -> Option<GpuSurface> {
        Some(self.surface.clone())
    }
}
