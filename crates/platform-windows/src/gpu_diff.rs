//! Per-tile change detection on the capture device.
//!
//! DXGI dirty rects are coarse (whole windows, compositor regions): a small
//! animation inside a large window reports hundreds of tiles. Reading all of
//! them back and comparing on the CPU costs milliseconds per frame, so the
//! session keeps the previous desktop image on the GPU, compares the tiles
//! under the DXGI areas in a compute shader and reports only the tiles whose
//! pixels really changed. The readback is one `u32` per compared tile.

use anyhow::{bail, Context, Result};
use gpu_interop::d3d11::D3D11Frame;
use remothing_core::geom::Rect;
use windows::core::{s, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_SHADER_MACRO};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4,
    ID3D11Fence, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_FENCE_FLAG_NONE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_USAGE, D3D11_USAGE_DEFAULT,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Tile edge in pixels, the core's default `server.tile`. The provider does
/// not know the configured tile size; with another size the reported areas
/// stay a correct superset of the changes, only less tight.
pub const TILE: u32 = 64;

// Each thread of a 16x16 group compares a lattice of pixels of its tile.
const _: () = assert!(TILE.is_multiple_of(16));

/// When at least 15/16 of a comparison of `PASS_THROUGH_MIN` or more tiles
/// changed (full-screen video, animations), the next `PASS_THROUGH_FRAMES`
/// frames report the selected tiles without comparing: the core shows such
/// tiles from video without reading them, so waiting for the GPU there would
/// only add latency. The comparison resumes on its own afterwards.
const PASS_THROUGH_MIN: usize = 64;
const PASS_THROUGH_FRAMES: u32 = 15;

/// Compiled at session open with D3DCompile (d3dcompiler_47.dll ships with
/// Windows 10+): the source stays reviewable here and the build needs no
/// Windows SDK shader compiler. Unorm-to-float conversion is exact, so float
/// equality is pixel equality; out-of-range loads of edge tiles read 0 from
/// both textures.
const SHADER: &str = r"
Texture2D<float4> current : register(t0);
Texture2D<float4> previous : register(t1);
StructuredBuffer<uint> tiles : register(t2);
RWStructuredBuffer<uint> changed : register(u0);

groupshared uint differs;

[numthreads(16, 16, 1)]
void main(uint3 group : SV_GroupID, uint3 thread : SV_GroupThreadID, uint index : SV_GroupIndex)
{
    if (index == 0)
        differs = 0;
    GroupMemoryBarrierWithGroupSync();
    uint packed = tiles[group.x];
    uint2 origin = uint2(packed & 0xffff, packed >> 16) * TILE + thread.xy;
    bool diff = false;
    [unroll]
    for (uint y = 0; y < TILE; y += 16) {
        [unroll]
        for (uint x = 0; x < TILE; x += 16) {
            int3 p = int3(origin + uint2(x, y), 0);
            diff = diff || any(current.Load(p) != previous.Load(p));
        }
    }
    if (diff)
        InterlockedOr(differs, 1);
    GroupMemoryBarrierWithGroupSync();
    if (index == 0)
        changed[group.x] = differs;
}
";

pub struct GpuDiff {
    shader: ID3D11ComputeShader,
    /// Bound at t0..t2: current image, previous image, tile list.
    views: [Option<ID3D11ShaderResourceView>; 3],
    flags_view: Option<ID3D11UnorderedAccessView>,
    /// Desktop image as of the previous frame.
    previous: ID3D11Texture2D,
    /// Tiles to compare, `x | y << 16` in tile units, row-major.
    tile_list: ID3D11Buffer,
    /// One changed flag per listed tile.
    flags: ID3D11Buffer,
    readback: ID3D11Buffer,
    done: GpuFence,
    width: u32,
    height: u32,
    tiles_x: u32,
    touched: Vec<bool>,
    list: Vec<u32>,
    /// The selected tiles as merged rects.
    runs: Vec<Rect>,
    /// Frames left that skip the comparison.
    pass_through: u32,
}

