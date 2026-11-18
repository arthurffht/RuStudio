//! Native Win32 + Direct3D 11 window host using winit 0.30 and egui-winit 0.36.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use super::device::{read_tex2d_rgba8, RenderState};
use super::swapchain::Swapchain;

const INVISIBLE_WINDOW_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Application callbacks driven by [`run`].
pub trait WindowApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let _ = (ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui);

    fn on_exit(&mut self) {}
}

/// Configuration for the primary window and DXGI presentation.
pub struct WindowOptions {
    pub viewport: egui::ViewportBuilder,
    pub vsync: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            viewport: egui::ViewportBuilder::default(),
            vsync: true,
        }
    }
}

#[derive(Debug)]
enum UserEvent {
    RequestRepaint {
        when: Instant,
        cumulative_pass_nr: u64,
        viewport_id: egui::ViewportId,
    },
}

// Scoped access to `ActiveEventLoop` so re-entrant immediate viewport callbacks can create OS windows.
std::thread_local! {
    static ACTIVE_EVENT_LOOP: Cell<*const ActiveEventLoop> = const { Cell::new(std::ptr::null()) };
}

struct EventLoopGuard {
    prev: *const ActiveEventLoop,
}

impl Drop for EventLoopGuard {
    fn drop(&mut self) {
        ACTIVE_EVENT_LOOP.with(|cell| cell.set(self.prev));
    }
}

fn with_event_loop_context<R>(event_loop: &ActiveEventLoop, f: impl FnOnce() -> R) -> R {
    let ptr: *const ActiveEventLoop = event_loop;
    let prev = ACTIVE_EVENT_LOOP.with(|cell| cell.replace(ptr));
    let _guard = EventLoopGuard { prev };
    f()
}

fn with_current_event_loop<R>(f: impl FnOnce(&ActiveEventLoop) -> R) -> Option<R> {
    let ptr = ACTIVE_EVENT_LOOP.with(|cell| cell.get());
    if ptr.is_null() {
        None
    } else {
        // SAFETY: `ACTIVE_EVENT_LOOP` is only non-null while inside `with_event_loop_context`.
        Some(f(unsafe { &*ptr }))
    }
}

