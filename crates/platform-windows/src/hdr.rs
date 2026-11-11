//! HDR displays: their state, and the conversion of the desktop image.
//!
//! In HDR mode Windows composes the desktop in linear light, 16-bit float,
//! 1.0 = 80 nits (scRGB), with ordinary programs placed at the "SDR content
//! brightness" level. Asked for 8 bits, Desktop Duplication cuts that off at
//! 80 nits (the picture comes out 3.5 times too bright at 280 nits, every
//! light grey white). So the session asks for the float image and converts it
//! here, on the GPU, relative to the host's SDR white read from Windows:
//!
//! - SDR picture: up to SDR white, exactly the sRGB an SDR program drew;
//!   brighter (real HDR content) scaled into range and turned towards white
//!   as it approaches the display's peak, the same way on every frame.
//! - HDR picture: `proto::hdr_split`'s curve, into a 10-bit image for tiles
//!   and the 8-bit one the encoders take.
//!
//! Colours outside sRGB (negative scRGB channels) move towards the grey of
//! their luminance until in range.

use anyhow::{bail, Context, Result};
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC,
    D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_RASTERIZER_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput, IDXGIOutput6};

use remothing_core::geom::Rect;
use remothing_core::proto;

/// scRGB 1.0, and Windows' SDR white when nothing says otherwise.
const SCRGB_NITS: f32 = 80.0;

/// A display's HDR state, as Windows reports it now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrState {
    /// HDR mode is on (the desktop is composed in scRGB).
    pub on: bool,
    /// "SDR content brightness": where ordinary programs' white sits, nits.
    pub white: f32,
    /// The display's peak brightness, nits.
    pub peak: f32,
}

impl HdrState {
    /// Brightest value in multiples of SDR white the display can show.
    pub fn max(&self) -> f32 {
        self.peak / self.white
    }
}

pub fn state(output: &IDXGIOutput) -> Result<HdrState> {
    // SAFETY: plain getters on a live output.
    let desc = unsafe { output.cast::<IDXGIOutput6>().context("IDXGIOutput6")?.GetDesc1() }.context("GetDesc1")?;
    let on = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let white = sdr_white(&desc.DeviceName).unwrap_or(SCRGB_NITS);
    Ok(HdrState { on, white, peak: desc.MaxLuminance })
}

/// The SDR white level of the display with GDI name `device`, nits.
fn sdr_white(device: &[u16; 32]) -> Option<f32> {
    let name = &device[..device.iter().position(|&c| c == 0).unwrap_or(device.len())];
    // SAFETY: buffers sized by GetDisplayConfigBufferSizes; request packets carry their own size.
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).ok().ok()?;
        for path in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let gdi = &source.viewGdiDeviceName;
            if &gdi[..gdi.iter().position(|&c| c == 0).unwrap_or(gdi.len())] != name {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            // Thousandths of 80 nits.
            return (DisplayConfigGetDeviceInfo(&mut white.header) == 0).then(|| white.SDRWhiteLevel as f32 / 1000.0 * SCRGB_NITS);
        }
        None
    }
}

/// Compiled at session open like the tile diff's shader (`gpu_diff.rs`).
const SHADER: &str = r"
Texture2D<float4> image : register(t0);
cbuffer Params : register(b0) {
    float scale;   // scRGB to multiples of SDR white: 80 / white nits
    float ln_max;  // ln of the brightest value, in multiples of SDR white
    float split;   // proto::hdr_split: the SDR range's share of the codes
    float hdr;     // 1: the HDR curve; 0: SDR
};

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}

float3 srgb(float3 v) {
    return v <= 0.0031308 ? 12.92 * v : 1.055 * pow(abs(v), 1.0 / 2.4) - 0.055;
}

// Towards the grey of the same luminance until no channel is negative.
float3 in_gamut(float3 v) {
    float y = dot(v, float3(0.2126, 0.7152, 0.0722));
    if (y <= 0)
        return 0;
    float lo = min(v.r, min(v.g, v.b));
    return lo >= 0 ? v : y + (v - y) * (y / (y - lo));
}

struct Out {
    float4 tiles : SV_Target0;
    float4 video : SV_Target1;
};

Out ps(float4 pos : SV_Position) {
    float3 v = in_gamut(image.Load(int3(pos.xy, 0)).rgb * scale);
    float3 e;
    if (hdr > 0.5) {
        e = v <= 1 ? split * srgb(v) : split + (1 - split) * log(max(v, 1)) / ln_max;
    } else {
        float m = max(v.r, max(v.g, v.b));
        if (m > 1)
            v = lerp(v / m, 1, ln_max > 0 ? saturate(log(m) / ln_max) : 1);
        e = srgb(v);
    }
    Out o;
    o.tiles = float4(saturate(e), 1);
    o.video = o.tiles;
    return o;
}
";