// SAFETY: see `DxgiSession`: free-threaded D3D11 objects, context use under `context_lock`.
unsafe impl Send for GpuDiff {}

impl GpuDiff {
    /// Diff state for `image` (a texture of `frame`'s device and size); the
    /// previous image must be set with [`GpuDiff::reset`] before the first [`GpuDiff::submit`].
    pub fn new(frame: &D3D11Frame, image: &ID3D11Texture2D) -> Result<Self> {
        let device = &frame.device;
        let (width, height) = (frame.width, frame.height);
        let tiles_x = width.div_ceil(TILE);
        let tiles = tiles_x * height.div_ceil(TILE);
        let code = compile()?;
        let mut shader = None;
        // SAFETY: bytecode produced by D3DCompile for cs_5_0; valid out pointer.
        unsafe { device.CreateComputeShader(&code, None, Some(&mut shader)) }.context("CreateComputeShader")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out pointer.
        unsafe { image.GetDesc(&mut desc) };
        let srv = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        let previous = crate::dxgi::create_texture(device, width, height, desc.Format, D3D11_USAGE_DEFAULT.0, srv, 0)?;
        let structured = D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32;
        let tile_list = buffer(device, tiles, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE.0 as u32, 0, structured)?;
        let flags = buffer(device, tiles, D3D11_USAGE_DEFAULT, D3D11_BIND_UNORDERED_ACCESS.0 as u32, 0, structured)?;
        let readback = buffer(device, tiles, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32, structured)?;
        let (mut current_view, mut previous_view, mut list_view, mut flags_view) = (None, None, None, None);
        // SAFETY: resources of this device; null descriptors view whole resources in their own format.
        unsafe {
            device.CreateShaderResourceView(image, None, Some(&mut current_view)).context("current image view")?;
            device.CreateShaderResourceView(&previous, None, Some(&mut previous_view)).context("previous image view")?;
            device.CreateShaderResourceView(&tile_list, None, Some(&mut list_view)).context("tile list view")?;
            device.CreateUnorderedAccessView(&flags, None, Some(&mut flags_view)).context("tile flags view")?;
        }
        Ok(Self {
            shader: shader.context("CreateComputeShader returned no shader")?,
            views: [current_view, previous_view, list_view],
            flags_view,
            previous,
            tile_list,
            flags,
            readback,
            done: GpuFence::new(frame)?,
            width,
            height,
            tiles_x,
            touched: vec![false; tiles as usize],
            list: Vec::with_capacity(tiles as usize),
            runs: Vec::new(),
            pass_through: 0,
        })
    }

    /// Makes the previous image equal to `current` (first frame, re-duplication).
    /// The caller holds the context lock.
    pub fn reset(&self, ctx: &ID3D11DeviceContext, current: &ID3D11Texture2D) {
        // SAFETY: same size and format.
        unsafe { ctx.CopyResource(&self.previous, current) };
    }

