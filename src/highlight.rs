//! Highlight instances, as Studio 0.747 draws them (checks/mesh-deform/
//! AUDIT.md (g), tools/highlight.py): over the finished image, per
//! Highlight,
//! - the fill: FillColor over every pixel of the adornee's silhouette,
//!   alpha 1 − FillTransparency, a plain blend of display values (green at
//!   0.5 over a 198 grey part: 99, 226, 99);
//! - the outline: 2 px along the silhouette's edge, one each side of it,
//!   OutlineColor at 1 − OutlineTransparency over what is there;
//! - DepthMode AlwaysOnTop: the whole silhouette, through anything in front
//!   (fill and outline drawn over a wall hiding half the part); Occluded:
//!   only where the adornee is the nearest surface, the outline also along
//!   the edge of what hides it.
//! The silhouette is the adornee's drawn geometry (skinned meshes deformed,
//! layered clothing on), rendered into a mask with and without the scene's
//! depth.

use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use windows::core::s;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT,
    DXGI_FORMAT_R8G8_UNORM,
};

use crate::d3d11::{self, shaders, Tex2dSpec};

/// One Highlight's geometry and look for the frame.
pub struct HighlightDraw {
    /// The parts it covers (the renderer's items).
    pub items: Vec<rbx_dom_weak::types::Ref>,
    /// Display-value colours, alpha = 1 − transparency.
    pub fill: [f32; 4],
    pub outline: [f32; 4],
    /// DepthMode AlwaysOnTop.
    pub on_top: bool,
}

/// Studio draws at most this many Highlights at once.
pub const MAX: usize = 31;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Inst {
    pub r0: [f32; 4],
    pub r1: [f32; 4],
    pub r2: [f32; 4],
    pub size: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    view_proj: [[f32; 4]; 4],
    fill: [f32; 4],
    outline: [f32; 4],
    /// x: 1 AlwaysOnTop, 0 Occluded.
    mode: [f32; 4],
}

struct Target {
    size: [u32; 2],
    samples: u32,
    mask_ms_tex: ID3D11Texture2D,
    mask_ms_rtv: ID3D11RenderTargetView,
    mask_one_tex: ID3D11Texture2D,
    mask_one_srv: ID3D11ShaderResourceView,
}

pub struct HighlightPass {
    vs_mask: ID3D11VertexShader,
    ps_mask: ID3D11PixelShader,
    vs_full: ID3D11VertexShader,
    ps_comp: ID3D11PixelShader,
    il: ID3D11InputLayout,
    rast: ID3D11RasterizerState,
    depth_off: ID3D11DepthStencilState,
    depth_ge: ID3D11DepthStencilState,
    blend_red: ID3D11BlendState,
    blend_green: ID3D11BlendState,
    blend_comp: ID3D11BlendState,
    params_cb: ID3D11Buffer,
    insts: ID3D11Buffer,
    inst_cap: usize,
    target: Option<Target>,
}

const MASK_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8_UNORM;

fn create_write_mask_blend(device: &ID3D11Device, mask: u8) -> ID3D11BlendState {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: false.into(),
        RenderTargetWriteMask: mask,
        ..Default::default()
    };
    let mut out = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.expect("CreateBlendState mask");
    out.unwrap()
}

fn create_comp_blend(device: &ID3D11Device) -> ID3D11BlendState {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut out = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.expect("CreateBlendState comp");
    out.unwrap()
}

