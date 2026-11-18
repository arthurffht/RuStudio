//! GPU side of Studio's scene environment probe (env_probe.rs): two 128 px
//! cube slots (PrefilteredEnvIndoorTexture, PrefilteredEnvBlendTargetTexture)
//! that start as the default indoor cube, the capture's source pyramid, and
//! the FastConvDownsample / FastConvolution passes (env_probe.hlsl).

use crate::d3d11::{self, shaders, BlendMode, Tex2dSpec};
use crate::env_probe::{self, Task};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R16G16B16A16_FLOAT,
};

const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
const LEVELS: u32 = 8; // 128 .. 1

pub struct ProbeGpu {
    pub slots: [ID3D11Texture2D; 2],
    pub slot_srvs: [ID3D11ShaderResourceView; 2],
    /// The slot the shader reads as PrefilteredEnvIndoorTexture.
    pub cur: usize,
    slot_rtvs: [[[ID3D11RenderTargetView; 6]; LEVELS as usize]; 2],
    src: ID3D11Texture2D,
    src_srv: ID3D11ShaderResourceView,
    src_level_srvs: Vec<ID3D11ShaderResourceView>,
    src_rtvs: [[ID3D11RenderTargetView; 6]; LEVELS as usize],
    pub face_msaa: ID3D11Texture2D,
    pub face_rtv: ID3D11RenderTargetView,
    pub face_depth: ID3D11Texture2D,
    pub face_dsv: ID3D11DepthStencilView,
    pub face_resolve: ID3D11Texture2D,
    face_resolve_srv: ID3D11ShaderResourceView,
    samples: u32,
    pub globals_cb: ID3D11Buffer,
    params_cb: ID3D11Buffer,
    coeff_cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    vs_probe: ID3D11VertexShader,
    ps_default: ID3D11PixelShader,
    ps_blit: ID3D11PixelShader,
    ps_down: ID3D11PixelShader,
    ps_conv: ID3D11PixelShader,
    rast: ID3D11RasterizerState,
    depth_state: ID3D11DepthStencilState,
    blend: ID3D11BlendState,
}

fn cube_tex(device: &ID3D11Device) -> ID3D11Texture2D {
    d3d11::create_tex2d(
        device,
        &Tex2dSpec {
            width: env_probe::SIZE,
            height: env_probe::SIZE,
            mips: LEVELS,
            array_size: 6,
            format: FORMAT,
            samples: 1,
            bind_flags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            misc_flags: D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32,
        },
        None,
    )
}

fn cube_face_rtvs(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
) -> [[ID3D11RenderTargetView; 6]; LEVELS as usize] {
    std::array::from_fn(|m| {
        std::array::from_fn(|f| {
            d3d11::create_rtv_2d_array_slice(device, tex, FORMAT, m as u32, f as u32)
        })
    })
}

fn create_face_targets(
    device: &ID3D11Device,
    samples: u32,
) -> (
    ID3D11Texture2D,
    ID3D11RenderTargetView,
    ID3D11Texture2D,
    ID3D11DepthStencilView,
    ID3D11Texture2D,
    ID3D11ShaderResourceView,
) {
    let samples = samples.max(1);
    let msaa = samples > 1;
    let face_resolve = d3d11::create_tex2d(
        device,
        &Tex2dSpec {
            width: env_probe::SIZE,
            height: env_probe::SIZE,
            mips: 1,
            array_size: 1,
            format: FORMAT,
            samples: 1,
            bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            misc_flags: 0,
        },
        None,
    );
    let face_resolve_srv = d3d11::create_srv_2d(device, &face_resolve, FORMAT, false, 0, 1);
    let (face_msaa, face_rtv) = if msaa {
        let tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: env_probe::SIZE,
                height: env_probe::SIZE,
                mips: 1,
                array_size: 1,
                format: FORMAT,
                samples,
                bind_flags: D3D11_BIND_RENDER_TARGET.0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let rtv = d3d11::create_rtv(device, &tex, FORMAT, 0, true);
        (tex, rtv)
    } else {
        let rtv = d3d11::create_rtv(device, &face_resolve, FORMAT, 0, false);
        (face_resolve.clone(), rtv)
    };
    let face_depth = d3d11::create_tex2d(
        device,
        &Tex2dSpec {
            width: env_probe::SIZE,
            height: env_probe::SIZE,
            mips: 1,
            array_size: 1,
            format: DXGI_FORMAT_D32_FLOAT,
            samples,
            bind_flags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
            misc_flags: 0,
        },
        None,
    );
    let face_dsv = d3d11::create_dsv(device, &face_depth, DXGI_FORMAT_D32_FLOAT, msaa, false);
    (
        face_msaa,
        face_rtv,
        face_depth,
        face_dsv,
        face_resolve,
        face_resolve_srv,
    )
}