/// Extract the Win32 `HWND` from a `winit` window handle.
pub fn window_hwnd(window: &Window) -> Result<HWND, String> {
    let handle = window
        .window_handle()
        .map_err(|e| format!("Failed to read window handle: {e}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        other => Err(format!("Expected Win32 window handle, got {other:?}")),
    }
}

struct ViewportSlot {
    ids: egui::ViewportIdPair,
    class: egui::ViewportClass,
    builder: egui::ViewportBuilder,
    deferred_ui_cb: Option<Arc<egui::DeferredViewportUiCallback>>,
    window: Option<Arc<Window>>,
    egui_state: Option<egui_winit::State>,
    swapchain: Option<Swapchain>,
    info: egui::ViewportInfo,
    deferred_commands: Vec<egui::ViewportCommand>,
    actions_requested: Vec<egui_winit::ActionRequested>,
    cursor_visible: bool,
    initial_visible: bool,
    shown_once: bool,
}

impl ViewportSlot {
    fn new(
        ids: egui::ViewportIdPair,
        class: egui::ViewportClass,
        builder: egui::ViewportBuilder,
        deferred_ui_cb: Option<Arc<egui::DeferredViewportUiCallback>>,
    ) -> Self {
        let initial_visible = builder.visible.unwrap_or(true);
        Self {
            ids,
            class,
            builder,
            deferred_ui_cb,
            window: None,
            egui_state: None,
            swapchain: None,
            info: Default::default(),
            deferred_commands: Vec::new(),
            actions_requested: Vec::new(),
            cursor_visible: true,
            initial_visible,
            shown_once: false,
        }
    }

    fn process_commands(
        &mut self,
        egui_ctx: &egui::Context,
        mut commands: Vec<egui::ViewportCommand>,
    ) {
        let mut requested_close = false;
        for cmd in &commands {
            match cmd {
                egui::ViewportCommand::CursorVisible(v) => self.cursor_visible = *v,
                egui::ViewportCommand::Close => requested_close = true,
                _ => {}
            }
        }
        self.deferred_commands.append(&mut commands);

        if let Some(window) = self.window.as_ref() {
            egui_winit::process_viewport_commands(
                egui_ctx,
                &mut self.info,
                std::mem::take(&mut self.deferred_commands),
                window,
                &mut self.actions_requested,
            );
            if !self.cursor_visible {
                window.set_cursor_visible(false);
            }
        }
        if requested_close {
            egui_ctx.request_repaint_of(self.ids.this);
        }
    }

    fn ensure_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        egui_ctx: &egui::Context,
        rs: &RenderState,
        viewport_from_window: &mut HashMap<WindowId, egui::ViewportId>,
        hide_until_first_paint: bool,
    ) -> Result<(), String> {
        if self.window.is_some() {
            return Ok(());
        }

        let create_builder = if hide_until_first_paint {
            self.builder.clone().with_visible(false)
        } else {
            self.builder.clone()
        };

        let window = egui_winit::create_window(egui_ctx, event_loop, &create_builder)
            .map_err(|e| format!("Failed to create window for {:?}: {e}", self.ids.this))?;
        let hwnd = window_hwnd(&window)?;
        let phys = window.inner_size();
        let swapchain = Swapchain::new(rs, hwnd, phys.width.max(1), phys.height.max(1))?;

        let mut egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            self.ids.this,
            event_loop,
            Some(window.scale_factor() as f32),
            event_loop.system_theme(),
            Some(rs.caps.max_texture_dim as usize),
        );
        egui_state.set_max_texture_side(rs.caps.max_texture_dim as usize);

        egui_winit::update_viewport_info(&mut self.info, egui_ctx, &window, true);

        let window = Arc::new(window);
        viewport_from_window.insert(window.id(), self.ids.this);
        self.window = Some(window);
        self.egui_state = Some(egui_state);
        self.swapchain = Some(swapchain);

        if !self.deferred_commands.is_empty() {
            let cmds = std::mem::take(&mut self.deferred_commands);
            self.process_commands(egui_ctx, cmds);
        }
        Ok(())
    }
}

struct SharedHostState {
    egui_ctx: egui::Context,
    rs: RenderState,
    app: Box<dyn WindowApp>,
    viewports: HashMap<egui::ViewportId, ViewportSlot>,
    viewport_from_window: HashMap<WindowId, egui::ViewportId>,
    focused_viewport: Option<egui::ViewportId>,
    pending_deltas: egui::TexturesDelta,
    start_time: Instant,
    vsync: bool,
    should_close: bool,
    exited: bool,
}

impl Drop for SharedHostState {
    fn drop(&mut self) {
        self.pending_deltas.clear();
    }
}

struct D3d11WindowRunner {
    shared: Rc<RefCell<SharedHostState>>,
    next_repaints: HashMap<WindowId, Instant>,
    root_options: Option<WindowOptions>,
    exit_error: Option<String>,
}

