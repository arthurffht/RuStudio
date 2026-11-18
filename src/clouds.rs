//! Terrain.Clouds: Studio's volumetric cloud layer, its Beer shadow map, and
//! the "sun cover" it reads back to soften and dim the sun (2026-11-12,
//! checks/grass-shadow-stability/clouds/FINDINGS.md).
//!
//! Ported from Studio (Studio build, shaders_glsl3.pack and the Vulkan pack
//! it draws with):
//!
//! * Parameters (Studio, from the Clouds instance): CloudsParams =
//!   DirDist (wind offset xyz in km, w = render time / 1000), LightColorSize,
//!   CoverDensityQuality (Cover² on the quality path, else 1.78·Cover⁴ +
//!   0.03; Density; 18 steps), Color (Clouds.Color), DetailScale (0.25, 0.25).
//!   GlobalWind is clamped to 600 long. Each frame the offset
//!   moves by −0.00084 · wind · dt km and wraps at 32.0513 (fmodf).
//! * Passes: CloudsBsmFS into a 512² Beer shadow map when the
//!   cover / density / quality or the sun's height change; CloudsSunCoverFS
//!   into one texel, read back; CloudsWindFS at a quarter of
//!   the view; CloudsReprojectFilterFS / CloudsUpsampleNoHistoryFS at half;
//!   CloudsCompFS over the scene. Textures: rbxasset://sky/clouds.dds (the
//!   256×256×32 distance field), cloudAdvection.dds, cloudDetail3D.dds, bn.dds
//!   (byte-identical to what a running Studio binds, RenderDoc 2026-11-12).
//! * Sun cover (Studio, getter Studio): target = max(texel, 0.5)
//!   (this), smoothed cover += (1 − 0.15^dt) · (target − cover), 1.0
//!   until the first read. It then
//!   - widens the sun's shadow blur (Studio, before Studio):
//!     softness = min(ShadowSoftness + 1 − cover, 1);
//!   - rides in Globals SkyAmbient.w (Studio → renderer), which
//!     the Vulkan pack's surface shaders use as: sun diffuse × cover, sun
//!     specular × cover², SkyAmbientNoIBL × skylight × (2 − cover)
//!     (DefaultUnified*, SmoothCluster, SmoothWater, Grass). rustudio keeps
//!     it in Globals.outdoor.w (shader.wgsl `sun_shadow`, `sun_cover`).

use crate::doc::Document;
use glam::Vec3;
use rbx_dom_weak::types::Variant;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11DepthStencilState, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_COMPARISON_ALWAYS, D3D11_CPU_ACCESS_READ, D3D11_CULL_NONE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_TEXTURE_ADDRESS_WRAP, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_SAMPLE_DESC,
};

use crate::d3d11::{self, BlendMode, Tex2dSpec};

/// What the place asks for: the active Clouds (the first under Terrain).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudsLook {
    pub cover: f32,
    pub density: f32,
    /// Clouds.Color (0..1, as the shader takes it: not squared).
    pub color: [f32; 3],
    /// Workspace.GlobalWind (studs / s).
    pub wind: Vec3,
    /// Studio's FogParams: the clouds fade into
    /// the sky with it toward the horizon.
    pub fog: [f32; 4],
}

/// Clouds.Cover / Density / Color defaults (the class's).
const DEFAULT_COVER: f32 = 0.5;
const DEFAULT_DENSITY: f32 = 0.7;
/// CoverDensityQuality.z / .w at Studio's top quality levels (RenderDoc).
const QUALITY_STEPS: f32 = 18.0;
const QUALITY_W: f32 = 17.0;
/// DetailScale (RenderDoc, CloudsParams[4]).
const DETAIL_SCALE: [f32; 2] = [0.25, 0.25];
/// The wind offset per stud/s of GlobalWind per second, in km ([Studio]),
/// its wrap ([Studio]) and the wind's longest ([Studio]).
const WIND_KM: f32 = 0.00084;
const WIND_WRAP: f32 = 32.051_28;
const WIND_MAX: f32 = 600.0;
/// Sun cover: Params1 (40, 10) set in the constructor Studio, the floor
/// ( 0.5) and the smoothing base ([Studio] 0.15 per second).
const COVER_K: f32 = 40.0;
const COVER_DISK_TEXELS: f32 = 10.0;
const COVER_FLOOR: f32 = 0.5;
const COVER_SMOOTH: f32 = 0.15;
const BSM_SIZE: u32 = 512;

