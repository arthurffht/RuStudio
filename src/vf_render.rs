//! ViewportFrames drawn by the 3D view's own renderer (a child module of
//! render.rs). A ViewportFrame's parts go through the same scene and draw
//! resolution as the world's (scene.rs `Scene::viewport_frames`, the
//! viewport's `sync_draws`): file meshes, skinning, live EditableMeshes,
//! vertex colours, textures, Decals / Textures, SurfaceAppearances and
//! material packs are the world's own; only the light differs. Each frame
//! is its own layer of renderer items, drawn into its own target with the
//! part shader's ViewportFrame lighting (shader.wgsl `part_shade`, lamp1.w):
//! the frame's Ambient + LightColor × N·L (from LightDirection), clamped to
//! 1, times the surface colour, in sRGB; no shadows, sky, fog or local
//! lights. Then it is turned into the 8-bit premultiplied sRGB image the
//! GUI composites (ImageColor3 / ImageTransparency there).
//!
//! A frame is drawn again only when what it shows changed: one of its parts
//! (any `set_item` / removal in its layer), its camera, size or lighting.
//! A static frame is drawn once.
use std::collections::HashMap;
use std::time::Instant;

use glam::{Mat4, Vec3};
use rbx_dom_weak::types::Ref;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11DepthStencilState, ID3D11DepthStencilView,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_DEPTH_STENCIL,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CLEAR_DEPTH,
    D3D11_COMPARISON_ALWAYS, D3D11_CULL_NONE, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_TEXTURE_ADDRESS_CLAMP,
};

use super::{
    draw_key, frustum, pass_of, radius_of, shows, sphere_visible, to_raw, transparent_order,
    Globals, InstanceRaw, Pass, Renderer, TexMode, DEPTH_FORMAT, DISPLAY_FORMAT, HDR_FORMAT,
};
use crate::d3d11::{self, BlendMode, RenderState, Tex2dSpec};
use crate::math::Xform;

/// What a ViewportFrame shows, as the GUI walk read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Request {
    /// Render size, physical pixels.
    pub size: [u32; 2],
    /// Its CurrentCamera: CFrame and vertical field of view (radians).
    pub cam: Xform,
    pub fov_y: f32,
    /// Ambient, LightColor (sRGB 0..1) and the direction light travels.
    pub ambient: [f32; 3],
    pub light: [f32; 3],
    pub light_dir: Vec3,
    /// BackgroundColor3 (sRGB) and 1 - BackgroundTransparency: the image's
    /// colour where no part is.
    pub background: [f32; 4],
}

/// Each frame's parts and how often they changed.
#[derive(Default)]
pub struct Layers {
    /// Item → the ViewportFrame it is drawn in.
    pub layer_of: HashMap<Ref, Ref>,
    /// Frame → its parts, each with its instances as the frame draws them
    /// (made when its draws change, `set_item`: a redraw only gathers and
    /// sorts them).
    items: HashMap<Ref, rustc_hash::FxHashMap<Ref, Vec<Prep>>>,
    gen: HashMap<Ref, u64>,
    targets: HashMap<Ref, Target>,
    blit: Option<Blit>,
    /// The world's globals from the last frame (material params, exposure).
    pub(super) base: Option<Globals>,
    /// Frames drawn in the last call (bench / checks).
    pub drawn: u32,
}

impl Layers {
    pub fn touch(&mut self, layer: Ref) {
        *self.gen.entry(layer).or_default() += 1;
    }

