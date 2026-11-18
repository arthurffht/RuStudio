//! Direct3D 11 device creation, hardware adapter selection, and resource helpers.

use std::sync::{Arc, RwLock};
use windows::core::Interface;
use super::EguiD3d11;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_SRV_DIMENSION_BUFFEREX, D3D_SRV_DIMENSION_TEXTURE2D, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
    D3D_SRV_DIMENSION_TEXTURE2DMS, D3D_SRV_DIMENSION_TEXTURE3D, D3D_SRV_DIMENSION_TEXTURECUBE,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

#[derive(Clone, Debug)]
pub struct GpuCaps {
    pub adapter_name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram_mb: u64,
    pub shared_mem_mb: u64,
    pub feature_level: D3D_FEATURE_LEVEL,
    pub is_uma: bool,
    pub msaa_fp16_max_samples: u32,
    pub msaa_rgba8_max_samples: u32,
    pub max_texture_dim: u32,
    pub max_anisotropy: u32,
}

#[derive(Clone)]
pub struct RenderState {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub factory: IDXGIFactory2,
    pub caps: GpuCaps,
    pub renderer: Arc<RwLock<EguiD3d11>>,
}

impl RenderState {
    pub fn register_native_texture(
        &self,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) -> egui::TextureId {
        self.renderer
            .write()
            .expect("egui renderer lock")
            .register_native_texture(srv, size)
    }

    pub fn update_native_texture(
        &self,
        id: egui::TextureId,
        srv: ID3D11ShaderResourceView,
        size: [u32; 2],
    ) {
        self.renderer
            .write()
            .expect("egui renderer lock")
            .update_native_texture(id, srv, size);
    }

    pub fn unregister_native_texture(&self, id: egui::TextureId) {
        self.renderer
            .write()
            .expect("egui renderer lock")
            .unregister_native_texture(id);
    }

