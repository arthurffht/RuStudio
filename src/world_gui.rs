//! SurfaceGuis and BillboardGuis in the world (round 23): their egui
//! triangles drawn onto the finished view, each pixel tested against the
//! scene's depth, so parts in front hide them as in Studio (AlwaysOnTop
//! false). AlwaysOnTop ones and ScreenGuis are still painted over the view
//! by egui.
//!
//! The scene's depth is 4× multisampled and reverse-Z (1 near, 0 far); the
//! fragment shader reads sample 0 and drops what lies behind it. Colours are
//! egui's: premultiplied, gamma-space, on an Rgba8Unorm target (egui's
//! "gamma framebuffer" path), so they come out as egui would paint them.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11DepthStencilState, ID3D11Device, ID3D11DeviceContext,
    ID3D11InputLayout, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11VertexShader,
    D3D11_APPEND_ALIGNED_ELEMENT, D3D11_BIND_INDEX_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_ZERO,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_ALWAYS, D3D11_CULL_NONE,
    D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_INSTANCE_DATA, D3D11_INPUT_PER_VERTEX_DATA,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_TEXTURE_ADDRESS_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32G32B32A32_FLOAT,
    DXGI_FORMAT_R32G32B32_FLOAT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32_UINT,
    DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8_UNORM,
};

use crate::d3d11::{self, BlendMode, RenderState, Tex2dSpec};

/// One vertex: NDC position, reverse-Z depth, egui's UV and colour, and
/// the light it takes (linear RGB
/// factor: Brightness × (1 − LightInfluence) + LightInfluence × the light).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GuiVertex {
    pub ndc: [f32; 2],
    pub depth: f32,
    pub uv: [f32; 2],
    pub color: [u8; 4],
    pub light: [f32; 3],
    /// Studs from the camera (fog).
    pub dist: f32,
    /// Particles, beams and trails (round 25, src/effects.rs): the clip-space
    /// w, with `ndc` and `depth` holding clip x, y and z, so the GPU clips at
    /// the near plane and interpolates perspective-correctly. 0 for GUIs.
    pub clip_w: f32,
    /// What alpha is scaled by (1 for GUIs).
    pub alpha_mul: f32,
}

/// How an effect is lit, per vertex / particle: x Brightness × (1 − w),
/// y w = √LightInfluence (0: unlit, Brightness alone), z the mode (0 unlit,
/// 1 lit, 2 lit with the sun's shadow per pixel: beams), w 1 − LightEmission
/// (alpha scale: additive toward 1 under premultiplied blending). The GPU
/// lights it from the light grid, the local lights and the sun
/// (`fx_light`, the same terms as effects::SceneLight::at).
pub type FxLook = [f32; 4];

/// A beam / trail / constraint strip vertex in world space (Studio's
/// TrailVSLightUnified takes world positions and lights at the pixel's
/// light-grid coordinate): the GPU projects and lights it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FxVertex {
    /// World position before the strip's ZOffset.
    pub pos: [f32; 3],
    /// Studs the row moves toward the camera (Beam.ZOffset, TrailVS).
    pub zoff: f32,
    /// Where the light is read: the row's centre (before ZOffset).
    pub at: [f32; 3],
    pub color: [u8; 4],
    pub uv: [f32; 2],
    pub look: FxLook,
    /// RUSTUDIO_FX_LIGHT_CHECK: the CPU's light (SceneLight), compared.
    pub check: [f32; 3],
    /// The glass in front (first, count in FxFrame::glass_idx).
    pub glass: [u32; 2],
    /// FaceCamera beams: the row's tangent and ± half width; the GPU puts
    /// the vertex at `pos` + normalize(tangent × (camera − pos)) × w
    /// (0: `pos` is the vertex).
    pub face: [f32; 4],
    /// Taken off the texture's v (Beam.TextureSpeed).
    pub scroll: f32,
}

/// One particle (Studio's ParticleCustomVS: the CPU writes the particle,
/// the vertex shader makes its quad): the four corners are centre ± e1 ±
/// e2 (size, squash, rotation and orientation already in e1 / e2).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FxParticle {
    pub centre: [f32; 3],
    /// ParticleEmitter.ZOffset: moves the depth only (ParticleCustomVS).
    pub zoffset: f32,
    pub e1: [f32; 3],
    /// Soft particles (ParticleCustomFS_Soft): 1 / the fade distance (the
    /// half size), 0 = hard.
    pub soft: f32,
    pub e2: [f32; 3],
    /// FlipbookBlendFrames: how far toward the next frame (ParticleCustomFS
    /// mixes the two frames' texels).
    pub blend: f32,
    /// The flipbook frame's and the next frame's top-left UV, and the cell
    /// size (0 when untextured: both UVs egui's white texel).
    pub uv0: [f32; 2],
    pub uv1: [f32; 2],
    pub cell: [f32; 2],
    pub color: [u8; 4],
    pub look: FxLook,
    pub check: [f32; 3],
    /// The glass in front (first, count in FxFrame::glass_idx).
    pub glass: [u32; 2],
}

/// A see-through part effects behind it are dimmed by (effects::Glass):
/// its rotation's columns with the half size in w, and its position with
/// the opacity in w.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FxGlass {
    pub x: [f32; 4],
    pub y: [f32; 4],
    pub z: [f32; 4],
    pub pos: [f32; 4],
}

