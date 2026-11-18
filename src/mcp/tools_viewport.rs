//! Viewport camera, live GPU/window capture, and lighting/atmosphere MCP tools.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Instant;

use glam::Vec3;
use rbx_dom_weak::types::{Ref, Variant, VariantType};
use rbx_dom_weak::{InstanceBuilder, Ustr};
use serde_json::{json, Map, Value};

use crate::camera::{Camera, CameraMode};
use crate::doc::Document;
use crate::mcp::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use crate::mcp::server::{CallOutcome, CaptureTarget, PendingCapture};
use crate::mcp::variants::{
    instance_path, json_to_variant, ref_to_str, resolve_datamodel, resolve_datamodel_mut,
    resolve_target, variant_to_json_with_doc,
};
use crate::scene::Scene;

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

pub fn schemas() -> Vec<ToolDef> {
    vec![
        tool(
            "viewport_camera",
            "Get or set the 3D editor viewport camera (position, look_at, yaw/pitch degrees, FOV, Orbit/Fly mode) or frame/focus any target instance, Model, selection, or the entire workspace.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["edit", "play"], "description": "DataModel used when resolving focus_target (defaults to active)." },
                    "pos": { "type": ["array", "object"], "description": "Camera world position [x, y, z] or {x, y, z}." },
                    "look_at": { "type": ["array", "object", "string"], "description": "Target world point [x, y, z], {x, y, z}, or instance path/ref to orient the camera toward." },
                    "focus": { "type": ["array", "object"], "description": "Explicit orbit focus point [x, y, z] without changing orientation." },
                    "yaw_deg": { "type": "number", "description": "Camera yaw angle in degrees (0 looks along -Z)." },
                    "pitch_deg": { "type": "number", "description": "Camera pitch angle in degrees (clamped to [-89.5, 89.5])." },
                    "fov_deg": { "type": "number", "description": "Vertical field of view in degrees (clamped to [1, 120])." },
                    "mode": { "type": "string", "enum": ["Fly", "Orbit"], "description": "Camera navigation mode." },
                    "focus_target": {
                        "description": "Frame an instance/Model (ref or path), array of targets, 'selection', 'all', or true (frames current selection).",
                        "type": ["string", "array", "object", "boolean"]
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "viewport_capture",
            "Capture a live screenshot of the 3D viewport (GPU framebuffer fast path) or full Studio window. Returns base64 image/png and optionally saves to disk.",
            json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["viewport", "window"], "default": "viewport", "description": "Capture only the 3D viewport render or the full application window." },
                    "camera": {
                        "type": "object",
                        "description": "Optional camera configuration applied right before capturing (same fields as viewport_camera)."
                    },
                    "clean_view": { "type": "boolean", "description": "Hide editor gizmos/UI overlays cleanly during capture (restored automatically afterwards)." },
                    "settle_frames": { "type": "integer", "minimum": 0, "maximum": 60, "description": "Frames to wait before reading pixels (automatically >= 1 when camera or clean_view changes)." },
                    "save_path": { "type": "string", "description": "Optional PNG file path to save the captured image." },
                    "max_width": { "type": "integer", "minimum": 16, "maximum": 8192, "description": "Downscale image proportionally so its width does not exceed max_width." },
                    "return_image": { "type": "boolean", "default": true, "description": "Whether to return an inline MCP image/png content block." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "lighting_and_atmosphere",
            "Inspect or configure Lighting properties, Sky, Atmosphere, Workspace.Terrain.Clouds, post-processing effects (Bloom, Blur, ColorCorrection, DepthOfField, SunRays), or apply complete lighting presets.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["edit", "play"] },
                    "preset": {
                        "type": "string",
                        "enum": ["daylight", "golden_hour", "cyberpunk_night", "foggy_horror", "studio_neutral"],
                        "description": "Apply a curated environment preset before any per-object overrides."
                    },
                    "lighting": { "type": "object", "description": "Map of Lighting properties to set (e.g. Brightness, ClockTime, TimeOfDay, GeographicLatitude, Ambient, OutdoorAmbient, ColorShift_Top, ColorShift_Bottom, GlobalShadows, FogStart, FogEnd, FogColor, ExposureCompensation, EnvironmentDiffuseScale, EnvironmentSpecularScale)." },
                    "sky": { "type": ["object", "null"], "description": "Properties for Lighting.Sky (creates one if missing; pass null to remove Sky)." },
                    "atmosphere": { "type": ["object", "null"], "description": "Properties for Lighting.Atmosphere (creates one if missing; pass null to remove Atmosphere)." },
                    "clouds": { "type": ["object", "null"], "description": "Properties for Workspace.Terrain.Clouds (Enabled, Cover, Density, Color; pass null to remove Clouds)." },
                    "effects": {
                        "type": "object",
                        "description": "Map of effect class names (BloomEffect, BlurEffect, ColorCorrectionEffect, DepthOfFieldEffect, SunRaysEffect, ColorGradingEffect) to property objects (creates/updates child under Lighting, or null to delete)."
                    }
                },
                "additionalProperties": false
            }),
        ),
    ]
}