    pub fn texture_srv(
        &self,
        id: egui::TextureId,
    ) -> Option<(ID3D11ShaderResourceView, [u32; 2])> {
        self.renderer
            .read()
            .expect("egui renderer lock")
            .texture_srv(id)
            .map(|(srv, sz)| (srv.clone(), sz))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tex2dSpec {
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub array_size: u32,
    pub format: DXGI_FORMAT,
    pub samples: u32,
    pub bind_flags: u32,
    pub misc_flags: u32,
}

impl Default for Tex2dSpec {
    fn default() -> Self {
        Self {
            width: 1,
            height: 1,
            mips: 1,
            array_size: 1,
            format: DXGI_FORMAT_R8G8B8A8_UNORM,
            samples: 1,
            bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            misc_flags: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlendMode {
    Opaque,
    AlphaBlend,
    PremulAlpha,
    Additive,
    NoColorWrite,
}

pub fn create_device() -> Result<RenderState, String> {
    let factory1: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1 failed: {e}"))?;

    let allow_warp = std::env::var_os("RUSTUDIO_ALLOW_WARP").is_some();
    let d3d_debug = std::env::var_os("RUSTUDIO_D3D11_DEBUG").is_some();

    let mut chosen: Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> = None;
    let mut best_score: i64 = -1;
    let mut idx = 0u32;

    loop {
        let adapter = match unsafe { factory1.EnumAdapters1(idx) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(_) => break,
        };
        let desc = unsafe { adapter.GetDesc1() }
            .map_err(|e| format!("IDXGIAdapter1::GetDesc1 failed: {e}"))?;
        let name = String::from_utf16_lossy(
            desc.Description
                .split(|&c| c == 0)
                .next()
                .unwrap_or(&[]),
        );
        let is_sw = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        let is_warp = desc.VendorId == 0x1414;
        let vram_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
        let shared_mb = (desc.SharedSystemMemory as u64) / (1024 * 1024);

        eprintln!(
            "DXGI Adapter[{idx}]: \"{name}\" (Vendor=0x{:04X}, Device=0x{:04X}, VRAM={vram_mb}MB, Shared={shared_mb}MB, SW={is_sw})",
            desc.VendorId,
            desc.DeviceId
        );

        idx += 1;
        if (is_sw || is_warp) && !allow_warp {
            continue;
        }
        // Prefer discrete GPU over integrated, and integrated over software fallback.
        let discrete_bonus = if !is_sw && !is_warp && vram_mb > 512 {
            10_000_000_000i64
        } else if !is_sw && !is_warp {
            1_000_000_000i64
        } else {
            0
        };
        let score = discrete_bonus + (vram_mb as i64) * 1024 + (shared_mb as i64);
        if score > best_score {
            best_score = score;
            chosen = Some((adapter, desc));
        }
    }

    let (adapter1, desc) = chosen.ok_or_else(|| {
        "No Direct3D 11 hardware adapter found (Microsoft WARP rejected)".to_string()
    })?;
    let adapter: IDXGIAdapter = adapter1
        .cast()
        .map_err(|e| format!("Cast IDXGIAdapter1->IDXGIAdapter failed: {e}"))?;

    let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    if d3d_debug {
        flags |= D3D11_CREATE_DEVICE_DEBUG;
    }

    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut fl = D3D_FEATURE_LEVEL_11_0;

    unsafe {
        D3D11CreateDevice(
            Some(&adapter),
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut fl),
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;

    let device = device.ok_or("D3D11CreateDevice returned null device")?;
    let context = context.ok_or("D3D11CreateDevice returned null context")?;
    let factory: IDXGIFactory2 = factory1
        .cast()
        .map_err(|e| format!("Cast IDXGIFactory1->IDXGIFactory2 failed: {e}"))?;

    let adapter_name = String::from_utf16_lossy(
        desc.Description
            .split(|&c| c == 0)
            .next()
            .unwrap_or(&[]),
    );
    let dedicated_vram_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
    let shared_mem_mb = (desc.SharedSystemMemory as u64) / (1024 * 1024);

    let fp16_4x = unsafe { device.CheckMultisampleQualityLevels(DXGI_FORMAT_R16G16B16A16_FLOAT, 4) }
        .unwrap_or(0);
    let rgba8_4x =
        unsafe { device.CheckMultisampleQualityLevels(DXGI_FORMAT_R8G8B8A8_UNORM, 4) }.unwrap_or(0);

    let mut is_uma = dedicated_vram_mb <= 512 && desc.VendorId == 0x8086;
    if let Ok(dev3) = device.cast::<ID3D11Device3>() {
        let mut opts2 = D3D11_FEATURE_DATA_D3D11_OPTIONS2::default();
        if unsafe {
            dev3.CheckFeatureSupport(
                D3D11_FEATURE_D3D11_OPTIONS2,
                (&mut opts2 as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS2).cast(),
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D11_OPTIONS2>() as u32,
            )
        }
        .is_ok()
        {
            is_uma = is_uma || opts2.UnifiedMemoryArchitecture.as_bool();
        }
    }

    let caps = GpuCaps {
        adapter_name: adapter_name.clone(),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_vram_mb,
        shared_mem_mb,
        feature_level: fl,
        is_uma,
        msaa_fp16_max_samples: if fp16_4x > 0 { 4 } else { 1 },
        msaa_rgba8_max_samples: if rgba8_4x > 0 { 4 } else { 1 },
        max_texture_dim: if fl.0 >= D3D_FEATURE_LEVEL_11_0.0 {
            16384
        } else {
            8192
        },
        max_anisotropy: 16,
    };

    eprintln!(
        "D3D11 initialized: \"{}\" (Vendor=0x{:04X}, Device=0x{:04X}, FeatureLevel=0x{:X}, VRAM={}MB, UMA={}, MSAA_FP16={}x)",
        caps.adapter_name,
        caps.vendor_id,
        caps.device_id,
        caps.feature_level.0,
        caps.dedicated_vram_mb,
        caps.is_uma,
        caps.msaa_fp16_max_samples
    );

    let renderer = Arc::new(RwLock::new(EguiD3d11::new(&device)));

    Ok(RenderState {
        device,
        context,
        factory,
        caps,
        renderer,
    })
}

pub fn create_immutable_buffer(device: &ID3D11Device, bind_flags: u32, data: &[u8]) -> ID3D11Buffer {
    if data.is_empty() {
        return create_default_buffer(device, bind_flags, 16, None);
    }
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: data.len() as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let sub = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr().cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut out = None;
    unsafe { device.CreateBuffer(&desc, Some(&sub), Some(&mut out)) }.expect("CreateBuffer immutable");
    out.unwrap()
}

pub fn create_default_buffer(
    device: &ID3D11Device,
    bind_flags: u32,
    byte_len: usize,
    init: Option<&[u8]>,
) -> ID3D11Buffer {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (byte_len.max(16)) as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let sub = init.map(|d| D3D11_SUBRESOURCE_DATA {
        pSysMem: d.as_ptr().cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    });
    let mut out = None;
    unsafe {
        device.CreateBuffer(
            &desc,
            sub.as_ref().map(|s| s as *const _),
            Some(&mut out),
        )
    }
    .expect("CreateBuffer default");
    out.unwrap()
}

pub fn create_dynamic_buffer(device: &ID3D11Device, bind_flags: u32, byte_len: usize) -> ID3D11Buffer {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: byte_len.max(16) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: bind_flags,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut out = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut out)) }.expect("CreateBuffer dynamic");
    out.unwrap()
}

pub fn create_cb(device: &ID3D11Device, byte_len: usize) -> ID3D11Buffer {
    let aligned = ((byte_len + 15) & !15).max(16);
    create_dynamic_buffer(device, D3D11_BIND_CONSTANT_BUFFER.0 as u32, aligned)
}

pub fn update_dynamic(ctx: &ID3D11DeviceContext, buf: &ID3D11Buffer, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        ctx.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .expect("Map WRITE_DISCARD");
        std::ptr::copy_nonoverlapping(data.as_ptr(), mapped.pData.cast(), data.len());
        ctx.Unmap(buf, 0);
    }
}

pub fn upload_cb<T: bytemuck::NoUninit>(ctx: &ID3D11DeviceContext, cb: &ID3D11Buffer, value: &T) {
    update_dynamic(ctx, cb, bytemuck::bytes_of(value));
}

pub fn create_structured_srv(
    device: &ID3D11Device,
    stride: u32,
    count: u32,
    dynamic: bool,
    init: Option<&[u8]>,
) -> (ID3D11Buffer, ID3D11ShaderResourceView) {
    let count = count.max(1);
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: stride * count,
        Usage: if dynamic { D3D11_USAGE_DYNAMIC } else { D3D11_USAGE_DEFAULT },
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: if dynamic { D3D11_CPU_ACCESS_WRITE.0 as u32 } else { 0 },
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: stride,
    };
    let sub = init.map(|d| D3D11_SUBRESOURCE_DATA {
        pSysMem: d.as_ptr().cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    });
    let mut buf = None;
    unsafe { device.CreateBuffer(&desc, sub.as_ref().map(|s| s as *const _), Some(&mut buf)) }
        .expect("CreateBuffer structured");
    let buf = buf.unwrap();

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D_SRV_DIMENSION_BUFFEREX,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            BufferEx: D3D11_BUFFEREX_SRV {
                FirstElement: 0,
                NumElements: count,
                Flags: 0,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&buf, Some(&srv_desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView structured");
    (buf, srv.unwrap())
}

pub fn create_raw_uav_buffer(
    device: &ID3D11Device,
    byte_len: usize,
    init: Option<&[u8]>,
) -> (ID3D11Buffer, ID3D11UnorderedAccessView, ID3D11ShaderResourceView) {
    let byte_len = ((byte_len + 15) & !15).max(16);
    let words = (byte_len / 4) as u32;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: byte_len as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS).0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
        StructureByteStride: 0,
    };
    let sub = init.map(|d| D3D11_SUBRESOURCE_DATA {
        pSysMem: d.as_ptr().cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    });
    let mut buf = None;
    unsafe { device.CreateBuffer(&desc, sub.as_ref().map(|s| s as *const _), Some(&mut buf)) }
        .expect("CreateBuffer raw UAV");
    let buf = buf.unwrap();

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: words,
                Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
            },
        },
    };
    let mut uav = None;
    unsafe { device.CreateUnorderedAccessView(&buf, Some(&uav_desc), Some(&mut uav)) }
        .expect("CreateUnorderedAccessView raw");

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D_SRV_DIMENSION_BUFFEREX,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            BufferEx: D3D11_BUFFEREX_SRV {
                FirstElement: 0,
                NumElements: words,
                Flags: D3D11_BUFFEREX_SRV_FLAG_RAW.0 as u32,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&buf, Some(&srv_desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView raw");

    (buf, uav.unwrap(), srv.unwrap())
}

pub fn create_tex2d(
    device: &ID3D11Device,
    spec: &Tex2dSpec,
    init: Option<&[D3D11_SUBRESOURCE_DATA]>,
) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: spec.width.max(1),
        Height: spec.height.max(1),
        MipLevels: spec.mips.max(1),
        ArraySize: spec.array_size.max(1),
        Format: spec.format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: spec.samples.max(1),
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: spec.bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: spec.misc_flags,
    };
    let mut out = None;
    unsafe { device.CreateTexture2D(&desc, init.map(|s| s.as_ptr()), Some(&mut out)) }
        .expect("CreateTexture2D");
    out.unwrap()
}