/// One draw of effects, far to near: a texture and a range of `FxFrame`'s
/// strip indices or particles.
#[derive(Clone, Copy, Debug)]
pub struct FxDraw {
    pub texture: egui::TextureId,
    pub wrap: bool,
    pub particles: bool,
    pub start: u32,
    pub end: u32,
}

/// The frame's particles, beams and trails (effects.rs), and what the GPU
/// needs to project and light them.
#[derive(Default)]
pub struct FxFrame {
    /// Reverse-Z view-projection the effects were laid out for.
    pub vp: [[f32; 4]; 4],
    /// Camera position; w 1 = RUSTUDIO_FX_LIGHT_CHECK.
    pub cam: [f32; 4],
    /// The camera's right (FaceCamera's fallback side).
    pub right: [f32; 4],
    /// effects::SceneLight: ambient, outdoor, sun, env (linear RGB).
    pub light: [[f32; 4]; 4],
    pub draws: Vec<FxDraw>,
    pub strips: Vec<FxVertex>,
    pub indices: Vec<u32>,
    pub particles: Vec<FxParticle>,
    /// The see-through parts in view, and per effect the ones in front of
    /// it (FxVertex / FxParticle::glass).
    pub glass: Vec<FxGlass>,
    pub glass_idx: Vec<u32>,
}

impl FxFrame {
    pub fn clear(&mut self) {
        self.glass.clear();
        self.glass_idx.clear();
        self.draws.clear();
        self.strips.clear();
        self.indices.clear();
        self.particles.clear();
    }

    /// Appends a strip's triangles, joining the last draw when it has the
    /// same texture.
    pub fn push_strip(&mut self, texture: egui::TextureId, wrap: bool, vertices: &[FxVertex], indices: &[u32]) {
        let base = self.strips.len() as u32;
        self.strips.extend_from_slice(vertices);
        let start = self.indices.len() as u32;
        self.indices.extend(indices.iter().map(|i| base + i));
        self.join(FxDraw { texture, wrap, particles: false, start, end: self.indices.len() as u32 });
    }

    /// Appends particles (one draw, joined like `push_strip`).
    pub fn push_particles(&mut self, texture: egui::TextureId, particles: &[FxParticle]) {
        let start = self.particles.len() as u32;
        self.particles.extend_from_slice(particles);
        self.join(FxDraw { texture, wrap: false, particles: true, start, end: self.particles.len() as u32 });
    }

    fn join(&mut self, d: FxDraw) {
        if d.start == d.end {
            return;
        }
        if let Some(l) = self.draws.last_mut() {
            if l.texture == d.texture && l.wrap == d.wrap && l.particles == d.particles && l.end == d.start {
                l.end = d.end;
                return;
            }
        }
        self.draws.push(d);
    }
}

/// Triangles drawn with one egui texture, in drawing order.
#[derive(Default, Clone)]
pub struct GuiBatch {
    pub texture: Option<egui::TextureId>,
    /// Sampled repeating (beam and trail textures tile), else clamped.
    pub wrap: bool,
    pub vertices: Vec<GuiVertex>,
    pub indices: Vec<u32>,
}

/// Native textures registered outside egui's own atlas (ViewportFrame
/// images in vf_render.rs), with a generation counter bumped each time one
/// is redrawn.
static NATIVE_GEN: Mutex<Option<HashMap<egui::TextureId, u64>>> = Mutex::new(None);

pub fn register_native(id: egui::TextureId) {
    NATIVE_GEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(id, 0);
}

pub fn native_changed(id: egui::TextureId) {
    if let Some(g) = NATIVE_GEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.get_mut(&id))
    {
        *g += 1;
    }
}

pub fn unregister_native(id: egui::TextureId) {
    if let Some(m) = NATIVE_GEN.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(&id);
    }
}

fn native_gen(id: egui::TextureId) -> Option<u64> {
    NATIVE_GEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&id)
        .copied()
}

pub fn push(batches: &mut Vec<GuiBatch>, texture: egui::TextureId, vertices: Vec<GuiVertex>, indices: &[u32]) {
    push_wrap(batches, texture, false, vertices, indices);
}

/// `push`, with the texture sampled repeating (`wrap`) or clamped.
pub fn push_wrap(batches: &mut Vec<GuiBatch>, texture: egui::TextureId, wrap: bool, vertices: Vec<GuiVertex>, indices: &[u32]) {
    if batches.last().is_none_or(|b| b.texture != Some(texture) || b.wrap != wrap) {
        batches.push(GuiBatch { texture: Some(texture), wrap, ..Default::default() });
    }
    let b = batches.last_mut().expect("batch");
    let base = b.vertices.len() as u32;
    b.vertices.extend(vertices);
    b.indices.extend(indices.iter().map(|i| base + i));
}

/// The sun's shadow cascades as the beams' shader reads them
/// (render.rs Globals sun_casc / sun_sphere / sun_rect).
#[derive(Clone, Copy, Default)]
pub struct SunCascades {
    pub local: [[[f32; 4]; 4]; 4],
    pub sphere: [[f32; 4]; 4],
    pub rect: [[f32; 4]; 4],
}

/// The scene's HDR format (render.rs HDR_FORMAT) and the effects' buffer.
const FX_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

struct DynBuf {
    buf: ID3D11Buffer,
    bytes: usize,
}