pub fn call(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    name: &str,
    args: &Value,
    reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    match name {
        "viewport_camera" => Some(CallOutcome::Immediate(handle_camera(ed, ctx, args))),
        "viewport_capture" => Some(handle_capture(ed, ctx, args, reply)),
        "lighting_and_atmosphere" => Some(CallOutcome::Immediate(handle_lighting(ed, ctx, args))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Camera helpers & tool
// ---------------------------------------------------------------------------

fn dm_label(ed: &crate::app::Editor, doc: &Document) -> &'static str {
    if std::ptr::eq(doc, &ed.doc) {
        "edit"
    } else {
        "play"
    }
}

fn inst_name(doc: &Document, r: Ref) -> &str {
    doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or("")
}

fn handle_camera(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let dm_arg = args.get("datamodel").and_then(Value::as_str);
    let doc = match resolve_datamodel(ed, dm_arg) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };
    let dm_name = dm_label(ed, doc);
    let mut cam = ed.cam.clone();
    let framed_bounds = match apply_camera_params(doc, &ed.scene, &mut cam, args) {
        Ok(b) => b,
        Err(e) => return ToolCallResult::error(e),
    };
    let mutated = args
        .as_object()
        .is_some_and(|m| m.keys().any(|k| k != "datamodel"));
    if mutated {
        ed.cam = cam;
        ctx.request_repaint();
    }
    let mut out = camera_json(&ed.cam, ed.viewport_rect);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("datamodel".into(), json!(dm_name));
        if let Some((lo, hi)) = framed_bounds {
            obj.insert(
                "framed_bounds".into(),
                json!({
                    "min": [lo.x, lo.y, lo.z],
                    "max": [hi.x, hi.y, hi.z],
                    "center": [
                        (lo.x + hi.x) * 0.5,
                        (lo.y + hi.y) * 0.5,
                        (lo.z + hi.z) * 0.5
                    ]
                }),
            );
        }
    }
    ToolCallResult::json(&out)
}

fn handle_capture(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
    reply: &Sender<BridgeResponse>,
) -> CallOutcome {
    if ed
        .mcp
        .as_ref()
        .is_some_and(|m| m.state.pending_capture.is_some())
    {
        return CallOutcome::Immediate(ToolCallResult::error(
            "a viewport capture is already in progress",
        ));
    }

    let target = match args.get("target").and_then(Value::as_str) {
        None | Some("viewport") => CaptureTarget::Viewport,
        Some("window") => CaptureTarget::Window,
        Some(other) => {
            return CallOutcome::Immediate(ToolCallResult::error(format!(
                "invalid capture target '{other}', expected 'viewport' or 'window'"
            )));
        }
    };

    let mut cam_changed = false;
    if let Some(cam_val) = args.get("camera").filter(|v| !v.is_null()) {
        let doc = match resolve_datamodel(ed, args.get("datamodel").and_then(Value::as_str)) {
            Ok(v) => v,
            Err(e) => return CallOutcome::Immediate(ToolCallResult::error(e)),
        };
        let mut next_cam = ed.cam.clone();
        if let Err(e) = apply_camera_params(doc, &ed.scene, &mut next_cam, cam_val) {
            return CallOutcome::Immediate(ToolCallResult::error(format!(
                "invalid camera override: {e}"
            )));
        }
        ed.cam = next_cam;
        cam_changed = true;
    }

    let (saved_clean_view, clean_changed) =
        match args.get("clean_view").and_then(Value::as_bool) {
            Some(cv) => {
                let prev = ed.clean_view;
                ed.clean_view = cv;
                (Some(prev), prev != cv)
            }
            None => (None, false),
        };

    let save_path = match args.get("save_path").and_then(Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => {
            let p = PathBuf::from(raw);
            if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    if let Some(prev) = saved_clean_view {
                        ed.clean_view = prev;
                    }
                    return CallOutcome::Immediate(ToolCallResult::error(format!(
                        "cannot create parent directory for '{}': {e}",
                        p.display()
                    )));
                }
            }
            Some(p)
        }
        _ => None,
    };

    let return_image = args
        .get("return_image")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_width = args
        .get("max_width")
        .and_then(Value::as_u64)
        .map(|v| (v as u32).max(16));
    let requested_settle = args
        .get("settle_frames")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(60) as u32;
    // `mcp_tick` executes at the end of the frame after the 3D scene has drawn,
    // so changing camera or clean_view needs at least 1 additional frame before readback.
    let settle_frames = if cam_changed || clean_changed {
        requested_settle.max(1)
    } else {
        requested_settle
    };

    let Some(mcp) = ed.mcp.as_mut() else {
        if let Some(prev) = saved_clean_view {
            ed.clean_view = prev;
        }
        return CallOutcome::Immediate(ToolCallResult::error("MCP bridge is not running"));
    };

    mcp.state.pending_capture = Some(PendingCapture {
        target,
        save_path,
        return_image,
        max_width,
        saved_clean_view,
        asked_at: Instant::now(),
        settle_frames,
        screenshot_requested: false,
        reply: reply.clone(),
    });
    ctx.request_repaint();
    CallOutcome::Deferred
}

fn apply_camera_params(
    doc: &Document,
    scene: &Scene,
    cam: &mut Camera,
    params: &Value,
) -> Result<Option<(Vec3, Vec3)>, String> {
    let Some(obj) = params.as_object() else {
        if params.is_null() {
            return Ok(None);
        }
        return Err("camera parameters must be a JSON object".into());
    };

    if let Some(fov) = obj.get("fov_deg").and_then(Value::as_f64) {
        if !fov.is_finite() {
            return Err("fov_deg must be finite".into());
        }
        cam.fov_deg = (fov as f32).clamp(1.0, 120.0);
    }

    if let Some(mode) = obj.get("mode").and_then(Value::as_str) {
        cam.mode = match mode.to_ascii_lowercase().as_str() {
            "fly" => CameraMode::Fly,
            "orbit" => CameraMode::Orbit,
            _ => return Err(format!("unknown camera mode '{mode}', expected Fly or Orbit")),
        };
    }

    let mut orient_touched = false;
    if let Some(yaw) = obj.get("yaw_deg").and_then(Value::as_f64) {
        if !yaw.is_finite() {
            return Err("yaw_deg must be finite".into());
        }
        cam.yaw = (yaw as f32).to_radians();
        orient_touched = true;
    }
    if let Some(pitch) = obj.get("pitch_deg").and_then(Value::as_f64) {
        if !pitch.is_finite() {
            return Err("pitch_deg must be finite".into());
        }
        cam.pitch = (pitch as f32).clamp(-89.5, 89.5).to_radians();
        orient_touched = true;
    }

    let mut framed_bounds = None;
    if let Some(ft) = obj.get("focus_target").filter(|v| !v.is_null()) {
        if !matches!(ft, Value::Bool(false)) {
            let (lo, hi) = resolve_focus_bounds(doc, scene, ft)?;
            let center = (lo + hi) * 0.5;
            let radius = ((hi - lo) * 0.5).length().max(0.5);
            if orient_touched {
                cam.basis = None;
            }
            cam.frame(center, radius);
            cam.view_transition = None;
            framed_bounds = Some((lo, hi));
        }
    } else if orient_touched {
        cam.basis = None;
        cam.view_transition = None;
    }

    if let Some(pos_v) = obj.get("pos") {
        cam.pos = parse_vec3(pos_v, "pos")?;
        cam.view_transition = None;
    }

    if let Some(focus_v) = obj.get("focus") {
        if focus_v.is_null() {
            cam.focus = cam.pos + cam.forward() * 12.0;
        } else {
            cam.focus = parse_vec3(focus_v, "focus")?;
        }
    }

    if let Some(look_v) = obj.get("look_at") {
        let target = resolve_point_or_target(doc, scene, look_v, "look_at")?;
        let dir = (target - cam.pos).normalize_or_zero();
        if dir.length_squared() > 1e-8 {
            cam.pitch = dir.y.clamp(-1.0, 1.0).asin();
            cam.yaw = (-dir.x).atan2(-dir.z);
            cam.basis = None;
            cam.view_transition = None;
        }
        cam.focus = target;
    }

    Ok(framed_bounds)
}

