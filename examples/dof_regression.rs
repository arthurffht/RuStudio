//! Executes the production CoC and radius expressions at known stud depths
//! (near .75 takes the reduced chain, radius = intensity; far .025 the cheap
//! pass, radius 1.4 × intensity) directly via D3D11 Compute + FXC SM 5.0.

use std::ffi::CString;
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;

fn main() {
    let source = std::fs::read_to_string("src/shaders/post.hlsl").unwrap();
    let coc = source
        .split("float dof_coc(")
        .nth(1)
        .unwrap()
        .split("float4 fs_dof_h")
        .next()
        .unwrap();
    let radius = source
        .split("static const float DOF_CHEAP_BELOW")
        .nth(1)
        .unwrap()
        .split("float dof_weight")
        .next()
        .unwrap();

    let shader = format!(
        r#"
struct Globals {{ float4 dof; }};
static Globals g;
float fx_distance(float2 uv) {{ return uv.x; }}
float dof_coc({coc}
static const float DOF_CHEAP_BELOW{radius}
RWByteAddressBuffer result : register(u0);
[numthreads(1, 1, 1)]
void main() {{
    g.dof = float4(100.0f, 20.0f, 0.75f, 0.025f);
    float depths[5] = {{ 40.0f, 80.0f, 100.0f, 120.0f, 240.0f }};
    [unroll]
    for (uint i = 0u; i < 5u; i++) {{
        float c = dof_coc(float2(depths[i], 0.0f));
        float intensity = (c < 0.0f) ? g.dof.z : g.dof.w;
        result.Store(i * 4u, asuint(dof_radius(abs(c), intensity)));
    }}
}}
"#
    );

    let entry = CString::new("main").unwrap();
    let target = CString::new("cs_5_0").unwrap();
    let mut code: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(
            shader.as_ptr().cast(),
            shader.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut errs),
        )
    }
    .unwrap_or_else(|e| {
        let msg = errs
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer().cast(),
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        panic!("D3DCompile failed: {e}\n{msg}");
    });
    let blob = code.unwrap();
    let dxbc = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };

    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
    }
    .expect("D3D11CreateDevice");
    let dev = dev.unwrap();
    let ctx = ctx.unwrap();

    let mut cs = None;
    unsafe { dev.CreateComputeShader(dxbc, None, Some(&mut cs)) }.expect("CreateComputeShader");
    let cs = cs.unwrap();

    let out_desc = D3D11_BUFFER_DESC {
        ByteWidth: 20,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
        ..Default::default()
    };
    let mut out = None;
    unsafe { dev.CreateBuffer(&out_desc, None, Some(&mut out)) }.unwrap();
    let out = out.unwrap();

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: 5,
                Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
            },
        },
    };
    let mut uav = None;
    unsafe { dev.CreateUnorderedAccessView(&out, Some(&uav_desc), Some(&mut uav)) }.unwrap();

    let read_desc = D3D11_BUFFER_DESC {
        ByteWidth: 20,
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut read = None;
    unsafe { dev.CreateBuffer(&read_desc, None, Some(&mut read)) }.unwrap();
    let read = read.unwrap();

    unsafe {
        ctx.CSSetShader(&cs, None);
        let uavs = [uav];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        ctx.Dispatch(1, 1, 1);
        let empty: [Option<ID3D11UnorderedAccessView>; 1] = [None];
        ctx.CSSetUnorderedAccessViews(0, 1, Some(empty.as_ptr()), None);
        ctx.CopyResource(&read, &out);
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { ctx.Map(&read, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.unwrap();
    let mut actual = [0.0_f32; 5];
    unsafe {
        std::ptr::copy_nonoverlapping(mapped.pData as *const f32, actual.as_mut_ptr(), 5);
        ctx.Unmap(&read, 0);
    }

    for (value, expected) in actual.iter().zip([
        0.75,
        0.75 * (0.5_f32 / 80.0).powi(2),
        0.0,
        0.0,
        1.4 * 0.025 * (119.5_f32 / 240.0).powi(2),
    ]) {
        assert!((value - expected).abs() < 1e-6, "{actual:?}");
    }
    println!(
        "PASS GPU production CoC/radius near40 / focus80,100,120 / far240 studs: {actual:?}"
    );
}
