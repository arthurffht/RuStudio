//! Native Direct3D 11 painter for `egui` 0.36 meshes, textures, and native texture callbacks.

use std::collections::HashMap;
use windows::Win32::Graphics::Direct3D11::*;

pub struct EguiD3d11 {
    native_textures: HashMap<egui::TextureId, (ID3D11ShaderResourceView, [u32; 2])>,
    egui_textures: HashMap<egui::TextureId, (ID3D11Texture2D, ID3D11ShaderResourceView, [u32; 2])>,
    next_user_id: u64,
}

impl EguiD3d11 {
    pub fn new(_device: &ID3D11Device) -> Self {
        Self {
            native_textures: HashMap::new(),
            egui_textures: HashMap::new(),
            next_user_id: 1,
        }
    }

    pub fn update_textures(
        &mut self,
        _device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        _delta: &egui::TexturesDelta,
    ) {
        // Implemented by d3d11-core worker.
    }

    pub fn register_native_texture(
        &mut self,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) -> egui::TextureId {
        let id = egui::TextureId::User(self.next_user_id);
        self.next_user_id += 1;
        self.native_textures.insert(id, (srv, size));
        id
    }

    pub fn update_native_texture(
        &mut self,
        id: egui::TextureId,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) {
        self.native_textures.insert(id, (srv, size));
    }

    pub fn unregister_native_texture(&mut self, id: egui::TextureId) {
        self.native_textures.remove(&id);
    }

    pub fn texture_srv(&self, id: egui::TextureId) -> Option<(&ID3D11ShaderResourceView, [u32; 2])> {
        if let Some((srv, sz)) = self.native_textures.get(&id) {
            return Some((srv, *sz));
        }
        self.egui_textures.get(&id).map(|(_, srv, sz)| (srv, *sz))
    }

    pub fn paint(
        &mut self,
        _device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        _rtv: &ID3D11RenderTargetView,
        _screen_size_px: [u32; 2],
        _pixels_per_point: f32,
        _primitives: &[egui::ClippedPrimitive],
    ) {
        // Implemented by d3d11-core worker.
    }
}
