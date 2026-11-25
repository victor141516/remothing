//! Zero-copy D3D11 encode check and latency benchmark (Windows only).
//!
//! Renders synthetic desktop content into a D3D11 BGRA texture (no screen
//! capture, nothing drawn on screen), feeds it to encoders from the
//! `EncoderProvider` the way the core does (tile-row rects of changed 64x64
//! tiles), with GPU input (`load_gpu`) or system-memory input (`load_cpu`),
//! decodes every access unit with FFmpeg's software decoder and compares the
//! last decoded picture with the source.
//!
//! `cargo run --release -p remothing-encoder --features static --example d3d11_bench -- [flags]`
//!
//! - `--frames N`: frames per case (default 600)
//! - `--size WxH`: repeatable (default 1920x1080 and 3168x1440)
//! - `--scene S`: repeatable, default all: `window` (640x400 animated window
//!   over a static gradient), `noise` (every pixel changes, no usable motion),
//!   `plasma` (rt-testapp `--mode full`), `box` (rt-testapp `--mode box`),
//!   `scroll` (rt-testapp `--mode scroll`: text moving 3 px a frame)
//! - `--enc E,..`: `nvenc-gpu` (NVENC with D3D11 input: through the SDK,
//!   FFmpeg's when that does not open), `nvenc-ffmpeg` (FFmpeg's NVENC with
//!   D3D11 input), `nvenc-cpu`, `x264` (default `nvenc-gpu,nvenc-cpu,x264`)
//! - `--set k=v,..`: NVENC option overrides; every `--set` is one swept
//!   configuration (`--set default` = no overrides; default: just that). The
//!   SDK path takes `preset=p1|p2|p3` and nothing else (it falls back to
//!   FFmpeg's NVENC for other options)
//! - `--fps F`: pace frames like a capture at F Hz (default: back to back)
//! - `--capture-sim`: a second thread uses the shared immediate context like
//!   the DXGI capture thread at 144 Hz (staging readback of 20 tiles under
//!   `context_lock`) and reports its read time and lock waits
//! - `--dupl NAME`: a thread duplicates display NAME (e.g. `\\.\DISPLAY5`) on
//!   the bench device like the DXGI capture: AcquireNextFrame(50 ms), copy
//!   under `context_lock`, release; `--dupl-poll`: AcquireNextFrame(0) polled
//!   every 250 µs instead (a blocking acquire holds the device lock and
//!   stalls NVENC). Run something animated on that display meanwhile.
//! - `--gpu-priority N`: IDXGIDevice::SetGPUThreadPriority of the device (-7..7)
//! - `--no-check`: skip decoding (faster sweeps; no MAE)
//! - `--scale F`: encode at F times the size (as motion mode does for a
//!   small client: GPU video processor or CPU scaler); no MAE (the decoded
//!   picture is smaller than the source)
//! - `--codec C`: `h264` (default), `hevc` or `av1` (AV1 is decoded with
//!   libdav1d when FFmpeg has it, otherwise with NVDEC, `av1_cuvid`); `--set`
//!   applies to it
//! - `--dump DIR`: writes every case's stream to DIR (`.h264`/`.hevc` Annex
//!   B, `.obu` AV1 low-overhead OBUs), to check it with other decoders; with
//!   `--lose` also the stream the client got (`-lossy`) and the indices of
//!   its frames (`-kept.txt`)
//! - `--rate-steps N`: changes the bitrate cap every N frames (5, 12, 30, 80,
//!   20 Mbit/s in turn) and reports the keyframes after the first; no
//!   mid-stream keyframe is forced (nor with `--lose` and `--unseen`)
//! - `--lose N`: drops N frames from the decoder's input once a period (60
//!   frames, longer for long runs), then calls `invalidate_from` (a keyframe
//!   where it says no); a second decoder gets every frame, and each frame the
//!   client's decoder shows is compared with it: after a recovery they must
//!   be the same picture. Frames are acknowledged 8 frames late, as a client
//!   would. Decodes without frame threads
//! - `--unseen`: runs every case twice, the second time with the left half
//!   of the picture unseen (`set_unseen`), and reports the bytes saved
//! - `--quality`: PSNR and SSIM of the luma of every decoded frame against
//!   the source (converted with the matrix the stream signals); decodes
//!   without frame threads

#[cfg(not(windows))]
fn main() {
    eprintln!("d3d11_bench needs Windows");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    bench::run()
}

#[cfg(windows)]
#[path = "common/check.rs"]
mod check;