pub fn create_tex3d(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    depth: u32,
    mips: u32,
    format: DXGI_FORMAT,
    bind_flags: u32,
    init: Option<&[D3D11_SUBRESOURCE_DATA]>,
) -> ID3D11Texture3D {
    let desc = D3D11_TEXTURE3D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        Depth: depth.max(1),
        MipLevels: mips.max(1),
        Format: format,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut out = None;
    unsafe { device.CreateTexture3D(&desc, init.map(|s| s.as_ptr()), Some(&mut out)) }
        .expect("CreateTexture3D");
    out.unwrap()
}

pub fn create_rtv(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    mip: u32,
    msaa: bool,
) -> ID3D11RenderTargetView {
    let desc = if msaa {
        D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMS,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2DMS: D3D11_TEX2DMS_RTV::default(),
            },
        }
    } else {
        D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: mip },
            },
        }
    };
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(tex, Some(&desc), Some(&mut rtv)) }
        .expect("CreateRenderTargetView");
    rtv.unwrap()
}

pub fn create_rtv_2d_array_slice(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    mip: u32,
    slice: u32,
) -> ID3D11RenderTargetView {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                MipSlice: mip,
                FirstArraySlice: slice,
                ArraySize: 1,
            },
        },
    };
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(tex, Some(&desc), Some(&mut rtv)) }
        .expect("CreateRenderTargetView array slice");
    rtv.unwrap()
}

