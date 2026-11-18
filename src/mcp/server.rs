//! OmniMCP TCP bridge server and main-thread tick / deferred request state.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use image::ImageEncoder as _;

use super::protocol::{
    remove_instance_record, write_instance_record, BridgeRequest, BridgeResponse, InstanceRecord,
    ToolCallResult,
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome returned by an OmniMCP tool handler.
pub enum CallOutcome {
    /// Completed synchronously within this frame's `mcp_tick`.
    Immediate(ToolCallResult),
    /// Asynchronous / multi-frame operation; `reply` was stored in `McpState`
    /// and will receive `BridgeResponse::ToolResult` when finished.
    Deferred,
}

/// Which surface a screenshot capture targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    Viewport,
    Window,
}

/// Deferred screenshot capture driven across frames in `mcp_tick`.
pub struct PendingCapture {
    pub target: CaptureTarget,
    pub save_path: Option<PathBuf>,
    pub return_image: bool,
    pub max_width: Option<u32>,
    pub saved_clean_view: Option<bool>,
    pub asked_at: Instant,
    pub settle_frames: u32,
    pub screenshot_requested: bool,
    pub reply: Sender<BridgeResponse>,
}

/// Deferred simulation step request driven across frames in `mcp_tick`.
pub struct PendingSteps {
    pub remaining_frames: Option<u32>,
    pub target_elapsed_secs: Option<f64>,
    pub frames_stepped: u32,
    pub started_at: Instant,
    pub start_play_time: f64,
    pub saved_fixed_dt: Option<Option<f32>>,
    pub log_start_index: usize,
    pub reply: Sender<BridgeResponse>,
}

/// Mutable OmniMCP runtime state stored on `McpServer` and accessible to tool handlers.
#[derive(Default)]
pub struct McpState {
    pub pending_capture: Option<PendingCapture>,
    pub pending_steps: Option<PendingSteps>,
    pub paused: bool,
}

struct IncomingRequest {
    req: BridgeRequest,
    reply: Sender<BridgeResponse>,
}

/// Localhost TCP server bridging `rustudio-mcp` (or direct TCP clients) into `Editor`.
pub struct McpServer {
    pub port: u16,
    pub pid: u32,
    pub state: McpState,
    rx: Mutex<Receiver<IncomingRequest>>,
    started_unix: u64,
    last_record_meta: (String, Option<String>, String),
    stop: Arc<AtomicBool>,
}

impl McpServer {
    /// Binds on `127.0.0.1` (`cli_port` or `RUSTUDIO_MCP_PORT`, else `44777..=44790`, fallback `0`),
    /// publishes `%LOCALAPPDATA%\rustudio\mcp-instances\<pid>.json`, and starts the listener thread.
    pub fn start(
        egui_ctx: egui::Context,
        cli_port: Option<u16>,
        title: &str,
        place_path: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        // Prune any dead instances before publishing ours.
        let _ = super::protocol::list_live_instances();

        let explicit_port = cli_port.or_else(|| {
            std::env::var("RUSTUDIO_MCP_PORT")
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
        });

        let listener = if let Some(p) = explicit_port {
            TcpListener::bind(("127.0.0.1", p))?
        } else {
            let mut bound = None;
            for p in 44777..=44790 {
                if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
                    bound = Some(l);
                    break;
                }
            }
            match bound {
                Some(l) => l,
                None => TcpListener::bind(("127.0.0.1", 0))?,
            }
        };

        let port = listener.local_addr()?.port();
        let pid = std::process::id();
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let place_str = place_path.map(|p| p.display().to_string());
        let mode = "Edit".to_string();

        let rec = InstanceRecord {
            pid,
            port,
            title: title.to_string(),
            place_path: place_str.clone(),
            mode: mode.clone(),
            started_unix,
        };
        if let Err(e) = write_instance_record(&rec) {
            eprintln!("[mcp] warning: failed to write instance record: {e}");
        }