impl D3d11WindowRunner {
    fn run_ui_and_paint(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) -> Result<bool, String> {
        self.next_repaints.remove(&window_id);
        let viewport_id = {
            let shared = self.shared.borrow();
            let Some(&id) = shared.viewport_from_window.get(&window_id) else {
                return Ok(false);
            };
            id
        };

        let (raw_input, deferred_cb, egui_ctx, is_root) = {
            let mut shared = self.shared.borrow_mut();
            let SharedHostState {
                egui_ctx,
                viewports,
                start_time,
                ..
            } = &mut *shared;

            let viewports_info: egui::ViewportIdMap<egui::ViewportInfo> = viewports
                .iter()
                .map(|(id, vp)| (*id, vp.info.clone()))
                .collect();

            let Some(vp) = viewports.get_mut(&viewport_id) else {
                return Ok(false);
            };
            let (Some(window), Some(egui_state)) = (&vp.window, &mut vp.egui_state) else {
                return Ok(false);
            };

            egui_winit::update_viewport_info(&mut vp.info, egui_ctx, window, false);
            let mut input = egui_state.take_egui_input(window);
            input.viewports = viewports_info;
            input.time = Some(start_time.elapsed().as_secs_f64());

            let deferred = if viewport_id == egui::ViewportId::ROOT {
                None
            } else {
                vp.deferred_ui_cb.clone()
            };
            (input, deferred, egui_ctx.clone(), viewport_id == egui::ViewportId::ROOT)
        };

        if !is_root && deferred_cb.is_none() {
            return Ok(false);
        }

        let close_requested = raw_input.viewport().close_requested();

        // Prepare final raw_input via WindowApp hook for ROOT before running egui.
        let mut raw_input = raw_input;
        let full_output = if is_root {
            {
                let mut shared = self.shared.borrow_mut();
                shared.app.raw_input_hook(&egui_ctx, &mut raw_input);
            }

            // Hold NO RefCell borrow across `run_ui`: immediate viewports re-enter `shared`.
            egui_ctx.run_ui(raw_input, |ui| {
                let mut shared = self.shared.borrow_mut();
                shared.app.ui(ui);
            })
        } else {
            let cb = deferred_cb.expect("checked above");
            egui_ctx.run_ui(raw_input, |ui| {
                cb(ui);
            })
        };

        self.finish_viewport_frame(event_loop, viewport_id, is_root, close_requested, full_output, false)?;
        let should_close = self.shared.borrow().should_close;
        Ok(should_close)
    }

