//! Editor selection, undo/redo history, UI/settings control, and GUI inspection MCP tools.

use std::collections::HashSet;
use std::sync::mpsc::Sender;

use glam::Vec3;
use rbx_dom_weak::types::{Ref, UDim2, Variant};
use serde_json::{json, Map, Value};

use crate::app::{Tab, Tool};
use crate::doc::Document;
use crate::mcp::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use crate::mcp::server::CallOutcome;
use crate::mcp::variants::{
    instance_path, ref_to_str, resolve_datamodel, resolve_datamodel_mut, resolve_target,
    variant_to_json,
};
use crate::rustudio_settings::{self as rs, OptKind, OPTS};
use crate::studio_settings::{self as ss, Def, Kind as SettingKind, Val, DEFS, PAGES};

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

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

pub fn schemas() -> Vec<ToolDef> {
    vec![
        tool(
            "editor_selection",
            "Read or manipulate Studio's selected instances: get, set, add, remove, clear, or select_all_of_class.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get", "set", "add", "remove", "clear", "select_all_of_class"],
                        "default": "get"
                    },
                    "datamodel": { "type": "string", "enum": ["edit", "play"] },
                    "targets": {
                        "type": ["array", "string", "object"],
                        "description": "Target instance ref(s) or path(s) for set, add, or remove."
                    },
                    "class_name": { "type": "string", "description": "Class name for select_all_of_class." },
                    "include_subclasses": { "type": "boolean", "default": false, "description": "Match subclasses via IsA during select_all_of_class." },
                    "root": { "type": ["string", "object"], "description": "Optional subtree root for select_all_of_class (defaults to DataModel root)." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "editor_undo_redo",
            "Inspect, execute, or clear the undo/redo history stack.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["status", "undo", "redo", "clear"], "default": "status" },
                    "steps": { "type": "integer", "minimum": 1, "maximum": 200, "default": 1, "description": "Number of undo or redo steps to execute." },
                    "datamodel": { "type": "string", "enum": ["edit", "play"] }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "editor_ui_and_settings",
            "Read or control editor tools (Select/Move/Scale/Rotate/Transform), snapping/grid/collision toggles, panels & Studio actions, RuStudio engine options, and Roblox Studio Settings.",
            json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "enum": ["none", "select", "move", "scale", "rotate", "transform"] },
                    "snap_move": { "type": "boolean" },
                    "move_inc": { "type": "number", "minimum": 0.001 },
                    "snap_rotate": { "type": "boolean" },
                    "rotate_inc": { "type": "number", "minimum": 0.01 },
                    "collisions": { "type": "boolean" },
                    "join_surfaces": { "type": "boolean" },
                    "local_space": { "type": "boolean" },
                    "show_grid": { "type": "boolean" },
                    "physical_drag": { "type": "boolean" },
                    "part_snap": { "type": "boolean" },
                    "use_bounding_boxes": { "type": "boolean" },
                    "align_dragged": { "type": "boolean" },
                    "clean_view": { "type": "boolean" },
                    "show_gui": { "type": "boolean" },
                    "grid_size": { "type": "number", "minimum": 0.05 },
                    "action": { "type": "string", "description": "Invoke a Studio UI action by name (e.g. 'panel_explorer', 'panel_properties', 'panel_output', 'toolbox', 'terrain_editor', 'avatar_settings', 'studio_settings', 'manage_plugins', 'show_grid', 'show_ui', 'reset_layout', 'dock_all', 'focus', 'frame_all')." },
                    "list_actions": { "type": "boolean", "description": "Include known Studio action names and their enabled/checked state." },
                    "panel": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "enum": ["viewport", "explorer", "properties", "output", "avatar_settings", "terrain_editor", "toolbox"] },
                            "open": { "type": "boolean", "description": "Omit to toggle the panel." }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "rustudio_settings_set": { "type": "object", "description": "Map of rustudio_settings keys to new values (null resets to default)." },
                    "rustudio_settings_query": { "type": ["string", "boolean"], "description": "Filter string or true to list rustudio_settings options." },
                    "studio_settings_set": {
                        "type": ["array", "object"],
                        "description": "Either array of {class, prop, value} or object mapping 'Class.Prop' to value."
                    },
                    "studio_settings_query": {
                        "type": ["object", "string", "boolean"],
                        "description": "Filter Studio Settings by string, true, or {page?, class?, query?}."
                    },
                    "reset_studio_settings": { "type": "boolean", "description": "Reset all Studio Settings to defaults." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "gui_inspect",
            "Walk StarterGui (or PlayerGui / Workspace SurfaceGui & BillboardGui) computing 2D layout rects (AbsolutePosition, AbsoluteSize, AbsoluteRotation), effective visibility, Text, ZIndex, BackgroundColor3, and optional 2D hit-testing at [x, y].",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["edit", "play"] },
                    "root": { "type": ["string", "object"], "description": "Optional GUI root container ref/path (defaults to StarterGui or active PlayerGui)." },
                    "include_world_guis": { "type": "boolean", "default": true, "description": "Include Workspace SurfaceGui and BillboardGui objects when no explicit root is specified." },
                    "include_hidden": { "type": "boolean", "default": true, "description": "Include objects with Visible=false or inside disabled ScreenGuis." },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "default": 32 },
                    "at": {
                        "type": "array",
                        "items": { "type": "number" },
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Optional point [x, y] to hit-test visible GUI elements from front to back."
                    },
                    "coord_space": { "type": "string", "enum": ["viewport", "window"], "default": "viewport", "description": "Coordinate space for `at` hit-test point." }
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
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let res = match name {
        "editor_selection" => handle_selection(ed, ctx, args),
        "editor_undo_redo" => handle_undo_redo(ed, ctx, args),
        "editor_ui_and_settings" => handle_ui_and_settings(ed, ctx, args),
        "gui_inspect" => handle_gui_inspect(ed, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(res))
}

// ---------------------------------------------------------------------------
// editor_selection
// ---------------------------------------------------------------------------

fn handle_selection(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let dm_arg = args.get("datamodel").and_then(Value::as_str);
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("get");

    if action == "get" {
        let doc = match resolve_datamodel(ed, dm_arg) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        let dm_name = dm_label(ed, doc);
        return ToolCallResult::json(&selection_snapshot(doc, dm_name));
    }

    let targets = match args.get("targets") {
        Some(v) if !v.is_null() => {
            let doc = match resolve_datamodel(ed, dm_arg) {
                Ok(d) => d,
                Err(e) => return ToolCallResult::error(e),
            };
            match parse_target_list(doc, v) {
                Ok(list) => list,
                Err(e) => return ToolCallResult::error(e),
            }
        }
        _ => Vec::new(),
    };

    let touch_editor_selection = matches!(dm_arg, None | Some("") | Some("active"))
        || (dm_arg == Some("edit") && ed.play.is_none());
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

    let new_sel: Vec<Ref> = match action {
        "set" => {
            if args.get("targets").is_none() {
                return ToolCallResult::error("targets is required for selection set");
            }
            dedup_valid_refs(doc, targets)
        }
        "add" => {
            let mut merged = doc.selection.clone();
            merged.extend(targets);
            dedup_valid_refs(doc, merged)
        }
        "remove" => {
            let rm: HashSet<Ref> = targets.into_iter().collect();
            dedup_valid_refs(
                doc,
                doc.selection
                    .iter()
                    .copied()
                    .filter(|r| !rm.contains(r))
                    .collect(),
            )
        }
        "clear" => Vec::new(),
        "select_all_of_class" => {
            let Some(class_name) = args.get("class_name").and_then(Value::as_str) else {
                return ToolCallResult::error("class_name is required for select_all_of_class");
            };
            let include_sub = args
                .get("include_subclasses")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let root = match args.get("root").filter(|v| !v.is_null()) {
                Some(rv) => match resolve_target(doc, rv) {
                    Ok(r) => r,
                    Err(e) => return ToolCallResult::error(e),
                },
                None => doc.dom.root_ref(),
            };
            collect_matching_class(doc, root, class_name, include_sub)
        }
        other => {
            return ToolCallResult::error(format!(
                "unknown selection action '{other}', expected get, set, add, remove, clear, or select_all_of_class"
            ));
        }
    };

    if touch_editor_selection {
        ed.select(new_sel);
        ctx.request_repaint();
        return ToolCallResult::json(&selection_snapshot(&ed.doc, dm_name));
    }

    doc.selection = new_sel;
    ctx.request_repaint();
    ToolCallResult::json(&selection_snapshot(doc, dm_name))
}