fn child_of_class(d: &Document, parent: rbx_dom_weak::types::Ref, class: &str) -> Option<rbx_dom_weak::types::Ref> {
    d.dom.get_by_ref(parent)?.children().iter().copied().find(|r| d.class_of(*r).is_some_and(|c| c == class))
}

/// The place's active clouds (Workspace.Terrain's Clouds, Enabled).
pub fn look(d: &Document) -> Option<CloudsLook> {
    let ws = d.workspace()?;
    let terrain = child_of_class(d, ws, "Terrain")?;
    let c = child_of_class(d, terrain, "Clouds")?;
    if matches!(d.prop(c, "Enabled"), Some(Variant::Bool(false))) {
        return None;
    }
    let f = |n: &str, dflt: f32| crate::scene::f32_of(d.prop(c, n)).unwrap_or(dflt);
    let color = match d.prop(c, "Color") {
        Some(Variant::Color3(c)) => [c.r, c.g, c.b],
        Some(Variant::Color3uint8(c)) => [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0],
        _ => [1.0; 3],
    };
    let wind = crate::scene::vec3_of(d.prop(ws, "GlobalWind")).unwrap_or(Vec3::ZERO);
    let fog = match crate::lighting::atmosphere(d) {
        // (0, Offset, −0.14427 · clamp(Density)⁴, 0): fog_k is that in natural log.
        Some(a) => [0.0, a.offset, -a.fog_k / std::f32::consts::LN_2, 0.0],
        None => {
            let l = d.find_child(d.dom.root_ref(), "Lighting");
            let g = |n: &str, dflt: f32| l.and_then(|l| crate::scene::f32_of(d.prop(l, n))).unwrap_or(dflt);
            let (s, e) = (g("FogStart", 0.0), g("FogEnd", 100_000.0));
            let span = (e - s).max(1e-3);
            [e / span, 0.0, -1.0 / span, 1.0]
        }
    };
    Some(CloudsLook {
        cover: f("Cover", DEFAULT_COVER).clamp(0.0, 1.0),
        density: f("Density", DEFAULT_DENSITY).clamp(0.0, 1.0),
        color,
        wind,
        fog,
    })
}

/// The smoothed sun cover (f32 bits): 1 = clear.
static COVER: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// The sun cover for this frame's lighting (1 without clouds). Resets when
/// the clouds go away.
pub fn sun_cover(active: bool) -> f32 {
    if !active {
        COVER.store(1.0f32.to_bits(), Ordering::Relaxed);
        return 1.0;
    }
    f32::from_bits(COVER.load(Ordering::Relaxed))
}

