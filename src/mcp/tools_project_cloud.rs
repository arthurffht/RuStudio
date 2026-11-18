//! Rojo project export/build/sourcemap, plugin control, asset import, and Roblox reflection API search MCP tools.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use glam::Vec3;
use rbx_dom_weak::types::{BinaryString, CFrame, Color3, Matrix3, Ref, Variant, Vector3};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_reflection::{DataType, PropertyKind, PropertyTag, Scriptability};
use serde_json::{json, Map, Value};

use crate::doc::Document;
use crate::mcp::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use crate::mcp::server::CallOutcome;
use crate::mcp::variants::{
    instance_path, ref_to_str, resolve_datamodel, resolve_datamodel_mut, resolve_target,
    variant_to_json,
};
use crate::plugin_host::{PluginId, PluginSource, Trigger, UiKind};
use crate::project_export::{export, ExportOptions, CONTAINERS};
use crate::toolbox::api::{fetch_details, AnonHttp, Http, OfflineHttp, SessionHttp};
use crate::toolbox::categories::Kind as AssetKind;
use crate::toolbox::insert::{aabb, download, encode, translate};

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

fn inst_name(doc: &Document, r: Ref) -> &str {
    doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or("")
}

pub fn schemas() -> Vec<ToolDef> {
    vec![
        tool(
            "project_rojo",
            "Export the current place to a Rojo project folder, generate a Rojo sourcemap JSON, or build a .project.json folder into an .rbxl/.rbxlx place file.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["export", "sourcemap", "build"] },
                    "dir": { "type": "string", "description": "Output folder for 'export'." },
                    "project_path": { "type": "string", "description": "Path to default.project.json or project folder for 'sourcemap'/'build'." },
                    "output_path": { "type": "string", "description": "Output .rbxl/.rbxlx file path for 'build'." },
                    "write_path": { "type": "string", "description": "Optional file path to write sourcemap JSON." },
                    "name": { "type": "string", "description": "Rojo project name for 'export'." },
                    "containers": { "type": "array", "items": { "type": "string" }, "description": "Containers/services to export (defaults to Studio defaults)." },
                    "lua": { "type": "string", "enum": ["luau", "lua"], "default": "luau" },
                    "sourcemap": { "type": "boolean", "default": true, "description": "Write sourcemap.json during export." },
                    "include_non_scripts": { "type": "boolean", "default": true, "description": "Include non-script instances in sourcemap." },
                    "place_id": { "type": "integer", "minimum": 0 },
                    "game_id": { "type": "integer", "minimum": 0 },
                    "clean": { "type": "boolean", "default": false, "description": "Clear non-empty target export directory before exporting." },
                    "open": { "type": "boolean", "default": false, "description": "Open exported project or built .rbxl place in Studio afterwards." },
                    "datamodel": { "type": "string", "enum": ["edit", "play"] }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        ),
        tool(
            "plugin_control",
            "List, load local .lua/.luau/.rbxm/.rbxmx plugins, reload/unload loaded plugins, inspect plugin UI (toolbars, buttons, actions, menus, dock widgets), and trigger plugin toolbar buttons or actions.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "load", "reload", "unload", "list_ui", "trigger"] },
                    "path": { "type": "string", "description": "Local plugin file or directory path for 'load'." },
                    "plugin_id": { "type": "integer", "minimum": 0, "description": "Numeric plugin id for reload/unload/trigger(deactivate)." },
                    "name": { "type": "string", "description": "Plugin name matcher for reload/unload when plugin_id is omitted." },
                    "toolbar": { "type": "string", "description": "Toolbar name for triggering a button (defaults to '*' wildcard)." },
                    "button": { "type": "string", "description": "Button id or text for triggering a plugin toolbar button." },
                    "action_id": { "type": "string", "description": "PluginAction id or full ActionId to trigger." },
                    "menu_choice": { "type": "string", "description": "Answer choice string for an open PluginMenu popup." },
                    "deactivate": { "type": "integer", "minimum": 0, "description": "Deactivate plugin id." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        ),
        tool(
            "asset_import",
            "Import local .rbxm/.rbxmx models, .fbx 3D models or animations, .obj meshes, or insert a Roblox asset by numeric ID (or rbxassetid://...) into Workspace or any target parent.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local path to .rbxm, .rbxmx, .fbx, or .obj file." },
                    "asset_id": { "type": ["integer", "string"], "description": "Numeric Roblox asset ID or 'rbxassetid://...' URI." },
                    "parent": { "type": ["string", "object"], "description": "Destination parent instance ref or path (defaults to Workspace)." },
                    "name": { "type": "string", "description": "Optional name override for the imported root instance(s)." },
                    "position": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3, "description": "Optional world [x, y, z] position for imported model/part center." },
                    "mode": { "type": "string", "enum": ["auto", "model", "animation"], "default": "auto", "description": "For .fbx files: import meshes ('model'), KeyframeSequence ('animation'), or auto-detect." },
                    "r15": { "type": "boolean", "default": true, "description": "Include R15 AnimationRigData when importing FBX animations." },
                    "datamodel": { "type": "string", "enum": ["edit", "play"] }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "reflection_api_search",
            "Query Roblox's embedded reflection database and API dump: search classes/enums, inspect full class schemas (properties, types, defaults, scriptability, security, methods, events, callbacks), and list enum items & values.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["search", "inspect_class", "inspect_enum", "list_classes", "list_enums"],
                        "description": "Operation mode (auto-inferred from class_name/enum_name/query when omitted)."
                    },
                    "query": { "type": "string", "description": "Search substring for class/enum/member matching." },
                    "class_name": { "type": "string", "description": "Exact or case-insensitive class name for inspect_class." },
                    "enum_name": { "type": "string", "description": "Exact or case-insensitive enum name for inspect_enum." },
                    "superclass": { "type": "string", "description": "Filter list_classes by inheritance from superclass." },
                    "creatable_only": { "type": "boolean", "default": false },
                    "services_only": { "type": "boolean", "default": false },
                    "include_inherited": { "type": "boolean", "default": true },
                    "include_hidden": { "type": "boolean", "default": true },
                    "include_deprecated": { "type": "boolean", "default": true },
                    "member_query": { "type": "string", "description": "Filter inspect_class members by name/category substring." },
                    "member_kind": { "type": "string", "enum": ["all", "property", "function", "event", "callback"], "default": "all" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
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
        "project_rojo" => handle_project_rojo(ed, ctx, args),
        "plugin_control" => handle_plugin_control(ed, ctx, args),
        "asset_import" => handle_asset_import(ed, ctx, args),
        "reflection_api_search" => handle_reflection_search(args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(res))
}

// ---------------------------------------------------------------------------
// project_rojo
// ---------------------------------------------------------------------------

struct ActiveConfigGuard(Option<Arc<crate::project_config::ProjectConfig>>);

impl ActiveConfigGuard {
    fn capture() -> Self {
        Self(crate::project_config::active())
    }
    fn release(mut self) {
        self.0.take();
    }
}

impl Drop for ActiveConfigGuard {
    fn drop(&mut self) {
        crate::project_config::set_active(self.0.take());
    }
}

fn resolve_project_file(raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw);
    if p.is_file() {
        return Ok(p);
    }
    if p.is_dir() {
        let candidate = p.join("default.project.json");
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(format!(
            "no default.project.json found inside directory '{}'",
            p.display()
        ));
    }
    Err(format!("project path '{}' does not exist", p.display()))
}