fn parse_target_list(doc: &Document, v: &Value) -> Result<Vec<Ref>, String> {
    match v {
        Value::Array(arr) => arr.iter().map(|item| resolve_target(doc, item)).collect(),
        _ => Ok(vec![resolve_target(doc, v)?]),
    }
}

fn dedup_valid_refs(doc: &Document, refs: Vec<Ref>) -> Vec<Ref> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        if r.is_some() && doc.dom.get_by_ref(r).is_some() && seen.insert(r) {
            out.push(r);
        }
    }
    out
}

fn collect_matching_class(
    doc: &Document,
    root: Ref,
    class_name: &str,
    include_subclasses: bool,
) -> Vec<Ref> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let dm_root = doc.dom.root_ref();
    while let Some(r) = stack.pop() {
        let Some(inst) = doc.dom.get_by_ref(r) else {
            continue;
        };
        if r != dm_root {
            let matches = if include_subclasses {
                crate::doc::is_a(&inst.class, class_name)
            } else {
                inst.class == class_name
            };
            if matches {
                out.push(r);
            }
        }
        for &c in inst.children().iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn selection_snapshot(doc: &Document, dm_name: &str) -> Value {
    let items: Vec<Value> = doc
        .selection
        .iter()
        .filter_map(|&r| {
            let inst = doc.dom.get_by_ref(r)?;
            Some(json!({
                "ref": ref_to_str(r),
                "name": inst.name,
                "class": inst.class.to_string(),
                "path": instance_path(doc, r),
            }))
        })
        .collect();
    let bounds = crate::toolbox::insert::aabb(&doc.dom, &doc.selection).map(|(lo, hi)| {
        json!({
            "min": [lo.x, lo.y, lo.z],
            "max": [hi.x, hi.y, hi.z],
        })
    });
    json!({
        "datamodel": dm_name,
        "count": items.len(),
        "selection": items,
        "bounds": bounds,
    })
}

// ---------------------------------------------------------------------------
// editor_undo_redo
// ---------------------------------------------------------------------------

fn handle_undo_redo(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let dm_arg = args.get("datamodel").and_then(Value::as_str);
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    let steps = args
        .get("steps")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 200) as usize;

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

    let mut executed = Vec::new();
    match action {
        "status" => {}
        "undo" => {
            for _ in 0..steps {
                let Some(label) = doc.undo() else {
                    break;
                };
                executed.push(label);
            }
            if !executed.is_empty() {
                ctx.request_repaint();
            }
        }
        "redo" => {
            for _ in 0..steps {
                let Some(label) = doc.redo() else {
                    break;
                };
                executed.push(label);
            }
            if !executed.is_empty() {
                ctx.request_repaint();
            }
        }
        "clear" => {
            doc.clear_history();
            ctx.request_repaint();
        }
        other => {
            return ToolCallResult::error(format!(
                "unknown undo/redo action '{other}', expected status, undo, redo, or clear"
            ));
        }
    }

    json_undo_status(doc, dm_name, action, executed)
}

fn json_undo_status(
    doc: &Document,
    dm_name: &str,
    action: &str,
    executed: Vec<String>,
) -> ToolCallResult {
    ToolCallResult::json(&json!({
        "datamodel": dm_name,
        "action": action,
        "steps_executed": executed.len(),
        "labels_applied": executed,
        "can_undo": doc.can_undo().is_some(),
        "next_undo": doc.can_undo(),
        "can_redo": doc.can_redo().is_some(),
        "next_redo": doc.can_redo(),
        "undo_count": doc.undo_len(),
    }))
}

// ---------------------------------------------------------------------------
// editor_ui_and_settings
// ---------------------------------------------------------------------------

const KNOWN_ACTIONS: &[&str] = &[
    "new",
    "save",
    "save_as",
    "undo",
    "redo",
    "cut",
    "copy",
    "paste",
    "paste_into",
    "duplicate",
    "delete",
    "select_all",
    "select_children",
    "group",
    "ungroup",
    "focus",
    "frame_all",
    "anchor_toggle",
    "play",
    "play_here",
    "run",
    "stop",
    "pause",
    "step",
    "show_grid",
    "show_ui",
    "fullscreen",
    "panel_explorer",
    "panel_properties",
    "panel_output",
    "toolbox",
    "terrain_editor",
    "avatar_settings",
    "studio_settings",
    "manage_plugins",
    "reset_layout",
    "dock_all",
    "screenshot",
];