fn parse_vec3(v: &Value, label: &str) -> Result<Vec3, String> {
    if let Some(arr) = v.as_array() {
        if arr.len() == 3 {
            let x = arr[0]
                .as_f64()
                .ok_or_else(|| format!("{label}[0] must be a number"))? as f32;
            let y = arr[1]
                .as_f64()
                .ok_or_else(|| format!("{label}[1] must be a number"))? as f32;
            let z = arr[2]
                .as_f64()
                .ok_or_else(|| format!("{label}[2] must be a number"))? as f32;
            let p = Vec3::new(x, y, z);
            if !p.is_finite() {
                return Err(format!("{label} coordinates must be finite"));
            }
            return Ok(p);
        }
    }
    if let Some(obj) = v.as_object() {
        let x = obj
            .get("x")
            .or_else(|| obj.get("X"))
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{label}.x must be a number"))? as f32;
        let y = obj
            .get("y")
            .or_else(|| obj.get("Y"))
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{label}.y must be a number"))? as f32;
        let z = obj
            .get("z")
            .or_else(|| obj.get("Z"))
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{label}.z must be a number"))? as f32;
        let p = Vec3::new(x, y, z);
        if !p.is_finite() {
            return Err(format!("{label} coordinates must be finite"));
        }
        return Ok(p);
    }
    Err(format!("{label} must be [x, y, z] or {{x, y, z}}"))
}

fn resolve_point_or_target(
    doc: &Document,
    scene: &Scene,
    v: &Value,
    label: &str,
) -> Result<Vec3, String> {
    if v.is_array() || v.as_object().is_some_and(|m| m.contains_key("x") || m.contains_key("X")) {
        return parse_vec3(v, label);
    }
    let r = resolve_target(doc, v)?;
    let (lo, hi) = target_bounds(doc, scene, &[r])
        .ok_or_else(|| format!("target '{}' has no spatial position", instance_path(doc, r)))?;
    Ok((lo + hi) * 0.5)
}

fn resolve_focus_bounds(doc: &Document, scene: &Scene, ft: &Value) -> Result<(Vec3, Vec3), String> {
    let refs: Vec<Ref> = match ft {
        Value::Bool(true) => doc.selection.clone(),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("selection") || trimmed.eq_ignore_ascii_case("selected")
            {
                doc.selection.clone()
            } else if trimmed.eq_ignore_ascii_case("all") {
                let Some(ws) = doc.find_child(doc.dom.root_ref(), "Workspace") else {
                    return Err("Workspace not found".into());
                };
                vec![ws]
            } else {
                vec![resolve_target(doc, ft)?]
            }
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(resolve_target(doc, item)?);
            }
            out
        }
        Value::Object(_) => vec![resolve_target(doc, ft)?],
        _ => return Err("focus_target must be a target ref/path, array, 'selection', 'all', or true".into()),
    };

    if refs.is_empty() {
        return Err("nothing selected to focus".into());
    }
    target_bounds(doc, scene, &refs).ok_or_else(|| {
        "focus_target contains no BasePart or positioned instances to frame".into()
    })
}

fn target_bounds(doc: &Document, scene: &Scene, refs: &[Ref]) -> Option<(Vec3, Vec3)> {
    if let Some((lo, hi)) = crate::toolbox::insert::aabb(&doc.dom, refs) {
        if (hi - lo).length_squared() >= 0.0 && lo.is_finite() && hi.is_finite() {
            return Some((lo, hi));
        }
    }
    // Fallback to scene AABBs or single CFrame/Position on non-part targets (Model/Camera/Attachment).
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    let mut stack: Vec<Ref> = refs.to_vec();
    let mut seen = HashSet::new();
    let mut found = false;

    while let Some(r) = stack.pop() {
        if !seen.insert(r) {
            continue;
        }
        let Some(inst) = doc.dom.get_by_ref(r) else {
            continue;
        };
        if let Some(sp) = scene.get(r) {
            let (p_lo, p_hi) = crate::viewport::part_aabb(sp);
            lo = lo.min(p_lo);
            hi = hi.max(p_hi);
            found = true;
        }
        for &child in inst.children() {
            stack.push(child);
        }
    }
    if found {
        return Some((lo, hi));
    }

    for &r in refs {
        if let Some(pos) = instance_world_pos(doc, r) {
            let half = Vec3::splat(2.0);
            lo = lo.min(pos - half);
            hi = hi.max(pos + half);
            found = true;
        }
    }
    found.then_some((lo, hi))
}

