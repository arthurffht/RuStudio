//! GPU skinning: a compute pass that writes a skinned part's deformed
//! positions and normals into a staging raw UAV buffer and copies it into
//! that part's own vertex buffer (so FL 11.0 vertex buffers never require
//! simultaneous UAV + VB bind flags).
//!
//! - Per skinned mesh (content id), once: a structured buffer of its corners
//!   (rest position and normal in the renderer's unit box, four weights and
//!   four bone indices), 64 bytes a corner.
//! - Per skinned part: its mesh slot (a copy of the mesh in a DEFAULT vertex
//!   buffer, uvs and vertex colours written once), a palette buffer (3 × vec4
//!   per bone: rows of current × bind⁻¹ in part space), a raw UAV staging
//!   buffer matching the vertex buffer byte size, and its parameters.
//! - Per frame the CPU only computes the palette (skin.rs `matrices`) and
//!   writes it (48 bytes a bone); the dispatch rewrites floats 0..6 of each
//!   64-byte vertex (shader `Vertex`: pos, normal) and leaves uv, projection
//!   and colour alone.
//!
//! A part whose bones are all at rest draws the shared mesh and costs
//! nothing here.

use std::collections::HashMap;
use std::sync::Arc;

use glam::Vec3;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;

use crate::d3d11::{self, shaders};
use crate::math::Xform;

/// A skinned mesh's corners for the GPU.
struct Source {
    _buf: ID3D11Buffer,
    srv: ID3D11ShaderResourceView,
    corners: u32,
}

/// One skinned part's GPU state.
struct Job {
    src_srv: ID3D11ShaderResourceView,
    palette_buf: ID3D11Buffer,
    palette_srv: ID3D11ShaderResourceView,
    params_cb: ID3D11Buffer,
    dst_buf: ID3D11Buffer,
    dst_uav: ID3D11UnorderedAccessView,
    vbuf: ID3D11Buffer,
    bones: u32,
    corners: u32,
    /// The mesh slot (Renderer::meshes index without built-ins) it writes.
    slot: u32,
    /// The vertex buffer COM pointer (to notice a reslot).
    vbuf_id: usize,
    vbuf_bytes: u32,
    /// True on initial setup or after `again(slot)` so uv/proj/color are seeded from `vbuf`.
    seed_from_vbuf: bool,
}

pub struct SkinGpu {
    cs: ID3D11ComputeShader,
    sources: HashMap<String, Arc<Source>>,
    jobs: HashMap<u32, Job>,
    due: Vec<u32>,
    /// Bytes written to the GPU for skinning since the last `take_bytes`.
    pub bytes: u64,
}

/// What one part needs skinned: its mesh's corners and this frame's
/// palette (current × bind⁻¹ per mesh bone, part space).
pub struct Request<'a> {
    pub key: &'a str,
    pub base: &'a crate::skin::Base,
    pub palette: &'a [Xform],
    pub scale: Vec3,
    /// The world origin in part space (skin::origin_of).
    pub origin: Vec3,
}

impl SkinGpu {
    pub fn new(device: &ID3D11Device) -> Self {
        let cs = d3d11::create_cs(device, shaders::CS_SKIN);
        SkinGpu {
            cs,
            sources: HashMap::new(),
            jobs: HashMap::new(),
            due: Vec::new(),
            bytes: 0,
        }
    }

    fn source(&mut self, device: &ID3D11Device, key: &str, base: &crate::skin::Base) -> Arc<Source> {
        if let Some(s) = self.sources.get(key) {
            return s.clone();
        }
        let n = base.positions.len();
        let mut data: Vec<[f32; 4]> = Vec::with_capacity((n * 4).max(4));
        for i in 0..n {
            let p = base.positions[i];
            let nm = base.normals.get(i).copied().unwrap_or(Vec3::Y);
            let ws = base.skin.corner_weights.get(i).copied().unwrap_or([0.0; 4]);
            let ids = base.skin.corner_bones.get(i).copied().unwrap_or([0; 4]);
            data.push([p.x, p.y, p.z, 0.0]);
            data.push([nm.x, nm.y, nm.z, 0.0]);
            data.push(ws);
            data.push(ids.map(|b| f32::from_bits(b as u32)));
        }
        if data.is_empty() {
            data.push([0.0; 4]);
        }
        let (buf, srv) = d3d11::create_structured_srv(
            device,
            16,
            data.len() as u32,
            false,
            Some(bytemuck::cast_slice(&data)),
        );
        self.bytes += (data.len() * 16) as u64;
        crate::bench::add("skin_upload_bytes", 0.0, (data.len() * 16) as u64);
        let s = Arc::new(Source {
            _buf: buf,
            srv,
            corners: n as u32,
        });
        self.sources.insert(key.to_string(), s.clone());
        s
    }