fn handle_ui_and_settings(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let mut dirty_ui = false;
    let mut warnings: Vec<String> = Vec::new();

    if let Some(t) = args.get("tool").and_then(Value::as_str) {
        match t.to_ascii_lowercase().as_str() {
            "none" => ed.tool = Tool::None,
            "select" => ed.tool = Tool::Select,
            "move" => ed.tool = Tool::Move,
            "scale" => ed.tool = Tool::Scale,
            "rotate" => ed.tool = Tool::Rotate,
            "transform" => ed.tool = Tool::Transform,
            other => return ToolCallResult::error(format!("unknown tool '{other}'")),
        }
        dirty_ui = true;
    }

    let mut apply_bool = |key: &str, dst: &mut bool| -> Result<(), String> {
        if let Some(v) = args.get(key) {
            *dst = v.as_bool().ok_or_else(|| format!("{key} must be a boolean"))?;
            dirty_ui = true;
        }
        Ok(())
    };
    if let Err(e) = apply_bool("snap_move", &mut ed.snap_move) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("snap_rotate", &mut ed.snap_rotate) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("collisions", &mut ed.collisions) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("join_surfaces", &mut ed.join_surfaces) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("local_space", &mut ed.local_space) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("show_grid", &mut ed.show_grid) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("physical_drag", &mut ed.physical_drag) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("part_snap", &mut ed.part_snap) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("use_bounding_boxes", &mut ed.use_bounding_boxes) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("align_dragged", &mut ed.align_dragged) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("clean_view", &mut ed.clean_view) {
        return ToolCallResult::error(e);
    }
    if let Err(e) = apply_bool("show_gui", &mut ed.ui.show_gui) {
        return ToolCallResult::error(e);
    }

    if let Some(v) = args.get("move_inc") {
        let Some(n) = v.as_f64().filter(|x| x.is_finite() && *x > 0.0) else {
            return ToolCallResult::error("move_inc must be a positive finite number");
        };
        ed.move_inc = n as f32;
        dirty_ui = true;
    }
    if let Some(v) = args.get("rotate_inc") {
        let Some(n) = v.as_f64().filter(|x| x.is_finite() && *x > 0.0) else {
            return ToolCallResult::error("rotate_inc must be a positive finite number");
        };
        ed.rotate_inc = n as f32;
        dirty_ui = true;
    }
    if let Some(v) = args.get("grid_size") {
        let Some(n) = v.as_f64().filter(|x| x.is_finite() && *x > 0.0) else {
            return ToolCallResult::error("grid_size must be a positive finite number");
        };
        ed.ui.grid_size = n as f32;
        dirty_ui = true;
    }

    if let Some(panel_obj) = args.get("panel").and_then(Value::as_object) {
        let Some(pname) = panel_obj.get("name").and_then(Value::as_str) else {
            return ToolCallResult::error("panel.name is required");
        };
        let tab = match pname {
            "viewport" => Tab::Viewport,
            "explorer" => Tab::Explorer,
            "properties" => Tab::Properties,
            "output" => Tab::Output,
            "avatar_settings" => Tab::AvatarSettings,
            "terrain_editor" => Tab::TerrainEditor,
            "toolbox" => Tab::Toolbox,
            other => return ToolCallResult::error(format!("unknown panel '{other}'")),
        };
        let open = panel_obj.get("open").and_then(Value::as_bool);
        ed.ui.panel_requests.push((tab, open));
        dirty_ui = true;
    }

    let mut action_result = None;
    if let Some(act) = args.get("action").and_then(Value::as_str) {
        let (enabled_before, checked_before) = ed.action_state(act);
        if !enabled_before {
            warnings.push(format!("action '{act}' is currently disabled"));
        } else {
            ed.ui_action(ctx, act);
            dirty_ui = true;
        }
        let (enabled_after, checked_after) = ed.action_state(act);
        action_result = Some(json!({
            "action": act,
            "executed": enabled_before,
            "enabled_before": enabled_before,
            "checked_before": checked_before,
            "enabled_after": enabled_after,
            "checked_after": checked_after,
        }));
    }

    let mut rustudio_updated = Vec::new();
    if let Some(rs_map) = args.get("rustudio_settings_set").and_then(Value::as_object) {
        for (k, val) in rs_map {
            let Some(opt) = OPTS.iter().find(|o| o.key == k) else {
                return ToolCallResult::error(format!("unknown rustudio setting '{k}'"));
            };
            if val.is_null() {
                rs::set(opt.key, None);
            } else {
                match opt.kind {
                    OptKind::Bool => {
                        let Some(b) = val.as_bool() else {
                            return ToolCallResult::error(format!("rustudio setting '{k}' requires a boolean"));
                        };
                        rs::set_bool(opt.key, b);
                    }
                    OptKind::Choice(choices) => {
                        let Some(s) = val.as_str() else {
                            return ToolCallResult::error(format!("rustudio setting '{k}' requires a string choice"));
                        };
                        if !choices.iter().any(|(v, _)| *v == s) {
                            let valid: Vec<&str> = choices.iter().map(|(v, _)| *v).collect();
                            return ToolCallResult::error(format!(
                                "invalid value '{s}' for rustudio setting '{k}', valid: {valid:?}"
                            ));
                        }
                        let default = choices.first().map(|(v, _)| *v).unwrap_or("");
                        rs::set_choice(opt.key, s, default);
                    }
                    OptKind::Path => {
                        let Some(s) = val.as_str() else {
                            return ToolCallResult::error(format!("rustudio setting '{k}' requires a path string"));
                        };
                        rs::set_path(opt.key, s);
                    }
                }
            }
            rustudio_updated.push(k.clone());
        }
        rs::flush();
        dirty_ui = true;
    }

    if args
        .get("reset_studio_settings")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ss::reset_all();
        ss::flush();
        dirty_ui = true;
    }

    let mut studio_updated = Vec::new();
    if let Some(ss_set) = args.get("studio_settings_set").filter(|v| !v.is_null()) {
        match apply_studio_settings_updates(ss_set) {
            Ok(updated) => {
                studio_updated = updated;
                ss::flush();
                dirty_ui = true;
            }
            Err(e) => return ToolCallResult::error(e),
        }
    }

    if dirty_ui {
        ctx.request_repaint();
    }

    let mut panels: Vec<&str> = ed.ui.panels_open.iter().copied().collect();
    panels.sort();

    let mut out = Map::new();
    out.insert("editor".into(), json!({
        "tool": ed.tool.label(),
        "snap_move": ed.snap_move,
        "move_inc": ed.move_inc,
        "snap_rotate": ed.snap_rotate,
        "rotate_inc": ed.rotate_inc,
        "collisions": ed.collisions,
        "join_surfaces": ed.join_surfaces,
        "local_space": ed.local_space,
        "show_grid": ed.show_grid,
        "physical_drag": ed.physical_drag,
        "part_snap": ed.part_snap,
        "use_bounding_boxes": ed.use_bounding_boxes,
        "align_dragged": ed.align_dragged,
        "clean_view": ed.clean_view,
        "show_gui": ed.ui.show_gui,
        "grid_size": ed.ui.grid_size,
        "panels_open": panels,
    }));
    if let Some(ar) = action_result {
        out.insert("action_result".into(), ar);
    }
    if !rustudio_updated.is_empty() {
        out.insert("rustudio_settings_updated".into(), json!(rustudio_updated));
    }
    if !studio_updated.is_empty() {
        out.insert("studio_settings_updated".into(), json!(studio_updated));
    }
    if !warnings.is_empty() {
        out.insert("warnings".into(), json!(warnings));
    }
    if args.get("list_actions").and_then(Value::as_bool).unwrap_or(false) {
        let actions: Vec<Value> = KNOWN_ACTIONS
            .iter()
            .map(|&a| {
                let (enabled, checked) = ed.action_state(a);
                json!({ "action": a, "enabled": enabled, "checked": checked })
            })
            .collect();
        out.insert("actions".into(), Value::Array(actions));
    }
    if let Some(q) = args.get("rustudio_settings_query").filter(|v| !v.is_null() && !matches!(v, Value::Bool(false))) {
        let needle = q.as_str().unwrap_or("").to_ascii_lowercase();
        let list: Vec<Value> = OPTS
            .iter()
            .filter(|o| {
                needle.is_empty()
                    || o.key.to_ascii_lowercase().contains(&needle)
                    || o.label.to_ascii_lowercase().contains(&needle)
                    || o.cat.to_ascii_lowercase().contains(&needle)
            })
            .map(rustudio_opt_json)
            .collect();
        out.insert("rustudio_settings".into(), Value::Array(list));
    }
    if let Some(q) = args.get("studio_settings_query").filter(|v| !v.is_null() && !matches!(v, Value::Bool(false))) {
        out.insert("studio_settings_pages".into(), json!(PAGES));
        out.insert("studio_settings".into(), query_studio_settings(q));
    }

    ToolCallResult::json(&Value::Object(out))
}

fn rustudio_opt_json(o: &rs::Opt) -> Value {
    let shown = rs::shown(o);
    let shown_val = match &shown {
        rs::Shown::Bool(b) => json!(b),
        rs::Shown::Choice(idx) => {
            if let OptKind::Choice(choices) = o.kind {
                if let Some((val, _label)) = choices.get(*idx) {
                    json!(val)
                } else {
                    json!(idx)
                }
            } else {
                json!(idx)
            }
        }
        rs::Shown::Path(v) => json!(v),
    };
    let options = match o.kind {
        OptKind::Choice(c) => json!(c.iter().map(|(v, l)| json!({ "value": v, "label": l })).collect::<Vec<_>>()),
        OptKind::Bool => json!(["true", "false"]),
        OptKind::Path => json!("directory path"),
    };
    json!({
        "key": o.key,
        "category": o.cat,
        "label": o.label,
        "description": o.desc,
        "env": o.env,
        "value": shown_val,
        "overridden_by": rs::overridden_by(o),
        "inactive_reason": rs::inactive(o),
        "options": options,
    })
}