fn handle_project_rojo(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let Some(action) = args.get("action").and_then(Value::as_str) else {
        return ToolCallResult::error("action is required");
    };

    match action {
        "export" => {
            let Some(dir_raw) = args.get("dir").and_then(Value::as_str) else {
                return ToolCallResult::error("`dir` is required for project_rojo export");
            };
            let dir = PathBuf::from(dir_raw);
            if dir.as_os_str().is_empty() || dir.parent().is_none() {
                return ToolCallResult::error("refusing to export into a root/empty path");
            }
            if args.get("clean").and_then(Value::as_bool).unwrap_or(false) && dir.exists() {
                if let Err(e) = clear_dir_contents(&dir) {
                    return ToolCallResult::error(format!(
                        "failed clearing target directory '{}': {e}",
                        dir.display()
                    ));
                }
            }

            let doc = match resolve_datamodel(ed, args.get("datamodel").and_then(Value::as_str)) {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(e),
            };
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    dir.file_name()
                        .and_then(|n| n.to_str())
                        .map(str::to_string)
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| doc.title.clone());

            let containers: Vec<String> = match args.get("containers").and_then(Value::as_array) {
                Some(arr) => arr
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                None => CONTAINERS
                    .iter()
                    .filter(|c| c.1)
                    .map(|c| c.0.to_string())
                    .collect(),
            };
            let lua = args
                .get("lua")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("lua"));
            let sourcemap = args
                .get("sourcemap")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let opts = ExportOptions {
                name: name.clone(),
                containers: containers.clone(),
                lua,
                sourcemap,
                place_id: args.get("place_id").and_then(Value::as_u64),
                game_id: args.get("game_id").and_then(Value::as_u64),
            };
            let report = match export(doc, &dir, &opts) {
                Ok(r) => r,
                Err(e) => return ToolCallResult::error(e),
            };

            let open_after = args.get("open").and_then(Value::as_bool).unwrap_or(false);
            if open_after {
                ed.open_project(&report.project_file);
                ctx.request_repaint();
            }

            ToolCallResult::json(&json!({
                "action": "export",
                "name": name,
                "dir": dir.display().to_string(),
                "project_file": report.project_file.display().to_string(),
                "scripts": report.scripts,
                "models": report.models,
                "folders": report.folders,
                "containers": containers,
                "warnings": report.warnings,
                "opened": open_after,
            }))
        }
        "sourcemap" => {
            let include_non_scripts = args
                .get("include_non_scripts")
                .and_then(Value::as_bool)
                .unwrap_or(true);

            let (sourcemap_val, project_file) =
                if let Some(proj_raw) = args.get("project_path").and_then(Value::as_str) {
                    let pf = match resolve_project_file(proj_raw) {
                        Ok(p) => p,
                        Err(e) => return ToolCallResult::error(e),
                    };
                    let _guard = ActiveConfigGuard::capture();
                    let (tmp_doc, st) = match crate::project_sync::open(&pf) {
                        Ok(v) => v,
                        Err(e) => return ToolCallResult::error(e),
                    };
                    (st.sourcemap_json(&tmp_doc, include_non_scripts), pf)
                } else {
                    let Some(st) = ed.project.as_ref() else {
                        return ToolCallResult::error(
                            "no Rojo project is open; pass project_path to generate a sourcemap",
                        );
                    };
                    (
                        st.sourcemap_json(&ed.doc, include_non_scripts),
                        st.file.clone(),
                    )
                };

            let mut written_to = None;
            if let Some(wp) = args.get("write_path").and_then(Value::as_str) {
                let p = PathBuf::from(wp);
                if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    let _ = std::fs::create_dir_all(parent);
                }
                let formatted = match serde_json::to_string_pretty(&sourcemap_val) {
                    Ok(s) => s,
                    Err(e) => return ToolCallResult::error(e.to_string()),
                };
                if let Err(e) = std::fs::write(&p, formatted) {
                    return ToolCallResult::error(format!(
                        "failed writing sourcemap '{}': {e}",
                        p.display()
                    ));
                }
                written_to = Some(p.display().to_string());
            }

            ToolCallResult::json(&json!({
                "action": "sourcemap",
                "project_file": project_file.display().to_string(),
                "written_to": written_to,
                "sourcemap": sourcemap_val,
            }))
        }
        "build" => {
            let Some(proj_raw) = args.get("project_path").and_then(Value::as_str) else {
                return ToolCallResult::error("project_path is required for project_rojo build");
            };
            let pf = match resolve_project_file(proj_raw) {
                Ok(p) => p,
                Err(e) => return ToolCallResult::error(e),
            };
            let guard = ActiveConfigGuard::capture();
            let (mut doc, st) = match crate::project_sync::open(&pf) {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(e),
            };

            let out_path = args
                .get("output_path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let stem = st.name.replace(['/', '\\', ':'], "_");
                    st.root.join(format!("{stem}.rbxl"))
                });
            if let Some(parent) = out_path.parent().filter(|d| !d.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return ToolCallResult::error(format!(
                        "failed creating output directory '{}': {e}",
                        parent.display()
                    ));
                }
            }
            if let Err(e) = doc.save_to(&out_path) {
                return ToolCallResult::error(e);
            }
            let instances = doc.dom.descendants().count();
            let open_after = args.get("open").and_then(Value::as_bool).unwrap_or(false);
            if open_after {
                guard.release();
                ed.open_path(out_path.clone());
                ctx.request_repaint();
            }

            ToolCallResult::json(&json!({
                "action": "build",
                "project_name": st.name,
                "project_file": pf.display().to_string(),
                "output_path": out_path.display().to_string(),
                "instances": instances,
                "warnings": st.warnings,
                "opened": open_after,
            }))
        }
        other => ToolCallResult::error(format!(
            "unknown project_rojo action '{other}', expected export, sourcemap, or build"
        )),
    }
}