    /// Selects the tiles overlapping `areas` (clipped to the display) for the
    /// next [`GpuDiff::submit`]; false when there are none.
    pub fn select<'a>(&mut self, areas: impl IntoIterator<Item = &'a Rect>) -> bool {
        for r in areas {
            if r.w == 0 || r.h == 0 {
                continue;
            }
            for ty in r.y / TILE..=(r.y + r.h - 1) / TILE {
                let row = (ty * self.tiles_x) as usize;
                self.touched[row + (r.x / TILE) as usize..=row + ((r.x + r.w - 1) / TILE) as usize].fill(true);
            }
        }
        self.list.clear();
        for (i, t) in self.touched.iter_mut().enumerate() {
            if std::mem::take(t) {
                let i = i as u32;
                self.list.push((i % self.tiles_x) | ((i / self.tiles_x) << 16));
            }
        }
        !self.list.is_empty()
    }

    /// Updates the previous image under the selected tiles from `current`
    /// (already updated to the new frame) and, unless passing through, first
    /// queues their comparison and its readback. `true`: collect the result
    /// with [`GpuDiff::collect`]; `false`: the caller reports its own areas.
    /// The caller holds the context lock.
    pub fn submit(&mut self, ctx: &ID3D11DeviceContext, current: &ID3D11Texture2D) -> Result<bool> {
        let mut runs = std::mem::take(&mut self.runs);
        runs.clear();
        self.merge(|_| true, &mut runs);
        self.runs = runs;
        let compare = self.pass_through == 0;
        self.pass_through = self.pass_through.saturating_sub(1);
        let n = self.list.len() as u32;
        let flag_bytes = D3D11_BOX { left: 0, top: 0, front: 0, right: n * 4, bottom: 1, back: 1 };
        // SAFETY: buffers hold one u32 per display tile and n <= tiles; views
        // and shader belong to this device. Everything bound is unbound again:
        // the immediate context is shared with the encoder.
        unsafe {
            if compare {
                ctx.UpdateSubresource(&self.tile_list, 0, Some(&flag_bytes), self.list.as_ptr().cast(), 0, 0);
                ctx.CSSetShader(&self.shader, None);
                ctx.CSSetShaderResources(0, Some(&self.views));
                ctx.CSSetUnorderedAccessViews(0, 1, Some(&self.flags_view), None);
                ctx.Dispatch(n, 1, 1);
                ctx.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
                ctx.CSSetShaderResources(0, Some(&[None, None, None]));
                ctx.CSSetShader(None::<&ID3D11ComputeShader>, None);
                ctx.CopySubresourceRegion(&self.readback, 0, 0, 0, 0, &self.flags, 0, Some(&flag_bytes));
            }
            // Outside the selected tiles both images are already equal.
            for r in &self.runs {
                let b = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
                ctx.CopySubresourceRegion(&self.previous, 0, r.x, r.y, 0, current, 0, Some(&b));
            }
        }
        if compare {
            self.done.signal(ctx)?;
        }
        Ok(compare)
    }

    /// Waits for the submitted comparison (without holding the context lock)
    /// and appends the changed tiles, clipped and merged into horizontal runs, to `out`.
    pub fn collect(&mut self, frame: &D3D11Frame, out: &mut Vec<Rect>) -> Result<()> {
        self.done.wait().context("wait for tile diff")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        frame
            // SAFETY: staging buffer with CPU read access; its copy is complete.
            .with_context(|ctx| unsafe { ctx.Map(&self.readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) })
            .context("map tile flags")?;
        // SAFETY: the buffer holds at least `list.len()` u32 flags (one per display tile).
        let flags = unsafe { std::slice::from_raw_parts(mapped.pData as *const u32, self.list.len()) };
        self.merge(|i| flags[i] != 0, out);
        let unchanged = flags.iter().filter(|&&f| f == 0).count();
        if flags.len() >= PASS_THROUGH_MIN && unchanged * 16 <= flags.len() {
            self.pass_through = PASS_THROUGH_FRAMES;
        }
        // SAFETY: mapped above.
        frame.with_context(|ctx| unsafe { ctx.Unmap(&self.readback, 0) });
        Ok(())
    }

    /// Appends the listed tiles `keep` accepts (by list index) as display
    /// rects, adjacent tiles of a row merged.
    fn merge(&self, keep: impl Fn(usize) -> bool, out: &mut Vec<Rect>) {
        for (i, &packed) in self.list.iter().enumerate() {
            if !keep(i) {
                continue;
            }
            let (x, y) = ((packed & 0xffff) * TILE, (packed >> 16) * TILE);
            let r = Rect::new(x, y, (x + TILE).min(self.width) - x, (y + TILE).min(self.height) - y);
            match out.last_mut() {
                Some(last) if last.y == r.y && last.x + last.w == r.x => last.w += r.w,
                _ => out.push(r),
            }
        }
    }
}