fn apply_studio_settings_updates(v: &Value) -> Result<Vec<String>, String> {
    let mut pairs: Vec<(&str, &str, &Value)> = Vec::new();
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let Some((cls, prop)) = k.split_once('.') else {
                    return Err(format!("Studio setting key '{k}' must be 'Class.Prop'"));
                };
                pairs.push((cls, prop, val));
            }
        }
        Value::Array(arr) => {
            for item in arr {
                let Some(obj) = item.as_object() else {
                    return Err("each studio_settings_set element must be {class, prop, value}".into());
                };
                let cls = obj
                    .get("class")
                    .and_then(Value::as_str)
                    .ok_or("studio_settings_set item missing 'class'")?;
                let prop = obj
                    .get("prop")
                    .and_then(Value::as_str)
                    .ok_or("studio_settings_set item missing 'prop'")?;
                let val = obj.get("value").ok_or("studio_settings_set item missing 'value'")?;
                pairs.push((cls, prop, val));
            }
        }
        _ => return Err("studio_settings_set must be an object or array".into()),
    }

    let mut applied = Vec::with_capacity(pairs.len());
    for (cls, prop, raw_val) in pairs {
        let Some(idx) = ss::index(cls, prop) else {
            return Err(format!("unknown Studio setting '{cls}.{prop}'"));
        };
        let def = &DEFS[idx];
        let val = parse_studio_setting_val(def, raw_val)?;
        if !ss::set(def.class, def.prop, val) {
            return Err(format!("Studio setting '{}.{}' cannot be set", def.class, def.prop));
        }
        applied.push(format!("{}.{}", def.class, def.prop));
    }
    Ok(applied)
}

fn parse_studio_setting_val(def: &Def, raw: &Value) -> Result<Val, String> {
    match def.kind {
        SettingKind::ReadOnly => Err(format!("'{}.{}' is read-only", def.class, def.prop)),
        SettingKind::Bool => raw
            .as_bool()
            .map(Val::Bool)
            .ok_or_else(|| format!("'{}.{}' requires boolean", def.class, def.prop)),
        SettingKind::Int => raw
            .as_i64()
            .map(|n| Val::Num(n as f64))
            .or_else(|| raw.as_f64().map(|n| Val::Num(n.round())))
            .ok_or_else(|| format!("'{}.{}' requires integer", def.class, def.prop)),
        SettingKind::Float => raw
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Val::Num)
            .ok_or_else(|| format!("'{}.{}' requires finite number", def.class, def.prop)),
        SettingKind::Enum(_, items) => {
            if let Some(u) = raw.as_u64() {
                let code = u as u32;
                if items.iter().any(|(_, v, _)| *v == code) {
                    return Ok(Val::Enum(code));
                }
            }
            if let Some(s) = raw.as_str() {
                if let Some((_, code, _)) = items
                    .iter()
                    .find(|(n, _, label)| n.eq_ignore_ascii_case(s) || label.eq_ignore_ascii_case(s))
                {
                    return Ok(Val::Enum(*code));
                }
                if let Some(v) = ss::parse(def, s) {
                    return Ok(v);
                }
            }
            let names: Vec<&str> = items.iter().map(|(n, _, _)| *n).collect();
            Err(format!("invalid enum value for '{}.{}', expected one of {names:?}", def.class, def.prop))
        }
        SettingKind::Color => {
            if let Some(s) = raw.as_str() {
                return ss::parse(def, s)
                    .ok_or_else(|| format!("invalid color '{s}' for '{}.{}'", def.class, def.prop));
            }
            if let Some(arr) = raw.as_array().filter(|a| a.len() == 3) {
                let r = arr[0].as_f64().ok_or("color[0] must be number")? as f32;
                let g = arr[1].as_f64().ok_or("color[1] must be number")? as f32;
                let b = arr[2].as_f64().ok_or("color[2] must be number")? as f32;
                let scale = if r > 1.0 || g > 1.0 || b > 1.0 { 1.0 / 255.0 } else { 1.0 };
                return Ok(Val::Color([
                    (r * scale).clamp(0.0, 1.0),
                    (g * scale).clamp(0.0, 1.0),
                    (b * scale).clamp(0.0, 1.0),
                ]));
            }
            Err(format!("'{}.{}' requires [r, g, b] or '#RRGGBB'", def.class, def.prop))
        }
        SettingKind::Str | SettingKind::Dir => raw
            .as_str()
            .map(|s| Val::Str(s.to_string()))
            .ok_or_else(|| format!("'{}.{}' requires string", def.class, def.prop)),
        SettingKind::Font => {
            if let Some(s) = raw.as_str() {
                return ss::parse(def, s)
                    .ok_or_else(|| format!("invalid font '{s}' for '{}.{}'", def.class, def.prop));
            }
            if let Some(obj) = raw.as_object() {
                let cur_sz = match ss::get(def.class, def.prop).unwrap_or_else(|| def.default_val()) {
                    Val::Font(_, sz) => sz,
                    _ => 12.0,
                };
                let family = obj
                    .get("family")
                    .and_then(Value::as_str)
                    .ok_or("font object missing family")?;
                let size = obj.get("size").and_then(Value::as_f64).unwrap_or(cur_sz);
                return Ok(Val::Font(family.to_string(), size));
            }
            Err(format!("'{}.{}' requires string or {{family, size}}", def.class, def.prop))
        }
    }
}

fn query_studio_settings(q: &Value) -> Value {
    let page_f = q
        .get("page")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let class_f = q
        .get("class")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let text_f = q
        .as_str()
        .or_else(|| q.get("query").and_then(Value::as_str))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let rows: Vec<Value> = DEFS
        .iter()
        .filter(|d| {
            page_f
                .as_ref()
                .is_none_or(|p| d.page.to_ascii_lowercase() == *p)
                && class_f
                    .as_ref()
                    .is_none_or(|c| d.class.to_ascii_lowercase() == *c)
                && (text_f.is_empty()
                    || d.prop.to_ascii_lowercase().contains(&text_f)
                    || d.label.to_ascii_lowercase().contains(&text_f)
                    || d.cat.to_ascii_lowercase().contains(&text_f)
                    || d.class.to_ascii_lowercase().contains(&text_f))
        })
        .map(|d| {
            let cur = ss::get(d.class, d.prop).unwrap_or_else(|| d.default_val());
            let mut item = json!({
                "page": d.page,
                "category": d.cat,
                "label": d.label,
                "class": d.class,
                "prop": d.prop,
                "display": ss::shown(d, &cur),
                "value": studio_val_to_json(&cur),
                "default": studio_val_to_json(&d.default_val()),
                "read_only": matches!(d.kind, SettingKind::ReadOnly),
            });
            if let SettingKind::Enum(ename, choices) = d.kind {
                item.as_object_mut().unwrap().insert(
                    "enum".into(),
                    json!({
                        "name": ename,
                        "items": choices.iter().map(|(n, v, l)| json!({"name": n, "value": v, "label": l})).collect::<Vec<_>>(),
                    }),
                );
            }
            item
        })
        .collect();
    Value::Array(rows)
}

fn studio_val_to_json(v: &Val) -> Value {
    match v {
        Val::Bool(b) => json!(b),
        Val::Num(n) => json!(n),
        Val::Enum(u) => json!(u),
        Val::Color(c) => json!(c),
        Val::Str(s) => json!(s),
        Val::Font(f, s) => json!({ "family": f, "size": s }),
    }
}

// ---------------------------------------------------------------------------
// gui_inspect
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct GuiEntry {
    r: Ref,
    name: String,
    class: String,
    path: String,
    pos: [f32; 2],
    size: [f32; 2],
    rot_deg: f32,
    clip_rect: Option<[f32; 4]>, // [x, y, w, h]
    visible: bool,
    effective_visible: bool,
    z_index: i32,
    display_order: i32,
    layout_order: i32,
    paint_seq: usize,
    text: Option<String>,
    text_bounds: Option<[f32; 2]>,
    bg_color: Option<Value>,
    bg_transparency: Option<f32>,
    image: Option<String>,
    clips_descendants: bool,
    source_kind: &'static str,
    children: Vec<Value>,
}