    /// Part `r`'s draws changed (`set_item`).
    pub(super) fn prepare(&mut self, layer: Ref, r: Ref, draws: &[super::PartDraw], bias: f32) {
        let solid_host = draws.first().is_none_or(|h| {
            h.tex_mode != TexMode::Decal && (!shows(h) || (h.alpha >= 0.999 && h.material_id != 1584))
        });
        let mut out = Vec::with_capacity(draws.len());
        for d in draws {
            if !shows(d) {
                continue;
            }
            let class = match pass_of(d) {
                Some(Pass::Opaque) => Class::Opaque,
                Some(Pass::Decal) => Class::Decal,
                // (terrain water has no place in a frame)
                None if d.custom_mesh.is_some() && d.material == 5.0 => continue,
                None if solid_host && d.tex_mode == TexMode::Decal && d.texture.is_some() => Class::SurfaceDecal,
                None => Class::Blended,
            };
            out.push(Prep { class, key: draw_key(d), raw: to_raw(d, bias), pos: d.xf.pos, radius: radius_of(d) });
        }
        self.items.entry(layer).or_default().insert(r, out);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Opaque,
    Decal,
    SurfaceDecal,
    Blended,
}

struct Prep {
    class: Class,
    key: (usize, usize),
    raw: InstanceRaw,
    pos: Vec3,
    radius: f32,
}

struct Target {
    size: [u32; 2],
    samples: u32,
    id: egui::TextureId,
    msaa_tex: ID3D11Texture2D,
    msaa_rtv: ID3D11RenderTargetView,
    depth_dsv: ID3D11DepthStencilView,
    resolve_tex: ID3D11Texture2D,
    resolve_srv: ID3D11ShaderResourceView,
    out_rtv: ID3D11RenderTargetView,
    globals_cb: ID3D11Buffer,
    inst_buf: ID3D11Buffer,
    inst_cap: usize,
    /// What it was last drawn from (request, layer generation).
    drawn: Option<(Request, u64)>,
    used: Instant,
}

struct Blit {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rast_state: ID3D11RasterizerState,
    depth_off: ID3D11DepthStencilState,
    blend_opaque: ID3D11BlendState,
}

impl Renderer {
    /// Puts item `r` in ViewportFrame `layer` (None: the world). Its draws
    /// are dropped; `set_item` hands them again.
    pub fn set_layer(&mut self, r: Ref, layer: Option<Ref>) {
        let old = self.vf.layer_of.get(&r).copied();
        if old == layer {
            return;
        }
        match old {
            Some(o) => {
                self.items.remove(&r);
                self.vf.layer_of.remove(&r);
                if let Some(s) = self.vf.items.get_mut(&o) {
                    s.remove(&r);
                }
                self.vf.touch(o);
            }
            None => self.remove_item(r),
        }
        if let Some(l) = layer {
            self.vf.layer_of.insert(r, l);
            self.vf.items.entry(l).or_default().insert(r, Vec::new());
            self.vf.touch(l);
        }
    }

    /// Removes `r` if it is a ViewportFrame's part (a part that moved to
    /// the world keeps its world item).
    pub fn remove_layered_item(&mut self, r: Ref) {
        if self.vf.layer_of.contains_key(&r) {
            self.set_layer(r, None);
        }
    }

    /// Every ViewportFrame part goes (their scene is rebuilt).
    pub fn clear_layered_items(&mut self) {
        let all: Vec<Ref> = self.vf.layer_of.keys().copied().collect();
        for r in all {
            self.set_layer(r, None);
        }
    }

    /// Something every frame may show changed in place (a live mesh's
    /// vertices): all are drawn again.
    pub fn touch_layers(&mut self) {
        let layers: Vec<Ref> = self.vf.items.keys().copied().collect();
        for l in layers {
            self.vf.touch(l);
        }
    }

    /// The texture a ViewportFrame was last drawn into, and its size.
    pub fn vf_texture(&self, vf: Ref) -> Option<(egui::TextureId, [u32; 2])> {
        self.vf.targets.get(&vf).map(|t| (t.id, t.size))
    }

