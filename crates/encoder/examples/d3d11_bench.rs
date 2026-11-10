//! Zero-copy D3D11 encode check and latency benchmark (Windows only).
//!
//! Renders synthetic desktop content into a D3D11 BGRA texture (no screen
//! capture, nothing drawn on screen), encodes it through the
//! `EncoderProvider` with GPU input (`load_gpu`) and with system-memory input
//! (`load_cpu`), decodes every access unit with FFmpeg's H.264 decoder and
//! compares the last decoded picture with the source.
//!
//! `cargo run --release -p remothing-encoder --features static --example d3d11_bench [frames]`

#[cfg(not(windows))]
fn main() {
    eprintln!("d3d11_bench needs Windows");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    bench::run()
}

#[cfg(windows)]
mod bench {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use ffmpeg_next as ff;
    use gpu_interop::d3d11::D3D11Frame;
    use remothing_core::encode::{EncoderProvider, EncoderRequest, VideoEncoder};
    use remothing_core::geom::Rect;
    use remothing_core::platform::GpuSurface;
    use remothing_encoder::FfmpegEncoders;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;

    /// Synthetic content: a static gradient desktop with a moving, animated
    /// window ("window"), or the whole screen scrolling every frame ("full").
    #[derive(Clone, Copy)]
    enum Scene {
        Window,
        Full,
    }

    struct Image {
        w: u32,
        h: u32,
        px: Vec<u8>,
    }

    impl Image {
        fn new(w: u32, h: u32) -> Self {
            let mut img = Self { w, h, px: vec![0; (w * h * 4) as usize] };
            img.paint(Rect::new(0, 0, w, h), |x, y| [(x / 8) as u8, (y / 8) as u8, ((x + y) / 16) as u8]);
            img
        }

        fn paint(&mut self, r: Rect, f: impl Fn(u32, u32) -> [u8; 3]) {
            let stride = self.w as usize * 4;
            for y in r.y..r.y + r.h {
                let row = &mut self.px[y as usize * stride..][..stride];
                for x in r.x..r.x + r.w {
                    let [b, g, r] = f(x, y);
                    row[x as usize * 4..][..4].copy_from_slice(&[b, g, r, 255]);
                }
            }
        }

        /// Advances the scene to frame `n`; returns the changed rectangles.
        fn step(&mut self, scene: Scene, n: u32) -> Vec<Rect> {
            match scene {
                Scene::Window => {
                    let (bw, bh) = (640, 400);
                    let pos = |n: u32| {
                        let t = n * 6;
                        Rect::new(t % (self.w - bw), (t / 2) % (self.h - bh), bw, bh)
                    };
                    let (old, new) = (pos(n.saturating_sub(1)), pos(n));
                    self.paint(old, |x, y| [(x / 8) as u8, (y / 8) as u8, ((x + y) / 16) as u8]);
                    self.paint(new, |x, y| {
                        let v = ((x - new.x) * 3 + (y - new.y) * 5 + n * 7) as u8;
                        [v, v.wrapping_mul(3), 255 - v]
                    });
                    vec![old, new]
                }
                Scene::Full => {
                    let full = Rect::new(0, 0, self.w, self.h);
                    // Changes every pixel without being a pure translation,
                    // so motion search cannot make it cheap.
                    self.paint(full, |x, y| {
                        let v = (x * x / 97 + y * y / 89 + n * 5) as u8;
                        [v, v ^ (x / 16) as u8, ((y / 4) as u8).wrapping_add((n * 3) as u8)]
                    });
                    vec![full]
                }
            }
        }
    }

    struct Gpu {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        adapter: String,
    }

    fn gpu() -> Result<Gpu> {
        unsafe {
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("no device")?;
            // Returns the previous state, not an error.
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            let desc = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?;
            let adapter = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
            Ok(Gpu { device, context: context.context("no context")?, adapter })
        }
    }