impl ProbeGpu {
    pub fn new(
        device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        samples: u32,
        globals_size: usize,
    ) -> Self {
        let slots = [cube_tex(device), cube_tex(device)];
        let slot_srvs = [
            d3d11::create_srv_cube(device, &slots[0], FORMAT, 0, LEVELS),
            d3d11::create_srv_cube(device, &slots[1], FORMAT, 0, LEVELS),
        ];
        let slot_rtvs = [cube_face_rtvs(device, &slots[0]), cube_face_rtvs(device, &slots[1])];
        let src = cube_tex(device);
        let src_srv = d3d11::create_srv_cube(device, &src, FORMAT, 0, LEVELS);
        let src_level_srvs = (0..LEVELS)
            .map(|m| d3d11::create_srv_cube(device, &src, FORMAT, m, 1))
            .collect();
        let src_rtvs = cube_face_rtvs(device, &src);
        let samples = samples.max(1);
        let (face_msaa, face_rtv, face_depth, face_dsv, face_resolve, face_resolve_srv) =
            create_face_targets(device, samples);
        let globals_cb = d3d11::create_cb(device, globals_size);
        let params_cb = d3d11::create_cb(device, 16);
        let coeff_cb = d3d11::create_immutable_buffer(
            device,
            D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            bytemuck::cast_slice(&env_probe::CONVOLUTION),
        );
        let sampler = d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let vs_probe = d3d11::create_vs(device, shaders::VS_PROBE);
        let ps_default = d3d11::create_ps(device, shaders::PS_PROBE_DEFAULT);
        let ps_blit = d3d11::create_ps(device, shaders::PS_PROBE_BLIT);
        let ps_down = d3d11::create_ps(device, shaders::PS_PROBE_DOWN);
        let ps_conv = d3d11::create_ps(device, shaders::PS_PROBE_CONV);
        let rast = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        let depth_state = d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        let blend = d3d11::create_blend_state(device, BlendMode::Opaque);
        ProbeGpu {
            slots,
            slot_srvs,
            cur: 0,
            slot_rtvs,
            src,
            src_srv,
            src_level_srvs,
            src_rtvs,
            face_msaa,
            face_rtv,
            face_depth,
            face_dsv,
            face_resolve,
            face_resolve_srv,
            samples,
            globals_cb,
            params_cb,
            coeff_cb,
            sampler,
            vs_probe,
            ps_default,
            ps_blit,
            ps_down,
            ps_conv,
            rast,
            depth_state,
            blend,
        }
    }

    pub fn set_samples(&mut self, device: &ID3D11Device, samples: u32) {
        let samples = samples.max(1);
        if self.samples == samples {
            return;
        }
        self.samples = samples;
        let (face_msaa, face_rtv, face_depth, face_dsv, face_resolve, face_resolve_srv) =
            create_face_targets(device, samples);
        self.face_msaa = face_msaa;
        self.face_rtv = face_rtv;
        self.face_depth = face_depth;
        self.face_dsv = face_dsv;
        self.face_resolve = face_resolve;
        self.face_resolve_srv = face_resolve_srv;
    }

    pub fn next(&self) -> usize {
        1 - self.cur
    }

    fn begin_state(&self, ctx: &ID3D11DeviceContext) {
        let cbs = [Some(self.params_cb.clone()), Some(self.coeff_cb.clone())];
        let samps = [Some(self.sampler.clone())];
        unsafe {
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.RSSetState(&self.rast);
            ctx.OMSetDepthStencilState(&self.depth_state, 0);
            ctx.OMSetBlendState(&self.blend, None, !0);
            ctx.VSSetShader(&self.vs_probe, None);
            ctx.PSSetConstantBuffers(0, Some(&cbs));
            ctx.PSSetSamplers(0, Some(&samps));
        }
    }