    fn finish_viewport_frame(
        &mut self,
        event_loop: &ActiveEventLoop,
        viewport_id: egui::ViewportId,
        is_root: bool,
        close_requested: bool,
        full_output: egui::FullOutput,
        for_immediate: bool,
    ) -> Result<(), String> {
        let mut shared = self.shared.borrow_mut();
        let SharedHostState {
            egui_ctx,
            rs,
            viewports,
            viewport_from_window,
            pending_deltas,
            vsync,
            should_close,
            ..
        } = &mut *shared;

        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        pending_deltas.append(textures_delta);

        if is_root && close_requested {
            let canceled = viewport_output
                .get(&egui::ViewportId::ROOT)
                .is_some_and(|out| out.commands.contains(&egui::ViewportCommand::CancelClose));
            if should_close_after_frame(true, canceled) {
                *should_close = true;
            }
        }

        if is_root {
            let active_ids: HashSet<egui::ViewportId> = viewport_output.keys().copied().collect();
            prune_dead_viewports(viewports, viewport_from_window, &active_ids);
        }

        if let Some(vp) = viewports.get_mut(&viewport_id) {
            vp.info.events.clear();
            let cursor_visible = vp.cursor_visible;
            let (Some(window), Some(egui_state), Some(swapchain)) = (
                vp.window.as_ref(),
                vp.egui_state.as_mut(),
                vp.swapchain.as_mut(),
            ) else {
                return Ok(());
            };

            if for_immediate {
                egui_state.handle_platform_output(window, platform_output);
            } else {
                egui_state.handle_platform_output_with_event_loop(window, event_loop, platform_output);
            }
            if !cursor_visible {
                window.set_cursor_visible(false);
            }

            let clipped_primitives = egui_ctx.tessellate(shapes, pixels_per_point);

            let mut screenshots = Vec::new();
            vp.actions_requested.retain(|cmd| {
                if let egui_winit::ActionRequested::Screenshot(data) = cmd {
                    screenshots.push(data.clone());
                    false
                } else {
                    true
                }
            });

            {
                let mut painter = rs.renderer.write().expect("egui painter lock");
                painter.update_textures(&rs.device, &rs.context, pending_deltas);

                let inner = window.inner_size();
                if let Some([w, h]) = effective_swapchain_size(inner.width, inner.height) {
                    if swapchain.size() != [w, h] {
                        swapchain.resize(&rs.device, &rs.context, w, h);
                    }
                    unsafe {
                        rs.context.ClearRenderTargetView(swapchain.rtv(), &CLEAR_COLOR);
                    }
                    painter.paint(
                        &rs.device,
                        &rs.context,
                        swapchain.rtv(),
                        [w, h],
                        pixels_per_point,
                        &clipped_primitives,
                    );

                    if !screenshots.is_empty() {
                        let [sw, sh] = swapchain.size();
                        let rgba = read_tex2d_rgba8(
                            &rs.device,
                            &rs.context,
                            swapchain.backbuffer(),
                            sw,
                            sh,
                        );
                        if rgba.len() == (sw as usize) * (sh as usize) * 4 {
                            let image = Arc::new(egui::ColorImage::from_rgba_premultiplied(
                                [sw as usize, sh as usize],
                                &rgba,
                            ));
                            for user_data in screenshots {
                                egui_state.egui_input_mut().events.push(egui::Event::Screenshot {
                                    viewport_id,
                                    user_data,
                                    image: Arc::clone(&image),
                                });
                            }
                            egui_ctx.request_repaint_of(viewport_id);
                        }
                    }

                    // Only vsync on ROOT so multiple popped-out child OS windows do not divide FPS.
                    let use_vsync = is_root && *vsync;
                    swapchain.present(use_vsync);
                    if !vp.shown_once {
                        vp.shown_once = true;
                        if vp.initial_visible {
                            window.set_visible(true);
                        }
                    }
                }

                painter.free_textures(pending_deltas);
                pending_deltas.clear();
            }

            let remaining_actions: Vec<_> = vp.actions_requested.drain(..).collect();
            for action in remaining_actions {
                match action {
                    egui_winit::ActionRequested::Screenshot(_) => {}
                    egui_winit::ActionRequested::Cut => {
                        egui_state.egui_input_mut().events.push(egui::Event::Cut);
                    }
                    egui_winit::ActionRequested::Copy => {
                        egui_state.egui_input_mut().events.push(egui::Event::Copy);
                    }
                    egui_winit::ActionRequested::Paste => {
                        if let Some(contents) = egui_state.clipboard_text() {
                            let contents = contents.replace("\r\n", "\n");
                            if !contents.is_empty() {
                                egui_state
                                    .egui_input_mut()
                                    .events
                                    .push(egui::Event::Paste(contents));
                            }
                        }
                    }
                }
            }
        }

        handle_viewport_output(egui_ctx, &viewport_output, viewports);
        if is_root {
            // Ensure Deferred viewports have OS windows created.
            for vp in viewports.values_mut() {
                if vp.class == egui::ViewportClass::Deferred && vp.window.is_none() {
                    if let Err(err) =
                        vp.ensure_window(event_loop, egui_ctx, rs, viewport_from_window, false)
                    {
                        eprintln!("Deferred viewport creation failed: {err}");
                    }
                }
            }
        }

        Ok(())
    }

    fn finish_exit(&mut self, event_loop: &ActiveEventLoop) {
        {
            let mut shared = self.shared.borrow_mut();
            if !shared.exited {
                shared.exited = true;
                shared.app.on_exit();
            }
            shared.viewports.clear();
            shared.viewport_from_window.clear();
            shared.pending_deltas.clear();
        }
        self.next_repaints.clear();
        event_loop.exit();
    }

    fn schedule_repaint(&mut self, window_id: WindowId, when: Instant) {
        self.next_repaints
            .entry(window_id)
            .and_modify(|prev| *prev = (*prev).min(when))
            .or_insert(when);
    }