fn clear_dir_contents(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(p)?;
        } else {
            std::fs::remove_file(p)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// plugin_control
// ---------------------------------------------------------------------------

fn handle_plugin_control(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let Some(action) = args.get("action").and_then(Value::as_str) else {
        return ToolCallResult::error("action is required");
    };

    match action {
        "list" => {
            let loaded = ed.plugins.loaded();
            let plugins: Vec<Value> = loaded.iter().map(plugin_info_json).collect();
            ToolCallResult::json(&json!({
                "enabled": ed.plugins.enabled(),
                "active_plugin": ed.plugins.active_plugin().map(|p| p.0),
                "count": plugins.len(),
                "plugins": plugins,
            }))
        }
        "load" => {
            if !ed.plugins.enabled() {
                return ToolCallResult::error("plugins are disabled in this Studio session");
            }
            let Some(raw) = args.get("path").and_then(Value::as_str) else {
                return ToolCallResult::error("path is required for plugin_control load");
            };
            let p = PathBuf::from(raw);
            if !p.exists() {
                return ToolCallResult::error(format!("plugin path '{}' does not exist", p.display()));
            }
            if p.is_dir() {
                let results = ed.plugins.load_dir(&p);
                let mut ids = Vec::new();
                let mut errs = Vec::new();
                for r in results {
                    match r {
                        Ok(id) => ids.push(id.0),
                        Err(e) => errs.push(e),
                    }
                }
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "load_dir",
                    "path": p.display().to_string(),
                    "loaded_ids": ids,
                    "errors": errs,
                }));
            }
            match ed.plugins.load(PluginSource::File(p.clone())) {
                Ok(id) => {
                    ctx.request_repaint();
                    ToolCallResult::json(&json!({
                        "action": "load",
                        "plugin_id": id.0,
                        "path": p.display().to_string(),
                        "queued": true,
                    }))
                }
                Err(e) => ToolCallResult::error(e),
            }
        }
        "reload" | "unload" => {
            let id = match resolve_plugin_id(ed, args) {
                Ok(id) => id,
                Err(e) => return ToolCallResult::error(e),
            };
            if action == "reload" {
                ed.plugins.reload(id);
            } else {
                ed.plugins.unload(id);
            }
            ctx.request_repaint();
            ToolCallResult::json(&json!({
                "action": action,
                "plugin_id": id.0,
                "queued": true,
            }))
        }
        "list_ui" => {
            let items = ed.plugins.ui_items();
            let toolbar_names: HashMap<Ref, String> = items
                .iter()
                .filter_map(|item| match &item.kind {
                    UiKind::Toolbar { name } => Some((item.r, name.clone())),
                    _ => None,
                })
                .collect();
            let list: Vec<Value> = items
                .iter()
                .map(|item| {
                    let mut obj = Map::new();
                    obj.insert("plugin_id".into(), json!(item.plugin.0));
                    obj.insert("ref".into(), json!(ref_to_str(item.r)));
                    match &item.kind {
                        UiKind::Mouse => {
                            obj.insert("kind".into(), json!("Mouse"));
                        }
                        UiKind::Toolbar { name } => {
                            obj.insert("kind".into(), json!("Toolbar"));
                            obj.insert("name".into(), json!(name));
                        }
                        UiKind::Button {
                            toolbar,
                            id,
                            tooltip,
                            icon,
                            text,
                        } => {
                            obj.insert("kind".into(), json!("Button"));
                            obj.insert("toolbar_ref".into(), json!(ref_to_str(*toolbar)));
                            if let Some(tb) = toolbar_names.get(toolbar) {
                                obj.insert("toolbar".into(), json!(tb));
                            }
                            obj.insert("id".into(), json!(id));
                            obj.insert("text".into(), json!(text));
                            obj.insert("tooltip".into(), json!(tooltip));
                            obj.insert("icon".into(), json!(icon));
                        }
                        UiKind::Action {
                            id,
                            text,
                            status_tip,
                            icon,
                            allow_binding,
                        } => {
                            obj.insert("kind".into(), json!("Action"));
                            obj.insert("id".into(), json!(id));
                            obj.insert("text".into(), json!(text));
                            obj.insert("status_tip".into(), json!(status_tip));
                            obj.insert("icon".into(), json!(icon));
                            obj.insert("allow_binding".into(), json!(allow_binding));
                        }
                        UiKind::Menu { id, title, icon } => {
                            obj.insert("kind".into(), json!("Menu"));
                            obj.insert("id".into(), json!(id));
                            obj.insert("title".into(), json!(title));
                            obj.insert("icon".into(), json!(icon));
                        }
                        UiKind::DockWidget { id, info } => {
                            obj.insert("kind".into(), json!("DockWidget"));
                            obj.insert("id".into(), json!(id));
                            obj.insert("initial_dock_state".into(), json!(info.initial_dock_state));
                            obj.insert("initial_enabled".into(), json!(info.initial_enabled));
                            obj.insert("override_enabled_restore".into(), json!(info.override_enabled_restore));
                            obj.insert("floating_size".into(), json!([info.float_x, info.float_y]));
                        }
                        UiKind::QWidget { id } => {
                            obj.insert("kind".into(), json!("QWidget"));
                            obj.insert("id".into(), json!(id));
                        }
                    }
                    Value::Object(obj)
                })
                .collect();

            let visible_rects: Vec<Value> = ed
                .plugin_bar
                .rects
                .iter()
                .map(|(k, r)| {
                    json!({
                        "key": k,
                        "rect": [r.min.x, r.min.y, r.width(), r.height()],
                    })
                })
                .collect();

            ToolCallResult::json(&json!({
                "action": "list_ui",
                "count": list.len(),
                "items": list,
                "rendered_button_rects": visible_rects,
            }))
        }
        "trigger" => {
            if !ed.plugins.enabled() {
                return ToolCallResult::error("plugins are disabled in this Studio session");
            }
            if let Some(btn) = args.get("button").and_then(Value::as_str) {
                let tb = args
                    .get("toolbar")
                    .and_then(Value::as_str)
                    .unwrap_or("*")
                    .to_string();
                ed.plugins.trigger(Trigger::Button(tb.clone(), btn.to_string()));
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "trigger",
                    "kind": "button",
                    "toolbar": tb,
                    "button": btn,
                    "queued": true,
                }));
            }
            if let Some(aid) = args.get("action_id").and_then(Value::as_str) {
                ed.plugins.trigger(Trigger::Action(aid.to_string()));
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "trigger",
                    "kind": "action",
                    "action_id": aid,
                    "queued": true,
                }));
            }
            if let Some(mc) = args.get("menu_choice").and_then(Value::as_str) {
                ed.plugins.trigger(Trigger::Menu(mc.to_string()));
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "trigger",
                    "kind": "menu",
                    "menu_choice": mc,
                    "queued": true,
                }));
            }
            if let Some(pid) = args.get("deactivate").and_then(Value::as_u64) {
                ed.plugins.trigger(Trigger::Deactivate(PluginId(pid as u32)));
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "trigger",
                    "kind": "deactivate",
                    "plugin_id": pid,
                    "queued": true,
                }));
            }
            ToolCallResult::error("specify button, action_id, menu_choice, or deactivate for plugin_control trigger")
        }
        other => ToolCallResult::error(format!(
            "unknown plugin_control action '{other}', expected list, load, reload, unload, list_ui, or trigger"
        )),
    }
}

fn plugin_info_json(p: &crate::plugin_host::PluginInfo) -> Value {
    let (source_kind, source_path) = match &p.source {
        PluginSource::File(path) => ("file", Some(path.display().to_string())),
        PluginSource::Nested { path, .. } => ("nested", Some(path.display().to_string())),
        PluginSource::Cloud { path, .. } => ("cloud", Some(path.display().to_string())),
        PluginSource::Tree { .. } => ("tree", None),
    };
    json!({
        "id": p.id.0,
        "name": p.name,
        "source_kind": source_kind,
        "path": source_path,
        "asset_id": p.source.asset_id(),
        "running": p.running,
        "errors": p.errors,
    })
}

fn resolve_plugin_id(ed: &crate::app::Editor, args: &Value) -> Result<PluginId, String> {
    if let Some(id) = args.get("plugin_id").and_then(Value::as_u64) {
        return Ok(PluginId(id as u32));
    }
    let Some(name) = args.get("name").and_then(Value::as_str) else {
        return Err("plugin_id or name is required".into());
    };
    let loaded = ed.plugins.loaded();
    if let Some(exact) = loaded.iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
        return Ok(exact.id);
    }
    let needle = name.to_ascii_lowercase();
    if let Some(part) = loaded
        .iter()
        .find(|p| p.name.to_ascii_lowercase().contains(&needle))
    {
        return Ok(part.id);
    }
    Err(format!("no loaded plugin matches '{name}'"))
}

// ---------------------------------------------------------------------------
// asset_import
// ---------------------------------------------------------------------------

fn handle_asset_import(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let has_path = args.get("path").and_then(Value::as_str).is_some();
    let has_asset_id = args.get("asset_id").is_some_and(|v| !v.is_null());
    if !has_path && !has_asset_id {
        return ToolCallResult::error("specify either `path` or `asset_id`");
    }

    let name_override = args.get("name").and_then(Value::as_str).map(str::to_string);
    let position = match args.get("position") {
        Some(v) if !v.is_null() => match parse_pos3(v) {
            Ok(p) => Some(p),
            Err(e) => return ToolCallResult::error(e),
        },
        _ => None,
    };
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    let r15 = args.get("r15").and_then(Value::as_bool).unwrap_or(true);

    // Build HTTP handle upfront if importing a cloud asset id (cannot borrow ed twice below).
    let http_client: Option<Arc<dyn Http>> = if !has_path {
        Some(build_http_client(ed))
    } else {
        None
    };

    let dm_arg = args.get("datamodel").and_then(Value::as_str);
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

    let parent = match args.get("parent").filter(|v| !v.is_null()) {
        Some(pv) => match resolve_target(doc, pv) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(e),
        },
        None => match doc.find_child(doc.dom.root_ref(), "Workspace") {
            Some(ws) => ws,
            None => doc.dom.root_ref(),
        },
    };

    let outcome = if let Some(path_raw) = args.get("path").and_then(Value::as_str) {
        import_local_path(
            doc,
            Path::new(path_raw),
            parent,
            name_override.as_deref(),
            position,
            mode,
            r15,
        )
    } else {
        let id = match parse_asset_id(args.get("asset_id").unwrap()) {
            Ok(id) => id,
            Err(e) => return ToolCallResult::error(e),
        };
        import_cloud_asset(
            doc,
            http_client.unwrap().as_ref(),
            id,
            parent,
            name_override.as_deref(),
            position,
            mode,
            r15,
        )
    };

    match outcome {
        Ok(mut payload) => {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("datamodel".into(), json!(dm_name));
            }
            ctx.request_repaint();
            ToolCallResult::json(&payload)
        }
        Err(e) => ToolCallResult::error(e),
    }
}