    fn run_pass(
        &self,
        ctx: &ID3D11DeviceContext,
        ps: &ID3D11PixelShader,
        target: &ID3D11RenderTargetView,
        env_srv: Option<&ID3D11ShaderResourceView>,
        face_srv: Option<&ID3D11ShaderResourceView>,
        p: [f32; 4],
    ) {
        d3d11::unbind_ps_srvs(ctx, 0, 2);
        d3d11::upload_cb(ctx, &self.params_cb, &p);
        let srvs = [env_srv.cloned(), face_srv.cloned()];
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            ctx.PSSetShader(ps, None);
            ctx.PSSetShaderResources(0, Some(&srvs));
            ctx.Draw(3, 0);
        }
    }

    /// Writes pending pass parameters (immediate context uploads per pass; kept for API parity).
    pub fn flush(&mut self, _ctx: &ID3D11DeviceContext) {}

    /// Both slots, every level, from the default indoor cube (`indoor_srv`,
    /// `indoor_size` px at its level 0): probe mip m = indoor lod log2(size / 128) + m.
    pub fn fill_default(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        indoor_srv: &ID3D11ShaderResourceView,
        indoor_size: u32,
    ) {
        crate::gpu_timing::scope(device, ctx, "gpu: env probe default", || {
            let offset = (indoor_size.max(1) as f32 / env_probe::SIZE as f32)
                .log2()
                .max(0.0);
            self.begin_state(ctx);
            for s in 0..2 {
                for m in 0..LEVELS {
                    let size = (env_probe::SIZE >> m).max(1) as f32;
                    d3d11::set_viewport(ctx, 0.0, 0.0, size, size);
                    for f in 0..6 {
                        self.run_pass(
                            ctx,
                            &self.ps_default,
                            &self.slot_rtvs[s][m as usize][f as usize],
                            Some(indoor_srv),
                            None,
                            [offset + m as f32, f as f32, 0.0, 0.0],
                        );
                    }
                }
            }
            d3d11::unbind_ps_srvs(ctx, 0, 2);
            unsafe { ctx.OMSetRenderTargets(None, None) };
        });
    }

    /// After a face is drawn into `face_msaa` (resolving to `face_resolve` when
    /// MSAA is active): mirror it into the source pyramid's level 0.
    pub fn store_face(&mut self, ctx: &ID3D11DeviceContext, face: u32) {
        unsafe { ctx.OMSetRenderTargets(None, None) };
        d3d11::unbind_ps_srvs(ctx, 0, 2);
        if self.samples > 1 {
            unsafe {
                ctx.ResolveSubresource(&self.face_resolve, 0, &self.face_msaa, 0, FORMAT);
            }
        }
        self.begin_state(ctx);
        d3d11::set_viewport(ctx, 0.0, 0.0, env_probe::SIZE as f32, env_probe::SIZE as f32);
        self.run_pass(
            ctx,
            &self.ps_blit,
            &self.src_rtvs[0][face as usize],
            None,
            Some(&self.face_resolve_srv),
            [0.0, face as f32, 0.0, 0.0],
        );
        d3d11::unbind_ps_srvs(ctx, 0, 2);
        unsafe { ctx.OMSetRenderTargets(None, None) };
    }

    /// Pyramid and convolution steps (`Task::Face` is drawn by the renderer).
    pub fn encode(&mut self, ctx: &ID3D11DeviceContext, task: Task) {
        match task {
            Task::Face(_) => {}
            Task::Pyramid => {
                self.begin_state(ctx);
                for m in 1..LEVELS {
                    let size = (env_probe::SIZE >> m).max(1) as f32;
                    d3d11::set_viewport(ctx, 0.0, 0.0, size, size);
                    for f in 0..6 {
                        self.run_pass(
                            ctx,
                            &self.ps_down,
                            &self.src_rtvs[m as usize][f as usize],
                            Some(&self.src_level_srvs[m as usize - 1]),
                            None,
                            [0.0, f as f32, 1.0 / size, 0.0],
                        );
                    }
                }
                d3d11::unbind_ps_srvs(ctx, 0, 2);
                unsafe { ctx.OMSetRenderTargets(None, None) };
                let next = self.next();
                for f in 0..6 {
                    // Subresource index = mip_slice(0) + array_slice(f) * mip_levels(LEVELS).
                    let sub = f * LEVELS;
                    unsafe {
                        ctx.CopySubresourceRegion(&self.slots[next], sub, 0, 0, 0, &self.src, sub, None);
                    }
                }
            }
            Task::Convolve { mip, first, count } => {
                let next = self.next();
                let size = (env_probe::SIZE >> mip).max(1) as f32;
                self.begin_state(ctx);
                d3d11::set_viewport(ctx, 0.0, 0.0, size, size);
                for f in first..(first + count).min(6) {
                    self.run_pass(
                        ctx,
                        &self.ps_conv,
                        &self.slot_rtvs[next][mip as usize][f as usize],
                        Some(&self.src_srv),
                        None,
                        [(mip - 1) as f32, f as f32, 0.0, 0.0],
                    );
                }
                d3d11::unbind_ps_srvs(ctx, 0, 2);
                unsafe { ctx.OMSetRenderTargets(None, None) };
            }
        }
    }
}