fn handle_gui_inspect(ed: &mut crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm_arg = args.get("datamodel").and_then(Value::as_str);
    let doc = match resolve_datamodel(ed, dm_arg) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };
    let dm_name = dm_label(ed, doc);

    let has_live_rect = ed.viewport_rect != egui::Rect::NOTHING;
    let vp_size = if has_live_rect {
        [
            ed.viewport_rect.width().max(1.0),
            ed.viewport_rect.height().max(1.0),
        ]
    } else {
        [1280.0, 720.0]
    };

    let include_hidden = args
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let include_world_guis = args
        .get("include_world_guis")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_depth = args
        .get("max_depth")
        .and_then(Value::as_u64)
        .unwrap_or(32)
        .clamp(1, 64) as usize;

    let mut roots: Vec<Ref> = Vec::new();
    if let Some(root_v) = args.get("root").filter(|v| !v.is_null()) {
        match resolve_target(doc, root_v) {
            Ok(r) => roots.push(r),
            Err(e) => return ToolCallResult::error(e),
        }
    } else {
        let dm_root = doc.dom.root_ref();
        if dm_name == "play" {
            if let Some(players) = doc.find_child(dm_root, "Players") {
                if let Some(p_inst) = doc.dom.get_by_ref(players) {
                    for &p in p_inst.children() {
                        if let Some(pg) = doc.find_child(p, "PlayerGui") {
                            roots.push(pg);
                        }
                    }
                }
            }
        }
        if roots.is_empty() {
            if let Some(sg) = doc.find_child(dm_root, "StarterGui") {
                roots.push(sg);
            }
        }
        if include_world_guis {
            if let Some(ws) = doc.find_child(dm_root, "Workspace") {
                collect_world_gui_roots(doc, ws, &mut roots);
            }
        }
    }

    let mut flat_hits: Vec<GuiEntry> = Vec::new();
    let mut trees: Vec<Value> = Vec::new();
    let mut seq = 0usize;

    for r in roots {
        if let Some(tree) = inspect_gui_root(
            &ed.gui,
            &ed.cam,
            ed.viewport_rect,
            doc,
            r,
            vp_size,
            include_hidden,
            max_depth,
            &mut seq,
            &mut flat_hits,
        ) {
            trees.push(tree);
        }
    }

    let mut result = json!({
        "datamodel": dm_name,
        "viewport_size": vp_size,
        "root_count": trees.len(),
        "roots": trees,
    });

    if let Some(at_arr) = args.get("at").and_then(Value::as_array) {
        if at_arr.len() != 2 {
            return ToolCallResult::error("`at` must be [x, y]");
        }
        let Some(ax) = at_arr[0].as_f64() else {
            return ToolCallResult::error("at[0] must be a number");
        };
        let Some(ay) = at_arr[1].as_f64() else {
            return ToolCallResult::error("at[1] must be a number");
        };
        let coord_space = args
            .get("coord_space")
            .and_then(Value::as_str)
            .unwrap_or("viewport");

        let (vx, vy) = if coord_space == "window" && has_live_rect {
            (
                ax as f32 - ed.viewport_rect.min.x,
                ay as f32 - ed.viewport_rect.min.y,
            )
        } else {
            (ax as f32, ay as f32)
        };

        let live_object_at = if dm_name == "edit" && has_live_rect {
            let screen_pt = egui::pos2(
                ed.viewport_rect.min.x + vx,
                ed.viewport_rect.min.y + vy,
            );
            ed.gui.object_at(screen_pt).map(|r| json!({
                "ref": ref_to_str(r),
                "name": inst_name(doc, r),
                "path": instance_path(doc, r),
            }))
        } else {
            None
        };

        let mut matches: Vec<&GuiEntry> = flat_hits
            .iter()
            .filter(|e| e.effective_visible && hit_test_entry(e, vx, vy))
            .collect();
        matches.sort_by(|a, b| {
            b.display_order
                .cmp(&a.display_order)
                .then(b.z_index.cmp(&a.z_index))
                .then(b.paint_seq.cmp(&a.paint_seq))
        });

        let hit_list: Vec<Value> = matches
            .iter()
            .map(|e| {
                json!({
                    "ref": ref_to_str(e.r),
                    "name": e.name,
                    "class": e.class,
                    "path": e.path,
                    "AbsolutePosition": e.pos,
                    "AbsoluteSize": e.size,
                    "ZIndex": e.z_index,
                    "Text": e.text,
                })
            })
            .collect();

        result.as_object_mut().unwrap().insert(
            "hit_test".into(),
            json!({
                "at": [ax, ay],
                "viewport_point": [vx, vy],
                "top_hit": hit_list.first().cloned().unwrap_or(Value::Null),
                "live_hit": live_object_at,
                "matches": hit_list,
            }),
        );
    }

    ToolCallResult::json(&result)
}