fn build_http_client(ed: &crate::app::Editor) -> Arc<dyn Http> {
    if let Some(c) = ed.account.shared_client() {
        if let Ok(guard) = c.try_lock() {
            if let Some(client) = guard.as_ref() {
                let uid = client.user_id();
                if std::env::var_os("RUSTUDIO_TOOLBOX_OFFLINE").is_some() {
                    return Arc::new(OfflineHttp(uid));
                }
                if std::env::var_os("RUSTUDIO_NO_SESSION").is_none() {
                    return Arc::new(SessionHttp(client.session.downloader()));
                }
            }
        }
    }
    Arc::new(AnonHttp)
}

fn parse_asset_id(v: &Value) -> Result<u64, String> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        let digits = trimmed
            .strip_prefix("rbxassetid://")
            .or_else(|| trimmed.strip_prefix("http://www.roblox.com/asset/?id="))
            .or_else(|| trimmed.strip_prefix("https://www.roblox.com/asset/?id="))
            .unwrap_or(trimmed);
        return digits
            .parse::<u64>()
            .map_err(|_| format!("invalid asset_id '{s}'"));
    }
    Err("asset_id must be a positive integer or rbxassetid:// string".into())
}

fn parse_pos3(v: &Value) -> Result<Vec3, String> {
    let arr = v.as_array().filter(|a| a.len() == 3).ok_or("position must be [x, y, z]")?;
    let x = arr[0].as_f64().ok_or("position[0] must be number")? as f32;
    let y = arr[1].as_f64().ok_or("position[1] must be number")? as f32;
    let z = arr[2].as_f64().ok_or("position[2] must be number")? as f32;
    let p = Vec3::new(x, y, z);
    if !p.is_finite() {
        return Err("position must be finite".into());
    }
    Ok(p)
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn import_local_path(
    doc: &mut Document,
    path: &Path,
    parent: Ref,
    name_override: Option<&str>,
    position: Option<Vec3>,
    mode: &str,
    r15: bool,
) -> Result<Value, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("failed reading '{}': {e}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Imported");
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let mut val = if ext == "rbxm" || ext == "rbxmx" || Document::decode(&bytes).is_ok() {
        import_rbxm_bytes(doc, &bytes, parent, name_override, position)?
    } else if ext == "fbx" || crate::fbx::is_fbx(&bytes) {
        import_fbx_bytes(doc, &bytes, stem, parent, name_override, position, mode, r15)?
    } else if ext == "obj" {
        import_obj_bytes(doc, &bytes, stem, parent, name_override, position)?
    } else {
        return Err(format!(
            "unsupported asset file extension '.{ext}' (expected .rbxm, .rbxmx, .fbx, or .obj)"
        ));
    };

    if let Some(obj) = val.as_object_mut() {
        obj.insert("source_path".into(), json!(path.display().to_string()));
    }
    Ok(val)
}

