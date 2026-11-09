//! Direct read-only mapping of the Xvfb framebuffer (`Xvfb -shmem`).
//!
//! Xvfb stores the screen as an XWD image in a SysV shared memory segment and
//! prints `screen N shmid ID` on startup. The XWD header is big-endian; pixels
//! follow the header, window name and colormap.

use anyhow::{bail, Result};

pub struct XvfbFramebuffer {
    base: *const u8,
    pixels: *const u8,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

// The mapping is read-only and lives for the process lifetime.
unsafe impl Send for XvfbFramebuffer {}
unsafe impl Sync for XvfbFramebuffer {}

const XWD_COLOR_SIZE: usize = 12;

impl XvfbFramebuffer {
    pub fn attach(shmid: i32) -> Result<Self> {
        // SAFETY: plain SysV shm attach; failure is checked below.
        let base = unsafe { libc::shmat(shmid, std::ptr::null(), libc::SHM_RDONLY) };
        if base as isize == -1 {
            bail!("shmat({shmid}) failed: {}", std::io::Error::last_os_error());
        }
        let mut ds: libc::shmid_ds = unsafe { std::mem::zeroed() };
        if unsafe { libc::shmctl(shmid, libc::IPC_STAT, &mut ds) } != 0 {
            bail!("shmctl failed: {}", std::io::Error::last_os_error());
        }
        let seg_len = ds.shm_segsz;
        let base = base as *const u8;
        let hdr = unsafe { std::slice::from_raw_parts(base, 100) };
        let be = |i: usize| u32::from_be_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
        let header_size = be(0) as usize;
        let width = be(4);
        let height = be(5);
        let bpp = be(11);
        let stride = be(12) as usize;
        let ncolors = be(19) as usize;
        if bpp != 32 {
            bail!("unsupported Xvfb bpp {bpp}; start Xvfb with depth 24");
        }
        let offset = header_size + ncolors * XWD_COLOR_SIZE;
        let needed = offset + stride * height as usize;
        if needed > seg_len {
            bail!("XWD header inconsistent: needs {needed} bytes, segment has {seg_len}");
        }
        Ok(Self { base, pixels: unsafe { base.add(offset) }, width, height, stride })
    }

    /// One scanline (BGRX). The X server may be writing concurrently; callers
    /// tolerate torn reads because the next damage event resends the area.
    #[inline]
    pub fn row(&self, y: u32) -> &[u8] {
        debug_assert!(y < self.height);
        unsafe { std::slice::from_raw_parts(self.pixels.add(y as usize * self.stride), self.width as usize * 4) }
    }
}

impl Drop for XvfbFramebuffer {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.base as *const libc::c_void) };
    }
}