fn instance_world_pos(doc: &Document, r: Ref) -> Option<Vec3> {
    for prop in ["CFrame", "WorldPivot", "WorldCFrame", "Position"] {
        match doc.prop(r, prop) {
            Some(Variant::CFrame(cf)) => return Some(Vec3::new(cf.position.x, cf.position.y, cf.position.z)),
            Some(Variant::Vector3(v)) => return Some(Vec3::new(v.x, v.y, v.z)),
            _ => {}
        }
    }
    None
}

fn camera_json(cam: &Camera, viewport_rect: egui::Rect) -> Value {
    let fwd = cam.forward();
    let right = cam.right();
    let up = cam.up();
    let focus = cam.focus;
    let mut obj = Map::new();
    obj.insert("pos".into(), json!([cam.pos.x, cam.pos.y, cam.pos.z]));
    obj.insert("focus".into(), json!([focus.x, focus.y, focus.z]));
    obj.insert("forward".into(), json!([fwd.x, fwd.y, fwd.z]));
    obj.insert("right".into(), json!([right.x, right.y, right.z]));
    obj.insert("up".into(), json!([up.x, up.y, up.z]));
    obj.insert("yaw_deg".into(), json!(cam.yaw.to_degrees()));
    obj.insert("pitch_deg".into(), json!(cam.pitch.to_degrees()));
    obj.insert("fov_deg".into(), json!(cam.fov_deg));
    obj.insert(
        "mode".into(),
        json!(match cam.mode {
            CameraMode::Fly => "Fly",
            CameraMode::Orbit => "Orbit",
        }),
    );
    obj.insert("focus_distance".into(), json!((focus - cam.pos).length()));
    if viewport_rect != egui::Rect::NOTHING {
        obj.insert(
            "viewport_size".into(),
            json!([viewport_rect.width(), viewport_rect.height()]),
        );
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Lighting & atmosphere
// ---------------------------------------------------------------------------

const LIGHTING_PROPS: &[&str] = &[
    "Brightness",
    "ClockTime",
    "TimeOfDay",
    "GeographicLatitude",
    "Ambient",
    "OutdoorAmbient",
    "ColorShift_Top",
    "ColorShift_Bottom",
    "GlobalShadows",
    "FogStart",
    "FogEnd",
    "FogColor",
    "ExposureCompensation",
    "EnvironmentDiffuseScale",
    "EnvironmentSpecularScale",
    "ShadowSoftness",
    "Technology",
    "LightingStyle",
];

const SKY_PROPS: &[&str] = &[
    "SkyboxBk",
    "SkyboxDn",
    "SkyboxFt",
    "SkyboxLf",
    "SkyboxRt",
    "SkyboxUp",
    "SkyboxOrientation",
    "SunTextureId",
    "MoonTextureId",
    "SunAngularSize",
    "MoonAngularSize",
    "StarCount",
    "CelestialBodiesShown",
];

const ATMOS_PROPS: &[&str] = &["Density", "Offset", "Color", "Decay", "Glare", "Haze"];

const CLOUDS_PROPS: &[&str] = &["Enabled", "Cover", "Density", "Color"];

const EFFECT_CLASSES: &[&str] = &[
    "BloomEffect",
    "BlurEffect",
    "ColorCorrectionEffect",
    "DepthOfFieldEffect",
    "SunRaysEffect",
    "ColorGradingEffect",
];

fn effect_default_props(class: &str) -> &'static [&'static str] {
    match class {
        "BloomEffect" => &["Enabled", "Intensity", "Size", "Threshold"],
        "BlurEffect" => &["Enabled", "Size"],
        "ColorCorrectionEffect" => &["Enabled", "Brightness", "Contrast", "Saturation", "TintColor"],
        "DepthOfFieldEffect" => &[
            "Enabled",
            "FarIntensity",
            "FocusDistance",
            "InFocusRadius",
            "NearIntensity",
        ],
        "SunRaysEffect" => &["Enabled", "Intensity", "Spread"],
        "ColorGradingEffect" => &["Enabled", "TonemapperPreset"],
        _ => &["Enabled"],
    }
}

fn handle_lighting(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let dm_arg = args.get("datamodel").and_then(Value::as_str);
    let has_writes = args.as_object().is_some_and(|m| {
        m.keys()
            .any(|k| matches!(k.as_str(), "preset" | "lighting" | "sky" | "atmosphere" | "clouds" | "effects"))
    });

    if !has_writes {
        let doc = match resolve_datamodel(ed, dm_arg) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        let dm_name = dm_label(ed, doc);
        return ToolCallResult::json(&read_lighting_state(doc, dm_name));
    }

    let dm_name = match dm_arg {
        Some("play") | Some("server") => "play",
        Some("edit") => "edit",
        _ => {
            if ed.play.is_some() {
                "play"
            } else {
                "edit"
            }
        }
    };
    let doc = match resolve_datamodel_mut(ed, dm_arg) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };

    let mut applied_preset = None;
    let mut overlay = args.clone();
    if let Some(preset_name) = args.get("preset").and_then(Value::as_str) {
        let preset = match build_preset(preset_name) {
            Some(p) => p,
            None => {
                return ToolCallResult::error(format!(
                    "unknown lighting preset '{preset_name}', expected daylight, golden_hour, cyberpunk_night, foggy_horror, or studio_neutral"
                ));
            }
        };
        applied_preset = Some(preset_name.to_string());
        overlay = merge_preset(preset, args);
    }

    if let Err(e) = apply_lighting_patch(doc, &overlay) {
        return ToolCallResult::error(e);
    }
    ctx.request_repaint();

    let mut out = read_lighting_state(doc, dm_name);
    if let Some(preset) = applied_preset {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("preset_applied".into(), json!(preset));
        }
    }
    ToolCallResult::json(&out)
}

