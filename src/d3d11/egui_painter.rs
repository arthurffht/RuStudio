//! Direct3D 11 painter for egui: shaders, texture atlas, dynamic VB/IB, and scissor-clipped draws.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::device::{
    create_blend_state, create_cb, create_depth_state, create_dynamic_buffer, create_input_layout,
    create_ps, create_raster_state, create_sampler, create_srv_2d, create_tex2d, create_vs,
    set_scissor, set_viewport, unbind_ps_srvs, update_dynamic, upload_cb, BlendMode, Tex2dSpec,
};
use super::shaders;

const INITIAL_VB_BYTES: usize = 64 * 1024;
const INITIAL_IB_BYTES: usize = 32 * 1024;

// Verify epaint::Vertex and Color32 are packed as expected by D3D11 input layout & RGBA8 upload.
const _: () = assert!(size_of::<egui::Color32>() == 4);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EguiCB {
    viewport_size_points: [f32; 2],
    _pad: [f32; 2],
}

struct ManagedTex {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    size: [u32; 2],
    options: egui::TextureOptions,
}

struct DrawCmd {
    index_count: u32,
    start_index: u32,
    base_vertex: i32,
    scissor: [i32; 4],
    texture_id: egui::TextureId,
}

/// D3D11 renderer for egui paint primitives, managed atlas textures, and native user SRVs.
pub struct EguiD3d11 {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    cb: ID3D11Buffer,
    raster: ID3D11RasterizerState,
    depth: ID3D11DepthStencilState,
    blend: ID3D11BlendState,
    samplers: HashMap<egui::TextureOptions, ID3D11SamplerState>,
    default_sampler: ID3D11SamplerState,

    vb: ID3D11Buffer,
    vb_bytes: usize,
    ib: ID3D11Buffer,
    ib_bytes: usize,

    managed: HashMap<egui::TextureId, ManagedTex>,
    native: HashMap<u64, (ID3D11ShaderResourceView, [u32; 2])>,
    next_native_id: u64,

    cpu_verts: Vec<egui::epaint::Vertex>,
    cpu_indices: Vec<u32>,
    draw_cmds: Vec<DrawCmd>,
}

impl EguiD3d11 {
    pub fn new(device: &ID3D11Device) -> Self {
        let vs = create_vs(device, shaders::VS_EGUI);
        let ps = create_ps(device, shaders::PS_EGUI);

        let layout_descs = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"POSITION".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: offset_of!(egui::epaint::Vertex, pos) as u32,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: offset_of!(egui::epaint::Vertex, uv) as u32,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"COLOR".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                InputSlot: 0,
                AlignedByteOffset: offset_of!(egui::epaint::Vertex, color) as u32,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let input_layout = create_input_layout(device, &layout_descs, shaders::VS_EGUI);
        let cb = create_cb(device, size_of::<EguiCB>());

        // egui emits mixed triangle winding; disable backface culling and enable scissor clipping.
        let raster = create_raster_state(device, D3D11_CULL_NONE, false, true, 0, 0.0);
        let depth = create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        let blend = create_blend_state(device, BlendMode::PremulAlpha);

        let default_opts = egui::TextureOptions::LINEAR;
        let default_sampler = build_sampler(device, default_opts);
        let mut samplers = HashMap::new();
        samplers.insert(default_opts, default_sampler.clone());

        let vb = create_dynamic_buffer(device, D3D11_BIND_VERTEX_BUFFER.0 as u32, INITIAL_VB_BYTES);
        let ib = create_dynamic_buffer(device, D3D11_BIND_INDEX_BUFFER.0 as u32, INITIAL_IB_BYTES);

        Self {
            vs,
            ps,
            input_layout,
            cb,
            raster,
            depth,
            blend,
            samplers,
            default_sampler,
            vb,
            vb_bytes: INITIAL_VB_BYTES,
            ib,
            ib_bytes: INITIAL_IB_BYTES,
            managed: HashMap::new(),
            native: HashMap::new(),
            next_native_id: 1,
            cpu_verts: Vec::new(),
            cpu_indices: Vec::new(),
            draw_cmds: Vec::new(),
        }
    }

