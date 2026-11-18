//! Offscreen RGBA8 D3D11 render target for `--headless`, `--screenshot`, and `--play --shots`.

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

use super::{create_rtv, create_tex2d, read_tex2d_rgba8, Tex2dSpec};

pub struct OffscreenTarget {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    size: [u32; 2],
}

impl OffscreenTarget {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let tex = create_tex2d(
            device,
            &Tex2dSpec {
                width,
                height,
                mips: 1,
                array_size: 1,
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                samples: 1,
                bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let rtv = create_rtv(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, 0, false);
        Self {
            tex,
            rtv,
            size: [width, height],
        }
    }

    pub fn resize(&mut self, device: &ID3D11Device, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.size != [width, height] {
            *self = Self::new(device, width, height);
        }
    }

    pub fn rtv(&self) -> &ID3D11RenderTargetView {
        &self.rtv
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.tex
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    pub fn read_rgba8(&self, device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Vec<u8> {
        read_tex2d_rgba8(device, ctx, &self.tex, self.size[0], self.size[1])
    }
}