fn collect_world_gui_roots(doc: &Document, r: Ref, out: &mut Vec<Ref>) {
    let Some(inst) = doc.dom.get_by_ref(r) else {
        return;
    };
    if inst.class == "SurfaceGui" || inst.class == "BillboardGui" {
        out.push(r);
        return;
    }
    for &c in inst.children() {
        collect_world_gui_roots(doc, c, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn inspect_gui_root(
    gui: &crate::gui::Gui,
    cam: &crate::camera::Camera,
    viewport_rect: egui::Rect,
    doc: &Document,
    r: Ref,
    vp_size: [f32; 2],
    include_hidden: bool,
    max_depth: usize,
    seq: &mut usize,
    flat: &mut Vec<GuiEntry>,
) -> Option<Value> {
    let inst = doc.dom.get_by_ref(r)?;
    let cls = inst.class.as_str();

    // Container such as StarterGui or PlayerGui: recurse into ScreenGui / Folder / GuiObject children.
    if cls == "StarterGui" || cls == "PlayerGui" || cls == "Folder" || cls == "Configuration" {
        let mut children = Vec::new();
        for &c in inst.children() {
            if let Some(child_v) = inspect_gui_root(
                gui,
                cam,
                viewport_rect,
                doc,
                c,
                vp_size,
                include_hidden,
                max_depth,
                seq,
                flat,
            ) {
                children.push(child_v);
            }
        }
        return Some(json!({
            "ref": ref_to_str(r),
            "name": inst.name,
            "class": cls,
            "path": instance_path(doc, r),
            "children": children,
        }));
    }

    if cls == "ScreenGui" || cls == "PluginGui" || cls == "DockWidgetPluginGui" {
        let enabled = prop_bool(doc, r, "Enabled", true);
        let display_order = prop_i32(doc, r, "DisplayOrder", 0);
        let ignore_inset = prop_bool(doc, r, "IgnoreGuiInset", false);
        let screen_insets = prop_enum_u32(doc, r, "ScreenInsets").unwrap_or(2);
        let top_inset = if ignore_inset || screen_insets == 0 || screen_insets == 1 {
            0.0
        } else {
            crate::mech::GUI_TOP_INSET
        };
        let root_pos = [0.0, top_inset];
        let root_size = [vp_size[0], (vp_size[1] - top_inset).max(0.0)];

        if !include_hidden && !enabled {
            return None;
        }

        let mut children = Vec::new();
        walk_gui_children(
            gui,
            doc,
            inst.children(),
            root_pos,
            root_size,
            None,
            enabled,
            display_order,
            1,
            max_depth,
            include_hidden,
            "ScreenGui",
            seq,
            flat,
            &mut children,
        );
        return Some(json!({
            "ref": ref_to_str(r),
            "name": inst.name,
            "class": cls,
            "path": instance_path(doc, r),
            "Enabled": enabled,
            "DisplayOrder": display_order,
            "IgnoreGuiInset": ignore_inset,
            "AbsolutePosition": root_pos,
            "AbsoluteSize": root_size,
            "children": children,
        }));
    }

    if cls == "SurfaceGui" || cls == "BillboardGui" {
        let enabled = prop_bool(doc, r, "Enabled", true);
        if !include_hidden && !enabled {
            return None;
        }
        let (root_pos, root_size, world_vis) = world_gui_rect(gui, cam, viewport_rect, doc, r, vp_size);
        let eff_vis = enabled && world_vis;
        let mut children = Vec::new();
        walk_gui_children(
            gui,
            doc,
            inst.children(),
            root_pos,
            root_size,
            None,
            eff_vis,
            0,
            1,
            max_depth,
            include_hidden,
            if cls == "SurfaceGui" { "SurfaceGui" } else { "BillboardGui" },
            seq,
            flat,
            &mut children,
        );
        return Some(json!({
            "ref": ref_to_str(r),
            "name": inst.name,
            "class": cls,
            "path": instance_path(doc, r),
            "Enabled": enabled,
            "visible_on_screen": world_vis,
            "AbsolutePosition": root_pos,
            "AbsoluteSize": root_size,
            "children": children,
        }));
    }

    if crate::doc::is_a(cls, "GuiObject") {
        let mut out = Vec::new();
        walk_gui_children(
            gui,
            doc,
            &[r],
            [0.0, 0.0],
            vp_size,
            None,
            true,
            0,
            1,
            max_depth,
            include_hidden,
            "GuiObject",
            seq,
            flat,
            &mut out,
        );
        return out.into_iter().next();
    }

    None
}

fn zero_udim2() -> UDim2 {
    UDim2::new(
        rbx_dom_weak::types::UDim::new(0.0, 0),
        rbx_dom_weak::types::UDim::new(0.0, 0),
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_gui_children(
    gui: &crate::gui::Gui,
    doc: &Document,
    children_refs: &[Ref],
    parent_pos: [f32; 2],
    parent_size: [f32; 2],
    parent_clip: Option<[f32; 4]>,
    parent_visible: bool,
    display_order: i32,
    depth: usize,
    max_depth: usize,
    include_hidden: bool,
    source_kind: &'static str,
    seq: &mut usize,
    flat: &mut Vec<GuiEntry>,
    out: &mut Vec<Value>,
) {
    if depth > max_depth {
        return;
    }

    let (pad_l, pad_t, pad_r, pad_b, scale) = container_modifiers(doc, children_refs, parent_size);
    let inner_pos = [parent_pos[0] + pad_l, parent_pos[1] + pad_t];
    let inner_size = [
        (parent_size[0] - pad_l - pad_r).max(0.0),
        (parent_size[1] - pad_t - pad_b).max(0.0),
    ];

    let list_layout = find_layout_controller(doc, children_refs);

    // Collect GuiObject siblings in layout order when a UIListLayout / UIGridLayout exists.
    let mut ordered_refs: Vec<Ref> = children_refs
        .iter()
        .copied()
        .filter(|&c| doc.class_of(c).is_some_and(|cl| crate::doc::is_a(cl.as_str(), "GuiObject")))
        .collect();
    if let Some(ctrl) = &list_layout {
        if ctrl.sort_by_layout_order() {
            ordered_refs.sort_by_key(|&c| prop_i32(doc, c, "LayoutOrder", 0));
        }
    }

    let mut cursor_main = 0.0f32;
    let mut grid_idx = 0usize;

    for r in ordered_refs {
        let Some(inst) = doc.dom.get_by_ref(r) else {
            continue;
        };
        let visible = prop_bool(doc, r, "Visible", true);
        let eff_visible = parent_visible && visible;
        if !include_hidden && !eff_visible {
            continue;
        }

        let size_ud = prop_udim2(doc, r, "Size").unwrap_or_else(default_gui_size);
        let pos_ud = prop_udim2(doc, r, "Position").unwrap_or_else(zero_udim2);
        let anchor = prop_vec2(doc, r, "AnchorPoint", [0.0, 0.0]);
        let rot_deg = prop_f32(doc, r, "Rotation", 0.0);
        let z_index = prop_i32(doc, r, "ZIndex", 1);
        let layout_order = prop_i32(doc, r, "LayoutOrder", 0);
        let clips = prop_bool(doc, r, "ClipsDescendants", false);

        let mut w = (size_ud.x.scale * inner_size[0] + size_ud.x.offset as f32).max(0.0) * scale;
        let mut h = (size_ud.y.scale * inner_size[1] + size_ud.y.offset as f32).max(0.0) * scale;
        apply_child_constraints(doc, inst.children(), &mut w, &mut h);

        let (mut abs_x, mut abs_y) = match &list_layout {
            Some(LayoutCtrl::List {
                horizontal,
                padding,
                sort_by_layout_order: _,
                h_align,
                v_align,
            }) => {
                if *horizontal {
                    let x = inner_pos[0] + cursor_main;
                    let y = align_cross(inner_pos[1], inner_size[1], h, *v_align);
                    cursor_main += w + padding;
                    (x, y)
                } else {
                    let x = align_cross(inner_pos[0], inner_size[0], w, *h_align);
                    let y = inner_pos[1] + cursor_main;
                    cursor_main += h + padding;
                    (x, y)
                }
            }
            Some(LayoutCtrl::Grid {
                cell_size,
                cell_pad,
                sort_by_layout_order: _,
            }) => {
                let cw = cell_size[0];
                let ch = cell_size[1];
                w = cw;
                h = ch;
                let step_x = (cw + cell_pad[0]).max(1.0);
                let cols = ((inner_size[0] + cell_pad[0]) / step_x).floor().max(1.0) as usize;
                let col = grid_idx % cols;
                let row = grid_idx / cols;
                grid_idx += 1;
                (
                    inner_pos[0] + col as f32 * (cw + cell_pad[0]),
                    inner_pos[1] + row as f32 * (ch + cell_pad[1]),
                )
            }
            None => {
                let px = inner_pos[0] + pos_ud.x.scale * inner_size[0] + pos_ud.x.offset as f32
                    - anchor[0] * w;
                let py = inner_pos[1] + pos_ud.y.scale * inner_size[1] + pos_ud.y.offset as f32
                    - anchor[1] * h;
                (px, py)
            }
        };

        // Prefer live computed GuiBox when available (especially for SurfaceGui/BillboardGui and TextBounds).
        let mut final_rot = rot_deg;
        let mut text_bounds = None;
        if let Some(gbox) = gui.layout.get(&r) {
            abs_x = gbox.pos[0];
            abs_y = gbox.pos[1];
            w = gbox.size[0];
            h = gbox.size[1];
            final_rot = gbox.rot.to_degrees();
            text_bounds = gbox.text;
        }

        let this_rect = [abs_x, abs_y, w, h];
        let next_clip = if clips {
            Some(match parent_clip {
                Some(pc) => intersect_rect(pc, this_rect),
                None => this_rect,
            })
        } else {
            parent_clip
        };

        *seq += 1;
        let paint_seq = *seq;
        let text = prop_string(doc, r, "Text");
        let bg_color = doc.prop(r, "BackgroundColor3").map(|v| variant_to_json(&v));
        let bg_transparency = doc.prop(r, "BackgroundTransparency").and_then(variant_f32);
        let image = prop_content_str(doc, r, "Image");

        let mut child_json = Vec::new();
        walk_gui_children(
            gui,
            doc,
            inst.children(),
            [abs_x, abs_y],
            [w, h],
            next_clip,
            eff_visible,
            display_order,
            depth + 1,
            max_depth,
            include_hidden,
            source_kind,
            seq,
            flat,
            &mut child_json,
        );

        let entry = GuiEntry {
            r,
            name: inst.name.clone(),
            class: inst.class.to_string(),
            path: instance_path(doc, r),
            pos: [abs_x, abs_y],
            size: [w, h],
            rot_deg: final_rot,
            clip_rect: parent_clip,
            visible,
            effective_visible: eff_visible,
            z_index,
            display_order,
            layout_order,
            paint_seq,
            text,
            text_bounds,
            bg_color,
            bg_transparency,
            image,
            clips_descendants: clips,
            source_kind,
            children: child_json,
        };
        out.push(entry_to_json(&entry));
        flat.push(entry);
    }
}

fn entry_to_json(e: &GuiEntry) -> Value {
    let mut obj = Map::new();
    obj.insert("ref".into(), json!(ref_to_str(e.r)));
    obj.insert("name".into(), json!(e.name));
    obj.insert("class".into(), json!(e.class));
    obj.insert("path".into(), json!(e.path));
    obj.insert("source".into(), json!(e.source_kind));
    obj.insert("AbsolutePosition".into(), json!(e.pos));
    obj.insert("AbsoluteSize".into(), json!(e.size));
    obj.insert("AbsoluteRotation".into(), json!(e.rot_deg));
    obj.insert("Visible".into(), json!(e.visible));
    obj.insert("effective_visible".into(), json!(e.effective_visible));
    obj.insert("ZIndex".into(), json!(e.z_index));
    obj.insert("LayoutOrder".into(), json!(e.layout_order));
    obj.insert("ClipsDescendants".into(), json!(e.clips_descendants));
    if let Some(bg) = &e.bg_color {
        obj.insert("BackgroundColor3".into(), bg.clone());
    }
    if let Some(tr) = e.bg_transparency {
        obj.insert("BackgroundTransparency".into(), json!(tr));
    }
    if let Some(txt) = &e.text {
        obj.insert("Text".into(), json!(txt));
    }
    if let Some(tb) = e.text_bounds {
        obj.insert("TextBounds".into(), json!(tb));
    }
    if let Some(img) = &e.image {
        if !img.is_empty() {
            obj.insert("Image".into(), json!(img));
        }
    }
    obj.insert("children".into(), Value::Array(e.children.clone()));
    Value::Object(obj)
}

enum LayoutCtrl {
    List {
        horizontal: bool,
        padding: f32,
        sort_by_layout_order: bool,
        h_align: u32,
        v_align: u32,
    },
    Grid {
        cell_size: [f32; 2],
        cell_pad: [f32; 2],
        sort_by_layout_order: bool,
    },
}

impl LayoutCtrl {
    fn sort_by_layout_order(&self) -> bool {
        match self {
            LayoutCtrl::List { sort_by_layout_order, .. } => *sort_by_layout_order,
            LayoutCtrl::Grid { sort_by_layout_order, .. } => *sort_by_layout_order,
        }
    }
}

fn container_modifiers(doc: &Document, children: &[Ref], parent_size: [f32; 2]) -> (f32, f32, f32, f32, f32) {
    let (mut pl, mut pt, mut pr, mut pb) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut scale = 1.0f32;
    for &c in children {
        let Some(cls) = doc.class_of(c) else { continue };
        if cls == "UIPadding" {
            let eval = |p: &str, dim: f32| match doc.prop(c, p) {
                Some(Variant::UDim(u)) => u.scale * dim + u.offset as f32,
                _ => 0.0,
            };
            pl += eval("PaddingLeft", parent_size[0]);
            pr += eval("PaddingRight", parent_size[0]);
            pt += eval("PaddingTop", parent_size[1]);
            pb += eval("PaddingBottom", parent_size[1]);
        } else if cls == "UIScale" {
            scale *= prop_f32(doc, c, "Scale", 1.0).max(0.0);
        }
    }
    (pl, pt, pr, pb, scale)
}

fn find_layout_controller(doc: &Document, children: &[Ref]) -> Option<LayoutCtrl> {
    for &c in children {
        let Some(cls) = doc.class_of(c) else { continue };
        if cls == "UIListLayout" {
            let horizontal = prop_enum_u32(doc, c, "FillDirection") == Some(0);
            let padding = match doc.prop(c, "Padding") {
                Some(Variant::UDim(u)) => u.offset as f32,
                _ => 0.0,
            };
            let sort = prop_enum_u32(doc, c, "SortOrder").unwrap_or(2) == 2;
            let h_align = prop_enum_u32(doc, c, "HorizontalAlignment").unwrap_or(1);
            let v_align = prop_enum_u32(doc, c, "VerticalAlignment").unwrap_or(1);
            return Some(LayoutCtrl::List {
                horizontal,
                padding,
                sort_by_layout_order: sort,
                h_align,
                v_align,
            });
        } else if cls == "UIGridLayout" {
            let cs = prop_udim2(doc, c, "CellSize").unwrap_or_else(|| {
                UDim2::new(rbx_dom_weak::types::UDim::new(0.0, 100), rbx_dom_weak::types::UDim::new(0.0, 100))
            });
            let cp = prop_udim2(doc, c, "CellPadding").unwrap_or_else(zero_udim2);
            let sort = prop_enum_u32(doc, c, "SortOrder").unwrap_or(2) == 2;
            return Some(LayoutCtrl::Grid {
                cell_size: [cs.x.offset as f32, cs.y.offset as f32],
                cell_pad: [cp.x.offset as f32, cp.y.offset as f32],
                sort_by_layout_order: sort,
            });
        }
    }
    None
}

fn align_cross(origin: f32, container: f32, item: f32, align: u32) -> f32 {
    match align {
        0 => origin + (container - item) * 0.5, // Center
        2 => origin + (container - item),       // Right / Bottom
        _ => origin,                            // Left / Top
    }
}

fn apply_child_constraints(doc: &Document, children: &[Ref], w: &mut f32, h: &mut f32) {
    for &c in children {
        let Some(cls) = doc.class_of(c) else { continue };
        if cls == "UISizeConstraint" {
            let mn = prop_vec2(doc, c, "MinSize", [0.0, 0.0]);
            let mx = prop_vec2(doc, c, "MaxSize", [f32::INFINITY, f32::INFINITY]);
            *w = w.clamp(mn[0], mx[0].max(mn[0]));
            *h = h.clamp(mn[1], mx[1].max(mn[1]));
        } else if cls == "UIAspectRatioConstraint" {
            let ratio = prop_f32(doc, c, "AspectRatio", 1.0).max(1e-4);
            let axis = prop_enum_u32(doc, c, "DominantAxis").unwrap_or(0);
            if axis == 1 {
                *w = *h * ratio;
            } else {
                *h = *w / ratio;
            }
        }
    }
}

fn world_gui_rect(
    gui: &crate::gui::Gui,
    cam: &crate::camera::Camera,
    viewport_rect: egui::Rect,
    doc: &Document,
    r: Ref,
    vp_size: [f32; 2],
) -> ([f32; 2], [f32; 2], bool) {
    let _ = viewport_rect;
    if let Some(gbox) = gui.layout.get(&r) {
        return (gbox.pos, gbox.size, true);
    }
    // Project BillboardGui Adornee / Parent onto screen if no live GuiBox exists.
    let adornee = match doc.prop(r, "Adornee") {
        Some(Variant::Ref(target)) if target.is_some() => Some(target),
        _ => doc.dom.get_by_ref(r).map(|i| i.parent()),
    };
    let Some(pos_3d) = adornee.and_then(|a| match doc.prop(a, "CFrame") {
        Some(Variant::CFrame(cf)) => Some(Vec3::new(cf.position.x, cf.position.y, cf.position.z)),
        _ => match doc.prop(a, "Position") {
            Some(Variant::Vector3(v)) => Some(Vec3::new(v.x, v.y, v.z)),
            _ => None,
        },
    }) else {
        return ([0.0, 0.0], [200.0, 50.0], false);
    };

    let offset = match doc.prop(r, "StudsOffset") {
        Some(Variant::Vector3(v)) => Vec3::new(v.x, v.y, v.z),
        _ => Vec3::ZERO,
    };
    let world_p = pos_3d + offset;
    let delta = world_p - cam.pos;
    let fwd = cam.forward();
    let depth = delta.dot(fwd);
    if depth <= 0.05 {
        return ([0.0, 0.0], [0.0, 0.0], false);
    }
    let right = cam.right();
    let up = cam.up();
    let half_h = (cam.fov_deg.to_radians() * 0.5).tan();
    let aspect = vp_size[0] / vp_size[1].max(1.0);
    let sx = ((delta.dot(right) / depth) / (half_h * aspect) * 0.5 + 0.5) * vp_size[0];
    let sy = (0.5 - (delta.dot(up) / depth) / half_h * 0.5) * vp_size[1];

    let size_ud = prop_udim2(doc, r, "Size").unwrap_or_else(|| {
        UDim2::new(rbx_dom_weak::types::UDim::new(0.0, 200), rbx_dom_weak::types::UDim::new(0.0, 50))
    });
    let w = (size_ud.x.scale * (vp_size[1] / (2.0 * half_h * depth)) + size_ud.x.offset as f32).max(0.0);
    let h = (size_ud.y.scale * (vp_size[1] / (2.0 * half_h * depth)) + size_ud.y.offset as f32).max(0.0);
    ([sx - w * 0.5, sy - h * 0.5], [w, h], true)
}

fn hit_test_entry(e: &GuiEntry, px: f32, py: f32) -> bool {
    if e.size[0] <= 0.0 || e.size[1] <= 0.0 {
        return false;
    }
    if let Some([cx, cy, cw, ch]) = e.clip_rect {
        if px < cx || px > cx + cw || py < cy || py > cy + ch {
            return false;
        }
    }
    let cx = e.pos[0] + e.size[0] * 0.5;
    let cy = e.pos[1] + e.size[1] * 0.5;
    let dx = px - cx;
    let dy = py - cy;
    let rad = -e.rot_deg.to_radians();
    let (s, c) = rad.sin_cos();
    let lx = dx * c - dy * s;
    let ly = dx * s + dy * c;
    lx.abs() <= e.size[0] * 0.5 && ly.abs() <= e.size[1] * 0.5
}

fn intersect_rect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

fn default_gui_size() -> UDim2 {
    UDim2::new(
        rbx_dom_weak::types::UDim::new(0.0, 100),
        rbx_dom_weak::types::UDim::new(0.0, 100),
    )
}

fn prop_bool(doc: &Document, r: Ref, p: &str, default: bool) -> bool {
    match doc.prop(r, p) {
        Some(Variant::Bool(b)) => b,
        _ => default,
    }
}

fn prop_i32(doc: &Document, r: Ref, p: &str, default: i32) -> i32 {
    match doc.prop(r, p) {
        Some(Variant::Int32(n)) => n,
        Some(Variant::Int64(n)) => n as i32,
        _ => default,
    }
}

fn prop_f32(doc: &Document, r: Ref, p: &str, default: f32) -> f32 {
    doc.prop(r, p).and_then(variant_f32).unwrap_or(default)
}

fn variant_f32(v: Variant) -> Option<f32> {
    match v {
        Variant::Float32(f) => Some(f),
        Variant::Float64(f) => Some(f as f32),
        Variant::Int32(i) => Some(i as f32),
        _ => None,
    }
}

fn prop_vec2(doc: &Document, r: Ref, p: &str, default: [f32; 2]) -> [f32; 2] {
    match doc.prop(r, p) {
        Some(Variant::Vector2(v)) => [v.x, v.y],
        _ => default,
    }
}

fn prop_udim2(doc: &Document, r: Ref, p: &str) -> Option<UDim2> {
    match doc.prop(r, p) {
        Some(Variant::UDim2(u)) => Some(u),
        _ => None,
    }
}

fn prop_enum_u32(doc: &Document, r: Ref, p: &str) -> Option<u32> {
    match doc.prop(r, p) {
        Some(Variant::Enum(e)) => Some(e.to_u32()),
        Some(Variant::EnumItem(ei)) => Some(ei.value),
        Some(Variant::Int32(i)) => Some(i as u32),
        _ => None,
    }
}

fn prop_string(doc: &Document, r: Ref, p: &str) -> Option<String> {
    match doc.prop(r, p) {
        Some(Variant::String(s)) => Some(s),
        _ => None,
    }
}

fn prop_content_str(doc: &Document, r: Ref, p: &str) -> Option<String> {
    match doc.prop(r, p) {
        Some(Variant::ContentId(c)) => Some(c.as_str().to_string()),
        Some(Variant::Content(c)) => c.as_uri().map(|u| u.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::types::{UDim, Vector2};
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    #[test]
    fn selection_and_undo_helpers_work() {
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("Workspace")
                    .with_name("Workspace")
                    .with_child(InstanceBuilder::new("Part").with_name("A"))
                    .with_child(InstanceBuilder::new("Part").with_name("B"))
                    .with_child(InstanceBuilder::new("Folder").with_name("C")),
            ),
        );
        let mut doc = Document::from_play_dom(dom, "sel_test".into());
        let ws = doc.find_child(doc.dom.root_ref(), "Workspace").unwrap();
        let parts = collect_matching_class(&doc, doc.dom.root_ref(), "Part", false);
        assert_eq!(parts.len(), 2);

        doc.rename(parts[0], "RenamedA");
        assert_eq!(inst_name(&doc, parts[0]), "RenamedA");
        assert_eq!(doc.undo().as_deref(), Some("Rename to RenamedA"));
        assert_eq!(inst_name(&doc, parts[0]), "A");
        assert_eq!(doc.redo().as_deref(), Some("Rename to RenamedA"));
        assert_eq!(inst_name(&doc, parts[0]), "RenamedA");
        assert!(ws.is_some());
    }

    #[test]
    fn gui_layout_and_hit_test() {
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("StarterGui")
                    .with_name("StarterGui")
                    .with_child(
                        InstanceBuilder::new("ScreenGui")
                            .with_name("Main")
                            .with_property("IgnoreGuiInset", true)
                            .with_child(
                                InstanceBuilder::new("TextButton")
                                    .with_name("PlayBtn")
                                    .with_property("Text", "Play")
                                    .with_property(
                                        "Position",
                                        UDim2::new(UDim::new(0.5, 0), UDim::new(0.5, 0)),
                                    )
                                    .with_property(
                                        "Size",
                                        UDim2::new(UDim::new(0.0, 200), UDim::new(0.0, 60)),
                                    )
                                    .with_property("AnchorPoint", Vector2::new(0.5, 0.5)),
                            ),
                    ),
            ),
        );
        let doc = Document::from_play_dom(dom, "gui_test".into());
        let gui = crate::gui::Gui::default();
        let cam = crate::camera::Camera::default();
        let mut seq = 0;
        let mut flat = Vec::new();
        let sg = doc.find_child(doc.dom.root_ref(), "StarterGui").unwrap();
        let _tree = inspect_gui_root(
            &gui,
            &cam,
            egui::Rect::NOTHING,
            &doc,
            sg,
            [1000.0, 800.0],
            true,
            16,
            &mut seq,
            &mut flat,
        )
        .unwrap();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].pos, [400.0, 370.0]);
        assert_eq!(flat[0].size, [200.0, 60.0]);
        assert!(hit_test_entry(&flat[0], 500.0, 400.0));
        assert!(!hit_test_entry(&flat[0], 50.0, 50.0));
    }
}