    /// Draws the ViewportFrames in `reqs` whose parts, camera, size or
    /// lighting changed since their last draw; frees the targets of frames
    /// not asked for in 30 s. After the frame's `render` (skinned vertices
    /// and the world's globals are this frame's).
    pub fn render_viewport_frames(&mut self, rs: &RenderState, reqs: &HashMap<Ref, Request>) {
        self.vf.drawn = 0;
        let now = Instant::now();
        let mut gone = Vec::new();
        self.vf.targets.retain(|r, t| {
            let keep = reqs.contains_key(r) || t.used.elapsed().as_secs() < 30;
            if !keep {
                gone.push(t.id);
            }
            keep
        });
        for id in gone {
            rs.unregister_native_texture(id);
            crate::world_gui::unregister_native(id);
        }
        // Layers no frame asks for and nothing holds: forget their generation.
        let Renderer { vf, .. } = self;
        vf.gen.retain(|l, _| vf.items.get(l).is_some_and(|s| !s.is_empty()) || vf.targets.contains_key(l));
        let Some(base) = self.vf.base else {
            return;
        };
        let samples = self.samples().max(1);
        let device = &rs.device;
        if self.vf.blit.is_none() {
            let vs = d3d11::create_vs(device, crate::d3d11::shaders::VS_VF_BLIT);
            let ps = d3d11::create_ps(device, crate::d3d11::shaders::PS_VF_BLIT);
            let sampler = d3d11::create_sampler(
                device,
                D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                D3D11_TEXTURE_ADDRESS_CLAMP,
                D3D11_TEXTURE_ADDRESS_CLAMP,
                D3D11_TEXTURE_ADDRESS_CLAMP,
                1,
            );
            let rast_state = d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
            let depth_off = d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
            let blend_opaque = d3d11::create_blend_state(device, BlendMode::Opaque);
            self.vf.blit = Some(Blit {
                vs,
                ps,
                sampler,
                rast_state,
                depth_off,
                blend_opaque,
            });
        }
        for (&vf, req) in reqs {
            let gen = self.vf.gen.get(&vf).copied().unwrap_or(0);
            let size = [req.size[0].clamp(1, 4096), req.size[1].clamp(1, 4096)];
            if self.vf.targets.get(&vf).is_none_or(|t| t.size != size || t.samples != samples) {
                if let Some(old) = self.vf.targets.remove(&vf) {
                    rs.unregister_native_texture(old.id);
                    crate::world_gui::unregister_native(old.id);
                }
                let t = self.vf_target(rs, size, samples);
                self.vf.targets.insert(vf, t);
            }
            let t = self.vf.targets.get_mut(&vf).unwrap();
            t.used = now;
            if t.drawn == Some((*req, gen)) {
                continue;
            }
            t.drawn = Some((*req, gen));
            let _s = crate::bench::scope("vf_draw");
            self.vf.drawn += 1;
            self.draw_vf(rs, vf, req, size, base);
        }
    }

    fn vf_target(&self, rs: &RenderState, size: [u32; 2], samples: u32) -> Target {
        let device = &rs.device;
        let msaa = samples > 1;
        let rt_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
        let rt_srv_flags = (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32;
        let resolve_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: size[0],
                height: size[1],
                mips: 1,
                array_size: 1,
                format: HDR_FORMAT,
                samples: 1,
                bind_flags: rt_srv_flags,
                misc_flags: 0,
            },
            None,
        );
        let resolve_srv = d3d11::create_srv_2d(device, &resolve_tex, HDR_FORMAT, false, 0, 1);
        let (msaa_tex, msaa_rtv) = if msaa {
            let tex = d3d11::create_tex2d(
                device,
                &Tex2dSpec {
                    width: size[0],
                    height: size[1],
                    mips: 1,
                    array_size: 1,
                    format: HDR_FORMAT,
                    samples,
                    bind_flags: rt_flags,
                    misc_flags: 0,
                },
                None,
            );
            let rtv = d3d11::create_rtv(device, &tex, HDR_FORMAT, 0, true);
            (tex, rtv)
        } else {
            let rtv = d3d11::create_rtv(device, &resolve_tex, HDR_FORMAT, 0, false);
            (resolve_tex.clone(), rtv)
        };
        let depth_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: size[0],
                height: size[1],
                mips: 1,
                array_size: 1,
                format: DEPTH_FORMAT,
                samples,
                bind_flags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let depth_dsv = d3d11::create_dsv(device, &depth_tex, DEPTH_FORMAT, msaa, false);
        let out_tex = d3d11::create_tex2d(
            device,
            &Tex2dSpec {
                width: size[0],
                height: size[1],
                mips: 1,
                array_size: 1,
                format: DISPLAY_FORMAT,
                samples: 1,
                bind_flags: rt_srv_flags,
                misc_flags: 0,
            },
            None,
        );
        let out_rtv = d3d11::create_rtv(device, &out_tex, DISPLAY_FORMAT, 0, false);
        let out_srv = d3d11::create_srv_2d(device, &out_tex, DISPLAY_FORMAT, false, 0, 1);
        let id = rs.register_native_texture(out_srv, size);
        // (the world GUI pass draws it on SurfaceGuis / BillboardGuis)
        crate::world_gui::register_native(id);
        let globals_cb = d3d11::create_cb(device, std::mem::size_of::<Globals>());
        Target {
            size,
            samples,
            id,
            msaa_tex,
            msaa_rtv,
            depth_dsv,
            resolve_tex,
            resolve_srv,
            out_rtv,
            globals_cb,
            inst_buf: Self::make_buf(device, 64 * std::mem::size_of::<InstanceRaw>()),
            inst_cap: 64,
            drawn: None,
            used: Instant::now(),
        }
    }