struct DynStructured {
    buf: ID3D11Buffer,
    srv: ID3D11ShaderResourceView,
    cap_bytes: usize,
}

struct MipChain {
    src_raw: usize,
    size: [u32; 2],
    full_srv: ID3D11ShaderResourceView,
    level_rtvs: Vec<ID3D11RenderTargetView>,
    level_srvs: Vec<ID3D11ShaderResourceView>,
    used: u64,
}

pub struct WorldGuiPass {
    tone_cb: ID3D11Buffer,
    /// The sun's shadow cascades (sun_shadow.rs): the EVSM atlas.
    pub sun_atlas: ID3D11ShaderResourceView,
    sun_sampler: ID3D11SamplerState,
    pub sun_grid: ID3D11ShaderResourceView,
    pub lightgrid: [f32; 4],
    pub sun_params: [f32; 4],
    pub sun_cascades: SunCascades,
    vs_gui: ID3D11VertexShader,
    ps_gui_1x: ID3D11PixelShader,
    ps_gui_msaa: ID3D11PixelShader,
    il_gui: ID3D11InputLayout,
    vs_strip: ID3D11VertexShader,
    il_strip: ID3D11InputLayout,
    vs_particle: ID3D11VertexShader,
    il_particle: ID3D11InputLayout,
    ps_fx_1x: ID3D11PixelShader,
    ps_fx_msaa: ID3D11PixelShader,
    vs_mip: ID3D11VertexShader,
    ps_mip: ID3D11PixelShader,
    vs_glass_mask: ID3D11VertexShader,
    ps_glass_in: ID3D11PixelShader,
    ps_glass_out: ID3D11PixelShader,
    rast_state: ID3D11RasterizerState,
    depth_off: ID3D11DepthStencilState,
    blend_opaque: ID3D11BlendState,
    blend_gui: ID3D11BlendState,
    blend_fx: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    /// Repeating, for beams' and trails' tiled textures (round 25).
    sampler_wrap: ID3D11SamplerState,
    /// Effects' textures with mip levels (Studio samples particle, beam and
    /// trail images mipmapped: far or small particles are the image's
    /// average, not a sparse sample of its texels), made from egui's copy
    /// on first use.
    mips: HashMap<egui::TextureId, MipChain>,
    mip_sampler: ID3D11SamplerState,
    sampler_mip: ID3D11SamplerState,
    sampler_mip_wrap: ID3D11SamplerState,
    frame: u64,
    /// The frame's local lights (render.rs lights SRV) and how many.
    pub lights: ID3D11ShaderResourceView,
    pub n_lights: u32,
    /// Vertex / index buffers kept from frame to frame, grown as needed
    /// (Studio's particle buffer is one 4 MB dynamic vertex buffer).
    bufs: [Option<DynBuf>; 5],
    /// The glass and its lists (FxFrame::glass / glass_idx), never empty.
    glass: [Option<DynStructured>; 2],
    /// Texture view & sampler cache keyed by `(id, wrap)`: `(src_raw, size, srv, sampler, used)`.
    tex_cache: HashMap<(egui::TextureId, bool), (usize, [u32; 2], ID3D11ShaderResourceView, ID3D11SamplerState, u64)>,
    /// Each native texture's draw count when its copy was last refilled.
    native_seen: HashMap<egui::TextureId, u64>,
    /// Untextured effects: one white texel.
    white_srv: ID3D11ShaderResourceView,
    fx_tex: Option<([u32; 2], ID3D11RenderTargetView, ID3D11ShaderResourceView)>,
}

/// `WorldGuiPass::upload`'s result.
struct Upload {
    glass_srv: ID3D11ShaderResourceView,
    glass_idx_srv: ID3D11ShaderResourceView,
    draws: Vec<(Kind, ID3D11ShaderResourceView, ID3D11SamplerState, u32, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gui,
    Strip,
    Particles,
}

fn fill_dyn(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    slot: &mut Option<DynBuf>,
    bind_flag: u32,
    data: &[u8],
) {
    if data.is_empty() {
        return;
    }
    let need = data.len().next_power_of_two().max(4096);
    if slot.as_ref().is_none_or(|b| b.bytes < data.len()) {
        *slot = Some(DynBuf {
            buf: d3d11::create_dynamic_buffer(device, bind_flag, need),
            bytes: need,
        });
    }
    if let Some(b) = slot {
        d3d11::update_dynamic(ctx, &b.buf, data);
    }
}

fn fill_structured(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    slot: &mut Option<DynStructured>,
    stride: u32,
    data: &[u8],
) {
    if data.is_empty() {
        return;
    }
    let count = (data.len() / stride as usize).max(1);
    let need_count = count.next_power_of_two().max(64);
    let need_bytes = need_count * stride as usize;
    if slot.as_ref().is_none_or(|s| s.cap_bytes < data.len()) {
        let (buf, srv) = d3d11::create_structured_srv(device, stride, need_count as u32, true, None);
        *slot = Some(DynStructured {
            buf,
            srv,
            cap_bytes: need_bytes,
        });
    }
    if let Some(s) = slot {
        d3d11::update_dynamic(ctx, &s.buf, data);
    }
}

fn elem_v(idx: u32, fmt: DXGI_FORMAT) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: windows::core::s!("TEXCOORD"),
        SemanticIndex: idx,
        Format: fmt,
        InputSlot: 0,
        AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

fn elem_i(idx: u32, fmt: DXGI_FORMAT) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: windows::core::s!("TEXCOORD"),
        SemanticIndex: idx,
        Format: fmt,
        InputSlot: 0,
        AlignedByteOffset: D3D11_APPEND_ALIGNED_ELEMENT,
        InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
        InstanceDataStepRate: 1,
    }
}