    pub fn update_textures(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        delta: &egui::TexturesDelta,
    ) {
        for (&id, image_deltas) in &delta.set {
            for img_delta in image_deltas {
                self.apply_image_delta(device, ctx, id, img_delta);
            }
        }
    }

    pub fn free_textures(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.managed.remove(id);
        }
    }

    fn apply_image_delta(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        id: egui::TextureId,
        delta: &egui::epaint::ImageDelta,
    ) {
        let egui::ImageData::Color(color_image) = &delta.image;
        let w = color_image.width() as u32;
        let h = color_image.height() as u32;
        if w == 0 || h == 0 || color_image.pixels.len() != (w as usize) * (h as usize) {
            return;
        }
        let pixels_ptr: *const u8 = color_image.pixels.as_ptr().cast();
        let row_pitch = w * 4;

        // Pre-warm sampler for this texture's filtering/wrapping options.
        self.samplers
            .entry(delta.options)
            .or_insert_with(|| build_sampler(device, delta.options));

        if let Some([px, py]) = delta.pos {
            let Some(existing) = self.managed.get_mut(&id) else {
                return;
            };
            let px = px as u32;
            let py = py as u32;
            if px + w > existing.size[0] || py + h > existing.size[1] {
                return;
            }
            let region = D3D11_BOX {
                left: px,
                top: py,
                front: 0,
                right: px + w,
                bottom: py + h,
                back: 1,
            };
            unsafe {
                ctx.UpdateSubresource(
                    &existing.tex,
                    0,
                    Some(&region),
                    pixels_ptr.cast(),
                    row_pitch,
                    0,
                );
            }
            existing.options = delta.options;
            return;
        }

        // Full texture update: reuse allocation when dimensions already match.
        if let Some(existing) = self.managed.get_mut(&id) {
            if existing.size == [w, h] {
                unsafe {
                    ctx.UpdateSubresource(
                        &existing.tex,
                        0,
                        None,
                        pixels_ptr.cast(),
                        row_pitch,
                        0,
                    );
                }
                existing.options = delta.options;
                return;
            }
        }

        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels_ptr.cast(),
            SysMemPitch: row_pitch,
            SysMemSlicePitch: 0,
        };
        let tex = create_tex2d(
            device,
            &Tex2dSpec {
                width: w,
                height: h,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                samples: 1,
                bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                misc_flags: 0,
            },
            Some(&[init]),
        );
        let srv = create_srv_2d(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, 1);
        self.managed.insert(
            id,
            ManagedTex {
                tex,
                srv,
                size: [w, h],
                options: delta.options,
            },
        );
    }

    pub fn register_native_texture(
        &mut self,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) -> egui::TextureId {
        let id = self.next_native_id;
        self.next_native_id += 1;
        self.native.insert(id, (srv, size));
        egui::TextureId::User(id)
    }

    pub fn update_native_texture(
        &mut self,
        id: egui::TextureId,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) {
        if let egui::TextureId::User(raw) = id {
            self.native.insert(raw, (srv, size));
        }
    }

    pub fn unregister_native_texture(&mut self, id: egui::TextureId) {
        if let egui::TextureId::User(raw) = id {
            self.native.remove(&raw);
        }
    }

    pub fn texture_srv(
        &self,
        id: egui::TextureId,
    ) -> Option<(&ID3D11ShaderResourceView, [u32; 2])> {
        match id {
            egui::TextureId::User(raw) => self.native.get(&raw).map(|(s, sz)| (s, *sz)),
            egui::TextureId::Managed(_) => self.managed.get(&id).map(|m| (&m.srv, m.size)),
        }
    }

    fn resolve_binding(
        &self,
        id: egui::TextureId,
    ) -> Option<(&ID3D11ShaderResourceView, &ID3D11SamplerState)> {
        match id {
            egui::TextureId::User(raw) => self
                .native
                .get(&raw)
                .map(|(srv, _)| (srv, &self.default_sampler)),
            egui::TextureId::Managed(_) => self.managed.get(&id).map(|m| {
                let samp = self.samplers.get(&m.options).unwrap_or(&self.default_sampler);
                (&m.srv, samp)
            }),
        }
    }

    pub fn paint(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        screen_size_px: [u32; 2],
        pixels_per_point: f32,
        primitives: &[egui::ClippedPrimitive],
    ) {
        if screen_size_px[0] == 0
            || screen_size_px[1] == 0
            || pixels_per_point <= 0.0
            || !pixels_per_point.is_finite()
            || primitives.is_empty()
        {
            return;
        }

        self.cpu_verts.clear();
        self.cpu_indices.clear();
        self.draw_cmds.clear();

        for clipped in primitives {
            let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive else {
                continue;
            };
            if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            let vp_px = egui::epaint::ViewportInPixels::from_points(
                &clipped.clip_rect,
                pixels_per_point,
                screen_size_px,
            );
            if vp_px.width_px <= 0 || vp_px.height_px <= 0 {
                continue;
            }

            let Ok(base_vertex) = i32::try_from(self.cpu_verts.len()) else {
                break;
            };
            let start_index = self.cpu_indices.len() as u32;
            let index_count = mesh.indices.len() as u32;

            self.cpu_verts.extend_from_slice(&mesh.vertices);
            self.cpu_indices.extend_from_slice(&mesh.indices);
            self.draw_cmds.push(DrawCmd {
                index_count,
                start_index,
                base_vertex,
                scissor: [vp_px.left_px, vp_px.top_px, vp_px.width_px, vp_px.height_px],
                texture_id: mesh.texture_id,
            });
        }

        if self.draw_cmds.is_empty() {
            return;
        }

        let vb_needed = self.cpu_verts.len() * size_of::<egui::epaint::Vertex>();
        if vb_needed > self.vb_bytes {
            let new_cap = vb_needed.next_power_of_two().max(INITIAL_VB_BYTES);
            self.vb = create_dynamic_buffer(device, D3D11_BIND_VERTEX_BUFFER.0 as u32, new_cap);
            self.vb_bytes = new_cap;
        }
        let ib_needed = self.cpu_indices.len() * size_of::<u32>();
        if ib_needed > self.ib_bytes {
            let new_cap = ib_needed.next_power_of_two().max(INITIAL_IB_BYTES);
            self.ib = create_dynamic_buffer(device, D3D11_BIND_INDEX_BUFFER.0 as u32, new_cap);
            self.ib_bytes = new_cap;
        }

        let vb_bytes = unsafe {
            std::slice::from_raw_parts(self.cpu_verts.as_ptr().cast::<u8>(), vb_needed)
        };
        let ib_bytes = unsafe {
            std::slice::from_raw_parts(self.cpu_indices.as_ptr().cast::<u8>(), ib_needed)
        };
        update_dynamic(ctx, &self.vb, vb_bytes);
        update_dynamic(ctx, &self.ib, ib_bytes);

        let cb_val = EguiCB {
            viewport_size_points: [
                screen_size_px[0] as f32 / pixels_per_point,
                screen_size_px[1] as f32 / pixels_per_point,
            ],
            _pad: [0.0; 2],
        };
        upload_cb(ctx, &self.cb, &cb_val);

        let stride = size_of::<egui::epaint::Vertex>() as u32;
        let offset = 0u32;
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.IASetInputLayout(&self.input_layout);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetVertexBuffers(0, 1, Some(&Some(self.vb.clone())), Some(&stride), Some(&offset));
            ctx.IASetIndexBuffer(&self.ib, DXGI_FORMAT_R32_UINT, 0);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.HSSetShader(None, None);
            ctx.DSSetShader(None, None);
            ctx.GSSetShader(None, None);
            ctx.CSSetShader(None, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.RSSetState(&self.raster);
            ctx.OMSetDepthStencilState(&self.depth, 0);
            ctx.OMSetBlendState(&self.blend, None, 0xFFFF_FFFF);
        }
        set_viewport(ctx, 0.0, 0.0, screen_size_px[0] as f32, screen_size_px[1] as f32);

        for cmd in &self.draw_cmds {
            let Some((srv, sampler)) = self.resolve_binding(cmd.texture_id) else {
                continue;
            };
            set_scissor(ctx, cmd.scissor[0], cmd.scissor[1], cmd.scissor[2], cmd.scissor[3]);
            unsafe {
                ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                ctx.PSSetSamplers(0, Some(&[Some(sampler.clone())]));
                ctx.DrawIndexed(cmd.index_count, cmd.start_index, cmd.base_vertex);
            }
        }

        // Release RTV and PS SRV bindings immediately so native user SRVs and backbuffers never alias RTV/SRV across passes.
        unbind_ps_srvs(ctx, 0, 1);
        unsafe { ctx.OMSetRenderTargets(None, None) };
    }
}