        let (tx, rx) = channel::<IncomingRequest>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        std::thread::Builder::new()
            .name("omnimcp-listener".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let tx = tx.clone();
                    let ctx = egui_ctx.clone();
                    let stop_client = stop_thread.clone();
                    let _ = std::thread::Builder::new()
                        .name("omnimcp-client".into())
                        .spawn(move || handle_client(stream, tx, ctx, stop_client));
                }
            })?;

        eprintln!("[mcp] OmniMCP bridge listening on 127.0.0.1:{port} (pid {pid})");
        Ok(Self {
            port,
            pid,
            state: McpState::default(),
            rx: Mutex::new(rx),
            started_unix,
            last_record_meta: (title.to_string(), place_str, mode),
            stop,
        })
    }

    fn drain_requests(&self) -> Vec<IncomingRequest> {
        let mut out = Vec::new();
        if let Ok(rx) = self.rx.lock() {
            while let Ok(item) = rx.try_recv() {
                out.push(item);
            }
        }
        out
    }

    fn refresh_record_if_changed(
        &mut self,
        title: &str,
        place_path: Option<&std::path::Path>,
        mode: &str,
    ) {
        let place_str = place_path.map(|p| p.display().to_string());
        if self.last_record_meta.0 == title
            && self.last_record_meta.1 == place_str
            && self.last_record_meta.2 == mode
        {
            return;
        }
        self.last_record_meta = (title.to_string(), place_str.clone(), mode.to_string());
        let rec = InstanceRecord {
            pid: self.pid,
            port: self.port,
            title: title.to_string(),
            place_path: place_str,
            mode: mode.to_string(),
            started_unix: self.started_unix,
        };
        let _ = write_instance_record(&rec);
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        remove_instance_record(self.pid);
    }
}

fn handle_client(
    stream: TcpStream,
    tx: Sender<IncomingRequest>,
    ctx: egui::Context,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(line) = line else {
            return;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: BridgeRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = BridgeResponse::Error {
                    code: -32700,
                    message: format!("invalid bridge request JSON: {e}"),
                };
                if write_response(&mut out, &resp).is_err() {
                    return;
                }
                continue;
            }
        };
        let (reply_tx, reply_rx) = channel();
        if tx.send(IncomingRequest { req, reply: reply_tx }).is_err() {
            let _ = write_response(
                &mut out,
                &BridgeResponse::Error {
                    code: -32000,
                    message: "RuStudio editor closed".into(),
                },
            );
            return;
        }
        ctx.request_repaint();

        let resp = match reply_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(r) => r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => BridgeResponse::Error {
                code: -32000,
                message: "request timed out waiting for RuStudio frame loop".into(),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => BridgeResponse::Error {
                code: -32000,
                message: "RuStudio editor dropped request".into(),
            },
        };
        if write_response(&mut out, &resp).is_err() {
            return;
        }
    }
}

fn write_response(out: &mut TcpStream, resp: &BridgeResponse) -> std::io::Result<()> {
    let line = serde_json::to_string(resp).map_err(std::io::Error::other)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

fn editor_mode_label(ed: &crate::app::Editor) -> &'static str {
    match ed.play.as_ref().map(|p| p.mode) {
        Some(crate::runtime::RunMode::Play) => "Play",
        Some(crate::runtime::RunMode::Run) => "Run",
        _ => "Edit",
    }
}