fn create_fx_blend(device: &ID3D11Device) -> ID3D11BlendState {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut out = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.expect("CreateBlendState fx");
    out.unwrap()
}

impl WorldGuiPass {
    pub fn new(device: &ID3D11Device, _ctx: &ID3D11DeviceContext) -> Self {
        let vs_gui = d3d11::create_vs(device, crate::d3d11::shaders::VS_GUI);
        let ps_gui_1x = d3d11::create_ps(device, crate::d3d11::shaders::PS_GUI_1X);
        let ps_gui_msaa = d3d11::create_ps(device, crate::d3d11::shaders::PS_GUI_MSAA);
        let il_gui = d3d11::create_input_layout(
            device,
            &[
                elem_v(0, DXGI_FORMAT_R32G32_FLOAT),
                elem_v(1, DXGI_FORMAT_R32_FLOAT),
                elem_v(2, DXGI_FORMAT_R32G32_FLOAT),
                elem_v(3, DXGI_FORMAT_R8G8B8A8_UNORM),
                elem_v(4, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_v(5, DXGI_FORMAT_R32_FLOAT),
                elem_v(6, DXGI_FORMAT_R32_FLOAT),
                elem_v(7, DXGI_FORMAT_R32_FLOAT),
            ],
            crate::d3d11::shaders::VS_GUI,
        );

        let vs_strip = d3d11::create_vs(device, crate::d3d11::shaders::VS_FX_STRIP);
        let il_strip = d3d11::create_input_layout(
            device,
            &[
                elem_v(0, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_v(1, DXGI_FORMAT_R32_FLOAT),
                elem_v(2, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_v(3, DXGI_FORMAT_R8G8B8A8_UNORM),
                elem_v(4, DXGI_FORMAT_R32G32_FLOAT),
                elem_v(5, DXGI_FORMAT_R32G32B32A32_FLOAT),
                elem_v(6, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_v(7, DXGI_FORMAT_R32G32_UINT),
                elem_v(8, DXGI_FORMAT_R32G32B32A32_FLOAT),
                elem_v(9, DXGI_FORMAT_R32_FLOAT),
            ],
            crate::d3d11::shaders::VS_FX_STRIP,
        );

        let vs_particle = d3d11::create_vs(device, crate::d3d11::shaders::VS_FX_PARTICLE);
        let il_particle = d3d11::create_input_layout(
            device,
            &[
                elem_i(0, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_i(1, DXGI_FORMAT_R32_FLOAT),
                elem_i(2, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_i(3, DXGI_FORMAT_R32_FLOAT),
                elem_i(4, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_i(5, DXGI_FORMAT_R32_FLOAT),
                elem_i(6, DXGI_FORMAT_R32G32_FLOAT),
                elem_i(7, DXGI_FORMAT_R32G32_FLOAT),
                elem_i(8, DXGI_FORMAT_R32G32_FLOAT),
                elem_i(9, DXGI_FORMAT_R8G8B8A8_UNORM),
                elem_i(11, DXGI_FORMAT_R32G32B32A32_FLOAT),
                elem_i(12, DXGI_FORMAT_R32G32B32_FLOAT),
                elem_i(13, DXGI_FORMAT_R32G32_UINT),
            ],
            crate::d3d11::shaders::VS_FX_PARTICLE,
        );

        let ps_fx_1x = d3d11::create_ps(device, crate::d3d11::shaders::PS_FX_1X);
        let ps_fx_msaa = d3d11::create_ps(device, crate::d3d11::shaders::PS_FX_MSAA);
        let vs_mip = d3d11::create_vs(device, crate::d3d11::shaders::VS_MIP);
        let ps_mip = d3d11::create_ps(device, crate::d3d11::shaders::PS_MIP);
        let vs_glass_mask = d3d11::create_vs(device, crate::d3d11::shaders::VS_GLASS_MASK);
        let ps_glass_in = d3d11::create_ps(device, crate::d3d11::shaders::PS_GLASS_IN);
        let ps_glass_out = d3d11::create_ps(device, crate::d3d11::shaders::PS_GLASS_OUT);

        let rast_state = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        let depth_off = d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        let blend_opaque = d3d11::create_blend_state(device, BlendMode::Opaque);
        let blend_gui = d3d11::create_blend_state(device, BlendMode::PremulAlpha);
        let blend_fx = create_fx_blend(device);

        let sampler = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let sampler_wrap = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let sampler_mip = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let sampler_mip_wrap = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let mip_sampler = sampler_mip.clone();
        let sun_sampler = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );

        let tone_cb = d3d11::create_cb(device, 640);
        let (_, lights) = d3d11::create_structured_srv(
            device,
            std::mem::size_of::<crate::render::LightRaw>() as u32,
            1,
            false,
            None,
        );

        let white_init = [255u8; 4];
        let white_sub = D3D11_SUBRESOURCE_DATA {
            pSysMem: white_init.as_ptr().cast(),
            SysMemPitch: 4,
            SysMemSlicePitch: 0,
        };
        let white_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: 1,
                height: 1,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                samples: 1,
                bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                misc_flags: 0,
            },
            Some(&[white_sub]),
        );
        let white_srv = d3d11::create_srv_2d(device, &white_tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, 1);

        let sun_atlas_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: 1,
                height: 1,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                samples: 1,
                bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let sun_atlas = d3d11::create_srv_2d(
            device,
            &sun_atlas_tex,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            false,
            0,
            1,
        );

        let sun_grid_tex = d3d11::create_tex3d(
            device,
            1,
            1,
            1,
            1,
            DXGI_FORMAT_R8G8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            None,
        );
        let sun_grid = d3d11::create_srv_3d(device, &sun_grid_tex, DXGI_FORMAT_R8G8_UNORM, 1);

        WorldGuiPass {
            tone_cb,
            sun_atlas,
            sun_sampler,
            sun_grid,
            lightgrid: [0.0; 4],
            sun_params: [0.0, 580.0, 0.05, 0.0],
            sun_cascades: Default::default(),
            vs_gui,
            ps_gui_1x,
            ps_gui_msaa,
            il_gui,
            vs_strip,
            il_strip,
            vs_particle,
            il_particle,
            ps_fx_1x,
            ps_fx_msaa,
            vs_mip,
            ps_mip,
            vs_glass_mask,
            ps_glass_in,
            ps_glass_out,
            rast_state,
            depth_off,
            blend_opaque,
            blend_gui,
            blend_fx,
            sampler,
            sampler_wrap,
            mips: Default::default(),
            mip_sampler,
            sampler_mip,
            sampler_mip_wrap,
            frame: 0,
            lights,
            n_lights: 0,
            bufs: Default::default(),
            glass: Default::default(),
            tex_cache: Default::default(),
            native_seen: Default::default(),
            white_srv,
            fx_tex: None,
        }
    }

    /// A mipmapped copy of an egui texture (egui's own have one level),
    /// made once: level 0 drawn from egui's, each next level downsampled
    /// from the one above. Gamma-space premultiplied texels averaged, as
    /// the effects' fragment shader mixes them.
    ///
    /// `live`: egui's texture changes in place (the font atlas gains glyphs
    /// as new text sizes are laid out), so the copy is refilled once a
    /// frame, not kept from the first frame: a SurfaceGui whose text size
    /// was first laid out after the copy was made drew nothing (Place A's
    /// Liquid Divinium counter).
    fn mipped(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        id: egui::TextureId,
        src_srv: &ID3D11ShaderResourceView,
        size: [u32; 2],
        live: bool,
    ) -> ID3D11ShaderResourceView {
        let frame = self.frame;
        let src_raw = src_srv.as_raw() as usize;
        let reuse = match self.mips.get_mut(&id) {
            Some(c) if c.src_raw == src_raw && c.size == size && (!live || c.used == frame) => {
                c.used = frame;
                return c.full_srv.clone();
            }
            Some(c) if c.src_raw == src_raw && c.size == size => {
                c.used = frame;
                true
            }
            _ => false,
        };
        if !reuse {
            let levels = 32 - size[0].max(size[1]).max(1).leading_zeros();
            let tex = d3d11::create_tex2d(
                device,
                &Tex2dSpec {
                    width: size[0],
                    height: size[1],
                    mips: levels,
                    array_size: 1,
                    format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    samples: 1,
                    bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                    misc_flags: 0,
                },
                None,
            );
            let full_srv = d3d11::create_srv_2d(
                device,
                &tex,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                false,
                0,
                levels,
            );
            let mut level_rtvs = Vec::with_capacity(levels as usize);
            let mut level_srvs = Vec::with_capacity(levels as usize);
            for l in 0..levels {
                level_rtvs.push(d3d11::create_rtv(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, l, false));
                level_srvs.push(d3d11::create_srv_2d(
                    device,
                    &tex,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    false,
                    l,
                    1,
                ));
            }
            self.mips.insert(
                id,
                MipChain {
                    src_raw,
                    size,
                    full_srv,
                    level_rtvs,
                    level_srvs,
                    used: frame,
                },
            );
        }
        let chain = self.mips.get(&id).expect("mip chain");
        crate::gpu_timing::scope(device, ctx, "gpu: world gui mips", || unsafe {
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_mip, None);
            ctx.PSSetShader(&self.ps_mip, None);
            ctx.RSSetState(&self.rast_state);
            ctx.OMSetDepthStencilState(&self.depth_off, 0);
            ctx.OMSetBlendState(&self.blend_opaque, None, 0xffff_ffff);
            let samplers = [Some(self.mip_sampler.clone())];
            ctx.PSSetSamplers(1, Some(&samplers));

            for l in 0..chain.level_rtvs.len() {
                let mw = (size[0] >> l as u32).max(1);
                let mh = (size[1] >> l as u32).max(1);
                d3d11::set_viewport(ctx, 0.0, 0.0, mw as f32, mh as f32);
                let src = if l == 0 { src_srv } else { &chain.level_srvs[l - 1] };
                ctx.ClearRenderTargetView(&chain.level_rtvs[l], &[0.0, 0.0, 0.0, 0.0]);
                ctx.OMSetRenderTargets(Some(&[Some(chain.level_rtvs[l].clone())]), None);
                ctx.PSSetShaderResources(7, Some(&[Some(src.clone())]));
                ctx.Draw(3, 0);
                ctx.OMSetRenderTargets(None, None);
                d3d11::unbind_ps_srvs(ctx, 7, 1);
            }
        });
        chain.full_srv.clone()
    }

    /// The world GUIs (`batches`) and, into the scene, the effects (`fx`),
    /// uploaded into the kept buffers.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        rs: &RenderState,
        batches: &[GuiBatch],
        fx: Option<&FxFrame>,
        tone_curve: Option<[f32; 5]>,
        fog: [f32; 4],
        fog_rgb: [f32; 3],
    ) -> Option<Upload> {
        let fx_any = fx.is_some_and(|f| !f.draws.is_empty());
        if !fx_any && batches.iter().all(|b| b.indices.is_empty()) {
            return None;
        }
        let device = &rs.device;
        let ctx = &rs.context;
        let t = tone_curve.unwrap_or([0.0; 5]);
        let mut u = Vec::with_capacity(160);
        u.extend_from_slice(&[
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            if tone_curve.is_some() { 1.0 } else { 0.0 },
            crate::mech::NEAR,
            crate::mech::FX_LIGHT_CLAMP,
            fog[0],
            fog[1],
            fog[2],
            fog[3],
            fog_rgb[0],
            fog_rgb[1],
            fog_rgb[2],
            0.0,
        ]);
        u.extend(self.sun_cascades.local.iter().flatten().flatten().copied());
        u.extend(self.sun_cascades.sphere.iter().flatten().copied());
        u.extend(self.sun_cascades.rect.iter().flatten().copied());
        u.extend(self.lightgrid);
        u.extend(self.sun_params);
        let empty = FxFrame::default();
        let f = fx.unwrap_or(&empty);
        u.extend(f.vp.iter().flatten().copied());
        u.extend(f.cam);
        let mut amb = f.light[0];
        amb[3] = self.n_lights as f32;
        u.extend(amb);
        u.extend(f.light[1]);
        u.extend(f.light[2]);
        u.extend(f.light[3]);
        u.extend(f.right);
        d3d11::update_dynamic(ctx, &self.tone_cb, bytemuck::cast_slice(&u));

        // Structured buffers can't be empty.
        let pad = [FxGlass::default()];
        let [g0, g1] = &mut self.glass;
        fill_structured(
            device,
            ctx,
            g0,
            std::mem::size_of::<FxGlass>() as u32,
            bytemuck::cast_slice(if f.glass.is_empty() { &pad } else { &f.glass }),
        );
        fill_structured(
            device,
            ctx,
            g1,
            std::mem::size_of::<u32>() as u32,
            bytemuck::cast_slice(if f.glass_idx.is_empty() { &[0u32][..] } else { &f.glass_idx }),
        );
        let (Some(g0), Some(g1)) = (&self.glass[0], &self.glass[1]) else { return None };
        let glass_srv = g0.srv.clone();
        let glass_idx_srv = g1.srv.clone();

        self.frame += 1;
        let frame = self.frame;
        self.mips.retain(|_, c| frame - c.used < 600);
        self.tex_cache.retain(|_, (_, _, _, _, used)| frame - *used < 600);

        let group = |this: &mut Self,
                     id: egui::TextureId,
                     wrap: bool|
         -> Option<(ID3D11ShaderResourceView, ID3D11SamplerState)> {
            let (src_srv, size) = rs.texture_srv(id)?;
            let src_raw = src_srv.as_raw() as usize;
            let gen_opt = native_gen(id);
            let native_new = gen_opt.is_some_and(|g| this.native_seen.insert(id, g) != Some(g));
            let live = id == egui::TextureId::default() || native_new;
            let mipped = size[0].max(size[1]) > 1;
            if let Some((cached_raw, cached_size, view, samp, used)) =
                this.tex_cache.get_mut(&(id, wrap))
            {
                if *cached_raw == src_raw && *cached_size == size {
                    let first = std::mem::replace(used, frame) != frame;
                    let view_out = view.clone();
                    let samp_out = samp.clone();
                    if first && live && mipped {
                        let new_view = this.mipped(device, ctx, id, &src_srv, size, true);
                        if let Some((_, _, v, _, _)) = this.tex_cache.get_mut(&(id, wrap)) {
                            *v = new_view.clone();
                        }
                        return Some((new_view, samp_out));
                    }
                    return Some((view_out, samp_out));
                }
            }
            let (view, samp) = if mipped {
                (
                    this.mipped(device, ctx, id, &src_srv, size, live),
                    if wrap {
                        this.sampler_mip_wrap.clone()
                    } else {
                        this.sampler_mip.clone()
                    },
                )
            } else {
                (
                    src_srv,
                    if wrap {
                        this.sampler_wrap.clone()
                    } else {
                        this.sampler.clone()
                    },
                )
            };
            this.tex_cache
                .insert((id, wrap), (src_raw, size, view.clone(), samp.clone(), frame));
            Some((view, samp))
        };

        let mut vertices: Vec<GuiVertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut draws: Vec<(Kind, ID3D11ShaderResourceView, ID3D11SamplerState, u32, u32)> = Vec::new();
        for b in batches {
            let Some(id) = b.texture else { continue };
            if b.indices.is_empty() {
                continue;
            }
            let Some((srv, samp)) = group(self, id, b.wrap) else { continue };
            let base = vertices.len() as u32;
            let start = indices.len() as u32;
            vertices.extend_from_slice(&b.vertices);
            indices.extend(b.indices.iter().map(|i| base + i));
            draws.push((Kind::Gui, srv, samp, start, indices.len() as u32));
        }
        // Effects after the world GUIs, far to near. (An untextured effect
        // reads a 1×1 white texture instead of the font atlas, so its mips
        // needn't be refilled every frame for it.)
        for d in &f.draws {
            let tex = if d.texture == egui::TextureId::default() {
                Some((self.white_srv.clone(), self.sampler.clone()))
            } else {
                group(self, d.texture, d.wrap)
            };
            let Some((srv, samp)) = tex else { continue };
            draws.push((
                if d.particles { Kind::Particles } else { Kind::Strip },
                srv,
                samp,
                d.start,
                d.end,
            ));
        }
        if draws.is_empty() {
            return None;
        }
        let vb_flag = D3D11_BIND_VERTEX_BUFFER.0 as u32;
        let ib_flag = D3D11_BIND_INDEX_BUFFER.0 as u32;
        let [b0, b1, b2, b3, b4] = &mut self.bufs;
        fill_dyn(device, ctx, b0, vb_flag, bytemuck::cast_slice(&vertices));
        fill_dyn(device, ctx, b1, ib_flag, bytemuck::cast_slice(&indices));
        fill_dyn(device, ctx, b2, vb_flag, bytemuck::cast_slice(&f.strips));
        fill_dyn(device, ctx, b3, ib_flag, bytemuck::cast_slice(&f.indices));
        fill_dyn(device, ctx, b4, vb_flag, bytemuck::cast_slice(&f.particles));
        Some(Upload {
            glass_srv,
            glass_idx_srv,
            draws,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        ctx: &ID3D11DeviceContext,
        target_rtv: &ID3D11RenderTargetView,
        size: [u32; 2],
        depth_srv: &ID3D11ShaderResourceView,
        msaa: bool,
        is_fx: bool,
        up: &Upload,
    ) {
        d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
        let ps = match (is_fx, msaa) {
            (false, false) => &self.ps_gui_1x,
            (false, true) => &self.ps_gui_msaa,
            (true, false) => &self.ps_fx_1x,
            (true, true) => &self.ps_fx_msaa,
        };
        let blend = if is_fx { &self.blend_fx } else { &self.blend_gui };
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(target_rtv.clone())]), None);
            ctx.RSSetState(&self.rast_state);
            ctx.OMSetDepthStencilState(&self.depth_off, 0);
            ctx.OMSetBlendState(blend, None, 0xffff_ffff);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.PSSetShader(ps, None);

            let cbs = [Some(self.tone_cb.clone())];
            ctx.VSSetConstantBuffers(0, Some(&cbs));
            ctx.PSSetConstantBuffers(0, Some(&cbs));

            let s0 = [Some(self.sun_sampler.clone())];
            ctx.VSSetSamplers(0, Some(&s0));
            ctx.PSSetSamplers(0, Some(&s0));

            let vs_srvs = [
                Some(self.sun_grid.clone()),
                Some(self.lights.clone()),
                Some(up.glass_srv.clone()),
                Some(up.glass_idx_srv.clone()),
            ];
            ctx.VSSetShaderResources(2, Some(&vs_srvs));

            let ps_srvs = [
                Some(depth_srv.clone()),
                Some(self.sun_atlas.clone()),
                Some(self.sun_grid.clone()),
            ];
            ctx.PSSetShaderResources(0, Some(&ps_srvs));

            let mut bound = None;
            for (kind, srv, samp, start, end) in &up.draws {
                if bound != Some(*kind) {
                    let [b0, b1, b2, b3, b4] = &self.bufs;
                    match kind {
                        Kind::Gui => {
                            let (Some(vb), Some(ib)) = (b0, b1) else { continue };
                            ctx.IASetInputLayout(&self.il_gui);
                            ctx.VSSetShader(&self.vs_gui, None);
                            let stride = std::mem::size_of::<GuiVertex>() as u32;
                            let vbs = [Some(vb.buf.clone())];
                            ctx.IASetVertexBuffers(0, 1, Some(vbs.as_ptr()), Some(&stride), Some(&0));
                            ctx.IASetIndexBuffer(&ib.buf, DXGI_FORMAT_R32_UINT, 0);
                        }
                        Kind::Strip => {
                            let (Some(vb), Some(ib)) = (b2, b3) else { continue };
                            ctx.IASetInputLayout(&self.il_strip);
                            ctx.VSSetShader(&self.vs_strip, None);
                            let stride = std::mem::size_of::<FxVertex>() as u32;
                            let vbs = [Some(vb.buf.clone())];
                            ctx.IASetVertexBuffers(0, 1, Some(vbs.as_ptr()), Some(&stride), Some(&0));
                            ctx.IASetIndexBuffer(&ib.buf, DXGI_FORMAT_R32_UINT, 0);
                        }
                        Kind::Particles => {
                            let Some(vb) = b4 else { continue };
                            ctx.IASetInputLayout(&self.il_particle);
                            ctx.VSSetShader(&self.vs_particle, None);
                            let stride = std::mem::size_of::<FxParticle>() as u32;
                            let vbs = [Some(vb.buf.clone())];
                            ctx.IASetVertexBuffers(0, 1, Some(vbs.as_ptr()), Some(&stride), Some(&0));
                        }
                    }
                    bound = Some(*kind);
                }
                ctx.PSSetShaderResources(6, Some(&[Some(srv.clone())]));
                ctx.PSSetSamplers(1, Some(&[Some(samp.clone())]));
                let tris = match kind {
                    Kind::Particles => 2 * (end - start),
                    _ => (end - start) / 3,
                };
                crate::runtime::domains::services::perf::draw(
                    crate::runtime::domains::services::perf::UI3D,
                    1,
                    tris as u64,
                );
                match kind {
                    Kind::Particles => ctx.DrawInstanced(6, end - start, 0, *start),
                    _ => ctx.DrawIndexed(end - start, *start, 0),
                }
            }

            ctx.OMSetRenderTargets(None, None);
            ctx.VSSetShaderResources(2, Some(&[None, None, None, None]));
        }
        d3d11::unbind_ps_srvs(ctx, 0, 8);
    }