fn address_mode(wrap: egui::TextureWrapMode) -> D3D11_TEXTURE_ADDRESS_MODE {
    match wrap {
        egui::TextureWrapMode::ClampToEdge => D3D11_TEXTURE_ADDRESS_CLAMP,
        egui::TextureWrapMode::Repeat => D3D11_TEXTURE_ADDRESS_WRAP,
        egui::TextureWrapMode::MirroredRepeat => D3D11_TEXTURE_ADDRESS_MIRROR,
    }
}

fn build_sampler(device: &ID3D11Device, opts: egui::TextureOptions) -> ID3D11SamplerState {
    use egui::TextureFilter::{Linear, Nearest};
    let filter = match (opts.minification, opts.magnification, opts.mipmap_mode) {
        (Nearest, Nearest, Some(Nearest) | None) => D3D11_FILTER_MIN_MAG_MIP_POINT,
        (Nearest, Nearest, Some(Linear)) => D3D11_FILTER_MIN_MAG_POINT_MIP_LINEAR,
        (Nearest, Linear, Some(Nearest) | None) => D3D11_FILTER_MIN_POINT_MAG_LINEAR_MIP_POINT,
        (Nearest, Linear, Some(Linear)) => D3D11_FILTER_MIN_POINT_MAG_MIP_LINEAR,
        (Linear, Nearest, Some(Nearest) | None) => D3D11_FILTER_MIN_LINEAR_MAG_MIP_POINT,
        (Linear, Nearest, Some(Linear)) => D3D11_FILTER_MIN_LINEAR_MAG_POINT_MIP_LINEAR,
        (Linear, Linear, Some(Nearest)) => D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        (Linear, Linear, Some(Linear) | None) => D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    };
    let addr = address_mode(opts.wrap_mode);
    create_sampler(device, filter, addr, addr, addr, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::{create_device, OffscreenTarget};

    fn solid_quad_primitive(
        rect: egui::Rect,
        clip_rect: egui::Rect,
        color: egui::Color32,
        tex_id: egui::TextureId,
        uv_rect: egui::Rect,
    ) -> egui::ClippedPrimitive {
        let mut mesh = egui::Mesh::with_texture(tex_id);
        mesh.vertices = vec![
            egui::epaint::Vertex { pos: rect.left_top(), uv: uv_rect.left_top(), color },
            egui::epaint::Vertex { pos: rect.right_top(), uv: uv_rect.right_top(), color },
            egui::epaint::Vertex { pos: rect.right_bottom(), uv: uv_rect.right_bottom(), color },
            egui::epaint::Vertex { pos: rect.left_bottom(), uv: uv_rect.left_bottom(), color },
        ];
        mesh.indices = vec![0, 1, 2, 0, 2, 3];
        egui::ClippedPrimitive {
            clip_rect,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn native_texture_registry_lifecycle() {
        let Ok(rs) = create_device() else {
            return;
        };
        let tex = create_tex2d(
            &rs.device,
            &Tex2dSpec {
                width: 8,
                height: 8,
                ..Tex2dSpec::default()
            },
            None,
        );
        let srv = create_srv_2d(&rs.device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, 1);
        let id = rs.register_native_texture(srv.clone(), [8, 8]);
        assert!(matches!(id, egui::TextureId::User(_)));
        assert_eq!(rs.texture_srv(id).unwrap().1, [8, 8]);

        rs.update_native_texture(id, srv, [16, 16]);
        assert_eq!(rs.texture_srv(id).unwrap().1, [16, 16]);

        rs.unregister_native_texture(id);
        assert!(rs.texture_srv(id).is_none());
    }

    #[test]
    fn offscreen_paint_renders_full_partial_texture_blend_and_scissor() {
        let Ok(rs) = create_device() else {
            return;
        };
        let target = OffscreenTarget::new(&rs.device, 16, 16);
        let clear = [0.0f32, 0.0, 0.0, 1.0];
        unsafe { rs.context.ClearRenderTargetView(target.rtv(), &clear) };

        // Upload a 4x4 white managed texture, then patch bottom-right 2x2 to solid red.
        let tex_id = egui::TextureId::Managed(0);
        let mut delta = egui::TexturesDelta::default();
        delta.push(
            tex_id,
            egui::epaint::ImageDelta::full(
                egui::ColorImage::filled([4, 4], egui::Color32::WHITE),
                egui::TextureOptions::NEAREST,
            ),
        );
        delta.push(
            tex_id,
            egui::epaint::ImageDelta::partial(
                [2, 2],
                egui::ColorImage::filled([2, 2], egui::Color32::from_rgba_premultiplied(255, 0, 0, 255)),
                egui::TextureOptions::NEAREST,
            ),
        );

        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let full_clip = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(16.0, 16.0));
        // Semi-transparent green premultiplied quad restricted to left half [0..8, 0..16] by scissor.
        let scissor_clip = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(8.0, 16.0));
        let primitives = vec![
            // Draw textured quad across full 16x16 viewport.
            solid_quad_primitive(full_clip, full_clip, egui::Color32::WHITE, tex_id, full_uv),
            // Overlay 50% green quad with clip_rect covering only left half (x < 8).
            solid_quad_primitive(
                full_clip,
                scissor_clip,
                egui::Color32::from_rgba_premultiplied(0, 128, 0, 128),
                tex_id,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.25, 0.25)),
            ),
        ];

        {
            let mut painter = rs.renderer.write().expect("renderer lock");
            painter.update_textures(&rs.device, &rs.context, &delta);
            assert!(painter.texture_srv(tex_id).is_some());
            painter.paint(&rs.device, &rs.context, target.rtv(), [16, 16], 1.0, &primitives);
            painter.free_textures(&delta);
        }
        delta.clear();

        let rgba = target.read_rgba8(&rs.device, &rs.context);
        assert_eq!(rgba.len(), 16 * 16 * 4);
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 16 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };

        // Top-left (x=2, y=2): white texture + 50% premul green overlay -> ~[127, 255, 127, 255].
        let tl = px(2, 2);
        assert!(
            (120..=132).contains(&tl[0]) && tl[1] >= 250 && (120..=132).contains(&tl[2]),
            "unexpected tl={tl:?}"
        );
        // Top-right (x=12, y=2): outside scissor -> untouched white [255, 255, 255, 255].
        assert_eq!(px(12, 2), [255, 255, 255, 255]);
        // Bottom-right (x=12, y=12): partial texture patch (red) outside scissor -> [255, 0, 0, 255].
        assert_eq!(px(12, 12), [255, 0, 0, 255]);

        // Verify free_textures releases managed textures cleanly.
        let mut free_delta = egui::TexturesDelta::default();
        free_delta.free(tex_id);
        {
            let mut painter = rs.renderer.write().expect("renderer lock");
            painter.free_textures(&free_delta);
            assert!(painter.texture_srv(tex_id).is_none());
        }
        free_delta.clear();
    }
}