/// Completion of submitted GPU work, waited on without the context lock and
/// without polling (the encoder keeps using the context meanwhile).
pub struct GpuFence {
    fence: ID3D11Fence,
    /// Auto-reset event the fence sets on completion.
    event: HANDLE,
    /// Last signaled value.
    value: u64,
}

// SAFETY: the fence is free-threaded and the event handle is a kernel object.
unsafe impl Send for GpuFence {}

impl GpuFence {
    /// Requires D3D11.4 fences (Windows 10 1703+, WDDM 2 drivers).
    pub fn new(frame: &D3D11Frame) -> Result<Self> {
        let device: ID3D11Device5 = frame.device.cast().context("ID3D11Device5 (D3D11 fences) not available")?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: valid out pointer; unnamed auto-reset event.
        unsafe {
            device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence).context("CreateFence")?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).context("CreateEvent")?;
            Ok(Self { fence: fence.context("CreateFence returned no fence")?, event, value: 0 })
        }
    }

    /// Signals after the commands queued so far and submits them. The caller
    /// holds the context lock of `ctx`.
    pub fn signal(&mut self, ctx: &ID3D11DeviceContext) -> Result<()> {
        let ctx: ID3D11DeviceContext4 = ctx.cast().context("ID3D11DeviceContext4")?;
        self.value += 1;
        // SAFETY: fence of this context's device.
        unsafe {
            ctx.Signal(&self.fence, self.value).context("signal fence")?;
            ctx.Flush();
        }
        Ok(())
    }

    /// Blocks until the GPU passed the last [`GpuFence::signal`].
    pub fn wait(&self) -> Result<()> {
        const TIMEOUT_MS: u32 = 2000;
        // SAFETY: fence and event owned by self. A device removal completes
        // the fence with u64::MAX, so this returns and the next call fails.
        unsafe {
            while self.fence.GetCompletedValue() < self.value {
                self.fence.SetEventOnCompletion(self.value, self.event).context("SetEventOnCompletion")?;
                // Rechecked after the wake: a timed-out earlier wait may have left the event set.
                match WaitForSingleObject(self.event, TIMEOUT_MS) {
                    WAIT_OBJECT_0 => {}
                    WAIT_TIMEOUT => bail!("GPU work not finished after {TIMEOUT_MS} ms"),
                    _ => bail!("wait for fence event: {}", windows::core::Error::from_win32()),
                }
            }
        }
        Ok(())
    }
}

impl Drop for GpuFence {
    fn drop(&mut self) {
        // SAFETY: event created in `new`, closed once.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

fn buffer(device: &ID3D11Device, elements: u32, usage: D3D11_USAGE, bind: u32, cpu: u32, misc: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: elements * 4,
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: misc,
        StructureByteStride: 4,
    };
    let mut buffer = None;
    // SAFETY: valid descriptor and out pointer.
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }.context("CreateBuffer")?;
    buffer.context("CreateBuffer returned no buffer")
}

fn compile() -> Result<Vec<u8>> {
    let tile = format!("{TILE}\0");
    let defines = [
        D3D_SHADER_MACRO { Name: s!("TILE"), Definition: PCSTR(tile.as_ptr()) },
        D3D_SHADER_MACRO { Name: PCSTR::null(), Definition: PCSTR::null() },
    ];
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: source pointer/length and NUL-terminated strings outlive the call.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("gpu_diff.hlsl"),
            Some(defines.as_ptr()),
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let blob_bytes = |b: &ID3DBlob| {
        // SAFETY: a blob's pointer is valid for its size while the blob lives.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec()
    };
    if let Err(e) = result {
        let log = errors.as_ref().map(|b| String::from_utf8_lossy(&blob_bytes(b)).into_owned()).unwrap_or_default();
        bail!("compile tile diff shader: {e}: {}", log.trim_end_matches('\0').trim());
    }
    Ok(blob_bytes(&code.context("D3DCompile returned no bytecode")?))
}