    /// Draws the world GUIs among `batches` onto `target_rtv` (the shown view),
    /// tested against `depth_srv`. Effects go through `effects`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        rs: &RenderState,
        target_rtv: &ID3D11RenderTargetView,
        size: [u32; 2],
        depth_srv: &ID3D11ShaderResourceView,
        msaa: bool,
        batches: &[GuiBatch],
        tone_curve: Option<[f32; 5]>,
        fog: [f32; 4],
        fog_rgb: [f32; 3],
    ) {
        let Some(up) = self.upload(rs, batches, None, tone_curve, fog, fog_rgb) else { return };
        crate::gpu_timing::scope(&rs.device, &rs.context, "gpu: world gui", || {
            self.record(&rs.context, target_rtv, size, depth_srv, msaa, false, &up);
        });
    }

    /// Draws world GUIs and effects into the scene's HDR image (`hdr_rtv` / `hdr_srv`,
    /// linear light, before postprocessing), blended in the
    /// square root of linear light as Studio blends them: the image is
    /// copied into that space, the effects drawn over it far to near, and
    /// squared back. False when there were none (the view needn't be
    /// finished again).
    #[allow(clippy::too_many_arguments)]
    pub fn effects(
        &mut self,
        rs: &RenderState,
        hdr_rtv: &ID3D11RenderTargetView,
        hdr_srv: &ID3D11ShaderResourceView,
        size: [u32; 2],
        depth_srv: &ID3D11ShaderResourceView,
        msaa: bool,
        batches: &[GuiBatch],
        fx: Option<&FxFrame>,
        tone_curve: Option<[f32; 5]>,
        fog: [f32; 4],
        fog_rgb: [f32; 3],
    ) -> bool {
        let up = {
            let _b = crate::bench::scope("fx_upload");
            self.upload(rs, batches, fx, tone_curve, fog, fog_rgb)
        };
        let Some(up) = up else { return false };
        let _b = crate::bench::scope("fx_encode");
        let device = &rs.device;
        let ctx = &rs.context;
        if self.fx_tex.as_ref().is_none_or(|(s, _, _)| *s != size) {
            let tex = d3d11::create_tex2d(
                device,
                &Tex2dSpec {
                    width: size[0],
                    height: size[1],
                    mips: 1,
                    array_size: 1,
                    format: FX_FORMAT,
                    samples: 1,
                    bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                    misc_flags: 0,
                },
                None,
            );
            let rtv = d3d11::create_rtv(device, &tex, FX_FORMAT, 0, false);
            let srv = d3d11::create_srv_2d(device, &tex, FX_FORMAT, false, 0, 1);
            self.fx_tex = Some((size, rtv, srv));
        }
        let Some((_, fx_rtv, fx_srv)) = self.fx_tex.as_ref() else { return false };

        crate::gpu_timing::scope(device, ctx, "gpu: world gui effects", || {
            d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_glass_mask, None);
                ctx.RSSetState(&self.rast_state);
                ctx.OMSetDepthStencilState(&self.depth_off, 0);
                ctx.OMSetBlendState(&self.blend_opaque, None, 0xffff_ffff);

                // Effects in: sqrt(max(hdr, 0)) -> fx_tex.
                ctx.OMSetRenderTargets(Some(&[Some(fx_rtv.clone())]), None);
                ctx.PSSetShader(&self.ps_glass_in, None);
                ctx.PSSetShaderResources(7, Some(&[Some(hdr_srv.clone())]));
                ctx.Draw(3, 0);
                ctx.OMSetRenderTargets(None, None);
            }
            d3d11::unbind_ps_srvs(ctx, 7, 1);

            // World GUIs and effects blended over fx_tex.
            self.record(ctx, fx_rtv, size, depth_srv, msaa, true, &up);

            // Effects out: square fx_tex -> hdr_rtv.
            d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_glass_mask, None);
                ctx.RSSetState(&self.rast_state);
                ctx.OMSetDepthStencilState(&self.depth_off, 0);
                ctx.OMSetBlendState(&self.blend_opaque, None, 0xffff_ffff);
                ctx.OMSetRenderTargets(Some(&[Some(hdr_rtv.clone())]), None);
                ctx.PSSetShader(&self.ps_glass_out, None);
                ctx.PSSetShaderResources(7, Some(&[Some(fx_srv.clone())]));
                ctx.Draw(3, 0);
                ctx.OMSetRenderTargets(None, None);
            }
            d3d11::unbind_ps_srvs(ctx, 7, 1);
        });
        true
    }
}
