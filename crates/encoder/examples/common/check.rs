//! Decoding check shared by the encoder examples (`d3d11_bench` on Windows, `nvenc_check` on
//! Linux): decodes what an encoder produced with FFmpeg's decoders and compares it with the
//! source, and with `--lose` the pictures a client that missed frames shows against the
//! loss-free decode of the same frames.

use std::collections::VecDeque;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use ffmpeg_next as ff;
use remothing_core::encode::Codec;

/// A decoder whose pictures carry the index of the access unit that made them.
pub struct Decoder(ff::decoder::Video);

impl Decoder {
    /// `threaded`: frame threads, which hold pictures back.
    pub fn new(codec: Codec, threaded: bool) -> Result<Self> {
        let codec = match codec {
            Codec::H264 => ff::decoder::find(ff::codec::Id::H264).context("no h264 decoder")?,
            Codec::Hevc => ff::decoder::find(ff::codec::Id::HEVC).context("no hevc decoder")?,
            // FFmpeg's own AV1 decoder needs a hardware decoder underneath:
            // dav1d where the build has it, NVDEC otherwise.
            Codec::Av1 => ff::decoder::find_by_name("libdav1d")
                .or_else(|| ff::decoder::find_by_name("av1_cuvid"))
                .context("no AV1 decoder in this FFmpeg: --no-check")?,
        };
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        if threaded {
            ctx.set_threading(ff::threading::Config { kind: ff::threading::Type::Frame, ..Default::default() });
        }
        Ok(Self(ctx.decoder().video()?))
    }

    fn feed(&mut self, index: u64, au: &[u8], out: &mut Vec<ff::frame::Video>) -> Result<()> {
        if !au.is_empty() {
            let mut packet = ff::Packet::copy(au);
            packet.set_pts(Some(index as i64));
            packet.set_dts(Some(index as i64));
            self.0.send_packet(&packet)?;
        }
        self.receive(out);
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<ff::frame::Video>) -> Result<()> {
        self.0.send_eof()?;
        self.receive(out);
        Ok(())
    }

    fn receive(&mut self, out: &mut Vec<ff::frame::Video>) {
        loop {
            let mut f = ff::frame::Video::empty();
            if self.0.receive_frame(&mut f).is_err() {
                break;
            }
            out.push(f);
        }
    }
}

/// Index of the access unit a picture came from.
fn index_of(f: &ff::frame::Video) -> i64 {
    f.pts().or(f.timestamp()).unwrap_or(-1)
}

/// Mean absolute difference of two pictures' luma.
fn luma_mae(a: &ff::frame::Video, b: &ff::frame::Video) -> f64 {
    let (w, h) = (a.width().min(b.width()) as usize, a.height().min(b.height()) as usize);
    let (sa, sb) = (a.stride(0), b.stride(0));
    let (da, db) = (a.data(0), b.data(0));
    let sum: u64 = (0..h).map(|y| da[y * sa..][..w].iter().zip(&db[y * sb..][..w]).map(|(p, q)| p.abs_diff(*q) as u64).sum::<u64>()).sum();
    sum as f64 / (w * h) as f64
}