    fn surface(gpu: &Gpu, w: u32, h: u32) -> Result<Arc<D3D11Frame>> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(Arc::new(D3D11Frame::new(gpu.device.clone(), gpu.context.clone(), tex.context("no texture")?, w, h)))
    }

    fn upload(frame: &D3D11Frame, img: &Image, rects: &[Rect]) {
        let stride = img.w as usize * 4;
        frame.with_context(|ctx| {
            for r in rects {
                let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                let src = img.px[r.y as usize * stride + r.x as usize * 4..].as_ptr();
                unsafe { ctx.UpdateSubresource(&frame.texture, 0, Some(&b), src.cast(), stride as u32, 0) };
            }
        });
    }

    /// Decodes the produced stream and checks it against the source.
    struct Check {
        dec: ff::decoder::Video,
        decoded: usize,
        last: ff::frame::Video,
    }

    impl Check {
        fn new() -> Result<Self> {
            let codec = ff::decoder::find(ff::codec::Id::H264).context("no h264 decoder")?;
            let dec = ff::codec::context::Context::new_with_codec(codec).decoder().video()?;
            Ok(Self { dec, decoded: 0, last: ff::frame::Video::empty() })
        }

        fn feed(&mut self, au: &[u8]) -> Result<()> {
            if !au.is_empty() {
                self.dec.send_packet(&ff::Packet::copy(au))?;
            }
            self.drain()
        }

        fn drain(&mut self) -> Result<()> {
            let mut f = ff::frame::Video::empty();
            while self.dec.receive_frame(&mut f).is_ok() {
                self.decoded += 1;
                std::mem::swap(&mut self.last, &mut f);
            }
            Ok(())
        }

        /// Mean absolute per-channel difference of the last picture vs `img`.
        fn finish(mut self, img: &Image) -> Result<(usize, f64)> {
            self.dec.send_eof()?;
            self.drain()?;
            if self.decoded == 0 {
                bail!("nothing decoded");
            }
            let (w, h) = (self.last.width(), self.last.height());
            if (w, h) != (img.w, img.h) {
                bail!("decoded {w}x{h}, expected {}x{}", img.w, img.h);
            }
            let mut bgr = ff::frame::Video::empty();
            ff::software::scaling::Context::get(
                self.last.format(),
                w,
                h,
                ff::format::Pixel::BGRZ,
                w,
                h,
                ff::software::scaling::Flags::BILINEAR,
            )?
            .run(&self.last, &mut bgr)?;
            let (stride, data) = (bgr.stride(0), bgr.data(0));
            let mut sum = 0u64;
            for y in 0..h as usize {
                let (a, b) = (&data[y * stride..][..w as usize * 4], &img.px[y * w as usize * 4..][..w as usize * 4]);
                for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
                    sum += (0..3).map(|i| p[i].abs_diff(q[i]) as u64).sum::<u64>();
                }
            }
            Ok((self.decoded, sum as f64 / (w as f64 * h as f64 * 3.0)))
        }
    }

    fn pct(v: &mut [Duration], p: f64) -> f64 {
        v.sort();
        v[((v.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
    }

    #[allow(clippy::too_many_arguments)]
    fn run_case(
        enc: &mut dyn VideoEncoder,
        gpu_surface: Option<&GpuSurface>,
        frame: &D3D11Frame,
        w: u32,
        h: u32,
        scene: Scene,
        frames: u32,
    ) -> Result<String> {
        let mut img = Image::new(w, h);
        let full = [Rect::new(0, 0, w, h)];
        upload(frame, &img, &full);
        let mut check = Check::new()?;
        let (mut total, mut encode) = (Vec::new(), Vec::new());
        let (mut bytes, mut keys, mut empty) = (0usize, 0, 0);
        for n in 0..frames {
            let rects = if n == 0 { full.to_vec() } else { img.step(scene, n) };
            if gpu_surface.is_some() {
                upload(frame, &img, &rects);
            }
            let t0 = Instant::now();
            match gpu_surface {
                Some(s) => enc.load_gpu(s, &rects)?,
                None => enc.load_cpu(&img.px, w as usize * 4, &rects)?,
            }
            let t1 = Instant::now();
            // An IDR at start and one mid-stream, as for a joining client.
            let out = enc.encode(n == 0 || n == frames / 2)?;
            let t2 = Instant::now();
            total.push(t2 - t0);
            encode.push(t2 - t1);
            bytes += out.data.len();
            keys += out.key as u32;
            empty += out.data.is_empty() as u32;
            check.feed(&out.data)?;
        }
        let (decoded, mae) = check.finish(&img)?;
        Ok(format!(
            "| {} | {w}x{h} | {} | {:.2} | {:.2} | {:.2} | {:.1} | {keys} | {empty} | {decoded}/{frames} | {mae:.2} |",
            enc.name(),
            match scene {
                Scene::Window => "window",
                Scene::Full => "full",
            },
            pct(&mut total, 0.5),
            pct(&mut total, 0.99),
            pct(&mut encode, 0.5),
            bytes as f64 / frames as f64 / 1024.0,
        ))
    }

    /// Reports encoder private options the encoder configuration relies on
    /// that this FFmpeg build does not know (FFmpeg ignores unknown ones).
    fn missing_options(codec: &str, names: &[&str]) -> Vec<String> {
        let Some(c) = ff::encoder::find_by_name(codec) else { return vec!["<codec missing>".into()] };
        unsafe {
            let class = (*c.as_ptr()).priv_class;
            names
                .iter()
                .filter(|n| {
                    let name = std::ffi::CString::new(**n).unwrap();
                    ff::ffi::av_opt_find(
                        &class as *const _ as *mut std::ffi::c_void,
                        name.as_ptr(),
                        std::ptr::null(),
                        0,
                        ff::ffi::AV_OPT_SEARCH_FAKE_OBJ,
                    )
                    .is_null()
                })
                .map(|n| n.to_string())
                .collect()
        }
    }

    pub fn run() -> Result<()> {
        let frames: u32 = std::env::args().nth(1).map(|a| a.parse()).transpose()?.unwrap_or(600);
        let provider = FfmpegEncoders::new()?;
        let gpu = gpu()?;
        println!("adapter: {}", gpu.adapter);
        println!("compiled backends: {:?}", provider.backends());
        for (codec, opts) in [
            ("h264_nvenc", &["profile", "preset", "tune", "zerolatency", "delay", "rc", "qp", "forced-idr"][..]),
            ("h264_amf", &["profile", "usage", "quality", "rc", "qp_i", "qp_p", "header_spacing"][..]),
            ("h264_qsv", &["profile", "preset", "async_depth", "low_power", "forced_idr", "look_ahead"][..]),
            ("libx264", &["profile", "preset", "tune", "crf", "forced-idr"][..]),
        ] {
            println!("{codec}: unknown options {:?}", missing_options(codec, opts));
        }

        for (w, h) in [(1920, 1080), (3168, 1440)] {
            let frame = surface(&gpu, w, h)?;
            let gs: GpuSurface = frame.clone();
            for backend in ["nvenc", "amf", "qsv", "x264"] {
                let req = EncoderRequest { width: w, height: h, preferred: Some(backend.into()), surface: Some(gs.clone()) };
                let e = provider.create(&req)?;
                println!("{w}x{h} prefer {backend}: opened {:?}, fallback: {:?}", e.name(), e.fallback_reason());
            }
            println!("| encoder | size | scene | total p50 ms | total p99 ms | encode p50 ms | KiB/frame | keys | empty | decoded | MAE |");
            println!("|---|---|---|---|---|---|---|---|---|---|---|");
            for (backend, gpu_input) in [("nvenc", true), ("nvenc", false), ("x264", false)] {
                let req = EncoderRequest {
                    width: w,
                    height: h,
                    preferred: Some(backend.into()),
                    surface: gpu_input.then(|| gs.clone()),
                };
                let mut e = provider.create(&req)?;
                if !e.name().starts_with(backend) || e.gpu_input() != gpu_input {
                    println!("| {} | {w}x{h} | skipped: {:?} |", e.name(), e.fallback_reason());
                    continue;
                }
                let input = gpu_input.then_some(&gs);
                for scene in [Scene::Window, Scene::Full] {
                    println!("{}", run_case(e.as_mut(), input, &frame, w, h, scene, frames)?);
                }
            }
        }
        Ok(())
    }
}
