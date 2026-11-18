//! Terrain water drawn like Studio's high-quality water shader (the
//! SmoothWater FS with GBufferColor / GBufferDepth; see
//! docs/MECHANICS.md "Round 24: water"): the scene behind the surface shows
//! through, bent by the ripples and blended toward the lit WaterColor by
//! depth; the sky reflects by the Fresnel term; the sun glints off the
//! animated normal maps (PlatformContent textures/water/normal_01..25.dds);
//! two crossing sine waves move the surface.
//!
//! The pass runs after the opaque scene, reading a copy of its colour and
//! its (multisampled) depth, so it tests depth itself and has no depth
//! attachment shared with that snapshot. A separate depth attachment orders
//! overlapping water surfaces. See-through parts are drawn after it.

use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11DepthStencilState, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11VertexShader, D3D11_BIND_SHADER_RESOURCE, D3D11_COMPARISON_GREATER, D3D11_CULL_NONE,
    D3D11_FILTER_ANISOTROPIC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

use crate::d3d11::{self, BlendMode, Tex2dSpec};

/// The pass's uniforms (`WaterParams` in shader.wgsl).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WaterRaw {
    /// x wave number (rad / stud), y phase (rad), z amplitude (studs), w frame blend.
    pub wave: [f32; 4],
    /// x frame a, y frame b (array layers), z depth scale (/ stud), w depth offset.
    pub frames: [f32; 4],
    /// xy target size (pixels), z wave fade (/ stud), w the near plane.
    pub view: [f32; 4],
}

/// What a frame's water needs from the place (Terrain and time).
#[derive(Clone, Copy, Debug, Default)]
pub struct WaterLook {
    /// Seconds since the view started (animation).
    pub time: f32,
    pub wave_size: f32,
    pub wave_speed: f32,
    pub transparency: f32,
}

pub const FRAMES: u32 = 25;
const SIZE: u32 = 256;

pub struct WaterPass {
    pub cb: ID3D11Buffer,
    vs: ID3D11VertexShader,
    ps_1x: ID3D11PixelShader,
    ps_msaa: ID3D11PixelShader,
    normals: ID3D11ShaderResourceView,
    sampler: ID3D11SamplerState,
    depth_state: ID3D11DepthStencilState,
    blend_state: ID3D11BlendState,
    rast_state: ID3D11RasterizerState,
    layers: u32,
}

impl WaterPass {
    pub fn new(device: &ID3D11Device, _ctx: &ID3D11DeviceContext, _samples: u32) -> Self {
        let vs = d3d11::create_vs(device, crate::d3d11::shaders::VS_WATER);
        let ps_1x = d3d11::create_ps(device, crate::d3d11::shaders::PS_WATER_1X);
        let ps_msaa = d3d11::create_ps(device, crate::d3d11::shaders::PS_WATER_MSAA);
        let cb = d3d11::create_cb(device, std::mem::size_of::<WaterRaw>());
        let (normals, layers) = load_normals(device);
        let sampler = d3d11::create_sampler(
            device,
            D3D11_FILTER_ANISOTROPIC,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            crate::mech::TEXTURE_ANISOTROPY as u32,
        );
        let depth_state = d3d11::create_depth_state(device, true, true, D3D11_COMPARISON_GREATER);
        let blend_state = d3d11::create_blend_state(device, BlendMode::Opaque);
        let rast_state = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        WaterPass {
            cb,
            vs,
            ps_1x,
            ps_msaa,
            normals,
            sampler,
            depth_state,
            blend_state,
            rast_state,
            layers,
        }
    }

    pub fn upload(&self, ctx: &ID3D11DeviceContext, raw: &WaterRaw) {
        d3d11::upload_cb(ctx, &self.cb, raw);
    }

    /// Bind shaders, pipeline states, uniforms (`b3`), normal array (`t21`),
    /// scene copy (`t22`), depth (`t23`), and sampler (`s4`).
    pub fn bind(
        &self,
        ctx: &ID3D11DeviceContext,
        scene_srv: &ID3D11ShaderResourceView,
        depth_srv: &ID3D11ShaderResourceView,
        msaa: bool,
    ) {
        let ps = if msaa { &self.ps_msaa } else { &self.ps_1x };
        unsafe {
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(ps, None);
            ctx.RSSetState(&self.rast_state);
            ctx.OMSetDepthStencilState(&self.depth_state, 0);
            ctx.OMSetBlendState(&self.blend_state, None, 0xffff_ffff);
            let cbs = [Some(self.cb.clone())];
            ctx.VSSetConstantBuffers(3, Some(&cbs));
            ctx.PSSetConstantBuffers(3, Some(&cbs));
            let srvs = [
                Some(self.normals.clone()),
                Some(scene_srv.clone()),
                Some(depth_srv.clone()),
            ];
            ctx.PSSetShaderResources(21, Some(&srvs));
            let samplers = [Some(self.sampler.clone())];
            ctx.PSSetSamplers(4, Some(&samplers));
        }
    }