pub fn create_dsv(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    msaa: bool,
    read_only: bool,
) -> ID3D11DepthStencilView {
    let mut desc = D3D11_DEPTH_STENCIL_VIEW_DESC {
        Format: format,
        Flags: if read_only { D3D11_DSV_READ_ONLY_DEPTH.0 as u32 } else { 0 },
        ViewDimension: if msaa {
            D3D11_DSV_DIMENSION_TEXTURE2DMS
        } else {
            D3D11_DSV_DIMENSION_TEXTURE2D
        },
        ..Default::default()
    };
    if !msaa {
        desc.Anonymous.Texture2D = D3D11_TEX2D_DSV { MipSlice: 0 };
    }
    let mut dsv = None;
    unsafe { device.CreateDepthStencilView(tex, Some(&desc), Some(&mut dsv)) }
        .expect("CreateDepthStencilView");
    dsv.unwrap()
}

pub fn create_srv_2d(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    msaa: bool,
    most_detailed_mip: u32,
    mip_levels: u32,
) -> ID3D11ShaderResourceView {
    let mut desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: if msaa {
            D3D_SRV_DIMENSION_TEXTURE2DMS
        } else {
            D3D_SRV_DIMENSION_TEXTURE2D
        },
        ..Default::default()
    };
    if !msaa {
        desc.Anonymous.Texture2D = D3D11_TEX2D_SRV {
            MostDetailedMip: most_detailed_mip,
            MipLevels: mip_levels,
        };
    }
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(tex, Some(&desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView 2D");
    srv.unwrap()
}

pub fn create_srv_2d_array(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    mip_levels: u32,
    first_slice: u32,
    array_size: u32,
) -> ID3D11ShaderResourceView {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: mip_levels,
                FirstArraySlice: first_slice,
                ArraySize: array_size,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(tex, Some(&desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView 2DArray");
    srv.unwrap()
}

pub fn create_srv_cube(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    most_detailed_mip: u32,
    mip_levels: u32,
) -> ID3D11ShaderResourceView {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURECUBE,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            TextureCube: D3D11_TEXCUBE_SRV {
                MostDetailedMip: most_detailed_mip,
                MipLevels: mip_levels,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(tex, Some(&desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView Cube");
    srv.unwrap()
}

pub fn create_srv_3d(
    device: &ID3D11Device,
    tex: &ID3D11Texture3D,
    format: DXGI_FORMAT,
    mip_levels: u32,
) -> ID3D11ShaderResourceView {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE3D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture3D: D3D11_TEX3D_SRV {
                MostDetailedMip: 0,
                MipLevels: mip_levels,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(tex, Some(&desc), Some(&mut srv)) }
        .expect("CreateShaderResourceView 3D");
    srv.unwrap()
}

pub fn create_sampler(
    device: &ID3D11Device,
    filter: D3D11_FILTER,
    addr_u: D3D11_TEXTURE_ADDRESS_MODE,
    addr_v: D3D11_TEXTURE_ADDRESS_MODE,
    addr_w: D3D11_TEXTURE_ADDRESS_MODE,
    max_aniso: u32,
) -> ID3D11SamplerState {
    let desc = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: addr_u,
        AddressV: addr_v,
        AddressW: addr_w,
        MipLODBias: 0.0,
        MaxAnisotropy: max_aniso.clamp(1, 16),
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut samp = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut samp)) }.expect("CreateSamplerState");
    samp.unwrap()
}

pub fn create_raster_state(
    device: &ID3D11Device,
    cull: D3D11_CULL_MODE,
    wireframe: bool,
    scissor: bool,
    depth_bias: i32,
    slope_bias: f32,
) -> ID3D11RasterizerState {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: if wireframe { D3D11_FILL_WIREFRAME } else { D3D11_FILL_SOLID },
        CullMode: cull,
        FrontCounterClockwise: true.into(),
        DepthBias: depth_bias,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: slope_bias,
        DepthClipEnable: true.into(),
        ScissorEnable: scissor.into(),
        MultisampleEnable: true.into(),
        AntialiasedLineEnable: false.into(),
    };
    let mut out = None;
    unsafe { device.CreateRasterizerState(&desc, Some(&mut out)) }.expect("CreateRasterizerState");
    out.unwrap()
}

pub fn create_depth_state(
    device: &ID3D11Device,
    enable: bool,
    write: bool,
    func: D3D11_COMPARISON_FUNC,
) -> ID3D11DepthStencilState {
    let desc = D3D11_DEPTH_STENCIL_DESC {
        DepthEnable: enable.into(),
        DepthWriteMask: if write {
            D3D11_DEPTH_WRITE_MASK_ALL
        } else {
            D3D11_DEPTH_WRITE_MASK_ZERO
        },
        DepthFunc: func,
        StencilEnable: false.into(),
        StencilReadMask: 0xFF,
        StencilWriteMask: 0xFF,
        FrontFace: D3D11_DEPTH_STENCILOP_DESC::default(),
        BackFace: D3D11_DEPTH_STENCILOP_DESC::default(),
    };
    let mut out = None;
    unsafe { device.CreateDepthStencilState(&desc, Some(&mut out)) }.expect("CreateDepthStencilState");
    out.unwrap()
}

pub fn create_blend_state(device: &ID3D11Device, mode: BlendMode) -> ID3D11BlendState {
    let rt = match mode {
        BlendMode::Opaque => D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: false.into(),
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        },
        BlendMode::AlphaBlend => D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        },
        BlendMode::PremulAlpha => D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_INV_DEST_ALPHA,
            DestBlendAlpha: D3D11_BLEND_ONE,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        },
        BlendMode::Additive => D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_ONE,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_ONE,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        },
        BlendMode::NoColorWrite => D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: false.into(),
            RenderTargetWriteMask: 0,
            ..Default::default()
        },
    };
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = rt;
    let mut out = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.expect("CreateBlendState");
    out.unwrap()
}