#[repr(C)]
struct Params {
    scale: f32,
    ln_max: f32,
    split: f32,
    hdr: f32,
}

/// The conversion pass: from the float desktop image (copied into `linear`
/// first, since duplication images are not guaranteed to be bindable) into
/// one target (SDR) or two (HDR: tiles, video).
pub struct Converter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    raster: ID3D11RasterizerState,
    params: ID3D11Buffer,
    linear_view: Option<ID3D11ShaderResourceView>,
    targets: Vec<Option<ID3D11RenderTargetView>>,
    width: u32,
    height: u32,
}

// SAFETY: see `DxgiSession`: free-threaded D3D11 objects, context use under `context_lock`.
unsafe impl Send for Converter {}

impl Converter {
    /// `linear`: the float copy of the desktop; `targets`: what to write
    /// (the SDR picture, or the HDR tiles and video pictures, in that order).
    pub fn new(device: &ID3D11Device, linear: &ID3D11Texture2D, targets: &[&ID3D11Texture2D], size: (u32, u32)) -> Result<Self> {
        let (vs_code, ps_code) = (compile("vs", s!("vs_5_0"))?, compile("ps", s!("ps_5_0"))?);
        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: true.into(),
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let params_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<Params>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let (mut vs, mut ps, mut raster, mut params, mut linear_view) = (None, None, None, None, None);
        let mut views = Vec::new();
        // SAFETY: bytecode from D3DCompile for these stages; resources of this device; valid out pointers.
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs)).context("CreateVertexShader")?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps)).context("CreatePixelShader")?;
            device.CreateRasterizerState(&raster_desc, Some(&mut raster)).context("CreateRasterizerState")?;
            device.CreateBuffer(&params_desc, None, Some(&mut params)).context("constant buffer")?;
            device.CreateShaderResourceView(linear, None, Some(&mut linear_view)).context("float image view")?;
            for t in targets {
                let mut view = None;
                device.CreateRenderTargetView(*t, None, Some(&mut view)).context("conversion target view")?;
                views.push(view);
            }
        }
        Ok(Self {
            vs: vs.context("no vertex shader")?,
            ps: ps.context("no pixel shader")?,
            raster: raster.context("no rasterizer state")?,
            params: params.context("no constant buffer")?,
            linear_view,
            targets: views,
            width: size.0,
            height: size.1,
        })
    }

    /// Converts `rects` of the float image with the display's current levels;
    /// `hdr_max` above 0: the HDR curve up to that, else SDR. The caller holds
    /// the context lock; everything bound is unbound again, since encoders
    /// share the immediate context.
    pub fn convert(&self, ctx: &ID3D11DeviceContext, state: &HdrState, hdr_max: f32, rects: &[Rect]) {
        let (max, hdr) = if hdr_max > 0.0 { (hdr_max, 1.0) } else { (state.max(), 0.0) };
        let params = Params { scale: SCRGB_NITS / state.white, ln_max: max.max(1.0).ln(), split: proto::hdr_split(max), hdr };
        let viewport = D3D11_VIEWPORT { Width: self.width as f32, Height: self.height as f32, MaxDepth: 1.0, ..Default::default() };
        // SAFETY: shaders, views and buffers of this device; the constant
        // buffer matches `Params`; scissor rects lie inside the viewport.
        unsafe {
            ctx.UpdateSubresource(&self.params, 0, None, (&params as *const Params).cast(), 0, 0);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(std::slice::from_ref(&self.linear_view)));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            ctx.RSSetState(&self.raster);
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.OMSetRenderTargets(Some(&self.targets), None);
            for r in rects {
                let scissor = RECT { left: r.x as i32, top: r.y as i32, right: (r.x + r.w) as i32, bottom: (r.y + r.h) as i32 };
                ctx.RSSetScissorRects(Some(&[scissor]));
                ctx.Draw(3, 0);
            }
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.PSSetConstantBuffers(0, Some(&[None]));
            ctx.RSSetState(None);
            ctx.VSSetShader(None, None);
            ctx.PSSetShader(None, None);
        }
    }
}

fn compile(entry: &str, target: PCSTR) -> Result<Vec<u8>> {
    let entry = format!("{entry}\0");
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: source pointer/length and NUL-terminated strings outlive the call.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("hdr.hlsl"),
            None,
            None,
            PCSTR(entry.as_ptr()),
            target,
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
        bail!("compile HDR conversion shader: {e}: {}", log.trim_end_matches('\0').trim());
    }
    Ok(blob_bytes(&code.context("D3DCompile returned no bytecode")?))
}
