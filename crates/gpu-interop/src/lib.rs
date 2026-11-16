//! Concrete GPU surface types behind `remothing_core::platform::GpuSurface`
//! (an `Arc<dyn Any>`): producers (capture) and consumers (encoders) of the
//! same platform agree on them here.

#[cfg(windows)]
pub mod d3d11 {
    use parking_lot::lock_api::RawMutex as _;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};

    /// Latest captured desktop image, always current, on the capture device.
    ///
    /// `texture` is a `DXGI_FORMAT_B8G8R8A8_UNORM` default-usage texture of
    /// `width` x `height`. The capture thread updates it in place (dirty
    /// regions only). Every use of `context` — by the capture thread, by
    /// encoders, and by FFmpeg through its D3D11 lock callbacks — must hold
    /// `context_lock`.
    pub struct D3D11Frame {
        pub device: ID3D11Device,
        pub context: ID3D11DeviceContext,
        pub texture: ID3D11Texture2D,
        pub width: u32,
        pub height: u32,
        pub context_lock: parking_lot::RawMutex,
    }

    // SAFETY: the device is created with multithread protection and all
    // immediate-context use is serialized through `context_lock`.
    unsafe impl Send for D3D11Frame {}
    unsafe impl Sync for D3D11Frame {}

    impl D3D11Frame {
        pub fn new(
            device: ID3D11Device,
            context: ID3D11DeviceContext,
            texture: ID3D11Texture2D,
            width: u32,
            height: u32,
        ) -> Self {
            Self { device, context, texture, width, height, context_lock: parking_lot::RawMutex::INIT }
        }

        /// Runs `f` with the immediate context locked.
        pub fn with_context<R>(&self, f: impl FnOnce(&ID3D11DeviceContext) -> R) -> R {
            self.context_lock.lock();
            let r = f(&self.context);
            // SAFETY: locked just above on this thread.
            unsafe { self.context_lock.unlock() };
            r
        }
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    use std::sync::Mutex;

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::CVPixelBuffer;

    /// One captured frame: an IOSurface-backed BGRA pixel buffer.
    #[derive(Clone)]
    pub struct PixelBuffer(pub CFRetained<CVPixelBuffer>);

    // SAFETY: CoreVideo buffers are reference counted atomically, and nobody
    // writes into a captured frame: the capture thread reads it locked
    // read-only, VideoToolbox reads it to encode.
    unsafe impl Send for PixelBuffer {}
    unsafe impl Sync for PixelBuffer {}

    /// The newest captured frame of a display. ScreenCaptureKit hands over a
    /// new buffer for every frame, so the capture thread replaces it here
    /// instead of writing into it; an encoder takes the one current when it
    /// encodes.
    #[derive(Default)]
    pub struct PixelBufferFrame {
        latest: Mutex<Option<PixelBuffer>>,
    }

    impl PixelBufferFrame {
        pub fn set(&self, buffer: PixelBuffer) {
            *self.latest.lock().unwrap() = Some(buffer);
        }

        pub fn latest(&self) -> Option<PixelBuffer> {
            self.latest.lock().unwrap().clone()
        }
    }
}