/// The sun's shadow softness with the clouds' share.
pub fn shadow_softness(softness: f32, cover: f32) -> f32 {
    (softness + 1.0 - cover).min(1.0)
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CloudsRaw {
    dir_dist: [f32; 4],
    cdq: [f32; 4],
    color: [f32; 4],
    detail: [f32; 4],
    fogp: [f32; 4],
    quarter: [f32; 4],
    half: [f32; 4],
    full: [f32; 4],
    cover: [f32; 4],
}

/// One DDS's pixel data: (width, height, depth, mips as (w, h, d, bytes)).
struct Dds {
    mips: Vec<(u32, u32, u32, Vec<u8>)>,
}

/// Reads an uncompressed or BC1 DDS (2D or 3D) with its mips; BC1 decoded
/// to RGBA8, `bpp` bytes per texel otherwise.
fn read_dds(bytes: &[u8]) -> Option<(Dds, bool)> {
    if bytes.len() < 128 || &bytes[0..4] != b"DDS " {
        return None;
    }
    let u = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let (h, w) = (u(12), u(16));
    let depth = u(24).max(1);
    let mips = u(28).max(1);
    let fourcc = &bytes[84..88];
    let (mut off, bc1, bpp, swap) = if fourcc == b"DX10" {
        // DXGI 61 R8_UNORM, 28 R8G8B8A8_UNORM.
        match u(128) {
            61 => (148usize, false, 1usize, false),
            28 => (148, false, 4, false),
            71 => (148, true, 0, false),
            _ => return None,
        }
    } else if fourcc == b"DXT1" {
        (128, true, 0, false)
    } else if u(88) == 32 {
        // 32-bit with masks: bn.dds is B8G8R8A8.
        (128, false, 4, u(92) == 0x00ff_0000)
    } else {
        return None;
    };
    let mut out = Vec::new();
    let (mut mw, mut mh, mut md) = (w, h, depth);
    for _ in 0..mips {
        let n = if bc1 {
            (mw.div_ceil(4) * mh.div_ceil(4) * 8) as usize * md as usize
        } else {
            (mw * mh * md) as usize * bpp
        };
        let src = bytes.get(off..off + n)?;
        off += n;
        let data = if bc1 {
            decode_bc1(src, mw, mh)
        } else if swap {
            src.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()
        } else {
            src.to_vec()
        };
        out.push((mw, mh, md, data));
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
        md = (md / 2).max(1);
    }
    Some((Dds { mips: out }, bc1 || bpp == 4))
}

fn decode_bc1(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    let c565 = |c: u16| {
        let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
        [((r << 3) | (r >> 2)) as f32, ((g << 2) | (g >> 4)) as f32, ((b << 3) | (b >> 2)) as f32]
    };
    for by in 0..h.div_ceil(4) {
        for bx in 0..w.div_ceil(4) {
            let i = ((by * w.div_ceil(4) + bx) * 8) as usize;
            let b = &src[i..i + 8];
            let (c0, c1) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
            let (a, z) = (c565(c0), c565(c1));
            let mix = |t: f32, al: u8| [0, 1, 2].map(|k| (a[k] + (z[k] - a[k]) * t).round() as u8).into_iter().chain([al]).collect::<Vec<u8>>();
            let pal: [Vec<u8>; 4] = if c0 > c1 {
                [mix(0.0, 255), mix(1.0, 255), mix(1.0 / 3.0, 255), mix(2.0 / 3.0, 255)]
            } else {
                [mix(0.0, 255), mix(1.0, 255), mix(0.5, 255), vec![0, 0, 0, 0]]
            };
            let bits = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            for py in 0..4 {
                for px in 0..4 {
                    let (x, y) = (bx * 4 + px, by * 4 + py);
                    if x < w && y < h {
                        let k = ((bits >> (2 * (py * 4 + px))) & 3) as usize;
                        let o = ((y * w + x) * 4) as usize;
                        out[o..o + 4].copy_from_slice(&pal[k]);
                    }
                }
            }
        }
    }
    out
}

/// A texture from Studio's content (`rbxasset://sky/…`), all its mips; a
/// 1-texel stand-in when missing.
fn content_texture(device: &ID3D11Device, id: &str, three_d: bool) -> ID3D11ShaderResourceView {
    let parsed = crate::assets::resolve(id, None)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| read_dds(&b));
    if parsed.is_none() {
        eprintln!("[clouds] {id} missing: clouds will not draw right");
    }
    let (dds, rgba) = parsed.unwrap_or_else(|| (Dds { mips: vec![(1, 1, 1, vec![255; 4])] }, true));
    let (w, h, d, _) = dds.mips[0];
    let format: DXGI_FORMAT = if rgba {
        DXGI_FORMAT_R8G8B8A8_UNORM
    } else {
        DXGI_FORMAT_R8_UNORM
    };
    let bpp = if rgba { 4 } else { 1 };
    let mips = dds.mips.len() as u32;
    let subresources: Vec<D3D11_SUBRESOURCE_DATA> = dds
        .mips
        .iter()
        .map(|(mw, mh, _md, data)| D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr().cast(),
            SysMemPitch: mw * bpp,
            SysMemSlicePitch: if three_d { mw * mh * bpp } else { 0 },
        })
        .collect();
    if three_d {
        let tex = d3d11::create_tex3d(
            device,
            w,
            h,
            d,
            mips,
            format,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            Some(&subresources),
        );
        d3d11::create_srv_3d(device, &tex, format, mips)
    } else {
        let tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: w,
                height: h,
                mips,
                array_size: 1,
                format,
                samples: 1,
                bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                misc_flags: 0,
            },
            Some(&subresources),
        );
        d3d11::create_srv_2d(device, &tex, format, false, 0, mips)
    }
}