#[allow(clippy::too_many_arguments)]
fn import_cloud_asset(
    doc: &mut Document,
    http: &dyn Http,
    id: u64,
    parent: Ref,
    name_override: Option<&str>,
    position: Option<Vec3>,
    mode: &str,
    r15: bool,
) -> Result<Value, String> {
    let asset_meta = fetch_details(http, &[id], &|| false)
        .ok()
        .and_then(|list| list.into_iter().next());
    let default_name = name_override
        .map(str::to_string)
        .or_else(|| asset_meta.as_ref().map(|a| a.name.clone()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Asset_{id}"));

    let bytes = download(http, id, &|| false).map_err(|f| f.describe())?;

    let mut out = if Document::decode(&bytes).is_ok() {
        import_rbxm_bytes(doc, &bytes, parent, name_override, position)?
    } else if crate::fbx::is_fbx(&bytes) {
        import_fbx_bytes(
            doc,
            &bytes,
            &default_name,
            parent,
            Some(&default_name),
            position,
            mode,
            r15,
        )?
    } else if let Ok(mesh) = crate::mesh::parse(&bytes) {
        let size = mesh_extent(&mesh.positions);
        let pos = position.unwrap_or(Vec3::ZERO);
        let builder = InstanceBuilder::new("MeshPart")
            .with_name(default_name.clone())
            .with_property("MeshId", Variant::ContentId(format!("rbxassetid://{id}").into()))
            .with_property("Size", Vector3::new(size.x, size.y, size.z))
            .with_property("InitialSize", Vector3::new(size.x, size.y, size.z))
            .with_property("MeshSize", Vector3::new(size.x, size.y, size.z))
            .with_property("Anchored", true)
            .with_property(
                "CFrame",
                CFrame::new(Vector3::new(pos.x, pos.y, pos.z), Matrix3::identity()),
            );
        let inserted = insert_builder_as_undoable(doc, builder, parent, "Import cloud MeshPart")?;
        json_imported_refs(doc, &[inserted], "MeshPart")
    } else if looks_like_image(&bytes) {
        let cls = doc.class_of(parent).map(|c| c.as_str()).unwrap_or("");
        let builder = if crate::doc::is_a(cls, "GuiObject") || cls == "ScreenGui" || cls == "StarterGui" {
            InstanceBuilder::new("ImageLabel")
                .with_name(default_name)
                .with_property("Image", Variant::ContentId(format!("rbxassetid://{id}").into()))
        } else {
            InstanceBuilder::new("Decal")
                .with_name(default_name)
                .with_property("Texture", Variant::ContentId(format!("rbxassetid://{id}").into()))
        };
        let inserted = insert_builder_as_undoable(doc, builder, parent, "Import cloud Image/Decal")?;
        json_imported_refs(doc, &[inserted], "Image")
    } else if looks_like_audio(&bytes)
        || asset_meta.as_ref().is_some_and(|a| AssetKind::from_asset_type(a.type_id) == Some(AssetKind::Audio))
    {
        let builder = InstanceBuilder::new("Sound")
            .with_name(default_name)
            .with_property("SoundId", Variant::ContentId(format!("rbxassetid://{id}").into()));
        let inserted = insert_builder_as_undoable(doc, builder, parent, "Import cloud Sound")?;
        json_imported_refs(doc, &[inserted], "Audio")
    } else {
        return Err(format!(
            "downloaded asset {id} ({} bytes), but its format is not a supported model/mesh/image/audio",
            bytes.len()
        ));
    };

    if let Some(obj) = out.as_object_mut() {
        obj.insert("asset_id".into(), json!(id));
        if let Some(meta) = asset_meta {
            obj.insert("asset_name".into(), json!(meta.name));
        }
    }
    Ok(out)
}

fn import_rbxm_bytes(
    doc: &mut Document,
    bytes: &[u8],
    parent: Ref,
    name_override: Option<&str>,
    position: Option<Vec3>,
) -> Result<Value, String> {
    let mut temp = Document::decode(bytes)?;
    let roots: Vec<Ref> = temp.root().children().to_vec();
    if roots.is_empty() {
        return Err("model file contains no root instances".into());
    }
    if let Some(new_name) = name_override {
        if roots.len() == 1 {
            if let Some(inst) = temp.get_by_ref_mut(roots[0]) {
                inst.name = new_name.to_string();
            }
        }
    }
    if let Some(target_pos) = position {
        if let Some((lo, hi)) = aabb(&temp, &roots) {
            let center = (lo + hi) * 0.5;
            translate(&mut temp, &roots, target_pos - center);
        }
    }
    let encoded = encode(&temp, &roots);
    let inserted = doc.paste(&encoded, parent, "MCP import model");
    if inserted.is_empty() {
        return Err("failed pasting decoded model instances".into());
    }
    Ok(json_imported_refs(doc, &inserted, "Model"))
}

#[allow(clippy::too_many_arguments)]
fn import_fbx_bytes(
    doc: &mut Document,
    bytes: &[u8],
    stem: &str,
    parent: Ref,
    name_override: Option<&str>,
    position: Option<Vec3>,
    mode: &str,
    r15: bool,
) -> Result<Value, String> {
    if mode == "animation" {
        return import_fbx_animation(doc, bytes, stem, parent, name_override, r15);
    }

    let nodes = crate::fbx::import(bytes);
    match nodes {
        Ok(nodes) if nodes.iter().any(|n| n.mesh.is_some()) => {
            let id = fnv1a_64(bytes);
            let mut dom = WeakDom::new(InstanceBuilder::new("Folder").with_name("Temp"));
            let temp_root = dom.root_ref();
            let model_name = name_override.unwrap_or(stem).to_string();
            let model = dom.insert(
                temp_root,
                InstanceBuilder::new("Model").with_name(model_name),
            );
            let mut made: Vec<Ref> = Vec::with_capacity(nodes.len());
            for (i, n) in nodes.iter().enumerate() {
                let p = n.parent.and_then(|idx| made.get(idx).copied()).unwrap_or(model);
                let r = match &n.mesh {
                    Some(g) => {
                        let mesh_uri = format!("rbxtemp://rustudio/fbx/{id}/{i}");
                        crate::assets::register_generated(&mesh_uri, crate::fbx::mesh_v2(g));
                        let (lo, hi) = points_aabb(&g.positions);
                        let size = (hi - lo).max(Vec3::splat(0.001));
                        let center = (hi + lo) * 0.5;
                        let color = n.color.unwrap_or([163.0 / 255.0, 162.0 / 255.0, 165.0 / 255.0]);
                        let mut b = InstanceBuilder::new("MeshPart")
                            .with_name(n.name.clone())
                            .with_property("MeshId", Variant::ContentId(mesh_uri.into()))
                            .with_property("Size", Vector3::new(size.x, size.y, size.z))
                            .with_property("InitialSize", Vector3::new(size.x, size.y, size.z))
                            .with_property("MeshSize", Vector3::new(size.x, size.y, size.z))
                            .with_property(
                                "CFrame",
                                CFrame::new(
                                    Vector3::new(center.x, center.y, center.z),
                                    Matrix3::identity(),
                                ),
                            )
                            .with_property("Color", Color3::new(color[0], color[1], color[2]))
                            .with_property("Anchored", true);
                        if let Some((_, tex)) = &n.texture {
                            let tex_uri = format!("rbxtemp://rustudio/fbx/{id}/{i}.tex");
                            crate::assets::register_generated(&tex_uri, tex.clone());
                            b = b.with_property("TextureID", Variant::ContentId(tex_uri.into()));
                        }
                        dom.insert(p, b)
                    }
                    None => dom.insert(p, InstanceBuilder::new("Model").with_name(n.name.clone())),
                };
                made.push(r);
            }
            if let Some(target_pos) = position {
                if let Some((lo, hi)) = aabb(&dom, &[model]) {
                    let center = (lo + hi) * 0.5;
                    translate(&mut dom, &[model], target_pos - center);
                }
            }
            let encoded = encode(&dom, &[model]);
            let inserted = doc.paste(&encoded, parent, "MCP import FBX");
            Ok(json_imported_refs(doc, &inserted, "FBXModel"))
        }
        Ok(_) if mode == "auto" => import_fbx_animation(doc, bytes, stem, parent, name_override, r15),
        Ok(_) => Err("FBX contains no mesh geometry".into()),
        Err(e) if mode == "auto" => {
            import_fbx_animation(doc, bytes, stem, parent, name_override, r15).map_err(|_| e)
        }
        Err(e) => Err(e),
    }
}

fn import_fbx_animation(
    doc: &mut Document,
    bytes: &[u8],
    stem: &str,
    parent: Ref,
    name_override: Option<&str>,
    r15: bool,
) -> Result<Value, String> {
    let clip = crate::fbx_anim::read(bytes, r15)?;
    let ease = (
        enum_variant("PoseEasingStyle", "Linear"),
        enum_variant("PoseEasingDirection", "In"),
    );
    let seq_name = name_override
        .map(str::to_string)
        .unwrap_or_else(|| format!("{stem}_{}", clip.name));
    let mut seq = InstanceBuilder::new("KeyframeSequence")
        .with_name(seq_name)
        .with_property("Loop", Variant::Bool(true))
        .with_property("Priority", enum_variant("AnimationPriority", "Action"))
        .with_property("AuthoredHipHeight", Variant::Float32(2.0));

    fn pose_builder(
        clip: &crate::fbx_anim::Clip,
        i: usize,
        frame: usize,
        ease: &(Variant, Variant),
    ) -> InstanceBuilder {
        let b = &clip.bones[i];
        let xf = &b.poses[frame];
        let cf = crate::math::Xform {
            pos: xf.pos.as_vec3(),
            rot: xf.rot.as_mat3(),
        }
        .to_cframe();
        let mut p = InstanceBuilder::new("Pose")
            .with_name(b.name.clone())
            .with_property("CFrame", Variant::CFrame(cf))
            .with_property("Weight", Variant::Float32(1.0))
            .with_property("EasingStyle", ease.0.clone())
            .with_property("EasingDirection", ease.1.clone());
        for (k, c) in clip.bones.iter().enumerate() {
            if c.parent == Some(i) {
                p = p.with_child(pose_builder(clip, k, frame, ease));
            }
        }
        p
    }

    let last = clip.times.len().saturating_sub(1);
    for (f, t) in clip.times.iter().enumerate() {
        let mut kf = InstanceBuilder::new("Keyframe")
            .with_name(if f == last { "End" } else { "Keyframe" })
            .with_property("Time", Variant::Float32(*t as f32));
        for (i, b) in clip.bones.iter().enumerate() {
            if b.parent.is_none() {
                kf = kf.with_child(pose_builder(&clip, i, f, &ease));
            }
        }
        if clip.file_root {
            let turn = crate::math::Xform {
                pos: Vec3::ZERO,
                rot: glam::Mat3::from_diagonal(Vec3::new(-1.0, 1.0, -1.0)),
            }
            .to_cframe();
            kf = kf.with_child(
                InstanceBuilder::new("Pose")
                    .with_name(stem.to_string())
                    .with_property("CFrame", Variant::CFrame(turn))
                    .with_property("Weight", Variant::Float32(1.0))
                    .with_property("EasingStyle", ease.0.clone())
                    .with_property("EasingDirection", ease.1.clone()),
            );
        }
        seq = seq.with_child(kf);
    }
    if r15 {
        seq = seq.with_child(anim_rig_data(clip.lower_torso_height, clip.file_root));
    }
    let inserted = insert_builder_as_undoable(doc, seq, parent, "MCP import FBX Animation")?;
    let mut out = json_imported_refs(doc, &[inserted], "KeyframeSequence");
    out.as_object_mut()
        .unwrap()
        .insert("keyframes".into(), json!(clip.times.len()));
    Ok(out)
}

fn anim_rig_data(lower_torso_height: Option<f64>, one: bool) -> InstanceBuilder {
    let le = |v: &[u32]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
    let count = if one { 1 } else { 2 };
    let (label, mut name, mut parent) = if one {
        (le(&[2, 1, 0]), le(&[1, 1, 0]), le(&[1, 1]))
    } else {
        (le(&[2, 2, 0, 0]), le(&[1, 2, 0, 16]), le(&[1, 2]))
    };
    if one {
        parent.extend_from_slice(&[0, 0]);
    } else {
        name.extend_from_slice(b"HumanoidRootPart");
        parent.extend_from_slice(&[0, 0, 1, 0]);
    }
    let mut xf = le(&[2, count]);
    for _ in 0..count {
        for f in [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0] {
            xf.extend_from_slice(&f.to_le_bytes());
        }
    }
    let floor = lower_torso_height.map(|h| -(h as f32) - 1.0).unwrap_or(-1.0);
    let mut generic = le(&[3]);
    for f in [floor, 2.0, 2.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
        generic.extend_from_slice(&f.to_le_bytes());
    }
    generic.extend_from_slice(&[0; 14]);
    let mut b = InstanceBuilder::new("AnimationRigData").with_name("AnimationRigData");
    for (k, v) in [
        ("label", label),
        ("name", name),
        ("parent", parent),
        ("transform", xf.clone()),
        ("preTransform", xf.clone()),
        ("postTransform", xf),
        ("Generic", generic),
    ] {
        b = b.with_property(k, Variant::BinaryString(BinaryString::from(v)));
    }
    b
}

fn enum_variant(enum_name: &str, item: &str) -> Variant {
    let val = crate::doc::db()
        .enums
        .get(enum_name)
        .and_then(|e| e.items.get(item).copied())
        .unwrap_or(0);
    Variant::Enum(rbx_dom_weak::types::Enum::from_u32(val))
}

fn import_obj_bytes(
    doc: &mut Document,
    bytes: &[u8],
    stem: &str,
    parent: Ref,
    name_override: Option<&str>,
    position: Option<Vec3>,
) -> Result<Value, String> {
    let mesh = crate::mesh::parse(bytes)?;
    if mesh.positions.is_empty() {
        return Err("OBJ mesh contains no triangles".into());
    }
    let id = fnv1a_64(bytes);
    let mesh_uri = format!("rbxtemp://rustudio/obj/{id}.obj");
    crate::assets::register_generated(&mesh_uri, bytes.to_vec());

    let (lo, hi) = points_aabb(&mesh.positions);
    let size = (hi - lo).max(Vec3::splat(0.001));
    let pos = position.unwrap_or((lo + hi) * 0.5);
    let name = name_override.unwrap_or(stem).to_string();

    let builder = InstanceBuilder::new("MeshPart")
        .with_name(name)
        .with_property("MeshId", Variant::ContentId(mesh_uri.into()))
        .with_property("Size", Vector3::new(size.x, size.y, size.z))
        .with_property("InitialSize", Vector3::new(size.x, size.y, size.z))
        .with_property("MeshSize", Vector3::new(size.x, size.y, size.z))
        .with_property("Anchored", true)
        .with_property(
            "CFrame",
            CFrame::new(Vector3::new(pos.x, pos.y, pos.z), Matrix3::identity()),
        );
    let inserted = insert_builder_as_undoable(doc, builder, parent, "MCP import OBJ")?;
    Ok(json_imported_refs(doc, &[inserted], "OBJMesh"))
}

fn insert_builder_as_undoable(
    doc: &mut Document,
    builder: InstanceBuilder,
    parent: Ref,
    label: &str,
) -> Result<Ref, String> {
    let mut dom = WeakDom::new(InstanceBuilder::new("Folder").with_name("Temp"));
    let r = dom.insert(dom.root_ref(), builder);
    let bytes = encode(&dom, &[r]);
    let pasted = doc.paste(&bytes, parent, label);
    pasted
        .into_iter()
        .next()
        .ok_or_else(|| "failed pasting imported instance into document".into())
}

fn points_aabb(pts: &[Vec3]) -> (Vec3, Vec3) {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for &p in pts {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    if !lo.is_finite() || !hi.is_finite() {
        (Vec3::ZERO, Vec3::ONE)
    } else {
        (lo, hi)
    }
}

fn mesh_extent(pts: &[Vec3]) -> Vec3 {
    let (lo, hi) = points_aabb(pts);
    (hi - lo).max(Vec3::splat(0.001))
}

fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
        || b.starts_with(&[0xff, 0xd8, 0xff])
        || b.starts_with(b"GIF8")
        || (b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

fn looks_like_audio(b: &[u8]) -> bool {
    b.starts_with(b"OggS")
        || b.starts_with(b"ID3")
        || b.starts_with(b"fLaC")
        || (b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WAVE")
        || b.starts_with(&[0xff, 0xfb])
}

fn json_imported_refs(doc: &Document, refs: &[Ref], kind: &str) -> Value {
    let instances: Vec<Value> = refs
        .iter()
        .map(|&r| {
            json!({
                "ref": ref_to_str(r),
                "name": inst_name(doc, r),
                "class": doc.class_of(r).map(|c| c.as_str()).unwrap_or(""),
                "path": instance_path(doc, r),
            })
        })
        .collect();
    let bounds = aabb(&doc.dom, refs).map(|(lo, hi)| {
        json!({
            "min": [lo.x, lo.y, lo.z],
            "max": [hi.x, hi.y, hi.z],
        })
    });
    json!({
        "imported_kind": kind,
        "count": instances.len(),
        "instances": instances,
        "bounds": bounds,
    })
}

// ---------------------------------------------------------------------------
// reflection_api_search
// ---------------------------------------------------------------------------

fn handle_reflection_search(args: &Value) -> ToolCallResult {
    let inferred = if args.get("class_name").and_then(Value::as_str).is_some() {
        "inspect_class"
    } else if args.get("enum_name").and_then(Value::as_str).is_some() {
        "inspect_enum"
    } else {
        "search"
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or(inferred);
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 500) as usize;

    match action {
        "search" => ToolCallResult::json(&search_classes_and_enums(query, limit)),
        "list_classes" => ToolCallResult::json(&list_classes(args, query, limit)),
        "inspect_class" => {
            let Some(cls) = args.get("class_name").and_then(Value::as_str) else {
                return ToolCallResult::error("class_name is required for inspect_class");
            };
            match inspect_class_schema(cls, args) {
                Ok(v) => ToolCallResult::json(&v),
                Err(e) => ToolCallResult::error(e),
            }
        }
        "list_enums" => ToolCallResult::json(&list_enums(query, limit)),
        "inspect_enum" => {
            let Some(ename) = args.get("enum_name").and_then(Value::as_str) else {
                return ToolCallResult::error("enum_name is required for inspect_enum");
            };
            match inspect_enum_schema(ename) {
                Ok(v) => ToolCallResult::json(&v),
                Err(e) => ToolCallResult::error(e),
            }
        }
        other => ToolCallResult::error(format!(
            "unknown reflection_api_search action '{other}', expected search, inspect_class, inspect_enum, list_classes, or list_enums"
        )),
    }
}

fn search_classes_and_enums(query: &str, limit: usize) -> Value {
    let db = crate::doc::db();
    let api = crate::api::get();
    let needle = query.to_ascii_lowercase();

    let mut class_names: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (name, c) in &db.classes {
        class_names.insert(name.to_string(), c.superclass.as_ref().map(|s| s.to_string()));
    }
    if let Some(a) = api {
        for (name, c) in &a.classes {
            class_names
                .entry(name.clone())
                .or_insert_with(|| c.superclass.clone());
        }
    }

    let mut classes: Vec<Value> = class_names
        .iter()
        .filter(|(n, _)| needle.is_empty() || n.to_ascii_lowercase().contains(&needle))
        .map(|(n, sup)| {
            let exact = !needle.is_empty() && n.eq_ignore_ascii_case(&needle);
            let starts = !needle.is_empty() && n.to_ascii_lowercase().starts_with(&needle);
            let prop_count = db.classes.get(n.as_str()).map(|c| c.properties.len()).unwrap_or(0);
            let api_c = api.and_then(|a| a.classes.get(n));
            (
                (!exact, !starts, n.clone()),
                json!({
                    "name": n,
                    "superclass": sup,
                    "property_count": prop_count,
                    "creatable": api_c.map(|c| c.creatable).unwrap_or(true),
                    "service": api_c.map(|c| c.service).unwrap_or(false),
                }),
            )
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .take(limit)
        .collect();
    if classes.len() > limit {
        classes.truncate(limit);
    }

    let mut enum_names: HashSet<String> = db.enums.keys().map(|k| k.to_string()).collect();
    if let Some(a) = api {
        enum_names.extend(a.enums.keys().cloned());
    }
    let mut enums_vec: Vec<String> = enum_names
        .into_iter()
        .filter(|n| needle.is_empty() || n.to_ascii_lowercase().contains(&needle))
        .collect();
    enums_vec.sort_by_key(|n| {
        (
            !(!needle.is_empty() && n.eq_ignore_ascii_case(&needle)),
            !(!needle.is_empty() && n.to_ascii_lowercase().starts_with(&needle)),
            n.clone(),
        )
    });
    let enums: Vec<Value> = enums_vec
        .into_iter()
        .take(limit)
        .map(|n| {
            let count = db
                .enums
                .get(n.as_str())
                .map(|e| e.items.len())
                .or_else(|| api.and_then(|a| a.enums.get(&n)).map(|v| v.len()))
                .unwrap_or(0);
            json!({ "name": n, "item_count": count })
        })
        .collect();

    json!({
        "query": query,
        "classes": classes,
        "enums": enums,
    })
}

fn list_classes(args: &Value, query: &str, limit: usize) -> Value {
    let db = crate::doc::db();
    let api = crate::api::get();
    let needle = query.to_ascii_lowercase();
    let sup_filter = args.get("superclass").and_then(Value::as_str);
    let creatable_only = args
        .get("creatable_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let services_only = args
        .get("services_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut names: Vec<String> = db.classes.keys().map(|k| k.to_string()).collect();
    if let Some(a) = api {
        for k in a.classes.keys() {
            if !db.classes.contains_key(k.as_str()) {
                names.push(k.clone());
            }
        }
    }
    names.sort();

    let mut matched = Vec::new();
    for n in names {
        if !needle.is_empty() && !n.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        if let Some(req_sup) = sup_filter {
            let is_sub = db
                .classes
                .get(n.as_str())
                .is_some_and(|c| db.superclasses_iter(c).any(|s| s.name.eq_ignore_ascii_case(req_sup)));
            if !is_sub {
                continue;
            }
        }
        let db_cls = db.classes.get(n.as_str());
        let api_cls = api.and_then(|a| a.classes.get(&n));
        let is_service = api_cls
            .map(|c| c.service)
            .or_else(|| db_cls.map(|c| c.tags.contains(&rbx_reflection::ClassTag::Service)))
            .unwrap_or(false);
        let is_creatable = api_cls
            .map(|c| c.creatable)
            .or_else(|| {
                db_cls.map(|c| {
                    !c.tags.contains(&rbx_reflection::ClassTag::NotCreatable)
                        && !c.tags.contains(&rbx_reflection::ClassTag::Service)
                })
            })
            .unwrap_or(false);

        if creatable_only && !is_creatable {
            continue;
        }
        if services_only && !is_service {
            continue;
        }

        matched.push(json!({
            "name": n,
            "superclass": db_cls.and_then(|c| c.superclass.as_deref()).or_else(|| api_cls.and_then(|c| c.superclass.as_deref())),
            "creatable": is_creatable,
            "service": is_service,
        }));
        if matched.len() >= limit {
            break;
        }
    }

    json!({
        "count": matched.len(),
        "classes": matched,
    })
}

fn inspect_class_schema(raw_name: &str, args: &Value) -> Result<Value, String> {
    let db = crate::doc::db();
    let api = crate::api::get();

    // Case-insensitive lookup across both databases.
    let canonical_name = db
        .classes
        .keys()
        .find(|k| k.eq_ignore_ascii_case(raw_name))
        .map(|k| k.to_string())
        .or_else(|| {
            api.and_then(|a| {
                a.classes
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(raw_name))
                    .cloned()
            })
        })
        .ok_or_else(|| format!("unknown Roblox class '{raw_name}'"))?;

    let include_inherited = args
        .get("include_inherited")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let include_hidden = args
        .get("include_hidden")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let include_deprecated = args
        .get("include_deprecated")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let member_query = args
        .get("member_query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let member_kind = args
        .get("member_kind")
        .and_then(Value::as_str)
        .unwrap_or("all");

    let db_cls = db.classes.get(canonical_name.as_str());
    let api_cls = api.and_then(|a| a.classes.get(&canonical_name));

    let mut chain: Vec<String> = Vec::new();
    if let Some(c) = db_cls {
        if include_inherited {
            for sup in db.superclasses_iter(c) {
                chain.push(sup.name.to_string());
            }
        } else {
            chain.push(c.name.to_string());
        }
    } else if let Some(a) = api {
        let mut cur = Some(canonical_name.as_str());
        while let Some(cname) = cur {
            chain.push(cname.to_string());
            if !include_inherited {
                break;
            }
            cur = a.classes.get(cname).and_then(|c| c.superclass.as_deref());
        }
    }

    let mut properties: Vec<Value> = Vec::new();
    let mut seen_props = HashSet::new();
    let mut functions: Vec<Value> = Vec::new();
    let mut events: Vec<Value> = Vec::new();
    let mut callbacks: Vec<Value> = Vec::new();
    let mut seen_members = HashSet::new();

    for cname in &chain {
        let cur_db = db.classes.get(cname.as_str());
        let cur_api = api.and_then(|a| a.classes.get(cname));

        if let Some(c) = cur_db {
            let mut keys: Vec<&str> = c.properties.keys().map(|k| k.as_ref()).collect();
            keys.sort();
            for pname in keys {
                if !seen_props.insert(pname.to_string()) {
                    continue;
                }
                let p = &c.properties[pname];
                let api_m = cur_api.and_then(|ac| ac.members.get(pname));
                let deprecated = p.tags.contains(&PropertyTag::Deprecated)
                    || api_m.is_some_and(|m| m.deprecated);
                let hidden = p.tags.contains(&PropertyTag::Hidden)
                    || api_m.is_some_and(|m| m.hidden);
                if (!include_deprecated && deprecated) || (!include_hidden && hidden) {
                    continue;
                }
                let category = api_m.map(|m| m.category.as_str()).unwrap_or("");
                if !member_query.is_empty()
                    && !pname.to_ascii_lowercase().contains(&member_query)
                    && !category.to_ascii_lowercase().contains(&member_query)
                {
                    continue;
                }
                if member_kind != "all" && member_kind != "property" {
                    continue;
                }

                let (type_label, enum_name) = match &p.data_type {
                    DataType::Value(vt) => (format!("{vt:?}"), None),
                    DataType::Enum(e) => (format!("Enum.{e}"), Some(e.to_string())),
                    other => (format!("{other:?}"), None),
                };
                let (kind_label, alias_for) = match &p.kind {
                    PropertyKind::Canonical { .. } => ("Canonical", None::<&str>),
                    PropertyKind::Alias { alias_for } => ("Alias", Some(alias_for.as_ref())),
                    _ => ("Other", None::<&str>),
                };
                let read_only = p.tags.contains(&PropertyTag::ReadOnly)
                    || api_m.is_some_and(|m| m.read_only)
                    || matches!(p.scriptability, Scriptability::Read | Scriptability::None);

                let default_val = c
                    .default_properties
                    .get(pname)
                    .map(variant_to_json)
                    .unwrap_or(Value::Null);

                properties.push(json!({
                    "name": pname,
                    "declaring_class": cname,
                    "type": type_label,
                    "enum_name": enum_name,
                    "kind": kind_label,
                    "alias_for": alias_for,
                    "default": default_val,
                    "scriptability": format!("{:?}", p.scriptability),
                    "security": api_m.map(|m| m.security.as_str()).unwrap_or("None"),
                    "read_only": read_only,
                    "can_load": api_m.map(|m| m.can_load).unwrap_or(!p.tags.contains(&PropertyTag::ReadOnly)),
                    "deprecated": deprecated,
                    "hidden": hidden,
                    "category": category,
                    "tags": p.tags.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>(),
                }));
            }
        }

        if let Some(ac) = cur_api {
            for mname in &ac.order {
                let Some(m) = ac.members.get(mname) else {
                    continue;
                };
                if m.kind == crate::api::Kind::Property {
                    if !seen_props.contains(mname) && (member_kind == "all" || member_kind == "property") {
                        if (!include_deprecated && m.deprecated) || (!include_hidden && m.hidden) {
                            continue;
                        }
                        if !member_query.is_empty()
                            && !mname.to_ascii_lowercase().contains(&member_query)
                            && !m.category.to_ascii_lowercase().contains(&member_query)
                        {
                            continue;
                        }
                        seen_props.insert(mname.clone());
                        properties.push(json!({
                            "name": mname,
                            "declaring_class": cname,
                            "security": m.security,
                            "read_only": m.read_only,
                            "can_load": m.can_load,
                            "deprecated": m.deprecated,
                            "hidden": m.hidden,
                            "category": m.category,
                        }));
                    }
                    continue;
                }

                if !seen_members.insert((m.kind.label(), mname.clone())) {
                    continue;
                }
                if (!include_deprecated && m.deprecated) || (!include_hidden && m.hidden) {
                    continue;
                }
                if !member_query.is_empty()
                    && !mname.to_ascii_lowercase().contains(&member_query)
                    && !m.category.to_ascii_lowercase().contains(&member_query)
                {
                    continue;
                }
                if member_kind != "all" && member_kind != m.kind.label() {
                    continue;
                }

                let entry = json!({
                    "name": mname,
                    "declaring_class": cname,
                    "security": m.security,
                    "deprecated": m.deprecated,
                    "hidden": m.hidden,
                    "category": m.category,
                });
                match m.kind {
                    crate::api::Kind::Function => functions.push(entry),
                    crate::api::Kind::Event => events.push(entry),
                    crate::api::Kind::Callback => callbacks.push(entry),
                    crate::api::Kind::Property => {}
                }
            }
        }
    }

    Ok(json!({
        "name": canonical_name,
        "superclass": db_cls.and_then(|c| c.superclass.as_deref()).or_else(|| api_cls.and_then(|c| c.superclass.as_deref())),
        "inheritance_chain": chain,
        "creatable": api_cls.map(|c| c.creatable).unwrap_or_else(|| db_cls.is_some_and(|c| !c.tags.contains(&rbx_reflection::ClassTag::NotCreatable) && !c.tags.contains(&rbx_reflection::ClassTag::Service))),
        "service": api_cls.map(|c| c.service).unwrap_or_else(|| db_cls.is_some_and(|c| c.tags.contains(&rbx_reflection::ClassTag::Service))),
        "tags": db_cls.map(|c| c.tags.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>()).unwrap_or_default(),
        "counts": {
            "properties": properties.len(),
            "functions": functions.len(),
            "events": events.len(),
            "callbacks": callbacks.len(),
        },
        "properties": properties,
        "functions": functions,
        "events": events,
        "callbacks": callbacks,
    }))
}

fn list_enums(query: &str, limit: usize) -> Value {
    let db = crate::doc::db();
    let api = crate::api::get();
    let needle = query.to_ascii_lowercase();

    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    for (k, e) in &db.enums {
        names.insert(k.to_string(), e.items.len());
    }
    if let Some(a) = api {
        for (k, items) in &a.enums {
            names.entry(k.clone()).or_insert(items.len());
        }
    }

    let list: Vec<Value> = names
        .into_iter()
        .filter(|(n, _)| needle.is_empty() || n.to_ascii_lowercase().contains(&needle))
        .take(limit)
        .map(|(n, count)| json!({ "name": n, "item_count": count }))
        .collect();

    json!({
        "count": list.len(),
        "enums": list,
    })
}

fn inspect_enum_schema(raw_name: &str) -> Result<Value, String> {
    let db = crate::doc::db();
    let api = crate::api::get();

    let canonical_name = db
        .enums
        .keys()
        .find(|k| k.eq_ignore_ascii_case(raw_name))
        .map(|k| k.to_string())
        .or_else(|| {
            api.and_then(|a| {
                a.enums
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(raw_name))
                    .cloned()
            })
        })
        .ok_or_else(|| format!("unknown Roblox Enum '{raw_name}'"))?;

    let mut items_map: BTreeMap<String, u32> = BTreeMap::new();
    if let Some(ed) = db.enums.get(canonical_name.as_str()) {
        for (k, v) in &ed.items {
            items_map.insert(k.to_string(), *v);
        }
    }
    if let Some(a) = api {
        if let Some(list) = a.enums.get(&canonical_name) {
            for (k, v) in list {
                items_map.entry(k.clone()).or_insert(*v);
            }
        }
    }

    let mut items: Vec<(String, u32)> = items_map.into_iter().collect();
    items.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    Ok(json!({
        "name": canonical_name,
        "count": items.len(),
        "items": items.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflection_search_finds_basepart_and_material_enum() {
        let part = inspect_class_schema("Part", &json!({ "member_query": "Anchored" })).unwrap();
        assert_eq!(part["name"], "Part");
        let props = part["properties"].as_array().unwrap();
        assert!(props.iter().any(|p| p["name"] == "Anchored" && p["declaring_class"] == "BasePart"));

        let mat = inspect_enum_schema("Material").unwrap();
        assert_eq!(mat["name"], "Material");
        let items = mat["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["name"] == "Plastic"));
    }

    #[test]
    fn obj_import_registers_generated_mesh_and_meshpart() {
        let obj_bytes = b"v 0 0 0\nv 2 0 0\nv 0 4 0\nf 1 2 3\n";
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel")
                .with_child(InstanceBuilder::new("Workspace").with_name("Workspace")),
        );
        let mut doc = Document::from_play_dom(dom, "obj_test".into());
        let ws = doc.find_child(doc.dom.root_ref(), "Workspace").unwrap();
        let out = import_obj_bytes(&mut doc, obj_bytes, "Tri", ws, None, Some(Vec3::new(10.0, 5.0, 0.0)))
            .unwrap();
        assert_eq!(out["count"], 1);
        let r_str = out["instances"][0]["ref"].as_str().unwrap();
        let r = crate::mcp::variants::str_to_ref(r_str).unwrap();
        assert_eq!(doc.class_of(r).map(|c| c.as_str()), Some("MeshPart"));
        assert_eq!(inst_name(&doc, r), "Tri");
    }
}