impl crate::app::Editor {
    /// Processes incoming OmniMCP TCP requests and advances any deferred captures/steps.
    /// Runs every frame (windowed, Start Page, `--no-ui`, `--headless`, `--no-render`).
    pub fn mcp_tick(&mut self, ctx: &egui::Context) {
        let Some((reqs, pid, port)) = self
            .mcp
            .as_ref()
            .map(|m| (m.drain_requests(), m.pid, m.port))
        else {
            return;
        };

        for IncomingRequest { req, reply } in reqs {
            match req {
                BridgeRequest::Ping => {
                    let _ = reply.send(BridgeResponse::Pong {
                        pid,
                        port,
                        title: self.doc.title.clone(),
                        mode: editor_mode_label(self).to_string(),
                    });
                }
                BridgeRequest::ListTools => {
                    let _ = reply.send(BridgeResponse::Tools {
                        tools: super::all_schemas(),
                    });
                }
                BridgeRequest::CallTool { name, arguments } => {
                    match super::dispatch_tool(self, ctx, &name, &arguments, &reply) {
                        CallOutcome::Immediate(result) => {
                            let _ = reply.send(BridgeResponse::ToolResult(result));
                        }
                        CallOutcome::Deferred => {}
                    }
                }
                BridgeRequest::ListResources => {
                    let _ = reply.send(BridgeResponse::Resources {
                        resources: super::resources_prompts::resources(),
                    });
                }
                BridgeRequest::ReadResource { uri } => {
                    let resp = match super::resources_prompts::read_resource(self, &uri) {
                        Ok(contents) => BridgeResponse::ResourceResult { contents },
                        Err(message) => BridgeResponse::Error { code: -32602, message },
                    };
                    let _ = reply.send(resp);
                }
                BridgeRequest::ListPrompts => {
                    let _ = reply.send(BridgeResponse::Prompts {
                        prompts: super::resources_prompts::prompts(),
                    });
                }
                BridgeRequest::GetPrompt { name, arguments } => {
                    let resp = match super::resources_prompts::get_prompt(self, &name, &arguments) {
                        Ok((description, messages)) => BridgeResponse::PromptResult {
                            description,
                            messages,
                        },
                        Err(message) => BridgeResponse::Error { code: -32602, message },
                    };
                    let _ = reply.send(resp);
                }
            }
        }

        self.tick_mcp_steps(ctx);
        self.tick_mcp_capture(ctx);

        let title = self.doc.title.clone();
        let path = self.doc.path.clone();
        let mode = editor_mode_label(self);
        if let Some(mcp) = self.mcp.as_mut() {
            mcp.refresh_record_if_changed(&title, path.as_deref(), mode);
        }
    }

    fn tick_mcp_steps(&mut self, ctx: &egui::Context) {
        let Some(mut steps) = self.mcp.as_mut().and_then(|m| m.state.pending_steps.take()) else {
            return;
        };
        steps.frames_stepped += 1;
        if let Some(rem) = steps.remaining_frames.as_mut() {
            *rem = rem.saturating_sub(1);
        }
        let done_frames = steps.remaining_frames.is_some_and(|r| r == 0);
        let done_time = steps
            .target_elapsed_secs
            .is_some_and(|s| steps.started_at.elapsed().as_secs_f64() >= s);

        if done_frames || done_time {
            if let Some(saved_dt) = steps.saved_fixed_dt.take() {
                self.fixed_dt = saved_dt;
            }
            let play_time = self.play.as_ref().map(|p| p.time).unwrap_or(0.0);
            let new_logs: Vec<serde_json::Value> = self
                .log
                .iter()
                .skip(steps.log_start_index)
                .map(|l| {
                    serde_json::json!({
                        "level": format!("{:?}", l.level),
                        "text": l.text,
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "frames_stepped": steps.frames_stepped,
                "mode": editor_mode_label(self),
                "play_time": play_time,
                "delta_play_time": play_time - steps.start_play_time,
                "logs": new_logs,
            });
            let _ = steps
                .reply
                .send(BridgeResponse::ToolResult(ToolCallResult::json(&payload)));
        } else {
            ctx.request_repaint();
            if let Some(mcp) = self.mcp.as_mut() {
                mcp.state.pending_steps = Some(steps);
            }
        }
    }

    fn tick_mcp_capture(&mut self, ctx: &egui::Context) {
        let Some(mut cap) = self.mcp.as_mut().and_then(|m| m.state.pending_capture.take()) else {
            return;
        };

        if cap.asked_at.elapsed() > CAPTURE_TIMEOUT {
            if let Some(saved) = cap.saved_clean_view.take() {
                self.clean_view = saved;
            }
            let _ = cap.reply.send(BridgeResponse::ToolResult(ToolCallResult::error(
                "screenshot capture timed out",
            )));
            return;
        }

        if cap.settle_frames > 0 {
            cap.settle_frames -= 1;
            ctx.request_repaint();
            if let Some(mcp) = self.mcp.as_mut() {
                mcp.state.pending_capture = Some(cap);
            }
            return;
        }

        if self.harness.no_render {
            if let Some(saved) = cap.saved_clean_view.take() {
                self.clean_view = saved;
            }
            let _ = cap.reply.send(BridgeResponse::ToolResult(ToolCallResult::error(
                "viewport capture unavailable under --no-render",
            )));
            return;
        }

        // Fast direct GPU readback for 3D viewport when available.
        if cap.target == CaptureTarget::Viewport && !cap.screenshot_requested {
            if let Some((w, h, rgba)) = self
                .renderer
                .as_ref()
                .zip(self.render_state.as_ref())
                .and_then(|(r, rs)| r.read_frame(rs))
            {
                if let Some(saved) = cap.saved_clean_view.take() {
                    self.clean_view = saved;
                }
                let result = finish_rgba_capture(w, h, rgba, &cap);
                let _ = cap.reply.send(BridgeResponse::ToolResult(result));
                return;
            }
        }

        if !cap.screenshot_requested {
            cap.screenshot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            ctx.request_repaint();
            if let Some(mcp) = self.mcp.as_mut() {
                mcp.state.pending_capture = Some(cap);
            }
            return;
        }

        if let Some(image) = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        }) {
            if let Some(saved) = cap.saved_clean_view.take() {
                self.clean_view = saved;
            }
            let (w, h, rgba) = match cap.target {
                CaptureTarget::Window => {
                    let [w, h] = image.size;
                    let mut rgba = Vec::with_capacity(w * h * 4);
                    for p in &image.pixels {
                        rgba.extend_from_slice(&[p.r(), p.g(), p.b(), 255]);
                    }
                    (w as u32, h as u32, rgba)
                }
                CaptureTarget::Viewport => {
                    let ppp = ctx.pixels_per_point();
                    let (x0, y0, w, h) =
                        crate::app::view_pixels(self.viewport_rect, ppp, image.size);
                    let mut rgba = Vec::with_capacity(w * h * 4);
                    for y in 0..h {
                        for x in 0..w {
                            let p = image.pixels[(y0 + y) * image.size[0] + x0 + x];
                            rgba.extend_from_slice(&[p.r(), p.g(), p.b(), 255]);
                        }
                    }
                    (w as u32, h as u32, rgba)
                }
            };
            let result = finish_rgba_capture(w, h, rgba, &cap);
            let _ = cap.reply.send(BridgeResponse::ToolResult(result));
            return;
        }

        ctx.request_repaint();
        if let Some(mcp) = self.mcp.as_mut() {
            mcp.state.pending_capture = Some(cap);
        }
    }
}

