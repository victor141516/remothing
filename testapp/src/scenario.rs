//! Platform-independent drawing: the barcode and the scenario content, into
//! a 32-bit 0x00RRGGBB (BGRX in memory) pixel buffer owned by the backend.

pub const BLOCK: usize = 16;
pub const BITS: usize = 64;
pub const BAR_W: usize = BLOCK * BITS;
pub const BAR_H: usize = BLOCK;
const MAGIC: u64 = 0xA5;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Static UI; only the barcode changes.
    Idle,
    /// A 256x256 box moving over static UI.
    Boxed,
    /// Whole window changes every frame with smooth video-like content.
    Full,
    /// Text-like content scrolling vertically.
    Scroll,
}

#[cfg(target_os = "linux")]
fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64
}

#[cfg(windows)]
fn now_us() -> u64 {
    // FILETIME counts 100 ns ticks since 1601-01-01; the precise variant
    // interpolates with QPC instead of the ~1 ms system tick.
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime() };
    let ticks = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
    (ticks - EPOCH_DIFF_100NS) / 10
}

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    pub w: usize,
    pub h: usize,
}

impl Canvas<'_> {
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for yy in y..(y + h).min(self.h) {
            let row = &mut self.px[yy * self.w..yy * self.w + self.w];
            row[x.min(self.w)..(x + w).min(self.w)].fill(c);
        }
    }

    pub fn barcode(&mut self, marker: u16) {
        let v = MAGIC | ((now_us() & ((1 << 40) - 1)) << 8) | ((marker as u64) << 48);
        for i in 0..BITS {
            let c = if v >> i & 1 == 1 { 0xFFFFFF } else { 0 };
            self.fill(i * BLOCK, 0, BLOCK, BLOCK, c);
        }
    }

    /// Fake UI text: rows of dark word-like blocks on a light background.
    pub fn text(&mut self, scroll: usize) {
        for y in BAR_H..self.h {
            let doc_y = y + scroll;
            let line = doc_y / 22;
            let in_line = doc_y % 22;
            let row = &mut self.px[y * self.w..(y + 1) * self.w];
            if !(6..18).contains(&in_line) {
                row.fill(0xF0F0F0);
                continue;
            }
            let mut seed = (line as u32).wrapping_mul(2654435761) | 1;
            let mut x = 24;
            row.fill(0xF0F0F0);
            while x < row.len() - 24 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let word = 12 + (seed % 70) as usize;
                let end = (x + word).min(row.len() - 24);
                let shade = (0x20 + (seed >> 24) / 4) * 0x010101;
                for (i, p) in row[x..end].iter_mut().enumerate() {
                    // Glyph-like strokes: regular within a row (compresses like real
                    // text) but with a phase unique to each row of a line, so rows do
                    // not repeat (scroll detection relies on that, as with real glyphs).
                    if (i + in_line * 7 + (seed as usize & 3)) % 13 < 7 {
                        *p = shade;
                    }
                }
                x = end + 8;
            }
        }
    }
}

pub struct Plasma {
    sx: Vec<u32>,
    sy: Vec<u32>,
}

impl Plasma {
    pub fn new() -> Self {
        let table = |period: f64| -> Vec<u32> {
            (0..4096).map(|i| ((((i as f64) * std::f64::consts::TAU / period).sin() + 1.0) * 63.0) as u32).collect()
        };
        Self { sx: table(700.0), sy: table(500.0) }
    }

    pub fn draw(&self, c: &mut Canvas, t: usize) {
        for y in BAR_H..c.h {
            let vy = self.sy[(y + t * 3) & 4095];
            let vy2 = self.sx[(y * 2 + t) & 4095];
            let row = &mut c.px[y * c.w..(y + 1) * c.w];
            for (x, p) in row.iter_mut().enumerate() {
                let a = self.sx[(x + t * 5) & 4095] + vy;
                let b = self.sy[(x * 2 + 4096 - ((t * 2) & 4095)) & 4095] + vy2;
                *p = (a << 16) | (b << 8) | ((a + b) >> 1);
            }
        }
    }
}