    /// Queues `slot`'s vertices to be rewritten from `req` this frame.
    /// `vbuf` is the slot's vertex buffer (`D3D11_USAGE_DEFAULT`).
    pub fn queue(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        slot: u32,
        vbuf: &ID3D11Buffer,
        req: &Request,
    ) {
        let src = self.source(device, req.key, req.base);
        let bones = req.palette.len().max(1) as u32;
        let vbuf_id = vbuf.as_raw() as usize;
        let mut vbuf_desc = D3D11_BUFFER_DESC::default();
        unsafe { vbuf.GetDesc(&mut vbuf_desc) };
        let vbuf_bytes = vbuf_desc.ByteWidth;
        let stale = self.jobs.get(&slot).is_none_or(|j| {
            j.bones != bones
                || j.corners != src.corners
                || j.vbuf_id != vbuf_id
                || j.vbuf_bytes != vbuf_bytes
                || j.slot != slot
        });
        if stale {
            let (palette_buf, palette_srv) =
                d3d11::create_structured_srv(device, 16, bones * 3, true, None);
            let params_cb = d3d11::create_cb(device, 80);
            let (dst_buf, dst_uav, _) = d3d11::create_raw_uav_buffer(device, vbuf_bytes as usize, None);
            self.jobs.insert(
                slot,
                Job {
                    src_srv: src.srv.clone(),
                    palette_buf,
                    palette_srv,
                    params_cb,
                    dst_buf,
                    dst_uav,
                    vbuf: vbuf.clone(),
                    bones,
                    corners: src.corners,
                    slot,
                    vbuf_id,
                    vbuf_bytes,
                    seed_from_vbuf: true,
                },
            );
        }
        let job = &self.jobs[&slot];
        let rows: Vec<[f32; 4]> = req
            .palette
            .iter()
            .flat_map(|x| {
                let r = x.rot.transpose(); // columns of the transpose = rows
                [
                    [r.x_axis.x, r.x_axis.y, r.x_axis.z, x.pos.x],
                    [r.y_axis.x, r.y_axis.y, r.y_axis.z, x.pos.y],
                    [r.z_axis.x, r.z_axis.y, r.z_axis.z, x.pos.z],
                ]
            })
            .collect();
        if !rows.is_empty() {
            d3d11::update_dynamic(ctx, &job.palette_buf, bytemuck::cast_slice(&rows));
            self.bytes += (rows.len() * 16) as u64;
        }
        let to_part = req.base.extent * req.scale;
        let from_part = Vec3::ONE / to_part.max(Vec3::splat(1e-6));
        let e = req.base.extent;
        let o = req.origin;
        let params: [[f32; 4]; 5] = [
            [to_part.x, to_part.y, to_part.z, 0.0],
            [from_part.x, from_part.y, from_part.z, 0.0],
            [e.x, e.y, e.z, 0.0],
            [f32::from_bits(src.corners), f32::from_bits(req.palette.len() as u32), 0.0, 0.0],
            [o.x, o.y, o.z, 0.0],
        ];
        d3d11::upload_cb(ctx, &job.params_cb, &params);
        self.bytes += 80;
        crate::bench::add("skin_upload_bytes", 0.0, rows.len() as u64 * 16 + 80);
        if !self.due.contains(&slot) {
            self.due.push(slot);
        }
    }

    /// Queues `slot` again with the palette and parameters it already
    /// holds (its vertex buffer was rewritten with the rest pose).
    pub fn again(&mut self, slot: u32) {
        if let Some(job) = self.jobs.get_mut(&slot) {
            job.seed_from_vbuf = true;
            if !self.due.contains(&slot) {
                self.due.push(slot);
            }
        }
    }

    /// Forgets a part's job (its slot went back to the shared mesh or away).
    pub fn drop_slot(&mut self, slot: u32) {
        self.jobs.remove(&slot);
        self.due.retain(|s| *s != slot);
    }

    /// Dispatches this frame's skinning passes and copies results into vertex buffers.
    pub fn encode(&mut self, ctx: &ID3D11DeviceContext) {
        if self.due.is_empty() {
            return;
        }
        unsafe { ctx.CSSetShader(&self.cs, None) };
        for slot in self.due.drain(..) {
            let Some(j) = self.jobs.get_mut(&slot) else {
                continue;
            };
            if j.corners == 0 {
                continue;
            }
            if j.seed_from_vbuf {
                unsafe { ctx.CopyResource(&j.dst_buf, &j.vbuf) };
                j.seed_from_vbuf = false;
            }
            let cbs = [Some(j.params_cb.clone())];
            let srvs = [Some(j.src_srv.clone()), Some(j.palette_srv.clone())];
            let uavs = [Some(j.dst_uav.clone())];
            unsafe {
                ctx.CSSetConstantBuffers(0, Some(&cbs));
                ctx.CSSetShaderResources(0, Some(&srvs));
                ctx.CSSetUnorderedAccessViews(0, Some(&uavs), None);
                ctx.Dispatch(j.corners.div_ceil(64), 1, 1);
            }
            d3d11::unbind_cs_uavs(ctx, 0, 8);
            unsafe { ctx.CopyResource(&j.vbuf, &j.dst_buf) };
        }
        unsafe {
            ctx.CSSetShaderResources(0, Some(&[None, None]));
            ctx.CSSetConstantBuffers(0, Some(&[None]));
            ctx.CSSetShader(None, None);
        }
    }

    pub fn take_bytes(&mut self) -> u64 {
        std::mem::take(&mut self.bytes)
    }
}