fn ensure_service(doc: &mut Document, class: &str) -> Ref {
    let root = doc.dom.root_ref();
    if let Some(r) = doc
        .dom
        .get_by_ref(root)
        .and_then(|inst| {
            inst.children()
                .iter()
                .copied()
                .find(|&c| doc.class_of(c).is_some_and(|cn| cn == class) || inst_name(doc, c) == class)
        })
    {
        return r;
    }
    doc.insert(root, InstanceBuilder::new(class).with_name(class))
}

fn ensure_terrain(doc: &mut Document) -> Ref {
    let ws = ensure_service(doc, "Workspace");
    if let Some(t) = doc.dom.get_by_ref(ws).and_then(|inst| {
        inst.children()
            .iter()
            .copied()
            .find(|&c| doc.class_of(c).is_some_and(|cn| cn == "Terrain"))
    }) {
        return t;
    }
    doc.insert(ws, InstanceBuilder::new("Terrain").with_name("Terrain"))
}

fn find_child_by_class(doc: &Document, parent: Ref, class: &str) -> Option<Ref> {
    doc.dom.get_by_ref(parent)?.children().iter().copied().find(|&c| {
        doc.class_of(c).is_some_and(|cn| cn == class)
    })
}

fn apply_lighting_patch(doc: &mut Document, patch: &Value) -> Result<(), String> {
    let lighting = ensure_service(doc, "Lighting");
    let mut prop_edits: Vec<(Ref, Ustr, Variant)> = Vec::new();
    let mut renames: Vec<(Ref, String)> = Vec::new();
    let mut delete_refs: Vec<Ref> = Vec::new();

    if let Some(l_obj) = patch.get("lighting").and_then(Value::as_object) {
        collect_object_prop_edits(doc, lighting, "Lighting", l_obj, &mut prop_edits, &mut renames)?;
        // Keep ClockTime and TimeOfDay coherent when one is edited directly.
        let has_clock = l_obj.contains_key("ClockTime");
        let has_tod = l_obj.contains_key("TimeOfDay");
        if has_clock && !has_tod {
            if let Some(clock) = l_obj.get("ClockTime").and_then(Value::as_f64) {
                prop_edits.push((
                    lighting,
                    Ustr::from("TimeOfDay"),
                    Variant::String(clock_to_time_of_day(clock as f32)),
                ));
            }
        } else if has_tod && !has_clock {
            if let Some(tod) = l_obj.get("TimeOfDay").and_then(Value::as_str) {
                if let Some(clock) = parse_time_of_day(tod) {
                    prop_edits.push((lighting, Ustr::from("ClockTime"), Variant::Float32(clock)));
                }
            }
        }
    }

    if let Some(sky_v) = patch.get("sky") {
        apply_singleton_child(
            doc,
            lighting,
            "Sky",
            sky_v,
            &mut prop_edits,
            &mut renames,
            &mut delete_refs,
        )?;
    }
    if let Some(at_v) = patch.get("atmosphere") {
        apply_singleton_child(
            doc,
            lighting,
            "Atmosphere",
            at_v,
            &mut prop_edits,
            &mut renames,
            &mut delete_refs,
        )?;
    }
    if let Some(clouds_v) = patch.get("clouds") {
        let terrain = ensure_terrain(doc);
        apply_singleton_child(
            doc,
            terrain,
            "Clouds",
            clouds_v,
            &mut prop_edits,
            &mut renames,
            &mut delete_refs,
        )?;
    }
    if let Some(effects_map) = patch.get("effects").and_then(Value::as_object) {
        for (class, cfg) in effects_map {
            apply_singleton_child(
                doc,
                lighting,
                class,
                cfg,
                &mut prop_edits,
                &mut renames,
                &mut delete_refs,
            )?;
        }
    }

    if !prop_edits.is_empty() {
        doc.set_props("MCP lighting and atmosphere", prop_edits, None);
    }
    for (r, name) in renames {
        doc.rename(r, &name);
    }
    if !delete_refs.is_empty() {
        doc.remove(&delete_refs);
    }
    Ok(())
}

fn apply_singleton_child(
    doc: &mut Document,
    parent: Ref,
    class: &str,
    cfg: &Value,
    prop_edits: &mut Vec<(Ref, Ustr, Variant)>,
    renames: &mut Vec<(Ref, String)>,
    delete_refs: &mut Vec<Ref>,
) -> Result<(), String> {
    if cfg.is_null() {
        if let Some(existing) = find_child_by_class(doc, parent, class) {
            delete_refs.push(existing);
        }
        return Ok(());
    }
    let Some(obj) = cfg.as_object() else {
        return Err(format!("{class} configuration must be an object or null"));
    };
    let target = match find_child_by_class(doc, parent, class) {
        Some(r) => r,
        None => doc.insert(parent, InstanceBuilder::new(class).with_name(class)),
    };
    collect_object_prop_edits(doc, target, class, obj, prop_edits, renames)
}

fn collect_object_prop_edits(
    doc: &Document,
    target: Ref,
    class: &str,
    obj: &Map<String, Value>,
    out: &mut Vec<(Ref, Ustr, Variant)>,
    renames: &mut Vec<(Ref, String)>,
) -> Result<(), String> {
    for (k, raw) in obj {
        if k == "Name" {
            let s = raw
                .as_str()
                .ok_or_else(|| format!("{class}.Name must be a string"))?;
            renames.push((target, s.to_string()));
            continue;
        }
        let normalized = normalize_lighting_value(class, k, raw);
        let var = json_to_variant(Some(doc), Some(class), Some(k), &normalized, None)?;
        out.push((target, Ustr::from(k.as_str()), var));
    }
    Ok(())
}

fn normalize_lighting_value(class: &str, prop: &str, v: &Value) -> Value {
    let expected = crate::mcp::variants::expected_variant_type(class, prop).map(|x| x.0);
    if matches!(expected, Some(VariantType::Color3)) {
        if let Some(arr) = v.as_array() {
            if arr.len() == 3 {
                let nums: Option<[f64; 3]> = (|| {
                    Some([arr[0].as_f64()?, arr[1].as_f64()?, arr[2].as_f64()?])
                })();
                if let Some([r, g, b]) = nums {
                    // Accept both normalized 0..1 and 0..255 RGB triplets transparently.
                    if r > 1.0 || g > 1.0 || b > 1.0 {
                        return json!([r / 255.0, g / 255.0, b / 255.0]);
                    }
                }
            }
        }
    }
    v.clone()
}

