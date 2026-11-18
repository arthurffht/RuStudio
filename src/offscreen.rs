//! `--headless` / `--no-render`: run the app with no window, no compositor
//! and no display server.
//!
//! `--headless` creates a headless Direct3D 11 device without a swapchain and
//! paints each egui frame into an offscreen `R8G8B8A8_UNORM` texture the size
//! of `--window WxH`; `ViewportCommand::Screenshot` reads that texture back
//! and answers with an `Event::Screenshot` the next frame, just like the
//! native window host, so the harness (`--shots`, `--screenshot`) shares one
//! code path. `--no-render` makes no GPU device at all: simulation, scripts,
//! physics and audio state run, egui lays the frame out, nothing is drawn,
//! and screenshots come back empty.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::d3d11::{self, OffscreenTarget, RenderState, WindowApp as _};

pub struct Options {
    pub size: [u32; 2],
    pub render: bool,
    pub paced: bool,
}

fn read_back(rs: &RenderState, t: &OffscreenTarget, size: [u32; 2]) -> egui::ColorImage {
    let rgba = t.read_rgba8(&rs.device, &rs.context);
    let expected = (size[0] as usize) * (size[1] as usize) * 4;
    if rgba.len() != expected {
        return egui::ColorImage::new([0, 0], Vec::new());
    }
    egui::ColorImage::from_rgba_premultiplied([size[0] as usize, size[1] as usize], &rgba)
}

/// Runs the app until it asks to close; returns the exit code.
pub fn run(
    opts: Options,
    make: impl FnOnce(&egui::Context, Option<RenderState>) -> crate::app::RustudioApp,
) -> i32 {
    let ctx = egui::Context::default();
    let rs = if opts.render {
        match d3d11::create_device() {
            Ok(rs) => Some(rs),
            Err(e) => {
                eprintln!("--headless: no D3D11 device without a window: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let size = [opts.size[0].max(1), opts.size[1].max(1)];
    let frame = rs
        .as_ref()
        .map(|rs| OffscreenTarget::new(&rs.device, size[0], size[1]));
    let mut app = make(&ctx, rs.clone());
    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / 60.0);
    let mut next = Instant::now();
    let mut events: Vec<egui::Event> = vec![egui::Event::WindowFocused(true)];
    loop {
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0] as f32, size[1] as f32),
            )),
            time: Some(start.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 60.0,
            focused: true,
            events: std::mem::take(&mut events),
            ..Default::default()
        };
        {
            let v = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
            v.native_pixels_per_point = Some(1.0);
            v.focused = Some(true);
            v.inner_rect = raw.screen_rect;
            v.outer_rect = raw.screen_rect;
            v.monitor_size = Some(egui::vec2(size[0] as f32, size[1] as f32));
        }
        app.raw_input_hook(&ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
        let mut close = false;
        let mut shots = Vec::new();
        for (_, v) in out.viewport_output.iter() {
            for c in &v.commands {
                match c {
                    egui::ViewportCommand::Close => close = true,
                    egui::ViewportCommand::Screenshot(data) => shots.push(data.clone()),
                    _ => {}
                }
            }
        }
        let ppp = out.pixels_per_point;
        if let (Some(rs), Some(t)) = (rs.as_ref(), frame.as_ref()) {
            let jobs = ctx.tessellate(out.shapes, ppp);
            let mut deltas = std::mem::take(&mut out.textures_delta);
            crate::gpu_timing::scope(&rs.device, &rs.context, "gpu: egui (offscreen)", || {
                let mut renderer = rs.renderer.write().expect("egui renderer lock");
                renderer.update_textures(&rs.device, &rs.context, &deltas);
                let clear = [0.0f32, 0.0, 0.0, 1.0];
                unsafe { rs.context.ClearRenderTargetView(t.rtv(), &clear) };
                renderer.paint(&rs.device, &rs.context, t.rtv(), size, ppp, &jobs);
                renderer.free_textures(&deltas);
            });
            deltas.clear();
            if !shots.is_empty() {
                let image = Arc::new(read_back(rs, t, size));
                for user_data in shots {
                    events.push(egui::Event::Screenshot {
                        viewport_id: egui::ViewportId::ROOT,
                        user_data,
                        image: image.clone(),
                    });
                }
            } else {
                unsafe { rs.context.Flush() };
            }
        } else {
            // Avoid debug assertion when dropping unconsumed texture deltas in `--no-render` mode.
            out.textures_delta.clear();
            // No GPU: the harness gets an empty picture (it writes none).
            for user_data in shots {
                events.push(egui::Event::Screenshot {
                    viewport_id: egui::ViewportId::ROOT,
                    user_data,
                    image: Arc::new(egui::ColorImage::new([0, 0], Vec::new())),
                });
            }
        }
        if close {
            app.on_exit();
            return 0;
        }
        if opts.paced {
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > period * 4 {
                // (fell behind: don't run a burst of catch-up frames)
                next = now;
            }
        }
    }
}