pub fn create_vs(device: &ID3D11Device, bytecode: &[u8]) -> ID3D11VertexShader {
    let mut out = None;
    unsafe { device.CreateVertexShader(bytecode, None, Some(&mut out)) }.expect("CreateVertexShader");
    out.unwrap()
}

pub fn create_ps(device: &ID3D11Device, bytecode: &[u8]) -> ID3D11PixelShader {
    let mut out = None;
    unsafe { device.CreatePixelShader(bytecode, None, Some(&mut out)) }.expect("CreatePixelShader");
    out.unwrap()
}

pub fn create_cs(device: &ID3D11Device, bytecode: &[u8]) -> ID3D11ComputeShader {
    let mut out = None;
    unsafe { device.CreateComputeShader(bytecode, None, Some(&mut out)) }.expect("CreateComputeShader");
    out.unwrap()
}

pub fn create_input_layout(
    device: &ID3D11Device,
    descs: &[D3D11_INPUT_ELEMENT_DESC],
    vs_bytecode: &[u8],
) -> ID3D11InputLayout {
    let mut out = None;
    unsafe { device.CreateInputLayout(descs, vs_bytecode, Some(&mut out)) }
        .expect("CreateInputLayout");
    out.unwrap()
}

pub fn unbind_ps_srvs(ctx: &ID3D11DeviceContext, start: u32, count: u32) {
    let slots: [Option<ID3D11ShaderResourceView>; 32] = std::array::from_fn(|_| None);
    let n = (count as usize).min(slots.len());
    unsafe { ctx.PSSetShaderResources(start, Some(&slots[..n])) };
}