/// PSNR (dB, 99 for identical) and mean SSIM over 8x8 blocks of a decoded luma plane against
/// the source's.
fn psnr_ssim(f: &ff::frame::Video, src: &[u8]) -> (f64, f64) {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (w, h, stride, data) = (f.width() as usize, f.height() as usize, f.stride(0), f.data(0));
    let mut se = 0u64;
    for y in 0..h {
        se += data[y * stride..][..w].iter().zip(&src[y * w..][..w]).map(|(p, q)| (p.abs_diff(*q) as u64).pow(2)).sum::<u64>();
    }
    let mse = se as f64 / (w * h) as f64;
    let psnr = if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() };
    let (mut sum, mut blocks) = (0.0, 0);
    for by in (0..h.saturating_sub(7)).step_by(8) {
        for bx in (0..w.saturating_sub(7)).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in by..by + 8 {
                for (&p, &q) in data[y * stride + bx..][..8].iter().zip(&src[y * w + bx..][..8]) {
                    let (p, q) = (p as u64, q as u64);
                    (sa, sb, saa, sbb, sab) = (sa + p, sb + q, saa + p * p, sbb + q * q, sab + p * q);
                }
            }
            let (ma, mb) = (sa as f64 / 64.0, sb as f64 / 64.0);
            let (va, vb, cov) = (saa as f64 / 64.0 - ma * ma, sbb as f64 / 64.0 - mb * mb, sab as f64 / 64.0 - ma * mb);
            sum += (2.0 * ma * mb + C1) * (2.0 * cov + C2) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            blocks += 1;
        }
    }
    (psnr, sum / blocks.max(1) as f64)
}

/// What decoding the stream showed.
pub struct Decoded {
    pub decoded: usize,
    /// The client's last picture against the source.
    pub mae: f64,
    /// `--quality`: mean luma PSNR and SSIM.
    pub quality: Option<(f64, f64)>,
    /// `--lose`: pictures the client's decoder showed, how many differ from the loss-free
    /// decode, the largest luma MAE among them, packets it refused.
    pub shown: usize,
    pub differing: usize,
    pub worst: f64,
    pub errors: usize,
}

/// Decodes the produced stream and checks it against the source: the last picture's MAE; with
/// `quality` every picture's PSNR and SSIM; with `lose` every picture the client's decoder
/// shows, against the loss-free decode of the same frame.
pub struct Check {
    full: Decoder,
    /// The client's decoder (`lose`).
    lossy: Option<Decoder>,
    /// Pictures of `full` the client's decoder has not shown yet.
    pending: VecDeque<ff::frame::Video>,
    /// `quality`: source luma of frames not decoded yet, and the scores.
    sources: Option<VecDeque<(i64, Rc<Vec<u8>>)>>,
    scores: Vec<(f64, f64)>,
    decoded: usize,
    shown: usize,
    differing: usize,
    worst: f64,
    errors: usize,
    last: ff::frame::Video,
}

impl Check {
    pub fn new(codec: Codec, lose: bool, quality: bool) -> Result<Self> {
        Ok(Self {
            full: Decoder::new(codec, !lose && !quality)?,
            lossy: lose.then(|| Decoder::new(codec, false)).transpose()?,
            pending: VecDeque::new(),
            sources: quality.then(VecDeque::new),
            scores: Vec::new(),
            decoded: 0,
            shown: 0,
            differing: 0,
            worst: 0.0,
            errors: 0,
            last: ff::frame::Video::empty(),
        })
    }

    /// Access unit `index`; `reached`: the client's decoder gets it too; `source`: its luma
    /// (`quality`).
    pub fn feed(&mut self, index: u64, au: &[u8], reached: bool, source: Option<Rc<Vec<u8>>>) -> Result<()> {
        if let (Some(sources), Some(luma)) = (&mut self.sources, source) {
            sources.push_back((index as i64, luma));
        }
        let mut out = Vec::new();
        self.full.feed(index, au, &mut out)?;
        self.take_full(out);
        let mut out = Vec::new();
        if let (Some(lossy), true) = (&mut self.lossy, reached) {
            // A packet the decoder refuses is a failed recovery: counted, not fatal.
            self.errors += lossy.feed(index, au, &mut out).is_err() as usize;
        }
        self.take_lossy(out);
        Ok(())
    }

    fn take_full(&mut self, frames: Vec<ff::frame::Video>) {
        for f in frames {
            self.decoded += 1;
            if let Some(sources) = &mut self.sources {
                while sources.front().is_some_and(|(i, _)| *i < index_of(&f)) {
                    sources.pop_front();
                }
                if let Some((_, luma)) = sources.pop_front() {
                    self.scores.push(psnr_ssim(&f, &luma));
                }
            }
            if self.lossy.is_some() {
                self.pending.push_back(f);
            } else {
                self.last = f;
            }
        }
    }