#[cfg(windows)]
mod bench {
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use ffmpeg_next as ff;
    use gpu_interop::d3d11::D3D11Frame;
    use parking_lot::lock_api::RawMutex as _;
    use remothing_core::encode::{Codec, EncoderProvider, EncoderRequest, VideoEncoder};
    use remothing_core::geom::Rect;
    use remothing_core::platform::GpuSurface;
    use remothing_encoder::FfmpegEncoders;
    use windows::core::Interface;
    use windows::Win32::Foundation::{HMODULE, S_OK};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIOutput1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO};

    use super::check::Check;

    /// The core's change-tracking tile: encoder rects are runs of changed
    /// tiles, one rect per tile row.
    const TILE: u32 = 64;
    /// Pre-rendered frames of full-screen scenes, played forth and back.
    const CYCLE: u32 = 16;
    /// rt-testapp's latency barcode: 64 blocks of 16x16 at the top left.
    const BAR_W: u32 = 1024;
    const BAR_H: u32 = 16;

    #[derive(Clone, Copy, PartialEq)]
    enum Scene {
        Window,
        Noise,
        Plasma,
        Box,
        Scroll,
    }

    impl Scene {
        const ALL: [Scene; 5] = [Scene::Window, Scene::Noise, Scene::Plasma, Scene::Box, Scene::Scroll];

        fn name(self) -> &'static str {
            match self {
                Scene::Window => "window",
                Scene::Noise => "noise",
                Scene::Plasma => "plasma",
                Scene::Box => "box",
                Scene::Scroll => "scroll",
            }
        }
    }

    /// An encoder to run: backend name, GPU (D3D11) input, NVENC through the SDK first.
    #[derive(Clone, Copy)]
    struct EncChoice {
        backend: &'static str,
        gpu_input: bool,
        native: bool,
    }

    struct Args {
        frames: u32,
        sizes: Vec<(u32, u32)>,
        scenes: Vec<Scene>,
        encoders: Vec<EncChoice>,
        configs: Vec<(String, Vec<(String, String)>)>,
        fps: Option<f64>,
        capture_sim: bool,
        dupl: Option<String>,
        dupl_poll: bool,
        gpu_priority: Option<i32>,
        check: bool,
        scale: f64,
        codec: Codec,
        dump: Option<std::path::PathBuf>,
        rate_steps: Option<u32>,
        lose: Option<u32>,
        unseen: bool,
        quality: bool,
    }

    fn args() -> Result<Args> {
        let mut a = Args {
            frames: 600,
            sizes: Vec::new(),
            scenes: Vec::new(),
            encoders: Vec::new(),
            configs: Vec::new(),
            fps: None,
            capture_sim: false,
            dupl: None,
            dupl_poll: false,
            gpu_priority: None,
            check: true,
            scale: 1.0,
            codec: Codec::H264,
            dump: None,
            rate_steps: None,
            lose: None,
            unseen: false,
            quality: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--frames" => a.frames = value()?.parse()?,
                "--size" => {
                    let v = value()?;
                    let (w, h) = v.split_once('x').with_context(|| format!("size {v}: expected WxH"))?;
                    a.sizes.push((w.parse()?, h.parse()?));
                }
                "--scene" => {
                    let v = value()?;
                    a.scenes.push(*Scene::ALL.iter().find(|s| s.name() == v).with_context(|| format!("unknown scene {v}"))?);
                }
                "--enc" => {
                    for e in value()?.split(',') {
                        let (backend, gpu_input, native) = match e {
                            "nvenc-gpu" => ("nvenc", true, true),
                            "nvenc-ffmpeg" => ("nvenc", true, false),
                            "nvenc-cpu" => ("nvenc", false, false),
                            "x264" => ("x264", false, false),
                            _ => bail!("unknown encoder {e}"),
                        };
                        a.encoders.push(EncChoice { backend, gpu_input, native });
                    }
                }
                "--set" => {
                    let v = value()?;
                    let opts = if v == "default" {
                        Vec::new()
                    } else {
                        v.split(',')
                            .map(|kv| {
                                let (k, v) = kv.split_once('=').with_context(|| format!("option {kv}: expected k=v"))?;
                                Ok((k.to_string(), v.to_string()))
                            })
                            .collect::<Result<_>>()?
                    };
                    a.configs.push((v, opts));
                }
                "--fps" => a.fps = Some(value()?.parse()?),
                "--capture-sim" => a.capture_sim = true,
                "--dupl" => a.dupl = Some(value()?),
                "--dupl-poll" => a.dupl_poll = true,
                "--gpu-priority" => a.gpu_priority = Some(value()?.parse()?),
                "--no-check" => a.check = false,
                "--scale" => a.scale = value()?.parse()?,
                "--codec" => {
                    let v = value()?;
                    a.codec = Codec::from_name(&v).with_context(|| format!("unknown codec {v}"))?;
                }
                "--dump" => a.dump = Some(value()?.into()),
                "--rate-steps" => a.rate_steps = Some(value()?.parse()?),
                "--lose" => a.lose = Some(value()?.parse()?),
                "--unseen" => a.unseen = true,
                "--quality" => a.quality = true,
                _ => bail!("unknown argument {arg} (see the doc comment of examples/d3d11_bench.rs)"),
            }
        }
        if a.sizes.is_empty() {
            a.sizes = vec![(1920, 1080), (3168, 1440)];
        }
        if a.scenes.is_empty() {
            a.scenes = Scene::ALL.to_vec();
        }
        if a.encoders.is_empty() {
            let choice = |backend, gpu_input, native| EncChoice { backend, gpu_input, native };
            a.encoders = vec![choice("nvenc", true, true), choice("nvenc", false, false), choice("x264", false, false)];
        }
        if a.configs.is_empty() {
            a.configs.push(("default".into(), Vec::new()));
        }
        Ok(a)
    }

    #[derive(Clone)]
    struct Image {
        w: u32,
        h: u32,
        px: Vec<u8>,
    }

    impl Image {
        fn new(w: u32, h: u32) -> Self {
            Self { w, h, px: vec![0; (w * h * 4) as usize] }
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

        /// Copies rect `r` from `src` (same size).
        fn copy_from(&mut self, src: &Image, r: Rect) {
            let stride = self.w as usize * 4;
            for y in r.y as usize..(r.y + r.h) as usize {
                let span = y * stride + r.x as usize * 4..y * stride + (r.x + r.w) as usize * 4;
                self.px[span.clone()].copy_from_slice(&src.px[span]);
            }
        }

        fn gradient(&mut self) {
            self.paint(Rect::new(0, 0, self.w, self.h), gradient);
        }

        /// rt-testapp's fake UI text (`Canvas::text(scroll)`) below the barcode bar.
        fn text(&mut self, scroll: usize) {
            let w = self.w as usize;
            for y in BAR_H as usize..self.h as usize {
                let (line, in_line) = ((y + scroll) / 22, (y + scroll) % 22);
                let mut row = vec![0xF0F0F0u32; w];
                if (6..18).contains(&in_line) {
                    let mut seed = (line as u32).wrapping_mul(2654435761) | 1;
                    let mut x = 24;
                    while x < w - 24 {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        let end = (x + 12 + (seed % 70) as usize).min(w - 24);
                        let shade = (0x20 + (seed >> 24) / 4) * 0x010101;
                        for (i, p) in row[x..end].iter_mut().enumerate() {
                            let bits = (i as u32).wrapping_mul(0x9E37_79B9) ^ (in_line as u32).wrapping_mul(0x85EB_CA6B) ^ seed;
                            if bits >> 29 > 2 {
                                *p = shade;
                            }
                        }
                        x = end + 8;
                    }
                }
                self.put_row(y, &row);
            }
        }

        fn put_row(&mut self, y: usize, row: &[u32]) {
            let dst = &mut self.px[y * self.w as usize * 4..][..row.len() * 4];
            for (d, &c) in dst.chunks_exact_mut(4).zip(row) {
                d.copy_from_slice(&[c as u8, (c >> 8) as u8, (c >> 16) as u8, 255]);
            }
        }

        /// rt-testapp's per-frame barcode; returns its rect.
        fn barcode(&mut self, n: u32) -> Rect {
            let v = (n as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let bar = Rect::new(0, 0, BAR_W.min(self.w), BAR_H);
            self.paint(bar, |x, _| if v >> (x / 16) & 1 == 1 { [255; 3] } else { [0; 3] });
            bar
        }

        /// rt-testapp's plasma (`Plasma::draw`) at time `t` below the bar.
        fn plasma(&mut self, tables: &(Vec<u32>, Vec<u32>), t: usize) {
            let (sx, sy) = tables;
            let w = self.w as usize;
            let mut row = vec![0u32; w];
            for y in BAR_H as usize..self.h as usize {
                let (vy, vy2) = (sy[(y + t * 3) & 4095], sx[(y * 2 + t) & 4095]);
                for (x, p) in row.iter_mut().enumerate() {
                    let a = sx[(x + t * 5) & 4095] + vy;
                    let b = sy[(x * 2 + 4096 - ((t * 2) & 4095)) & 4095] + vy2;
                    *p = (a << 16) | (b << 8) | ((a + b) >> 1);
                }
                self.put_row(y, &row);
            }
        }
    }

    fn gradient(x: u32, y: u32) -> [u8; 3] {
        [(x / 8) as u8, (y / 8) as u8, ((x + y) / 16) as u8]
    }

    /// Limited-range luma of `img` with the BT.601 or the BT.709 matrix.
    fn luma(img: &Image, bt601: bool) -> Vec<u8> {
        let (kr, kb) = if bt601 { (0.299, 0.114) } else { (0.2126, 0.0722) };
        let kg = 1.0 - kr - kb;
        let y = |p: &[u8]| 16.0 + 219.0 * (kr * p[2] as f64 + kg * p[1] as f64 + kb * p[0] as f64) / 255.0;
        img.px.chunks_exact(4).map(|p| y(p).round() as u8).collect()
    }

    fn plasma_tables() -> (Vec<u32>, Vec<u32>) {
        let table = |period: f64| -> Vec<u32> {
            (0..4096).map(|i| ((((i as f64) * std::f64::consts::TAU / period).sin() + 1.0) * 63.0) as u32).collect()
        };
        (table(700.0), table(500.0))
    }

    /// Runs of changed tiles, one rect per tile row (as the core hands them to encoders).
    fn tile_rects(dirty: &[Rect], w: u32, h: u32) -> Vec<Rect> {
        let tiles_x = w.div_ceil(TILE);
        let mut marked = vec![false; (tiles_x * h.div_ceil(TILE)) as usize];
        for r in dirty.iter().filter(|r| r.w > 0 && r.h > 0) {
            for ty in r.y / TILE..=(r.y + r.h - 1) / TILE {
                for tx in r.x / TILE..=(r.x + r.w - 1) / TILE {
                    marked[(ty * tiles_x + tx) as usize] = true;
                }
            }
        }
        let mut out = Vec::new();
        for (ty, row) in marked.chunks(tiles_x as usize).enumerate() {
            let mut tx = 0;
            while tx < row.len() {
                if !row[tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < row.len() && row[tx] {
                    tx += 1;
                }
                let (x, y) = (start as u32 * TILE, ty as u32 * TILE);
                out.push(Rect::new(x, y, (tx as u32 * TILE).min(w) - x, (y + TILE).min(h) - y));
            }
        }
        out
    }

    /// Scene content: full-screen scenes are pre-rendered (GPU copies per
    /// frame, like the DXGI capture), partial ones painted on the CPU and
    /// uploaded per changed rect.
    struct Source {
        scene: Scene,
        img: Image,
        background: Image,
        cycle: Vec<(Image, ID3D11Texture2D)>,
        current: usize,
        box_x: u32,
        box_dx: i32,
        /// `--quality`: luma of the cycle's images, in BT.601 or not.
        lumas: Vec<Option<(bool, Rc<Vec<u8>>)>>,
    }

    impl Source {
        fn new(gpu: &Gpu, scene: Scene, w: u32, h: u32) -> Result<Self> {
            let mut img = Image::new(w, h);
            let mut cycle = Vec::new();
            match scene {
                Scene::Window => img.gradient(),
                Scene::Box => {
                    img.text(0);
                    img.barcode(0);
                }
                Scene::Noise | Scene::Plasma | Scene::Scroll => {
                    let tables = plasma_tables();
                    for t in 0..CYCLE {
                        let mut f = Image::new(w, h);
                        if scene == Scene::Noise {
                            // Every pixel changes without being a translation,
                            // so motion search cannot make it cheap.
                            f.paint(Rect::new(0, 0, w, h), |x, y| {
                                let v = (x * x / 97 + y * y / 89 + t * 5) as u8;
                                [v, v ^ (x / 16) as u8, ((y / 4) as u8).wrapping_add((t * 3) as u8)]
                            });
                        } else if scene == Scene::Scroll {
                            f.text(t as usize * 3);
                            f.barcode(t);
                        } else {
                            f.plasma(&tables, t as usize + 1);
                            f.barcode(t);
                        }
                        let tex = gpu.texture(w, h, D3D11_BIND_SHADER_RESOURCE.0 as u32, Some(&f))?;
                        cycle.push((f, tex));
                    }
                    img = cycle[0].0.clone();
                }
            }
            let lumas = vec![None; cycle.len()];
            Ok(Self { scene, background: img.clone(), img, cycle, current: 0, box_x: 0, box_dx: 8, lumas })
        }

        /// Source image of the current frame.
        fn image(&self) -> &Image {
            if self.cycle.is_empty() {
                &self.img
            } else {
                &self.cycle[self.current].0
            }
        }

        /// Limited-range luma of the current image, as an encoder converting
        /// with BT.601 (`bt601`) or BT.709 makes it.
        fn luma(&mut self, bt601: bool) -> Rc<Vec<u8>> {
            if self.cycle.is_empty() {
                return Rc::new(luma(&self.img, bt601));
            }
            let (i, img) = (self.current, &self.cycle[self.current].0);
            match &self.lumas[i] {
                Some((b, l)) if *b == bt601 => l.clone(),
                _ => {
                    let l = Rc::new(luma(img, bt601));
                    self.lumas[i] = Some((bt601, l.clone()));
                    l
                }
            }
        }

        /// Puts the whole current image into the capture texture.
        fn upload_all(&self, frame: &D3D11Frame) {
            let (w, h) = (self.img.w, self.img.h);
            self.update(frame, &[Rect::new(0, 0, w, h)]);
        }

        /// Advances to frame `n` (> 0); returns the encoder rects.
        fn step(&mut self, n: u32) -> Vec<Rect> {
            let (w, h) = (self.img.w, self.img.h);
            let dirty = match self.scene {
                Scene::Window => {
                    let (bw, bh) = (640, 400);
                    let pos = |n: u32| {
                        let t = n * 6;
                        Rect::new(t % (w - bw), (t / 2) % (h - bh), bw, bh)
                    };
                    let (old, new) = (pos(n - 1), pos(n));
                    self.img.paint(old, gradient);
                    self.img.paint(new, |x, y| {
                        let v = ((x - new.x) * 3 + (y - new.y) * 5 + n * 7) as u8;
                        [v, v.wrapping_mul(3), 255 - v]
                    });
                    vec![old, new]
                }
                Scene::Box => {
                    // rt-testapp: 256x256 box moving 8 px per frame, bouncing, hue cycling.
                    let max = w - 256;
                    let old = self.box_x;
                    let nx = (self.box_x as i32 + self.box_dx).clamp(0, max as i32) as u32;
                    if nx == 0 || nx == max {
                        self.box_dx = -self.box_dx;
                    }
                    self.box_x = nx;
                    let by = h / 2 - 128;
                    let x0 = old.min(nx);
                    let span = Rect::new(x0, by, (old.max(nx) + 256 - x0).min(w - x0), 256);
                    self.img.copy_from(&self.background, span);
                    let hue = (n * 3) & 0xFF;
                    let c = 0x3060C0 | hue << 16;
                    self.img.paint(Rect::new(nx, by, 256, 256), |_, _| [c as u8, (c >> 8) as u8, (c >> 16) as u8]);
                    vec![self.img.barcode(n), span]
                }
                Scene::Noise | Scene::Plasma | Scene::Scroll => {
                    // Forth and back: the motion stays smooth at the turns.
                    let i = n % (2 * CYCLE - 2);
                    self.current = (if i < CYCLE { i } else { 2 * CYCLE - 2 - i }) as usize;
                    vec![Rect::new(0, 0, w, h)]
                }
            };
            tile_rects(&dirty, w, h)
        }

        /// Writes `rects` of the current image into the capture texture.
        fn update(&self, frame: &D3D11Frame, rects: &[Rect]) {
            frame.with_context(|ctx| {
                for r in rects {
                    let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                    if let Some((_, tex)) = self.cycle.get(self.current) {
                        unsafe { ctx.CopySubresourceRegion(&frame.texture, 0, r.x, r.y, 0, tex, 0, Some(&b)) };
                    } else {
                        let stride = self.img.w as usize * 4;
                        let src = self.img.px[r.y as usize * stride + r.x as usize * 4..].as_ptr();
                        unsafe { ctx.UpdateSubresource(&frame.texture, 0, Some(&b), src.cast(), stride as u32, 0) };
                    }
                }
            });
        }
    }

    struct Gpu {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        adapter: String,
    }

    impl Gpu {
        fn new(priority: Option<i32>) -> Result<Gpu> {
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
                let dxgi = device.cast::<IDXGIDevice>()?;
                if let Some(p) = priority {
                    dxgi.SetGPUThreadPriority(p).context("SetGPUThreadPriority")?;
                }
                let desc = dxgi.GetAdapter()?.GetDesc()?;
                let adapter = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
                Ok(Gpu { device, context: context.context("no context")?, adapter })
            }
        }

        fn texture(&self, w: u32, h: u32, bind: u32, init: Option<&Image>) -> Result<ID3D11Texture2D> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                ..Default::default()
            };
            let data = init.map(|img| D3D11_SUBRESOURCE_DATA {
                pSysMem: img.px.as_ptr().cast(),
                SysMemPitch: img.w * 4,
                SysMemSlicePitch: 0,
            });
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, data.as_ref().map(|d| d as *const _), Some(&mut tex))? };
            tex.context("no texture")
        }

        fn surface(&self, w: u32, h: u32) -> Result<Arc<D3D11Frame>> {
            let tex = self.texture(w, h, D3D11_BIND_SHADER_RESOURCE.0 as u32, None)?;
            Ok(Arc::new(D3D11Frame::new(self.device.clone(), self.context.clone(), tex, w, h)))
        }
    }

    fn pct(v: &mut [Duration], p: f64) -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort();
        v[((v.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
    }

    /// What a capture thread's readback costs while encoders run.
    #[derive(Default)]
    struct CaptureStats {
        /// Whole readback (copy, wait for the GPU, map, rows, unmap).
        read: Vec<Duration>,
        /// Time spent waiting for `context_lock` per readback.
        lock_wait: Vec<Duration>,
        /// Time spent inside D3D11 calls under the lock per readback.
        lock_hold: Vec<Duration>,
    }

    /// Imitates `DxgiSession::read` at 144 Hz: staging copy of 20 tiles,
    /// event query polled under the lock, map, row copies, unmap.
    fn capture_sim(frame: Arc<D3D11Frame>, stop: Arc<AtomicBool>) -> Result<CaptureStats> {
        let (w, h) = (frame.width, frame.height);
        let staging = {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut t = None;
            unsafe { frame.device.CreateTexture2D(&desc, None, Some(&mut t))? };
            t.context("no staging texture")?
        };
        let query = {
            let mut q = None;
            unsafe { frame.device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q))? };
            q.context("no query")?
        };
        let (cx, cy) = (w / 2 / TILE * TILE, h / 3 / TILE * TILE);
        let tiles: Vec<Rect> = (0..20).map(|i| Rect::new(cx + (i % 5) * TILE, cy + (i / 5) * TILE, TILE, TILE)).collect();
        let mut dst = vec![0u8; (w * h * 4) as usize];
        let mut stats = CaptureStats::default();
        let interval = Duration::from_secs_f64(1.0 / 144.0);
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            next += interval;
            let (mut wait, mut hold) = (Duration::ZERO, Duration::ZERO);
            let mut locked = |f: &mut dyn FnMut(&ID3D11DeviceContext)| {
                let t0 = Instant::now();
                frame.context_lock.lock();
                let t1 = Instant::now();
                f(&frame.context);
                // SAFETY: locked just above on this thread.
                unsafe { frame.context_lock.unlock() };
                wait += t1 - t0;
                hold += t1.elapsed();
            };
            let start = Instant::now();
            locked(&mut |ctx| unsafe {
                for r in &tiles {
                    let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                    ctx.CopySubresourceRegion(&staging, 0, r.x, r.y, 0, &frame.texture, 0, Some(&b));
                }
                ctx.End(&query);
                ctx.Flush();
            });
            loop {
                let mut done = S_OK;
                locked(&mut |ctx| unsafe {
                    done = (Interface::vtable(ctx).GetData)(Interface::as_raw(ctx), query.as_raw(), std::ptr::null_mut(), 0, 0);
                });
                if done == S_OK {
                    break;
                }
                done.ok()?;
                std::thread::yield_now();
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let mut map = Ok(());
            locked(&mut |ctx| map = unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) });
            map?;
            for r in &tiles {
                for y in r.y as usize..(r.y + r.h) as usize {
                    let (x, len) = (r.x as usize * 4, r.w as usize * 4);
                    let row = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize + x), len) };
                    dst[y * w as usize * 4 + x..][..len].copy_from_slice(row);
                }
            }
            locked(&mut |ctx| unsafe { ctx.Unmap(&staging, 0) });
            stats.read.push(start.elapsed());
            stats.lock_wait.push(wait);
            stats.lock_hold.push(hold);
        }
        Ok(stats)
    }

    /// Desktop duplication of `display` on the bench device, run the way the
    /// DXGI capture thread runs it: wait for a frame, copy it under the
    /// context lock, ReleaseFrame. `poll`: AcquireNextFrame(0) with 250 µs
    /// sleeps in between instead of a blocking AcquireNextFrame(50 ms), which
    /// holds the device lock while it waits. Returns the captured frames.
    fn dupl_sim(frame: Arc<D3D11Frame>, display: &str, poll: bool, stop: Arc<AtomicBool>) -> Result<u32> {
        unsafe {
            let adapter = frame.device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let output = (0..)
                .map_while(|i| adapter.EnumOutputs(i).ok())
                .find(|o| {
                    let name = o.GetDesc().map(|d| String::from_utf16_lossy(&d.DeviceName)).unwrap_or_default();
                    name.trim_end_matches('\0') == display
                })
                .with_context(|| format!("no output {display} on the bench adapter"))?;
            let dupl = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&frame.device)?;
            let mode = dupl.GetDesc().ModeDesc;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mode.Width,
                Height: mode.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut scratch = None;
            frame.device.CreateTexture2D(&desc, None, Some(&mut scratch))?;
            let scratch = scratch.context("no texture")?;
            let mut frames = 0;
            while !stop.load(Ordering::Relaxed) {
                let (mut info, mut res) = (DXGI_OUTDUPL_FRAME_INFO::default(), None);
                match dupl.AcquireNextFrame(if poll { 0 } else { 50 }, &mut info, &mut res) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                        if poll {
                            std::thread::sleep(Duration::from_micros(250));
                        }
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
                if let (Some(res), true) = (res, info.LastPresentTime != 0) {
                    let img: ID3D11Texture2D = res.cast()?;
                    frame.with_context(|ctx| ctx.CopyResource(&scratch, &img));
                    frames += 1;
                }
                dupl.ReleaseFrame()?;
            }
            Ok(frames)
        }
    }

    struct Case<'a> {
        frame: &'a Arc<D3D11Frame>,
        gpu_input: bool,
        frames: u32,
        pace: Option<Duration>,
        capture_sim: bool,
        dupl: Option<&'a str>,
        dupl_poll: bool,
        check: bool,
        /// File the encoded stream goes to.
        dump: Option<std::path::PathBuf>,
        rate_steps: Option<u32>,
        lose: Option<u32>,
        /// The left half of the picture is unseen (`set_unseen`).
        unseen: bool,
        quality: bool,
        /// An IDR mid-stream too, as for a joining client: not where something is counted
        /// that it would disturb (`--rate-steps`, `--lose`, both `--unseen` runs).
        mid_key: bool,
    }

    /// Caps `--rate-steps` cycles through, Mbit/s at 60 fps.
    const RATE_STEPS: [u64; 5] = [5, 12, 30, 80, 20];
    /// `--lose`: frames are acknowledged this many frames after they are coded.
    const ACK_DELAY: u64 = 8;

    #[derive(Default)]
    struct EncodeStats {
        upload: Vec<Duration>,
        encode: Vec<Duration>,
        total: Vec<Duration>,
        bytes: usize,
        keys: u32,
        rate_changes: u32,
        /// `--rate-steps`: bytes and frames coded under each cap (Mbit/s).
        per_cap: Vec<(u64, usize, u32)>,
        /// `--lose`: runs of lost frames, the ones predicted past without a
        /// keyframe, the size of those recovery frames and of the other P frames.
        losses: u32,
        recovered: u32,
        recovery_bytes: Vec<usize>,
        p_bytes: Vec<usize>,
        key_bytes: Vec<usize>,
    }

    /// `path` with `suffix` before its extension.
    fn beside(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        path.with_file_name(format!("{stem}{suffix}.{ext}"))
    }

    fn encode_frames(c: &Case, enc: &mut dyn VideoEncoder, src: &mut Source, check: &mut Option<Check>) -> Result<EncodeStats> {
        let (w, h) = (src.img.w, src.img.h);
        let surface: GpuSurface = c.frame.clone();
        let mut st = EncodeStats::default();
        let start = Instant::now();
        let (mut stream, mut lossy, mut kept) = (Vec::new(), Vec::new(), Vec::new());
        // `--lose N`: a run of N lost frames from the middle of every period but the first.
        let period = c.lose.map(|n| (n as u64 + 40).max(60));
        let mut reached = Vec::new();
        let mut lost_from = None;
        if c.unseen {
            enc.set_unseen(&[Rect::new(0, 0, w / 2, h)]);
        }
        for n in 0..c.frames {
            if let Some(p) = c.pace {
                let due = start + p * n;
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
            }
            let rects = if n == 0 {
                src.upload_all(c.frame);
                vec![Rect::new(0, 0, w, h)]
            } else {
                let rects = src.step(n);
                if c.gpu_input {
                    src.update(c.frame, &rects);
                }
                rects
            };
            let t0 = Instant::now();
            if c.gpu_input {
                enc.load_gpu(&surface, &rects)?;
            } else {
                enc.load_cpu(&src.image().px, w as usize * 4, 0, &rects)?;
            }
            let step = c.rate_steps.map(|s| RATE_STEPS[(n / s) as usize % RATE_STEPS.len()]);
            if let Some(cap) = step.filter(|_| n > 0 && n % c.rate_steps.unwrap_or(1) == 0) {
                enc.set_max_bitrate(cap * 1_000_000, 60);
                st.rate_changes += 1;
            }
            let i = n as u64;
            // The last run ends before the stream does, so the client's last picture is current.
            let lost = match (c.lose, period) {
                (Some(len), Some(p)) => {
                    let start = i - i % p + p / 2;
                    i >= p && (start..start + len as u64).contains(&i) && start + len as u64 + 1 < c.frames as u64
                }
                _ => false,
            };
            // An IDR at start (and one mid-stream).
            let mut force_key = n == 0 || (c.mid_key && n == c.frames / 2);
            // The client reports the loss once frames reach it again.
            if let (false, Some(first)) = (lost, lost_from) {
                lost_from = None;
                st.losses += 1;
                force_key |= !enc.invalidate_from(first);
            }
            let t1 = Instant::now();
            let out = enc.encode(force_key)?;
            let t2 = Instant::now();
            st.upload.push(t1 - t0);
            st.encode.push(t2 - t1);
            st.total.push(t2 - t0);
            st.bytes += out.data.len();
            st.keys += out.key as u32;
            // Under the first cap the encoder is still uncapped.
            if let Some(cap) = step.filter(|_| n >= c.rate_steps.unwrap_or(0)) {
                match st.per_cap.iter_mut().find(|(c, ..)| *c == cap) {
                    Some((_, bytes, frames)) => (*bytes, *frames) = (*bytes + out.data.len(), *frames + 1),
                    None => st.per_cap.push((cap, out.data.len(), 1)),
                }
            }
            if out.recovers.is_some() {
                st.recovered += 1;
                st.recovery_bytes.push(out.data.len());
            } else if out.key {
                st.key_bytes.push(out.data.len());
            } else {
                st.p_bytes.push(out.data.len());
            }
            if lost && lost_from.is_none() {
                lost_from = Some(out.index);
            }
            reached.push(!lost);
            if c.lose.is_some() && out.index >= ACK_DELAY && reached[(out.index - ACK_DELAY) as usize] {
                enc.acknowledge(out.index - ACK_DELAY);
            }
            if let Some(check) = check {
                let source = c.quality.then(|| src.luma(enc.bt601()));
                check.feed(out.index, &out.data, !lost, source)?;
            }
            if c.dump.is_some() {
                stream.extend_from_slice(&out.data);
                if !lost {
                    lossy.extend_from_slice(&out.data);
                    kept.push(out.index.to_string());
                }
            }
        }
        if let Some(path) = &c.dump {
            std::fs::write(path, &stream).with_context(|| format!("write {}", path.display()))?;
            if c.lose.is_some() {
                std::fs::write(beside(path, "-lossy"), &lossy)?;
                std::fs::write(beside(path, "-kept").with_extension("txt"), kept.join("\n"))?;
            }
        }
        Ok(st)
    }

    /// Median of `v` in KiB.
    fn median_kib(v: &mut [usize]) -> f64 {
        v.sort_unstable();
        v.get(v.len() / 2).map_or(f64::NAN, |&b| b as f64 / 1024.0)
    }

    /// One table row, and the KiB per frame.
    fn run_case(c: &Case, enc: &mut dyn VideoEncoder, src: &mut Source) -> Result<(String, f64)> {
        let mut check = if c.check { Some(Check::new(enc.codec(), c.lose.is_some(), c.quality)?) } else { None };
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let (st, sim, dupl) = std::thread::scope(|s| {
            let sim = c.capture_sim.then(|| {
                let (frame, stop) = (c.frame.clone(), stop.clone());
                s.spawn(move || capture_sim(frame, stop))
            });
            let dupl = c.dupl.map(|d| {
                let (frame, stop) = (c.frame.clone(), stop.clone());
                s.spawn(move || dupl_sim(frame, d, c.dupl_poll, stop))
            });
            let st = encode_frames(c, enc, src, &mut check);
            stop.store(true, Ordering::Relaxed);
            (st, sim.map(|h| h.join().expect("capture sim panicked")), dupl.map(|h| h.join().expect("dupl panicked")))
        });
        let elapsed = started.elapsed().as_secs_f64();
        let mut st = st?;
        let image = src.image();
        let decoded = check.map(|check| check.finish(&image.px, image.w, image.h)).transpose()?;
        let (count, mae) = match &decoded {
            Some(d) => (format!("{}/{}", d.decoded, c.frames), format!("{:.2}", d.mae)),
            None => ("-".into(), "-".into()),
        };
        let kib = st.bytes as f64 / c.frames as f64 / 1024.0;
        let mut row = format!(
            "| {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {kib:.1} | {} | {count} | {mae} |",
            pct(&mut st.upload, 0.5),
            pct(&mut st.encode, 0.5),
            pct(&mut st.encode, 0.9),
            pct(&mut st.encode, 0.99),
            pct(&mut st.total, 0.99),
            st.keys,
        );
        if let Some(sim) = sim {
            let mut cs = sim?;
            row += &format!(
                " {:.2} | {:.2} | {:.3} | {:.3} |",
                pct(&mut cs.read, 0.5),
                pct(&mut cs.read, 0.99),
                pct(&mut cs.lock_wait, 0.99),
                pct(&mut cs.lock_hold, 0.99),
            );
        }
        if let Some(d) = dupl {
            row += &format!(" dupl {:.0} fps |", d? as f64 / elapsed);
        }
        if c.rate_steps.is_some() {
            st.per_cap.sort_unstable();
            let caps: Vec<String> = st.per_cap.iter().map(|(cap, bytes, frames)| format!("{cap}: {:.1}", *bytes as f64 / *frames as f64 / 1024.0)).collect();
            row += &format!(" {} | {} | {} |", st.rate_changes, st.keys.saturating_sub(1), caps.join(", "));
        }
        if c.lose.is_some() {
            let client = decoded.as_ref().map_or("- | - | - | -".into(), |d| format!("{} | {} | {:.3} | {}", d.shown, d.differing, d.worst, d.errors));
            row += &format!(
                " {} | {} | {} | {:.1} | {:.1} | {:.1} | {client} |",
                st.losses,
                st.recovered,
                st.keys.saturating_sub(1),
                median_kib(&mut st.recovery_bytes),
                median_kib(&mut st.key_bytes),
                median_kib(&mut st.p_bytes),
            );
        }
        if c.quality {
            let q = decoded.and_then(|d| d.quality).map_or("- | -".into(), |(p, s)| format!("{p:.2} | {s:.4}"));
            row += &format!(" {q} |");
        }
        Ok((row, kib))
    }

    /// Reports encoder options the configuration relies on that this FFmpeg
    /// build does not know (FFmpeg ignores unknown ones).
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
        let args = args()?;
        let mut provider = FfmpegEncoders::new()?;
        let gpu = Gpu::new(args.gpu_priority)?;
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

        let pace = args.fps.map(|f| Duration::from_secs_f64(1.0 / f));
        let mut header = String::from(
            "| encoder | config | size | scene | upload p50 ms | encode p50 ms | encode p90 ms | encode p99 ms | total p99 ms | KiB/frame | keys | decoded | MAE |",
        );
        let mut rule = String::from("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        if args.capture_sim {
            header += " cap read p50 ms | cap read p99 ms | cap lock wait p99 ms | cap lock hold p99 ms |";
            rule += "---|---|---|---|";
        }
        if args.rate_steps.is_some() {
            header += " rate changes | keys after first | KiB/frame per cap (Mbit/s: KiB) |";
            rule += "---|---|---|";
        }
        if args.lose.is_some() {
            header += " losses | recovered | keys after first | recovery KiB p50 | key KiB p50 | P KiB p50 | client pictures | differing | worst MAE | refused |";
            rule += "---|---|---|---|---|---|---|---|---|---|";
        }
        if args.quality {
            header += " PSNR-Y dB | SSIM-Y |";
            rule += "---|---|";
        }
        for &(w, h) in &args.sizes {
            let frame = gpu.surface(w, h)?;
            let gs: GpuSurface = frame.clone();
            for backend in ["nvenc", "amf", "qsv", "x264"] {
                // Uncapped: the encoder clamps to its highest supported cap.
                let req = EncoderRequest {
                    codec: args.codec,
                    width: w,
                    height: h,
                    output: (w, h),
                    preferred: Some(backend.into()),
                    surface: Some(gs.clone()),
                    max_bitrate: u64::MAX,
                    fps: 60,
                    two_pass: false,
                    bt709: false,
                };
                let e = provider.create(&req)?;
                println!("{w}x{h} prefer {backend}: opened {:?}, fallback: {:?}", e.name(), e.fallback_reason());
            }
            println!("{header}\n{rule}");
            for &scene in &args.scenes {
                let mut src = Source::new(&gpu, scene, w, h)?;
                for &EncChoice { backend, gpu_input, native } in &args.encoders {
                    for (label, opts) in &args.configs {
                        let mut seen_kib = None;
                        for unseen in if args.unseen { &[false, true][..] } else { &[false][..] } {
                            provider.set_options("nvenc", args.codec, opts)?;
                            provider.set_native_nvenc(native);
                            let req = EncoderRequest {
                                codec: args.codec,
                                width: w,
                                height: h,
                                output: ((w as f64 * args.scale) as u32 & !1, (h as f64 * args.scale) as u32 & !1),
                                preferred: Some(backend.into()),
                                surface: gpu_input.then(|| gs.clone()),
                                max_bitrate: u64::MAX,
                                fps: 60,
                                // Swept with `--set multipass=qres` instead.
                                two_pass: false,
                                bt709: false,
                            };
                            let mut e = provider.create(&req)?;
                            let config = if *unseen { format!("{label}, half unseen") } else { label.clone() };
                            let prefix = format!("| {} | {config} | {w}x{h} | {} ", e.name(), scene.name());
                            if !e.name().starts_with(backend) || e.gpu_input() != gpu_input {
                                println!("{prefix}| skipped: {:?} |", e.fallback_reason());
                                continue;
                            }
                            if native && !e.name().contains("sdk") {
                                println!("(SDK path did not open: {:?})", e.fallback_reason());
                            }
                            let tag = match (native, gpu_input) {
                                (true, _) => format!("{backend}-sdk"),
                                (false, true) => format!("{backend}-gpu"),
                                (false, false) => format!("{backend}-cpu"),
                            };
                            let case = Case {
                                frame: &frame,
                                gpu_input,
                                frames: args.frames,
                                pace,
                                capture_sim: args.capture_sim,
                                dupl: args.dupl.as_deref(),
                                dupl_poll: args.dupl_poll,
                                check: args.check && args.scale == 1.0,
                                dump: args.dump.as_ref().map(|dir| {
                                    let ext = match args.codec {
                                        Codec::H264 => "h264",
                                        Codec::Hevc => "hevc",
                                        Codec::Av1 => "obu",
                                    };
                                    let label: String = config.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
                                    dir.join(format!("{}-{tag}-{w}x{h}-{}-{label}.{ext}", args.codec.name(), scene.name()))
                                }),
                                rate_steps: args.rate_steps,
                                lose: args.lose,
                                unseen: *unseen,
                                quality: args.quality,
                                mid_key: args.rate_steps.is_none() && args.lose.is_none() && !args.unseen,
                            };
                            let (row, kib) = run_case(&case, e.as_mut(), &mut src)?;
                            println!("{prefix}{row}");
                            match seen_kib {
                                None => seen_kib = Some(kib),
                                Some(seen) => println!("half unseen: {kib:.1} KiB/frame against {seen:.1}, {:.0} % saved", 100.0 * (1.0 - kib / seen)),
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