    /// Unbind water's PS SRV slots (`t21..=t23`) after drawing.
    pub fn unbind(&self, ctx: &ID3D11DeviceContext) {
        d3d11::unbind_ps_srvs(ctx, 21, 3);
    }

    /// This frame's uniforms.
    pub fn uniforms(&self, look: &WaterLook, size: [u32; 2]) -> WaterRaw {
        // The normal maps play through their frames (an estimate of Studio's
        // rate: WaterWaveSpeed 10 → 8 frames / s), blending between two.
        let t = look.time * look.wave_speed.max(0.0) * crate::mech::WATER_FRAMES_PER_SPEED;
        let n = self.layers.max(1) as f32;
        let a = t.floor().rem_euclid(n);
        let b = (a + 1.0).rem_euclid(n);
        // Waves (SmoothWater VS): y += (sin((z − x)k − φ) + sin((z + x)k + φ)) × amplitude.
        let phase = look.time * look.wave_speed * crate::mech::WATER_WAVE_PHASE_PER_SPEED;
        // Depth blend toward the water colour (Studio's CB3[2], updateWater
        // Studio): cover = clamp(depth × T / (1990 T + 10) + 1 − T), T
        // the transparency: at 0.3 a 50-stud bottom shows 28 %, the sky
        // under 500 studs of water 6 %.
        let clear = look.transparency.clamp(0.0, 1.0);
        let scale = clear / (1990.0 * clear + 10.0);
        WaterRaw {
            wave: [
                crate::mech::WATER_WAVE_NUMBER,
                phase,
                look.wave_size.max(0.0) * crate::mech::WATER_WAVE_AMPLITUDE,
                t.fract(),
            ],
            frames: [a, b, scale, 1.0 - clear],
            view: [size[0] as f32, size[1] as f32, crate::mech::WATER_WAVE_FADE, crate::mech::NEAR],
        }
    }
}

/// The 25 normal map frames as one mipmapped array (a flat map if missing).
fn load_normals(device: &ID3D11Device) -> (ID3D11ShaderResourceView, u32) {
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for i in 1..=FRAMES {
        let id = format!("rbxasset://textures/water/normal_{i:02}.dds");
        let img = crate::assets::resolve(&id, None)
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| crate::texture::decode(&b).ok())
            .filter(|img| img.width == SIZE && img.height == SIZE);
        match img {
            Some(img) => frames.push(img.pixels),
            None => break,
        }
    }
    if frames.len() != FRAMES as usize {
        // Flat: x, y at 0.5 (.wy), smooth.
        frames = vec![[0u8, 128, 128, 128].repeat((SIZE * SIZE) as usize)];
    }
    let layers = frames.len() as u32;
    let mips = 32 - SIZE.leading_zeros();
    // Keep each mip allocation alive across CreateTexture2D (LayerMajor order: layer * mips + mip).
    let mut level_bufs: Vec<(Vec<u8>, u32)> = Vec::with_capacity((layers * mips) as usize);
    for f in &frames {
        let mut level = f.clone();
        let mut s = SIZE;
        for _ in 0..mips {
            level_bufs.push((level.clone(), s * 4));
            if s == 1 {
                break;
            }
            level = halve(&level, s);
            s /= 2;
        }
    }
    let subresources: Vec<D3D11_SUBRESOURCE_DATA> = level_bufs
        .iter()
        .map(|(buf, pitch)| D3D11_SUBRESOURCE_DATA {
            pSysMem: buf.as_ptr().cast(),
            SysMemPitch: *pitch,
            SysMemSlicePitch: 0,
        })
        .collect();
    let tex = d3d11::create_tex2d(
        device,
        &Tex2dSpec {
            width: SIZE,
            height: SIZE,
            mips,
            array_size: layers,
            format: DXGI_FORMAT_R8G8B8A8_UNORM,
            samples: 1,
            bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            misc_flags: 0,
        },
        Some(&subresources),
    );
    let srv = d3d11::create_srv_2d_array(
        device,
        &tex,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        mips,
        0,
        layers,
    );
    (srv, layers)
}

/// A 2× box-filtered mip of a square RGBA8 image (linear: normal data).
fn halve(src: &[u8], s: u32) -> Vec<u8> {
    let h = s / 2;
    let mut out = vec![0u8; (h * h * 4) as usize];
    for y in 0..h {
        for x in 0..h {
            for c in 0..4 {
                let at = |xx: u32, yy: u32| src[((yy * s + xx) * 4 + c) as usize] as u32;
                let v = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                out[((y * h + x) * 4 + c) as usize] = ((v + 2) / 4) as u8;
            }
        }
    }
    out
}