fn finish_rgba_capture(w: u32, h: u32, rgba: Vec<u8>, cap: &PendingCapture) -> ToolCallResult {
    if w == 0 || h == 0 || rgba.is_empty() {
        return ToolCallResult::error("captured image is empty (0x0)");
    }
    let (out_w, out_h, out_rgba) = if let Some(max_w) = cap.max_width.filter(|&mw| mw > 0 && w > mw) {
        let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else {
            return ToolCallResult::error("invalid captured RGBA buffer");
        };
        let new_h = ((h as u64 * max_w as u64) / w as u64).max(1) as u32;
        let resized =
            image::imageops::resize(&img, max_w, new_h, image::imageops::FilterType::Triangle);
        (max_w, new_h, resized.into_raw())
    } else {
        (w, h, rgba)
    };

    let mut png_bytes = Vec::new();
    let enc = image::codecs::png::PngEncoder::new(&mut png_bytes);
    if let Err(e) = enc.write_image(&out_rgba, out_w, out_h, image::ExtendedColorType::Rgba8) {
        return ToolCallResult::error(format!("PNG encode failed: {e}"));
    }

    if let Some(path) = &cap.save_path {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return ToolCallResult::error(format!("create_dir_all {}: {e}", parent.display()));
                }
            }
        }
        if let Err(e) = std::fs::write(path, &png_bytes) {
            return ToolCallResult::error(format!("write {}: {e}", path.display()));
        }
    }

    let meta = serde_json::json!({
        "width": out_w,
        "height": out_h,
        "bytes": png_bytes.len(),
        "saved_path": cap.save_path.as_ref().map(|p| p.display().to_string()),
    })
    .to_string();

    if cap.return_image {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        ToolCallResult::image_png(b64, Some(meta))
    } else {
        ToolCallResult::text(meta)
    }
}