struct Sized {
    full: [u32; 2],
    quarter: [u32; 2],
    half: [u32; 2],
    quarter_rtv: ID3D11RenderTargetView,
    quarter_srv: ID3D11ShaderResourceView,
    half_rtv: ID3D11RenderTargetView,
    half_srv: ID3D11ShaderResourceView,
}

pub struct CloudsGpu {
    cb: ID3D11Buffer,
    dist: ID3D11ShaderResourceView,
    adv: ID3D11ShaderResourceView,
    detail: ID3D11ShaderResourceView,
    noise: ID3D11ShaderResourceView,
    bsm_rtv: ID3D11RenderTargetView,
    bsm_srv: ID3D11ShaderResourceView,
    cover_tex: ID3D11Texture2D,
    cover_rtv: ID3D11RenderTargetView,
    cover_staging: [ID3D11Texture2D; 2],
    cover_pending: [bool; 2],
    cover_write: usize,
    rep: ID3D11SamplerState,
    dist_s: ID3D11SamplerState,
    clamp: ID3D11SamplerState,
    vs: ID3D11VertexShader,
    ps_bsm: ID3D11PixelShader,
    ps_cover: ID3D11PixelShader,
    ps_wind: ID3D11PixelShader,
    ps_up_1x: ID3D11PixelShader,
    ps_up_msaa: ID3D11PixelShader,
    ps_comp: ID3D11PixelShader,
    rast_state: ID3D11RasterizerState,
    depth_off: ID3D11DepthStencilState,
    blend_opaque: ID3D11BlendState,
    sized: Option<Sized>,
    /// What the BSM was last drawn for: (Cover², Density, steps, Lamp0Dir.y).
    bsm_key: Option<[f32; 4]>,
    started: Instant,
    last: Instant,
    offset: Vec3,
    target: f32,
    cover: f32,
    have_cover: bool,
    /// Whether this frame draws clouds (set by `encode`, read by `composite`).
    active: bool,
}

fn create_staging_r8(device: &ID3D11Device) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 1,
        Height: 1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut out = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut out)) }.expect("CreateTexture2D staging");
    out.unwrap()
}

