//! DXGI swapchain wrapper with flip-model presentation, tearing support, and resize handling.

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::RenderState;

pub struct Swapchain {
    swapchain: IDXGISwapChain1,
    backbuffer: Option<ID3D11Texture2D>,
    rtv: Option<ID3D11RenderTargetView>,
    size: [u32; 2],
    allow_tearing: bool,
    flags: u32,
}

impl Swapchain {
    pub fn new(rs: &RenderState, hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
        let width = width.max(1);
        let height = height.max(1);

        let allow_tearing = rs
            .factory
            .cast::<IDXGIFactory5>()
            .ok()
            .and_then(|f5| {
                let mut support: windows::core::BOOL = false.into();
                unsafe {
                    f5.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        (&mut support as *mut windows::core::BOOL).cast(),
                        std::mem::size_of::<windows::core::BOOL>() as u32,
                    )
                }
                .ok()
                .map(|_| support.as_bool())
            })
            .unwrap_or(false);

        let _ = unsafe { rs.factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) };

        let flags = if allow_tearing {
            DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32
        } else {
            0
        };

        let mut desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
        };

        let swapchain = unsafe {
            rs.factory
                .CreateSwapChainForHwnd(&rs.device, hwnd, &desc, None, None)
        }
        .or_else(|_| {
            desc.BufferCount = 2;
            desc.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
            desc.Flags = 0;
            unsafe {
                rs.factory
                    .CreateSwapChainForHwnd(&rs.device, hwnd, &desc, None, None)
            }
        })
        .map_err(|e| format!("CreateSwapChainForHwnd failed: {e}"))?;

        let actual_tearing = desc.SwapEffect == DXGI_SWAP_EFFECT_FLIP_DISCARD && allow_tearing;
        let (bb, rtv) = Self::make_rtv(&rs.device, &swapchain)?;
        Ok(Self {
            swapchain,
            backbuffer: Some(bb),
            rtv: Some(rtv),
            size: [width, height],
            allow_tearing: actual_tearing,
            flags: desc.Flags,
        })
    }

    fn make_rtv(
        device: &ID3D11Device,
        swapchain: &IDXGISwapChain1,
    ) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), String> {
        let bb: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }
            .map_err(|e| format!("IDXGISwapChain1::GetBuffer(0) failed: {e}"))?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&bb, None, Some(&mut rtv)) }
            .map_err(|e| format!("CreateRenderTargetView backbuffer failed: {e}"))?;
        Ok((bb, rtv.unwrap()))
    }

    pub fn resize(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) {
        let width = width.max(1);
        let height = height.max(1);
        if self.size == [width, height] {
            return;
        }
        unsafe {
            ctx.OMSetRenderTargets(None, None);
        }
        self.rtv = None;
        self.backbuffer = None;
        if let Err(e) = unsafe {
            self.swapchain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(self.flags as i32))
        } {
            eprintln!("ResizeBuffers({width}x{height}) failed: {e}");
            return;
        }
        if let Ok((bb, rtv)) = Self::make_rtv(device, &self.swapchain) {
            self.backbuffer = Some(bb);
            self.rtv = Some(rtv);
            self.size = [width, height];
        }
    }

    pub fn rtv(&self) -> &ID3D11RenderTargetView {
        self.rtv.as_ref().expect("swapchain RTV missing")
    }

    pub fn backbuffer(&self) -> &ID3D11Texture2D {
        self.backbuffer.as_ref().expect("swapchain backbuffer missing")
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    pub fn present(&self, vsync: bool) {
        let sync = u32::from(vsync);
        let flags = if !vsync && self.allow_tearing {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        };
        if let Err(e) = unsafe { self.swapchain.Present(sync, flags).ok() } {
            eprintln!("IDXGISwapChain1::Present returned {e:?}");
        }
    }
}