pub fn unbind_cs_uavs(ctx: &ID3D11DeviceContext, start: u32, count: u32) {
    let slots: [Option<ID3D11UnorderedAccessView>; 8] = std::array::from_fn(|_| None);
    let n = (count as usize).min(slots.len());
    unsafe { ctx.CSSetUnorderedAccessViews(start, n as u32, Some(slots.as_ptr()), None) };
}

pub fn set_viewport(ctx: &ID3D11DeviceContext, x: f32, y: f32, w: f32, h: f32) {
    let vp = D3D11_VIEWPORT {
        TopLeftX: x,
        TopLeftY: y,
        Width: w.max(1.0),
        Height: h.max(1.0),
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    unsafe { ctx.RSSetViewports(Some(&[vp])) };
}

pub fn set_scissor(ctx: &ID3D11DeviceContext, x: i32, y: i32, w: i32, h: i32) {
    let rect = windows::Win32::Foundation::RECT {
        left: x,
        top: y,
        right: x + w.max(0),
        bottom: y + h.max(0),
    };
    unsafe { ctx.RSSetScissorRects(Some(&[rect])) };
}

fn read_tex2d_bytes(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
) -> Vec<u8> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc.Width = width;
    desc.Height = height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;

    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.expect("staging tex");
    let staging = staging.unwrap();
    unsafe { ctx.CopyResource(&staging, tex) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("Map readback");
    }
    let row_bytes = (width * bytes_per_pixel) as usize;
    let pitch = mapped.RowPitch as usize;
    let mut out = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        unsafe {
            let src = (mapped.pData as *const u8).add(y * pitch);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row_bytes), row_bytes);
        }
    }
    unsafe { ctx.Unmap(&staging, 0) };
    out
}

pub fn read_tex2d_rgba8(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Vec<u8> {
    read_tex2d_bytes(device, ctx, tex, width, height, 4)
}

pub fn read_tex2d_rgba16f(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Vec<u8> {
    read_tex2d_bytes(device, ctx, tex, width, height, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_device_reports_valid_caps() {
        let Ok(rs) = create_device() else {
            // Headless VM / CI without a D3D11 adapter: skip hardware assertions.
            return;
        };
        assert!(!rs.caps.adapter_name.is_empty());
        assert!(rs.caps.feature_level.0 >= D3D_FEATURE_LEVEL_10_0.0);
        let expected_dim = if rs.caps.feature_level.0 >= D3D_FEATURE_LEVEL_11_0.0 {
            16384
        } else {
            8192
        };
        assert_eq!(rs.caps.max_texture_dim, expected_dim);
        assert!(rs.caps.msaa_rgba8_max_samples >= 1);
        assert!(rs.caps.msaa_fp16_max_samples >= 1);
    }

    #[test]
    fn create_cb_rounds_up_to_16_bytes() {
        let Ok(rs) = create_device() else {
            return;
        };
        let cb = create_cb(&rs.device, 20);
        let mut desc = D3D11_BUFFER_DESC::default();
        unsafe { cb.GetDesc(&mut desc) };
        assert_eq!(desc.ByteWidth, 32);
        assert_eq!(desc.BindFlags, D3D11_BIND_CONSTANT_BUFFER.0 as u32);
    }

    #[test]
    fn read_tex2d_zero_size_returns_empty() {
        let Ok(rs) = create_device() else {
            return;
        };
        let tex = create_tex2d(
            &rs.device,
            &Tex2dSpec {
                width: 4,
                height: 4,
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                bind_flags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Tex2dSpec::default()
            },
            None,
        );
        assert!(read_tex2d_rgba8(&rs.device, &rs.context, &tex, 0, 4).is_empty());
        assert!(read_tex2d_rgba16f(&rs.device, &rs.context, &tex, 4, 0).is_empty());
    }
}
