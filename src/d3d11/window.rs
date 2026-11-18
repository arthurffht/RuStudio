//! `winit 0.30` + `egui-winit 0.36` window and secondary immediate-viewport event loop host.

use super::RenderState;

pub trait WindowApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput);
    fn update(&mut self, ctx: &egui::Context, render_state: &RenderState);
    fn on_exit(&mut self);
}

pub struct WindowOptions {
    pub title: String,
    pub inner_size: [f32; 2],
    pub min_inner_size: [f32; 2],
    pub maximized: bool,
    pub icon_rgba: Option<(Vec<u8>, u32, u32)>,
}

pub fn run(
    _options: WindowOptions,
    _init_rs: RenderState,
    _build_app: Box<dyn FnOnce(egui::Context, RenderState) -> Box<dyn WindowApp>>,
) -> Result<(), String> {
    Err("window::run to be implemented by d3d11-core".into())
}