    fn draw_vf(&mut self, rs: &RenderState, vf: Ref, req: &Request, size: [u32; 2], base: Globals) {
        let (device, ctx) = (&rs.device, &rs.context);
        // The frame's camera: reverse-Z like the world's.
        let aspect = size[0] as f32 / size[1].max(1) as f32;
        let fov = req.fov_y.clamp(1e-3, 3.1);
        let view = Mat4::look_to_rh(req.cam.pos, -req.cam.rot.z_axis, req.cam.rot.y_axis);
        let view_rel = Mat4::look_to_rh(Vec3::ZERO, -req.cam.rot.z_axis, req.cam.rot.y_axis);
        let reverse = Mat4::perspective_infinite_reverse_rh(fov, aspect, crate::mech::NEAR);
        let ordinary = Mat4::perspective_rh(fov, aspect, crate::mech::NEAR, crate::mech::FAR);
        let mut g = base;
        g.view_proj = (reverse * view).to_cols_array_2d();
        g.inv_view_proj = (ordinary.as_dmat4() * view_rel.as_dmat4()).inverse().as_mat4().to_cols_array_2d();
        g.cam_pos = [req.cam.pos.x, req.cam.pos.y, req.cam.pos.z, base.cam_pos[3]];
        let l = (-req.light_dir).normalize_or_zero();
        g.sun_dir = [l.x, l.y, l.z, 1.0];
        // (added squared: Studio's frames light in gamma-2 space)
        let sq = |c: [f32; 3]| c.map(|v| v * v);
        let (a, lc) = (sq(req.ambient), sq(req.light));
        g.ambient = [a[0], a[1], a[2], 0.0];
        g.sun_color = [lc[0], lc[1], lc[2], base.sun_color[3]];
        // ViewportFrame lighting (part_shade); no local lights, fog, sky light or shadows.
        g.lamp1[3] = 2.0;
        g.params[0] = 0.0;
        g.fog[2] = 0.0;
        g.bloom[2] = 0.0;
        g.env_spec[3] = 0.0;
        g.sky_rot[2] = 0.0;
        let planes = frustum(ordinary * view);
        let cam = req.cam.pos;
        let Some(t) = self.vf.targets.get(&vf) else { return };
        d3d11::upload_cb(ctx, &t.globals_cb, &g);
        // The frame's parts, as the world's per-frame list sorts them.
        let mut opaque: Vec<((usize, usize), InstanceRaw)> = Vec::new();
        let mut decal: Vec<((usize, usize), InstanceRaw)> = Vec::new();
        let mut surface_decals: Vec<(f32, (usize, usize), InstanceRaw)> = Vec::new();
        let mut blended: Vec<(f32, (usize, usize), InstanceRaw)> = Vec::new();
        let gather = crate::bench::scope("vf_gather");
        for preps in self.vf.items.get(&vf).into_iter().flat_map(|m| m.values()) {
            for p in preps {
                if !sphere_visible(&planes, p.pos, p.radius) {
                    continue;
                }
                match p.class {
                    Class::Opaque => opaque.push((p.key, p.raw)),
                    Class::Decal => decal.push((p.key, p.raw)),
                    Class::SurfaceDecal => surface_decals.push(((p.pos - cam).length(), p.key, p.raw)),
                    Class::Blended => blended.push(((p.pos - cam).length(), p.key, p.raw)),
                }
            }
        }
        drop(gather);
        let sort = crate::bench::scope("vf_sort");
        opaque.sort_unstable_by_key(|e| e.0);
        decal.sort_unstable_by_key(|e| e.0);
        // Back to front (glass with the rest: no scene behind it to bend).
        blended.sort_by(|a, b| transparent_order(0.0, a.0, 0.0, b.0));
        surface_decals.sort_by(|a, b| b.0.total_cmp(&a.0));
        let raw: Vec<InstanceRaw> = opaque
            .iter()
            .map(|e| e.1)
            .chain(decal.iter().map(|e| e.1))
            .chain(surface_decals.iter().map(|e| e.2))
            .chain(blended.iter().map(|e| e.2))
            .collect();
        drop(sort);
        let _up = crate::bench::scope("vf_upload");
        let t = self.vf.targets.get_mut(&vf).unwrap();
        if raw.len() > t.inst_cap {
            t.inst_cap = raw.len().next_power_of_two();
            t.inst_buf = Self::make_buf(device, t.inst_cap * std::mem::size_of::<InstanceRaw>());
        }
        if !raw.is_empty() {
            d3d11::update_dynamic(ctx, &t.inst_buf, bytemuck::cast_slice(&raw));
        }
        let runs = |keys: &mut dyn Iterator<Item = (usize, usize)>, base: u32| -> Vec<((usize, usize), u32, u32)> {
            let mut out: Vec<((usize, usize), u32, u32)> = Vec::new();
            for (i, k) in keys.enumerate() {
                let at = base + i as u32;
                match out.last_mut() {
                    Some(l) if l.0 == k && l.2 == at => l.2 = at + 1,
                    _ => out.push((k, at, at + 1)),
                }
            }
            out
        };
        let mut at = 0u32;
        let opaque_runs = runs(&mut opaque.iter().map(|e| e.0), at);
        at += opaque.len() as u32;
        let mut see_runs = runs(&mut decal.iter().map(|e| e.0), at);
        at += decal.len() as u32;
        see_runs.extend(runs(&mut surface_decals.iter().map(|e| e.1), at));
        at += surface_decals.len() as u32;
        see_runs.extend(runs(&mut blended.iter().map(|e| e.1), at));
        let t = self.vf.targets.get(&vf).unwrap();
        let blit = self.vf.blit.as_ref().unwrap();
        // The background, premultiplied, in the frame's squared space.
        let b = req.background;
        let clear = [b[0] * b[0] * b[3], b[1] * b[1] * b[3], b[2] * b[2] * b[3], b[3]];
        let tid = t.id;
        crate::gpu_timing::scope(device, ctx, "gpu: viewport frames", || {
            d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
            unsafe {
                ctx.ClearRenderTargetView(&t.msaa_rtv, &clear);
                ctx.ClearDepthStencilView(&t.depth_dsv, D3D11_CLEAR_DEPTH.0 as u32, 0.0, 0);
                ctx.OMSetRenderTargets(Some(&[Some(t.msaa_rtv.clone())]), Some(&t.depth_dsv));
            }
            self.bind_scene_globals_with(ctx, &t.globals_cb);
            for r in &opaque_runs {
                self.bind_part_pipeline(ctx, false, self.culls_back(r.0 .0));
                self.draw_part_run(ctx, r.0, &t.inst_buf, r.1..r.2);
            }
            for r in &see_runs {
                self.bind_part_pipeline(ctx, true, self.culls_back(r.0 .0));
                self.draw_part_run(ctx, r.0, &t.inst_buf, r.1..r.2);
            }
            unsafe {
                ctx.OMSetRenderTargets(None, None);
            }
            d3d11::unbind_ps_srvs(ctx, 0, 21);
            if t.samples > 1 {
                unsafe {
                    ctx.ResolveSubresource(&t.resolve_tex, 0, &t.msaa_tex, 0, HDR_FORMAT);
                }
            }
            unsafe {
                ctx.ClearRenderTargetView(&t.out_rtv, &[0.0, 0.0, 0.0, 0.0]);
                ctx.OMSetRenderTargets(Some(&[Some(t.out_rtv.clone())]), None);
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&blit.vs, None);
                ctx.PSSetShader(&blit.ps, None);
                ctx.RSSetState(&blit.rast_state);
                ctx.OMSetDepthStencilState(&blit.depth_off, 0);
                ctx.OMSetBlendState(&blit.blend_opaque, None, 0xffff_ffff);
                ctx.PSSetShaderResources(0, Some(&[Some(t.resolve_srv.clone())]));
                ctx.PSSetSamplers(0, Some(&[Some(blit.sampler.clone())]));
                ctx.Draw(3, 0);
                ctx.OMSetRenderTargets(None, None);
            }
            d3d11::unbind_ps_srvs(ctx, 0, 1);
        });
        crate::world_gui::native_changed(tid);
    }
}