fn read_lighting_state(doc: &Document, dm_name: &str) -> Value {
    let root = doc.dom.root_ref();
    let lighting = doc.find_child(root, "Lighting");
    let lighting_json = match lighting {
        Some(l) => json!({
            "ref": ref_to_str(l),
            "path": instance_path(doc, l),
            "properties": read_props_json(doc, l, "Lighting", LIGHTING_PROPS)
        }),
        None => Value::Null,
    };

    let (clock, lat) = match lighting {
        Some(l) => {
            let clock = match doc.prop(l, "ClockTime") {
                Some(Variant::Float32(c)) => c,
                Some(Variant::Float64(c)) => c as f32,
                _ => match doc.prop(l, "TimeOfDay") {
                    Some(Variant::String(s)) => parse_time_of_day(&s).unwrap_or(14.0),
                    _ => 14.0,
                },
            };
            let lat = match doc.prop(l, "GeographicLatitude") {
                Some(Variant::Float32(g)) => g,
                Some(Variant::Float64(g)) => g as f32,
                _ => 41.733,
            };
            (clock, lat)
        }
        None => (14.0, 41.733),
    };
    let sun_dir = crate::lighting::sun_direction(clock, lat);

    let sky_json = lighting
        .and_then(|l| find_child_by_class(doc, l, "Sky"))
        .map(|s| {
            json!({
                "ref": ref_to_str(s),
                "name": inst_name(doc, s),
                "path": instance_path(doc, s),
                "properties": read_props_json(doc, s, "Sky", SKY_PROPS),
                "effective_faces": crate::lighting::sky_faces(doc),
            })
        })
        .unwrap_or(Value::Null);

    let atmos_json = lighting
        .and_then(|l| find_child_by_class(doc, l, "Atmosphere"))
        .map(|a| {
            json!({
                "ref": ref_to_str(a),
                "name": inst_name(doc, a),
                "path": instance_path(doc, a),
                "properties": read_props_json(doc, a, "Atmosphere", ATMOS_PROPS),
            })
        })
        .unwrap_or(Value::Null);

    let clouds_json = doc
        .find_child(root, "Workspace")
        .and_then(|ws| doc.find_child(ws, "Terrain"))
        .and_then(|t| find_child_by_class(doc, t, "Clouds"))
        .map(|c| {
            json!({
                "ref": ref_to_str(c),
                "name": inst_name(doc, c),
                "path": instance_path(doc, c),
                "properties": read_props_json(doc, c, "Clouds", CLOUDS_PROPS),
            })
        })
        .unwrap_or(Value::Null);

    let mut effects = Vec::new();
    if let Some(l) = lighting {
        if let Some(inst) = doc.dom.get_by_ref(l) {
            for &child in inst.children() {
                if let Some(cls) = doc.class_of(child) {
                    if EFFECT_CLASSES.contains(&cls.as_str()) {
                        effects.push(json!({
                            "ref": ref_to_str(child),
                            "name": inst_name(doc, child),
                            "class": cls.as_str(),
                            "path": instance_path(doc, child),
                            "properties": read_props_json(doc, child, cls.as_str(), effect_default_props(cls.as_str())),
                        }));
                    }
                }
            }
        }
    }

    json!({
        "datamodel": dm_name,
        "lighting": lighting_json,
        "sun_direction": [sun_dir.x, sun_dir.y, sun_dir.z],
        "sky": sky_json,
        "atmosphere": atmos_json,
        "clouds": clouds_json,
        "effects": effects,
    })
}

fn read_props_json(doc: &Document, r: Ref, class: &str, known: &[&str]) -> Map<String, Value> {
    let mut keys: Vec<String> = known.iter().map(|s| (*s).to_string()).collect();
    if let Some(inst) = doc.dom.get_by_ref(r) {
        let mut extra: Vec<String> = inst
            .properties
            .keys()
            .map(|k| k.to_string())
            .filter(|k| !keys.iter().any(|existing| existing == k))
            .collect();
        extra.sort();
        keys.extend(extra);
    }

    let mut map = Map::new();
    for k in keys {
        if let Some(v) = doc.prop(r, &k) {
            map.insert(
                k.clone(),
                variant_to_json_with_doc(doc, Some(class), Some(&k), &v),
            );
        }
    }
    map
}

fn clock_to_time_of_day(clock: f32) -> String {
    let c = clock.rem_euclid(24.0);
    let total_secs = (c * 3600.0).round() as u32 % 86400;
    let hh = total_secs / 3600;
    let mm = (total_secs % 3600) / 60;
    let ss = total_secs % 60;
    format!("{hh:02}:{mm:02}:{ss:02}")
}

fn parse_time_of_day(s: &str) -> Option<f32> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    let h: f32 = parts.first()?.parse().ok()?;
    let m: f32 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let sec: f32 = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    Some((h + m / 60.0 + sec / 3600.0).rem_euclid(24.0))
}