impl CloudsGpu {
    pub fn new(device: &ID3D11Device, _ctx: &ID3D11DeviceContext) -> Self {
        let vs = d3d11::create_vs(device, crate::d3d11::shaders::VS_CLOUDS);
        let ps_bsm = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_BSM);
        let ps_cover = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_COVER);
        let ps_wind = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_WIND);
        let ps_up_1x = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_UP_1X);
        let ps_up_msaa = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_UP_MSAA);
        let ps_comp = d3d11::create_ps(device, crate::d3d11::shaders::PS_CLOUDS_COMP);

        let rt_flags = (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32;
        let bsm_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: BSM_SIZE,
                height: BSM_SIZE,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                samples: 1,
                bind_flags: rt_flags,
                misc_flags: 0,
            },
            None,
        );
        let bsm_rtv = d3d11::create_rtv(device, &bsm_tex, DXGI_FORMAT_R16G16B16A16_FLOAT, 0, false);
        let bsm_srv = d3d11::create_srv_2d(device, &bsm_tex, DXGI_FORMAT_R16G16B16A16_FLOAT, false, 0, 1);

        let cover_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: 1,
                height: 1,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R8_UNORM,
                samples: 1,
                bind_flags: D3D11_BIND_RENDER_TARGET.0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let cover_rtv = d3d11::create_rtv(device, &cover_tex, DXGI_FORMAT_R8_UNORM, 0, false);
        let cover_staging = [create_staging_r8(device), create_staging_r8(device)];

        let rep = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            1,
        );
        let dist_s = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let clamp = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let cb = d3d11::create_cb(device, std::mem::size_of::<CloudsRaw>());
        let rast_state = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        let depth_off = d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        let blend_opaque = d3d11::create_blend_state(device, BlendMode::Opaque);

        Self {
            cb,
            dist: content_texture(device, "rbxasset://sky/clouds.dds", true),
            adv: content_texture(device, "rbxasset://sky/cloudAdvection.dds", false),
            detail: content_texture(device, "rbxasset://sky/cloudDetail3D.dds", true),
            noise: content_texture(device, "rbxasset://sky/bn.dds", false),
            bsm_rtv,
            bsm_srv,
            cover_tex,
            cover_rtv,
            cover_staging,
            cover_pending: [false, false],
            cover_write: 0,
            rep,
            dist_s,
            clamp,
            vs,
            ps_bsm,
            ps_cover,
            ps_wind,
            ps_up_1x,
            ps_up_msaa,
            ps_comp,
            rast_state,
            depth_off,
            blend_opaque,
            sized: None,
            bsm_key: None,
            started: crate::sim_clock::now(),
            last: crate::sim_clock::now(),
            offset: Vec3::ZERO,
            target: 1.0,
            cover: 1.0,
            have_cover: false,
            active: false,
        }
    }

    fn ensure_size(&mut self, device: &ID3D11Device, full: [u32; 2]) {
        if self.sized.as_ref().is_some_and(|s| s.full == full) {
            return;
        }
        // (W + 1) >> 2 and >> 1, as Studio sizes them.
        let quarter = [((full[0] + 1) >> 2).max(1), ((full[1] + 1) >> 2).max(1)];
        let half = [((full[0] + 1) >> 1).max(1), ((full[1] + 1) >> 1).max(1)];
        let rt_flags = (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32;
        let make = |s: [u32; 2]| {
            let tex = d3d11::create_tex2d(
                device,
                &Tex2dSpec {
                    width: s[0],
                    height: s[1],
                    mips: 1,
                    array_size: 1,
                    format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                    samples: 1,
                    bind_flags: rt_flags,
                    misc_flags: 0,
                },
                None,
            );
            let rtv = d3d11::create_rtv(device, &tex, DXGI_FORMAT_R16G16B16A16_FLOAT, 0, false);
            let srv = d3d11::create_srv_2d(device, &tex, DXGI_FORMAT_R16G16B16A16_FLOAT, false, 0, 1);
            (rtv, srv)
        };
        let (quarter_rtv, quarter_srv) = make(quarter);
        let (half_rtv, half_srv) = make(half);
        self.sized = Some(Sized {
            full,
            quarter,
            half,
            quarter_rtv,
            quarter_srv,
            half_rtv,
            half_srv,
        });
    }

    /// The sun cover read back non-blockingly from our 2-frame staging ring, smoothed by dt.
    fn update_cover(&mut self, ctx: &ID3D11DeviceContext, dt: f32) {
        let other = 1 - self.cover_write;
        for idx in [other, self.cover_write] {
            if !self.cover_pending[idx] {
                continue;
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let hr = unsafe {
                ctx.Map(
                    &self.cover_staging[idx],
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            };
            if hr.is_ok() {
                let b = unsafe { *(mapped.pData as *const u8) };
                unsafe { ctx.Unmap(&self.cover_staging[idx], 0) };
                self.cover_pending[idx] = false;
                self.target = (b as f32 / 255.0).max(COVER_FLOOR);
                self.have_cover = true;
                if std::env::var_os("RUSTUDIO_CLOUDS_DEBUG").is_some() {
                    eprintln!(
                        "[clouds] sun cover texel {b} target {:.4} smoothed {:.4}",
                        self.target, self.cover
                    );
                }
            }
        }
        if self.have_cover {
            self.cover += (1.0 - COVER_SMOOTH.powf(dt)) * (self.target - self.cover);
        }
        let shown = if self.have_cover { self.cover } else { 1.0 };
        COVER.store(shown.to_bits(), Ordering::Relaxed);
    }

    fn bind_common(&self, ctx: &ID3D11DeviceContext) {
        unsafe {
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.RSSetState(&self.rast_state);
            ctx.OMSetDepthStencilState(&self.depth_off, 0);
            ctx.OMSetBlendState(&self.blend_opaque, None, 0xffff_ffff);
            let cbs = [Some(self.cb.clone())];
            ctx.PSSetConstantBuffers(4, Some(&cbs));
            let samplers = [
                Some(self.rep.clone()),
                Some(self.dist_s.clone()),
                Some(self.clamp.clone()),
            ];
            ctx.PSSetSamplers(5, Some(&samplers));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_pass(
        &self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        clear: bool,
        size: [u32; 2],
        ps: &ID3D11PixelShader,
        bsm: Option<&ID3D11ShaderResourceView>,
        input: Option<&ID3D11ShaderResourceView>,
        scene: Option<&ID3D11ShaderResourceView>,
        depth: Option<&ID3D11ShaderResourceView>,
    ) {
        d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
        unsafe {
            if clear {
                ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0]);
            }
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.PSSetShader(ps, None);
            let srvs = [
                Some(self.dist.clone()),
                Some(self.adv.clone()),
                Some(self.detail.clone()),
                Some(self.noise.clone()),
                bsm.cloned(),
                input.cloned(),
                scene.cloned(),
                depth.cloned(),
            ];
            ctx.PSSetShaderResources(24, Some(&srvs));
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(None, None);
        }
        d3d11::unbind_ps_srvs(ctx, 24, 8);
    }

    /// Records the cloud passes up to the half-resolution layer (before the
    /// scene); `composite` lays it over the scene later.
    pub fn encode(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        look: &CloudsLook,
        sun_y: f32,
        full: [u32; 2],
        depth_srv: &ID3D11ShaderResourceView,
        msaa: bool,
    ) {
        let now = crate::sim_clock::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(1.0);
        self.last = now;
        self.active = true;
        self.update_cover(ctx, dt);
        let time = std::env::var("RUSTUDIO_CLOUDS_TIME")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or_else(|| crate::sim_clock::since(self.started).as_secs_f32());
        let wind = if look.wind.length() > WIND_MAX {
            look.wind.normalize() * WIND_MAX
        } else {
            look.wind
        };
        // fmodf: the sign follows the dividend.
        self.offset.x = (self.offset.x - WIND_KM * wind.x * dt) % WIND_WRAP;
        self.offset.z = (self.offset.z - WIND_KM * wind.z * dt) % WIND_WRAP;
        self.ensure_size(device, full);
        let s = self.sized.as_ref().unwrap();
        let v = |a: [u32; 2]| [a[0] as f32, a[1] as f32, 1.0 / a[0] as f32, 1.0 / a[1] as f32];
        let cdq = [look.cover * look.cover, look.density, QUALITY_STEPS, QUALITY_W];
        let raw = CloudsRaw {
            dir_dist: [self.offset.x, 0.0, self.offset.z, time / 1000.0],
            cdq,
            color: [look.color[0], look.color[1], look.color[2], 1.0],
            detail: [DETAIL_SCALE[0], DETAIL_SCALE[1], 0.0, 0.0],
            fogp: look.fog,
            quarter: v(s.quarter),
            half: v(s.half),
            full: v(s.full),
            cover: [COVER_K, COVER_DISK_TEXELS, 1.0 / BSM_SIZE as f32, 0.0],
        };
        d3d11::upload_cb(ctx, &self.cb, &raw);
        self.bind_common(ctx);

        // The Beer shadow map, when what it depends on changed (Studio:
        // CoverDensityQuality, Lamp0Dir.y).
        let key = [cdq[0], cdq[1], cdq[2], sun_y];
        if self.bsm_key != Some(key) {
            self.bsm_key = Some(key);
            self.draw_pass(
                ctx,
                &self.bsm_rtv,
                true,
                [BSM_SIZE, BSM_SIZE],
                &self.ps_bsm,
                None,
                None,
                None,
                None,
            );
        }
        if !self.cover_pending[self.cover_write] {
            self.draw_pass(
                ctx,
                &self.cover_rtv,
                true,
                [1, 1],
                &self.ps_cover,
                Some(&self.bsm_srv),
                None,
                None,
                None,
            );
            unsafe {
                ctx.CopyResource(&self.cover_staging[self.cover_write], &self.cover_tex);
            }
            self.cover_pending[self.cover_write] = true;
            self.cover_write = 1 - self.cover_write;
        }
        self.draw_pass(
            ctx,
            &s.quarter_rtv,
            true,
            s.quarter,
            &self.ps_wind,
            Some(&self.bsm_srv),
            None,
            None,
            None,
        );
        let ps_up = if msaa { &self.ps_up_msaa } else { &self.ps_up_1x };
        self.draw_pass(
            ctx,
            &s.half_rtv,
            true,
            s.half,
            ps_up,
            Some(&self.bsm_srv),
            Some(&s.quarter_srv),
            None,
            Some(depth_srv),
        );
    }

    /// Lays the clouds over the scene (`hdr_rtv`, whose copy is `scene_srv`)
    /// where the 1x depth (`depth_1x_srv`) shows nothing was drawn.
    pub fn composite(
        &mut self,
        ctx: &ID3D11DeviceContext,
        hdr_rtv: &ID3D11RenderTargetView,
        scene_srv: &ID3D11ShaderResourceView,
        depth_1x_srv: &ID3D11ShaderResourceView,
    ) {
        if !self.active {
            return;
        }
        let Some(s) = self.sized.as_ref() else { return };
        self.bind_common(ctx);
        self.draw_pass(
            ctx,
            hdr_rtv,
            false,
            s.full,
            &self.ps_comp,
            Some(&self.bsm_srv),
            Some(&s.half_srv),
            Some(scene_srv),
            Some(depth_1x_srv),
        );
    }

    /// After the frame's submit: clears active flag for the next frame.
    pub fn after_submit(&mut self) {
        self.active = false;
    }

    /// No clouds this frame: forget the cover.
    pub fn idle(&mut self) {
        self.active = false;
        self.have_cover = false;
        self.cover = 1.0;
        self.target = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softness_takes_the_clouds_share() {
        assert!((shadow_softness(0.2, 1.0) - 0.2).abs() < 1e-6);
        assert!((shadow_softness(0.0, 0.506) - 0.494).abs() < 1e-6);
        assert_eq!(shadow_softness(0.9, 0.5), 1.0);
    }

    #[test]
    fn studio_cloud_textures_parse() {
        for (id, dims) in [
            ("rbxasset://sky/clouds.dds", (256, 256, 32)),
            ("rbxasset://sky/cloudDetail3D.dds", (64, 64, 64)),
            ("rbxasset://sky/cloudAdvection.dds", (256, 256, 1)),
            ("rbxasset://sky/bn.dds", (128, 128, 1)),
        ] {
            let Some(b) = crate::assets::resolve(id, None).and_then(|p| std::fs::read(p).ok()) else {
                eprintln!("{id}: no Studio content here, skipped");
                continue;
            };
            let (d, _) = read_dds(&b).expect(id);
            let (w, h, z, _) = &d.mips[0];
            assert_eq!((*w, *h, *z), dims, "{id}");
        }
    }
}
