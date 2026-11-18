//! DXGI swapchain wrapper with flip-model presentation, tearing support,
//! waitable frame latency synchronization, and resize handling.

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;

use super::RenderState;

pub struct Swapchain {
    swapchain: IDXGISwapChain1,
    backbuffer: Option<ID3D11Texture2D>,
    rtv: Option<ID3D11RenderTargetView>,
    waitable: Option<HANDLE>,
    size: [u32; 2],
    allow_tearing: bool,
    flags: u32,
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        if let Some(h) = self.waitable.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
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

        let waitable_flag = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        let tearing_flag = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;

        // DXGI forbids combining ALLOW_TEARING with FRAME_LATENCY_WAITABLE_OBJECT on some
        // Windows versions; try WaitableObject alone when tearing is unsupported, and when
        // tearing is supported prefer Waitable+Tearing if accepted, else ALLOW_TEARING
        // (relying on IDXGIDevice1::SetMaximumFrameLatency(1) configured in create_device).
        let mut candidates: Vec<(DXGI_SWAP_EFFECT, u32)> = Vec::with_capacity(5);
        if allow_tearing {
            candidates.push((DXGI_SWAP_EFFECT_FLIP_DISCARD, waitable_flag | tearing_flag));
            candidates.push((DXGI_SWAP_EFFECT_FLIP_DISCARD, tearing_flag));
            candidates.push((DXGI_SWAP_EFFECT_FLIP_DISCARD, waitable_flag));
        } else {
            candidates.push((DXGI_SWAP_EFFECT_FLIP_DISCARD, waitable_flag));
        }
        candidates.push((DXGI_SWAP_EFFECT_FLIP_DISCARD, 0));
        candidates.push((DXGI_SWAP_EFFECT_DISCARD, 0));

        let mut desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };

        let mut created: Option<(IDXGISwapChain1, DXGI_SWAP_CHAIN_DESC1)> = None;
        let mut last_err = None;
        for &(effect, flags) in &candidates {
            desc.SwapEffect = effect;
            desc.Flags = flags;
            match unsafe {
                rs.factory
                    .CreateSwapChainForHwnd(&rs.device, hwnd, &desc, None, None)
            } {
                Ok(sc) => {
                    created = Some((sc, desc));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let (swapchain, chosen) = created.ok_or_else(|| {
            format!(
                "CreateSwapChainForHwnd failed: {}",
                last_err.map_or_else(|| "unknown error".into(), |e| e.to_string())
            )
        })?;

        let waitable = if (chosen.Flags & waitable_flag) != 0 {
            swapchain.cast::<IDXGISwapChain2>().ok().and_then(|sc2| unsafe {
                let _ = sc2.SetMaximumFrameLatency(1);
                let h = sc2.GetFrameLatencyWaitableObject();
                if h.is_invalid() { None } else { Some(h) }
            })
        } else {
            None
        };

        let actual_tearing = chosen.SwapEffect == DXGI_SWAP_EFFECT_FLIP_DISCARD
            && (chosen.Flags & tearing_flag) != 0;
        let (bb, rtv) = Self::make_rtv(&rs.device, &swapchain)?;
        Ok(Self {
            swapchain,
            backbuffer: Some(bb),
            rtv: Some(rtv),
            waitable,
            size: [width, height],
            allow_tearing: actual_tearing,
            flags: chosen.Flags,
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

    #[allow(dead_code)] // Called from d3d11/window.rs on UI integration.
    pub fn wait_frame_latency(&self) {
        if let Some(h) = self.waitable {
            unsafe {
                let _ = WaitForSingleObjectEx(h, 100, true);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::create_device;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, CS_OWNDC,
        WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    #[test]
    fn swapchain_waitable_tearing_resize_and_present_on_live_gpu() {
        let rs = create_device().expect("live D3D11 device");
        let hinst = unsafe { GetModuleHandleW(PCWSTR::null()) }.unwrap_or_default();
        let class_name: Vec<u16> = "RustudioSwapchainTest\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: CS_OWNDC,
            lpfnWndProc: Some(test_wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(class_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                64,
                64,
                None,
                None,
                Some(hinst.into()),
                None,
            )
        }
        .expect("CreateWindowExW");

        let mut sc = Swapchain::new(&rs, hwnd, 64, 64).expect("Swapchain::new");
        let desc = unsafe { sc.swapchain.GetDesc1() }.expect("GetDesc1");
        assert_eq!(desc.BufferCount, 2);

        sc.wait_frame_latency();
        unsafe { rs.context.ClearRenderTargetView(sc.rtv(), &[0.1, 0.2, 0.3, 1.0]) };
        sc.present(true);

        sc.wait_frame_latency();
        unsafe { rs.context.ClearRenderTargetView(sc.rtv(), &[0.3, 0.2, 0.1, 1.0]) };
        sc.present(false);

        sc.resize(&rs.device, &rs.context, 96, 48);
        assert_eq!(sc.size(), [96, 48]);
        sc.wait_frame_latency();
        unsafe { rs.context.ClearRenderTargetView(sc.rtv(), &[0.0, 0.4, 0.0, 1.0]) };
        sc.present(true);

        drop(sc);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