impl HighlightPass {
    pub fn new(device: &ID3D11Device, _samples: u32) -> Self {
        let vs_mask = d3d11::create_vs(device, shaders::VS_HL_MASK);
        let ps_mask = d3d11::create_ps(device, shaders::PS_HL_MASK);
        let vs_full = d3d11::create_vs(device, shaders::VS_HL_FULL);
        let ps_comp = d3d11::create_ps(device, shaders::PS_HL_COMP);

        let elems = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 2,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 1,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 3,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 1,
                AlignedByteOffset: 16,
                InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 4,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 1,
                AlignedByteOffset: 32,
                InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 5,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 1,
                AlignedByteOffset: 48,
                InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
        ];
        let il = d3d11::create_input_layout(device, &elems, shaders::VS_HL_MASK);
        let rast = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        let depth_off = d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        // reverse-Z: nearer is greater; the adornee's own surfaces are equal.
        let depth_ge =
            d3d11::create_depth_state(device, true, false, D3D11_COMPARISON_GREATER_EQUAL);
        let blend_red = create_write_mask_blend(device, D3D11_COLOR_WRITE_ENABLE_RED.0 as u8);
        let blend_green = create_write_mask_blend(device, D3D11_COLOR_WRITE_ENABLE_GREEN.0 as u8);
        let blend_comp = create_comp_blend(device);
        let params_cb = d3d11::create_cb(device, std::mem::size_of::<Params>());
        let inst_cap = 64;
        let insts = Self::inst_buf(device, inst_cap);