    fn check_redraw_requests(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut invisible_windows = Vec::new();
        let mut due_visible = Vec::new();

        self.next_repaints.retain(|&win_id, &mut repaint_time| {
            if now < repaint_time {
                return true;
            }
            let shared = self.shared.borrow();
            let window = shared
                .viewport_from_window
                .get(&win_id)
                .and_then(|vp_id| shared.viewports.get(vp_id))
                .and_then(|vp| vp.window.clone());
            if let Some(window) = window {
                if is_invisible_or_minimized(&window) {
                    invisible_windows.push(win_id);
                } else {
                    due_visible.push(window);
                }
            }
            false
        });

        for window in due_visible {
            window.request_redraw();
        }

        // Invisible or minimized windows on Windows do not receive RedrawRequested.
        for win_id in &invisible_windows {
            match self.run_ui_and_paint(event_loop, *win_id) {
                Ok(true) => {
                    self.finish_exit(event_loop);
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    self.exit_error = Some(err);
                    self.finish_exit(event_loop);
                    return;
                }
            }
        }

        if !invisible_windows.is_empty() {
            let throttled = Instant::now() + INVISIBLE_WINDOW_REPAINT_INTERVAL;
            for win_id in invisible_windows {
                self.next_repaints
                    .entry(win_id)
                    .and_modify(|t| *t = (*t).min(throttled));
            }
        }

        let next = self.next_repaints.values().min().copied();
        event_loop.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

fn render_immediate_viewport(
    runner_shared: &Rc<RefCell<SharedHostState>>,
    immediate_vp: egui::ImmediateViewport<'_>,
) {
    let egui::ImmediateViewport {
        ids,
        builder,
        mut viewport_ui_cb,
    } = immediate_vp;

    let (raw_input, egui_ctx) = {
        let mut shared = runner_shared.borrow_mut();
        let SharedHostState {
            egui_ctx,
            rs,
            viewports,
            viewport_from_window,
            start_time,
            ..
        } = &mut *shared;

        let vp = viewports
            .entry(ids.this)
            .and_modify(|existing| {
                existing.ids = ids;
                existing.class = egui::ViewportClass::Immediate;
                existing.builder = builder.clone();
            })
            .or_insert_with(|| {
                ViewportSlot::new(ids, egui::ViewportClass::Immediate, builder, None)
            });

        if vp.window.is_none() {
            let init_res = with_current_event_loop(|event_loop| {
                vp.ensure_window(event_loop, egui_ctx, rs, viewport_from_window, false)
            });
            if !matches!(init_res, Some(Ok(()))) {
                return;
            }
        }

        let viewports_info: egui::ViewportIdMap<egui::ViewportInfo> = viewports
            .iter()
            .map(|(id, slot)| (*id, slot.info.clone()))
            .collect();

        let Some(vp) = viewports.get_mut(&ids.this) else {
            return;
        };
        let (Some(window), Some(egui_state)) = (&vp.window, &mut vp.egui_state) else {
            return;
        };
        egui_winit::update_viewport_info(&mut vp.info, egui_ctx, window, false);
        let mut input = egui_state.take_egui_input(window);
        input.viewports = viewports_info;
        input.time = Some(start_time.elapsed().as_secs_f64());
        (input, egui_ctx.clone())
    };

    // Release `shared` borrow while executing nested immediate viewport UI callback.
    let full_output = egui_ctx.run_ui(raw_input, |ui| {
        viewport_ui_cb(ui);
    });

    let mut shared = runner_shared.borrow_mut();
    let SharedHostState {
        egui_ctx,
        rs,
        viewports,
        pending_deltas,
        ..
    } = &mut *shared;

    let egui::FullOutput {
        platform_output,
        textures_delta,
        shapes,
        pixels_per_point,
        viewport_output,
    } = full_output;

    pending_deltas.append(textures_delta);

    let Some(vp) = viewports.get_mut(&ids.this) else {
        return;
    };
    vp.info.events.clear();
    let cursor_visible = vp.cursor_visible;
    let (Some(window), Some(egui_state), Some(swapchain)) = (
        vp.window.as_ref(),
        vp.egui_state.as_mut(),
        vp.swapchain.as_mut(),
    ) else {
        return;
    };

    egui_state.handle_platform_output(window, platform_output);
    if !cursor_visible {
        window.set_cursor_visible(false);
    }

    let clipped_primitives = egui_ctx.tessellate(shapes, pixels_per_point);
    {
        let mut painter = rs.renderer.write().expect("egui painter lock");
        painter.update_textures(&rs.device, &rs.context, pending_deltas);

        let inner = window.inner_size();
        if let Some([w, h]) = effective_swapchain_size(inner.width, inner.height) {
            if swapchain.size() != [w, h] {
                swapchain.resize(&rs.device, &rs.context, w, h);
            }
            unsafe {
                rs.context.ClearRenderTargetView(swapchain.rtv(), &CLEAR_COLOR);
            }
            painter.paint(
                &rs.device,
                &rs.context,
                swapchain.rtv(),
                [w, h],
                pixels_per_point,
                &clipped_primitives,
            );
            swapchain.present(false);
            if !vp.shown_once {
                vp.shown_once = true;
                if vp.initial_visible {
                    window.set_visible(true);
                }
            }
        }
        painter.free_textures(pending_deltas);
        pending_deltas.clear();
    }

    handle_viewport_output(egui_ctx, &viewport_output, viewports);
}

fn handle_viewport_output(
    egui_ctx: &egui::Context,
    viewport_output: &egui::OrderedViewportIdMap<egui::ViewportOutput>,
    viewports: &mut HashMap<egui::ViewportId, ViewportSlot>,
) {
    for (viewport_id, out) in viewport_output.clone() {
        let ids = egui::ViewportIdPair::from_self_and_parent(viewport_id, out.parent);
        let vp = viewports
            .entry(viewport_id)
            .and_modify(|existing| {
                existing.ids = ids;
                existing.class = out.class;
                existing.builder = out.builder.clone();
                existing.deferred_ui_cb = out.viewport_ui_cb.clone();
            })
            .or_insert_with(|| {
                ViewportSlot::new(ids, out.class, out.builder, out.viewport_ui_cb)
            });

        vp.process_commands(egui_ctx, out.commands);
    }
}

fn prune_dead_viewports(
    viewports: &mut HashMap<egui::ViewportId, ViewportSlot>,
    viewport_from_window: &mut HashMap<WindowId, egui::ViewportId>,
    active_ids: &HashSet<egui::ViewportId>,
) {
    viewports.retain(|id, _| active_ids.contains(id));
    viewport_from_window.retain(|_, id| active_ids.contains(id));
}

fn effective_swapchain_size(width: u32, height: u32) -> Option<[u32; 2]> {
    if width == 0 || height == 0 {
        None
    } else {
        Some([width, height])
    }
}

fn should_close_after_frame(close_requested: bool, cancel_close_sent: bool) -> bool {
    close_requested && !cancel_close_sent
}

fn is_invisible_or_minimized(window: &Window) -> bool {
    window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(true)
}

impl ApplicationHandler<UserEvent> for D3d11WindowRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        with_event_loop_context(event_loop, || {
            let root_vp = self
                .root_options
                .take()
                .map(|o| o.viewport)
                .unwrap_or_default();
            let root_id = egui::ViewportId::ROOT;
            let root_win_id = {
                let mut shared = self.shared.borrow_mut();
                let SharedHostState {
                    egui_ctx,
                    rs,
                    viewports,
                    viewport_from_window,
                    ..
                } = &mut *shared;
                let vp = viewports.entry(root_id).or_insert_with(|| {
                    ViewportSlot::new(
                        egui::ViewportIdPair::from_self_and_parent(root_id, root_id),
                        egui::ViewportClass::Root,
                        root_vp,
                        None,
                    )
                });
                match vp.ensure_window(event_loop, egui_ctx, rs, viewport_from_window, true) {
                    Ok(()) => vp.window.as_ref().map(|w| w.id()),
                    Err(err) => {
                        self.exit_error = Some(err);
                        None
                    }
                }
            };

            if self.exit_error.is_some() {
                self.finish_exit(event_loop);
                return;
            }
            if let Some(win_id) = root_win_id {
                self.schedule_repaint(win_id, Instant::now());
            }
        });
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: StartCause) {
        with_event_loop_context(event_loop, || {
            self.check_redraw_requests(event_loop);
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        with_event_loop_context(event_loop, || match event {
            UserEvent::RequestRepaint {
                when,
                cumulative_pass_nr,
                viewport_id,
            } => {
                let win_and_pass = {
                    let shared = self.shared.borrow();
                    let current = shared.egui_ctx.cumulative_pass_nr_for(viewport_id);
                    if current != cumulative_pass_nr && current != cumulative_pass_nr + 1 {
                        return;
                    }
                    shared
                        .viewports
                        .get(&viewport_id)
                        .and_then(|vp| vp.window.as_ref())
                        .map(|w| {
                            let throttled = if is_invisible_or_minimized(w) {
                                when.max(Instant::now() + INVISIBLE_WINDOW_REPAINT_INTERVAL)
                            } else {
                                when
                            };
                            (w.id(), throttled)
                        })
                };
                if let Some((win_id, throttled)) = win_and_pass {
                    self.schedule_repaint(win_id, throttled);
                }
            }
        });
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event {
            let win_to_repaint = {
                let mut shared = self.shared.borrow_mut();
                let target_id = shared.focused_viewport.unwrap_or(egui::ViewportId::ROOT);
                let mut repaint_win = None;
                if let Some(vp) = shared.viewports.get_mut(&target_id) {
                    if let Some(egui_state) = vp.egui_state.as_mut() {
                        if egui_state.on_mouse_motion(delta) || !vp.cursor_visible {
                            repaint_win = vp.window.as_ref().map(|w| w.id());
                        }
                    }
                }
                repaint_win
            };
            if let Some(win_id) = win_to_repaint {
                self.schedule_repaint(win_id, Instant::now());
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        with_event_loop_context(event_loop, || {
            if matches!(event, WindowEvent::RedrawRequested) {
                match self.run_ui_and_paint(event_loop, window_id) {
                    Ok(true) => self.finish_exit(event_loop),
                    Ok(false) => {}
                    Err(err) => {
                        self.exit_error = Some(err);
                        self.finish_exit(event_loop);
                    }
                }
                return;
            }

            let (repaint, repaint_now) = {
                let mut shared = self.shared.borrow_mut();
                let SharedHostState {
                    egui_ctx,
                    rs,
                    viewports,
                    viewport_from_window,
                    focused_viewport,
                    should_close,
                    ..
                } = &mut *shared;

                let Some(&viewport_id) = viewport_from_window.get(&window_id) else {
                    return;
                };

                let mut repaint_now = false;
                match &event {
                    WindowEvent::Focused(focused) => {
                        if *focused {
                            *focused_viewport = Some(viewport_id);
                        } else if *focused_viewport == Some(viewport_id) {
                            *focused_viewport = None;
                        }
                    }
                    WindowEvent::Resized(phys) => {
                        if let (Some(w), Some(h)) =
                            (NonZeroU32::new(phys.width), NonZeroU32::new(phys.height))
                        {
                            if let Some(vp) = viewports.get_mut(&viewport_id) {
                                if let Some(sc) = vp.swapchain.as_mut() {
                                    sc.resize(&rs.device, &rs.context, w.get(), h.get());
                                }
                            }
                            repaint_now = true;
                        }
                    }
                    WindowEvent::Occluded(occluded) => {
                        if let Some(vp) = viewports.get_mut(&viewport_id) {
                            vp.info.occluded = Some(*occluded);
                        }
                    }
                    WindowEvent::CloseRequested => {
                        if viewport_id == egui::ViewportId::ROOT && *should_close {
                            return;
                        }
                        if let Some(vp) = viewports.get_mut(&viewport_id) {
                            vp.info.events.push(egui::ViewportEvent::Close);
                            egui_ctx.request_repaint_of(viewport_id);
                            if viewport_id != egui::ViewportId::ROOT {
                                egui_ctx.request_repaint_of(vp.ids.parent);
                            }
                        }
                    }
                    _ => {}
                }

                let repaint = viewports
                    .get_mut(&viewport_id)
                    .and_then(|vp| {
                        let win = vp.window.as_deref()?;
                        let state = vp.egui_state.as_mut()?;
                        Some(state.on_window_event(win, &event).repaint)
                    })
                    .unwrap_or(false);
                (repaint || repaint_now, repaint_now)
            };

            if repaint_now {
                // Paint synchronously on Windows live-resize to prevent frame flicker.
                self.schedule_repaint(window_id, Instant::now());
                match self.run_ui_and_paint(event_loop, window_id) {
                    Ok(true) => self.finish_exit(event_loop),
                    Ok(false) => {}
                    Err(err) => {
                        self.exit_error = Some(err);
                        self.finish_exit(event_loop);
                    }
                }
            } else if repaint {
                self.schedule_repaint(window_id, Instant::now());
            }
        });
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        with_event_loop_context(event_loop, || {
            self.check_redraw_requests(event_loop);
        });
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.finish_exit(event_loop);
    }
}

/// Create the Win32 `EventLoop`, initialize egui + D3D11 window host, and run until exit.
pub fn run(
    options: WindowOptions,
    init_rs: RenderState,
    build_app: Box<dyn FnOnce(egui::Context, RenderState) -> Box<dyn WindowApp>>,
) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("Failed to create winit EventLoop: {e}"))?;

    let proxy = event_loop.create_proxy();
    let egui_ctx = egui::Context::default();
    egui_ctx.set_embed_viewports(false);
    egui_ctx.set_request_repaint_callback(move |info| {
        let when = Instant::now() + info.delay;
        let _ = proxy.send_event(UserEvent::RequestRepaint {
            when,
            cumulative_pass_nr: info.current_cumulative_pass_nr,
            viewport_id: info.viewport_id,
        });
    });

    let app = build_app(egui_ctx.clone(), init_rs.clone());
    let shared = Rc::new(RefCell::new(SharedHostState {
        egui_ctx: egui_ctx.clone(),
        rs: init_rs,
        app,
        viewports: HashMap::new(),
        viewport_from_window: HashMap::new(),
        focused_viewport: None,
        pending_deltas: Default::default(),
        start_time: Instant::now(),
        vsync: options.vsync,
        should_close: false,
        exited: false,
    }));

    let immediate_shared = Rc::downgrade(&shared);
    egui::Context::set_immediate_viewport_renderer(move |_ctx, immediate_vp| {
        if let Some(shared_rc) = immediate_shared.upgrade() {
            render_immediate_viewport(&shared_rc, immediate_vp);
        }
    });

    let mut runner = D3d11WindowRunner {
        shared,
        next_repaints: HashMap::new(),
        root_options: Some(options),
        exit_error: None,
    };

    event_loop
        .run_app(&mut runner)
        .map_err(|e| format!("EventLoop error: {e}"))?;

    if let Some(err) = runner.exit_error {
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_swapchain_size_ignores_zero_minimized_dimensions() {
        assert_eq!(effective_swapchain_size(0, 800), None);
        assert_eq!(effective_swapchain_size(1280, 0), None);
        assert_eq!(effective_swapchain_size(0, 0), None);
        assert_eq!(effective_swapchain_size(1280, 720), Some([1280, 720]));
    }

    #[test]
    fn close_request_honors_cancel_close_command() {
        assert!(!should_close_after_frame(false, false));
        assert!(!should_close_after_frame(false, true));
        assert!(!should_close_after_frame(true, true));
        assert!(should_close_after_frame(true, false));
    }

    #[test]
    fn prune_dead_viewports_drops_orphaned_window_mappings() {
        let mut viewports = HashMap::new();
        let root_id = egui::ViewportId::ROOT;
        let dead_id = egui::ViewportId::from_hash_of("dead_child");
        viewports.insert(
            root_id,
            ViewportSlot::new(
                egui::ViewportIdPair::from_self_and_parent(root_id, root_id),
                egui::ViewportClass::Root,
                Default::default(),
                None,
            ),
        );
        viewports.insert(
            dead_id,
            ViewportSlot::new(
                egui::ViewportIdPair::from_self_and_parent(dead_id, root_id),
                egui::ViewportClass::Immediate,
                Default::default(),
                None,
            ),
        );

        let mut win_map = HashMap::new();
        let win_root = WindowId::dummy();
        win_map.insert(win_root, root_id);

        let mut active = HashSet::new();
        active.insert(root_id);
        prune_dead_viewports(&mut viewports, &mut win_map, &active);

        assert!(viewports.contains_key(&root_id));
        assert!(!viewports.contains_key(&dead_id));
        assert_eq!(win_map.get(&win_root), Some(&root_id));
    }
}