    fn take_lossy(&mut self, frames: Vec<ff::frame::Video>) {
        for f in frames {
            self.shown += 1;
            let index = index_of(&f);
            while self.pending.front().is_some_and(|p| index_of(p) < index) {
                self.pending.pop_front();
            }
            let mae = match self.pending.pop_front() {
                Some(p) if index_of(&p) == index => luma_mae(&p, &f),
                // No loss-free picture of that frame: something else entirely.
                _ => 255.0,
            };
            if mae > 0.0 {
                self.differing += 1;
                self.worst = self.worst.max(mae);
            }
            self.last = f;
        }
    }

    /// Flushes the decoders; the MAE (mean absolute per-channel difference) of the client's
    /// last picture against `bgrx`, a `w` x `h` BGRX image.
    pub fn finish(mut self, bgrx: &[u8], w: u32, h: u32) -> Result<Decoded> {
        let mut out = Vec::new();
        self.full.flush(&mut out)?;
        self.take_full(out);
        let mut out = Vec::new();
        if let Some(lossy) = &mut self.lossy {
            lossy.flush(&mut out)?;
        }
        self.take_lossy(out);
        if self.decoded == 0 {
            bail!("nothing decoded");
        }
        if (self.last.width(), self.last.height()) != (w, h) {
            bail!("decoded {}x{}, expected {w}x{h}", self.last.width(), self.last.height());
        }
        let mut bgr = ff::frame::Video::empty();
        let mut sws = ff::software::scaling::Context::get(
            self.last.format(),
            w,
            h,
            ff::format::Pixel::BGRZ,
            w,
            h,
            ff::software::scaling::Flags::BILINEAR,
        )?;
        // Convert with the matrix the stream signals, as browsers do (swscale assumes BT.601
        // otherwise).
        if self.last.color_space() == ff::color::Space::BT709 {
            // SAFETY: the context is valid; details are read and written back with the matrix changed.
            unsafe {
                let (mut inv, mut src_range, mut table, mut dst_range) = (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
                let (mut brightness, mut contrast, mut saturation) = (0, 0, 0);
                ff::ffi::sws_getColorspaceDetails(
                    sws.as_mut_ptr(),
                    &mut inv,
                    &mut src_range,
                    &mut table,
                    &mut dst_range,
                    &mut brightness,
                    &mut contrast,
                    &mut saturation,
                );
                let bt709 = ff::ffi::sws_getCoefficients(ff::ffi::SWS_CS_ITU709);
                ff::ffi::sws_setColorspaceDetails(sws.as_mut_ptr(), bt709, src_range, table, dst_range, brightness, contrast, saturation);
            }
        }
        sws.run(&self.last, &mut bgr)?;
        let (stride, data) = (bgr.stride(0), bgr.data(0));
        let row = w as usize * 4;
        let mut sum = 0u64;
        for y in 0..h as usize {
            for (p, q) in data[y * stride..][..row].as_chunks::<4>().0.iter().zip(bgrx[y * row..][..row].as_chunks::<4>().0) {
                sum += (0..3).map(|i| p[i].abs_diff(q[i]) as u64).sum::<u64>();
            }
        }
        let n = self.scores.len().max(1) as f64;
        let quality = self.sources.is_some().then(|| {
            let (p, s) = self.scores.iter().fold((0.0, 0.0), |(p, s), (a, b)| (p + a, s + b));
            (p / n, s / n)
        });
        Ok(Decoded {
            decoded: self.decoded,
            mae: sum as f64 / (w as f64 * h as f64 * 3.0),
            quality,
            shown: self.shown,
            differing: self.differing,
            worst: self.worst,
            errors: self.errors,
        })
    }
}