        HighlightPass {
            vs_mask,
            ps_mask,
            vs_full,
            ps_comp,
            il,
            rast,
            depth_off,
            depth_ge,
            blend_red,
            blend_green,
            blend_comp,
            params_cb,
            insts,
            inst_cap,
            target: None,
        }
    }

    fn inst_buf(device: &ID3D11Device, n: usize) -> ID3D11Buffer {
        d3d11::create_dynamic_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            n * std::mem::size_of::<Inst>(),
        )
    }

    /// Draws the highlights over `display_rtv`: per highlight, the meshes
    /// (vertex buffer, vertex count, instance) of its parts.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        size: [u32; 2],
        samples: u32,
        depth_dsv: &ID3D11DepthStencilView,
        display_rtv: &ID3D11RenderTargetView,
        view_proj: Mat4,
        list: &[(&HighlightDraw, Vec<(&ID3D11Buffer, u32, Inst)>)],
    ) {
        if list.is_empty() {
            return;
        }
        let samples = samples.max(1);
        if self
            .target
            .as_ref()
            .is_none_or(|t| t.size != size || t.samples != samples)
        {
            let mask_one_tex = d3d11::create_tex2d(
                device,
                &Tex2dSpec {
                    width: size[0],
                    height: size[1],
                    mips: 1,
                    array_size: 1,
                    format: MASK_FORMAT,
                    samples: 1,
                    bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                    misc_flags: 0,
                },
                None,
            );
            let mask_one_srv =
                d3d11::create_srv_2d(device, &mask_one_tex, MASK_FORMAT, false, 0, 1);
            let (mask_ms_tex, mask_ms_rtv) = if samples > 1 {
                let tex = d3d11::create_tex2d(
                    device,
                    &Tex2dSpec {
                        width: size[0],
                        height: size[1],
                        mips: 1,
                        array_size: 1,
                        format: MASK_FORMAT,
                        samples,
                        bind_flags: D3D11_BIND_RENDER_TARGET.0 as u32,
                        misc_flags: 0,
                    },
                    None,
                );
                let rtv = d3d11::create_rtv(device, &tex, MASK_FORMAT, 0, true);
                (tex, rtv)
            } else {
                let rtv = d3d11::create_rtv(device, &mask_one_tex, MASK_FORMAT, 0, false);
                (mask_one_tex.clone(), rtv)
            };
            self.target = Some(Target {
                size,
                samples,
                mask_ms_tex,
                mask_ms_rtv,
                mask_one_tex,
                mask_one_srv,
            });
        }
        let n_inst: usize = list.iter().take(MAX).map(|(_, m)| m.len()).sum();
        if n_inst > self.inst_cap {
            self.inst_cap = n_inst.next_power_of_two();
            self.insts = Self::inst_buf(device, self.inst_cap);
        }
        let insts: Vec<Inst> = list
            .iter()
            .take(MAX)
            .flat_map(|(_, m)| m.iter().map(|x| x.2))
            .collect();
        if !insts.is_empty() {
            d3d11::update_dynamic(ctx, &self.insts, bytemuck::cast_slice(&insts));
        }
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let vertex_stride = std::mem::size_of::<crate::render::Vertex>() as u32;
        let inst_stride = std::mem::size_of::<Inst>() as u32;
        let vp_cols = view_proj.to_cols_array_2d();

        crate::gpu_timing::scope(device, ctx, "gpu: highlight", || {
            d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
            unsafe {
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.RSSetState(&self.rast);
            }
            let mut first = 0u32;
            for (h, meshes) in list.iter().take(MAX) {
                let p = Params {
                    view_proj: vp_cols,
                    fill: h.fill,
                    outline: h.outline,
                    mode: [if h.on_top { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
                };
                d3d11::upload_cb(ctx, &self.params_cb, &p);

                let range_start = first;
                first += meshes.len() as u32;

                let draw_meshes = || {
                    let cb = [Some(self.params_cb.clone())];
                    let ib = [Some(self.insts.clone())];
                    unsafe {
                        ctx.IASetInputLayout(&self.il);
                        ctx.VSSetShader(&self.vs_mask, None);
                        ctx.PSSetShader(&self.ps_mask, None);
                        ctx.VSSetConstantBuffers(0, Some(&cb));
                        ctx.IASetVertexBuffers(1, 1, Some(ib.as_ptr()), Some(&inst_stride), Some(&0));
                    }
                    for (k, (vbuf, count, _)) in meshes.iter().enumerate() {
                        let vb = [Some((*vbuf).clone())];
                        let n = range_start + k as u32;
                        unsafe {
                            ctx.IASetVertexBuffers(0, 1, Some(vb.as_ptr()), Some(&vertex_stride), Some(&0));
                            ctx.DrawInstanced(*count, 1, 0, n);
                        }
                    }
                };

                d3d11::unbind_ps_srvs(ctx, 0, 2);
                unsafe {
                    ctx.OMSetRenderTargets(Some(&[Some(target.mask_ms_rtv.clone())]), None);
                    ctx.ClearRenderTargetView(&target.mask_ms_rtv, &[0.0, 0.0, 0.0, 0.0]);
                    ctx.OMSetDepthStencilState(&self.depth_off, 0);
                    ctx.OMSetBlendState(&self.blend_red, None, !0);
                }
                draw_meshes();

                unsafe {
                    ctx.OMSetRenderTargets(
                        Some(&[Some(target.mask_ms_rtv.clone())]),
                        Some(depth_dsv),
                    );
                    ctx.OMSetDepthStencilState(&self.depth_ge, 0);
                    ctx.OMSetBlendState(&self.blend_green, None, !0);
                }
                draw_meshes();

                unsafe { ctx.OMSetRenderTargets(None, None) };
                if target.samples > 1 {
                    unsafe {
                        ctx.ResolveSubresource(
                            &target.mask_one_tex,
                            0,
                            &target.mask_ms_tex,
                            0,
                            MASK_FORMAT,
                        );
                    }
                }

                d3d11::unbind_ps_srvs(ctx, 0, 2);
                let cb = [Some(self.params_cb.clone())];
                let srvs = [Some(target.mask_one_srv.clone())];
                unsafe {
                    ctx.OMSetRenderTargets(Some(&[Some(display_rtv.clone())]), None);
                    ctx.IASetInputLayout(None);
                    ctx.OMSetDepthStencilState(&self.depth_off, 0);
                    ctx.OMSetBlendState(&self.blend_comp, None, !0);
                    ctx.VSSetShader(&self.vs_full, None);
                    ctx.PSSetShader(&self.ps_comp, None);
                    ctx.PSSetConstantBuffers(0, Some(&cb));
                    ctx.PSSetShaderResources(0, Some(&srvs));
                    ctx.Draw(3, 0);
                }
            }
            d3d11::unbind_ps_srvs(ctx, 0, 2);
            unsafe { ctx.OMSetRenderTargets(None, None) };
        });
    }
}