fn rgb(r: u8, g: u8, b: u8) -> Value {
    json!([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

fn build_preset(name: &str) -> Option<Value> {
    match name {
        "studio_neutral" => Some(json!({
            "lighting": {
                "Brightness": 2.0,
                "ClockTime": 14.0,
                "GeographicLatitude": 41.733,
                "Ambient": rgb(70, 70, 70),
                "OutdoorAmbient": rgb(128, 128, 128),
                "ColorShift_Top": rgb(0, 0, 0),
                "ColorShift_Bottom": rgb(0, 0, 0),
                "GlobalShadows": true,
                "FogStart": 0.0,
                "FogEnd": 100000.0,
                "FogColor": rgb(192, 192, 192),
                "ExposureCompensation": 0.0,
                "EnvironmentDiffuseScale": 1.0,
                "EnvironmentSpecularScale": 1.0
            },
            "atmosphere": {
                "Density": 0.3,
                "Offset": 0.25,
                "Color": rgb(199, 199, 199),
                "Decay": rgb(106, 112, 125),
                "Glare": 0.0,
                "Haze": 0.0
            },
            "effects": {
                "BloomEffect": { "Enabled": false },
                "BlurEffect": { "Enabled": false },
                "ColorCorrectionEffect": { "Enabled": false },
                "DepthOfFieldEffect": { "Enabled": false },
                "SunRaysEffect": { "Enabled": false }
            }
        })),
        "daylight" => Some(json!({
            "lighting": {
                "Brightness": 3.0,
                "ClockTime": 13.5,
                "GeographicLatitude": 35.0,
                "Ambient": rgb(64, 72, 84),
                "OutdoorAmbient": rgb(115, 130, 150),
                "ColorShift_Top": rgb(255, 250, 240),
                "ColorShift_Bottom": rgb(80, 96, 118),
                "GlobalShadows": true,
                "FogStart": 0.0,
                "FogEnd": 100000.0,
                "FogColor": rgb(192, 214, 232),
                "ExposureCompensation": 0.15,
                "EnvironmentDiffuseScale": 1.0,
                "EnvironmentSpecularScale": 1.0
            },
            "atmosphere": {
                "Density": 0.28,
                "Offset": 0.25,
                "Color": rgb(199, 224, 255),
                "Decay": rgb(106, 112, 125),
                "Glare": 0.2,
                "Haze": 1.0
            },
            "clouds": {
                "Enabled": true,
                "Cover": 0.45,
                "Density": 0.65,
                "Color": rgb(255, 255, 255)
            },
            "effects": {
                "BloomEffect": { "Enabled": true, "Intensity": 0.4, "Size": 24.0, "Threshold": 1.5 },
                "SunRaysEffect": { "Enabled": true, "Intensity": 0.08, "Spread": 0.4 },
                "ColorCorrectionEffect": { "Enabled": false },
                "BlurEffect": { "Enabled": false },
                "DepthOfFieldEffect": { "Enabled": false }
            }
        })),
        "golden_hour" => Some(json!({
            "lighting": {
                "Brightness": 2.6,
                "ClockTime": 17.75,
                "GeographicLatitude": 41.733,
                "Ambient": rgb(86, 66, 58),
                "OutdoorAmbient": rgb(146, 110, 88),
                "ColorShift_Top": rgb(255, 190, 120),
                "ColorShift_Bottom": rgb(90, 65, 80),
                "GlobalShadows": true,
                "FogStart": 0.0,
                "FogEnd": 100000.0,
                "FogColor": rgb(235, 178, 130),
                "ExposureCompensation": 0.2,
                "EnvironmentDiffuseScale": 1.0,
                "EnvironmentSpecularScale": 1.0
            },
            "atmosphere": {
                "Density": 0.36,
                "Offset": 0.35,
                "Color": rgb(255, 185, 130),
                "Decay": rgb(215, 115, 70),
                "Glare": 0.65,
                "Haze": 2.2
            },
            "clouds": {
                "Enabled": true,
                "Cover": 0.55,
                "Density": 0.7,
                "Color": rgb(255, 210, 165)
            },
            "effects": {
                "SunRaysEffect": { "Enabled": true, "Intensity": 0.18, "Spread": 0.7 },
                "BloomEffect": { "Enabled": true, "Intensity": 0.75, "Size": 30.0, "Threshold": 1.1 },
                "ColorCorrectionEffect": {
                    "Enabled": true,
                    "Brightness": 0.0,
                    "Contrast": 0.08,
                    "Saturation": 0.12,
                    "TintColor": rgb(255, 240, 224)
                },
                "BlurEffect": { "Enabled": false },
                "DepthOfFieldEffect": { "Enabled": false }
            }
        })),
        "cyberpunk_night" => Some(json!({
            "lighting": {
                "Brightness": 0.7,
                "ClockTime": 22.5,
                "GeographicLatitude": 41.733,
                "Ambient": rgb(22, 16, 48),
                "OutdoorAmbient": rgb(34, 28, 68),
                "ColorShift_Top": rgb(70, 35, 120),
                "ColorShift_Bottom": rgb(15, 45, 75),
                "GlobalShadows": true,
                "FogStart": 40.0,
                "FogEnd": 650.0,
                "FogColor": rgb(20, 12, 42),
                "ExposureCompensation": 0.1,
                "EnvironmentDiffuseScale": 0.6,
                "EnvironmentSpecularScale": 1.0
            },
            "atmosphere": {
                "Density": 0.42,
                "Offset": 0.1,
                "Color": rgb(60, 35, 110),
                "Decay": rgb(20, 45, 95),
                "Glare": 0.0,
                "Haze": 3.5
            },
            "clouds": {
                "Enabled": true,
                "Cover": 0.6,
                "Density": 0.8,
                "Color": rgb(85, 45, 135)
            },
            "effects": {
                "BloomEffect": { "Enabled": true, "Intensity": 1.35, "Size": 36.0, "Threshold": 0.9 },
                "ColorCorrectionEffect": {
                    "Enabled": true,
                    "Brightness": -0.02,
                    "Contrast": 0.2,
                    "Saturation": 0.25,
                    "TintColor": rgb(230, 220, 255)
                },
                "SunRaysEffect": { "Enabled": false },
                "BlurEffect": { "Enabled": false },
                "DepthOfFieldEffect": { "Enabled": false }
            }
        })),
        "foggy_horror" => Some(json!({
            "lighting": {
                "Brightness": 0.45,
                "ClockTime": 20.5,
                "GeographicLatitude": 41.733,
                "Ambient": rgb(18, 22, 24),
                "OutdoorAmbient": rgb(28, 34, 38),
                "ColorShift_Top": rgb(30, 36, 42),
                "ColorShift_Bottom": rgb(12, 14, 18),
                "GlobalShadows": true,
                "FogStart": 12.0,
                "FogEnd": 160.0,
                "FogColor": rgb(38, 44, 48),
                "ExposureCompensation": -0.15,
                "EnvironmentDiffuseScale": 0.4,
                "EnvironmentSpecularScale": 0.5
            },
            "atmosphere": {
                "Density": 0.65,
                "Offset": 0.0,
                "Color": rgb(48, 56, 62),
                "Decay": rgb(28, 34, 38),
                "Glare": 0.0,
                "Haze": 8.0
            },
            "clouds": {
                "Enabled": true,
                "Cover": 0.88,
                "Density": 0.95,
                "Color": rgb(60, 68, 72)
            },
            "effects": {
                "ColorCorrectionEffect": {
                    "Enabled": true,
                    "Brightness": -0.05,
                    "Contrast": 0.15,
                    "Saturation": -0.35,
                    "TintColor": rgb(215, 230, 225)
                },
                "BloomEffect": { "Enabled": true, "Intensity": 0.5, "Size": 24.0, "Threshold": 1.2 },
                "DepthOfFieldEffect": {
                    "Enabled": true,
                    "FarIntensity": 0.35,
                    "FocusDistance": 25.0,
                    "InFocusRadius": 15.0,
                    "NearIntensity": 0.0
                },
                "SunRaysEffect": { "Enabled": false },
                "BlurEffect": { "Enabled": false }
            }
        })),
        _ => None,
    }
}

fn merge_preset(mut base: Value, overlay: &Value) -> Value {
    let Some(base_obj) = base.as_object_mut() else {
        return overlay.clone();
    };
    let Some(ov_obj) = overlay.as_object() else {
        return base;
    };
    for key in ["lighting", "sky", "atmosphere", "clouds", "effects"] {
        if let Some(ov_val) = ov_obj.get(key) {
            if ov_val.is_null() {
                base_obj.insert(key.into(), Value::Null);
            } else if key == "effects" {
                let entry = base_obj
                    .entry(key.to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let (Some(bm), Some(om)) = (entry.as_object_mut(), ov_val.as_object()) {
                    for (eff, ev) in om {
                        if ev.is_null() {
                            bm.insert(eff.clone(), Value::Null);
                        } else if let Some(b_eff) = bm.get_mut(eff).and_then(Value::as_object_mut) {
                            if let Some(o_eff) = ev.as_object() {
                                for (pk, pv) in o_eff {
                                    b_eff.insert(pk.clone(), pv.clone());
                                }
                            }
                        } else {
                            bm.insert(eff.clone(), ev.clone());
                        }
                    }
                }
            } else if let (Some(bm), Some(om)) = (
                base_obj.get_mut(key).and_then(Value::as_object_mut),
                ov_val.as_object(),
            ) {
                for (pk, pv) in om {
                    bm.insert(pk.clone(), pv.clone());
                }
            } else {
                base_obj.insert(key.into(), ov_val.clone());
            }
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::WeakDom;

    fn sample_doc() -> Document {
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel")
                .with_child(
                    InstanceBuilder::new("Workspace").with_name("Workspace").with_child(
                        InstanceBuilder::new("Terrain").with_name("Terrain"),
                    ).with_child(
                        InstanceBuilder::new("Part")
                            .with_name("BoxPart")
                            .with_property("Size", rbx_dom_weak::types::Vector3::new(10.0, 4.0, 6.0))
                            .with_property(
                                "CFrame",
                                rbx_dom_weak::types::CFrame::new(
                                    rbx_dom_weak::types::Vector3::new(20.0, 2.0, -10.0),
                                    rbx_dom_weak::types::Matrix3::identity(),
                                ),
                            ),
                    ),
                )
                .with_child(InstanceBuilder::new("Lighting").with_name("Lighting")),
        );
        Document::from_play_dom(dom, "viewport_test".into())
    }

    #[test]
    fn camera_look_at_and_focus_target() {
        let doc = sample_doc();
        let scene = Scene::default();
        let mut cam = Camera::default();

        apply_camera_params(
            &doc,
            &scene,
            &mut cam,
            &json!({ "pos": [0.0, 0.0, 0.0], "look_at": [0.0, 0.0, -10.0], "fov_deg": 55.0 }),
        )
        .unwrap();
        assert!((cam.fov_deg - 55.0).abs() < 1e-4);
        assert!(cam.forward().z < -0.99);

        let bounds = apply_camera_params(
            &doc,
            &scene,
            &mut cam,
            &json!({ "focus_target": "Workspace.BoxPart", "yaw_deg": 45.0, "pitch_deg": -20.0 }),
        )
        .unwrap()
        .unwrap();
        assert!((bounds.0.x - 15.0).abs() < 1e-3);
        assert!((bounds.1.x - 25.0).abs() < 1e-3);
        assert!((cam.focus - Vec3::new(20.0, 2.0, -10.0)).length() < 1e-3);
    }

    #[test]
    fn lighting_presets_and_effects_round_trip() {
        let mut doc = sample_doc();
        let preset = build_preset("golden_hour").unwrap();
        let merged = merge_preset(
            preset,
            &json!({
                "lighting": { "Brightness": 3.2 },
                "sky": { "StarCount": 1500 }
            }),
        );
        apply_lighting_patch(&mut doc, &merged).unwrap();

        let state = read_lighting_state(&doc, "edit");
        let bright = state["lighting"]["properties"]["Brightness"].as_f64().unwrap();
        assert!((bright - 3.2).abs() < 1e-4);
        assert_eq!(state["sky"]["properties"]["StarCount"].as_i64(), Some(1500));
        assert!(state["atmosphere"].is_object());
        assert_eq!(state["clouds"]["properties"]["Enabled"].as_bool(), Some(true));
        assert!(!state["effects"].as_array().unwrap().is_empty());
    }
}
