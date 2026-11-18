//! Native Windows Direct3D 11 rendering backend, DXGI swapchain management,
//! egui DX11 painter, and winit OS window host.

pub mod device;
pub mod egui_painter;
pub mod offscreen;
pub mod shaders;
pub mod swapchain;
pub mod window;

pub use device::{
    create_blend_state, create_cb, create_cs, create_default_buffer, create_depth_state,
    create_device, create_dsv, create_dynamic_buffer, create_immutable_buffer,
    create_input_layout, create_ps, create_raster_state, create_raw_uav_buffer, create_rtv,
    create_rtv_2d_array_slice, create_sampler, create_srv_2d, create_srv_2d_array, create_srv_3d,
    create_srv_cube, create_structured_srv, create_tex2d, create_tex3d, create_vs,
    read_tex2d_rgba16f, read_tex2d_rgba8, set_scissor, set_viewport, unbind_cs_uavs, unbind_ps_srvs, update_dynamic,
    upload_cb, BlendMode, GpuCaps, RenderState, Tex2dSpec,
};
pub use egui_painter::EguiD3d11;
pub use offscreen::OffscreenTarget;
pub use window::{WindowApp, WindowOptions};
