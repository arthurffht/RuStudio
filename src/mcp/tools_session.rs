//! OmniMCP Session, Place & Diagnostics tools:
//! `studio_get_state`, `place_new`, `place_open`, `place_save`,
//! `place_diagnostics`, `studio_quit`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use rbx_dom_weak::types::{Ref, Variant};
use serde_json::{json, Value};

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::variants::{instance_path, ref_to_str, resolve_datamodel, resolve_datamodel_mut, resolve_target};
use crate::camera::CameraMode;
use crate::doc::{DocKind, Document};
use crate::rig::RigType;
use crate::runtime::RunMode;

pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "studio_get_state".into(),
            description: "Return a complete live snapshot of RuStudio state: mode (Edit/Play/Run), available DataModels, active place metadata, dirty/undo/redo status, selection, editor camera, FPS, frame work time, memory usage, physics solver, play time, character state, and active view side.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "place_new".into(),
            description: "Reset the editor to a fresh Baseplate place, stopping any active Play/Run session first and resetting editor caches, tabs, and Start Page cleanly.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "template": {
                        "type": "string",
                        "enum": ["baseplate", "studio_baseplate", "empty"],
                        "description": "Optional place template (default: 'baseplate' via Document::new_place())."
                    }
                }
            }),
        },
        ToolDef {
            name: "place_open".into(),
            description: "Open a local Roblox place/model (.rbxl, .rbxlx, .rbxm, .rbxmx) or a Rojo/Argon *.project.json file or project directory into the live editor, stopping Play/Run if active.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Filesystem path to a .rbxl/.rbxlx/.rbxm/.rbxmx file, a *.project.json file, or a project folder."
                    },
                    "project_file": {
                        "type": "string",
                        "description": "Optional project filename when 'path' is a directory containing multiple *.project.json files."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "place_save".into(),
            description: "Save the open place (Edit or Active DataModel, even Edit while Play is running) to disk, sync Rojo project files, or export selected/targeted instances as a .rbxm/.rbxmx model.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Destination file path (.rbxl, .rbxlx, .rbxm, or .rbxmx). Omit to save to the document's existing path or sync the active Rojo project."
                    },
                    "datamodel": {
                        "type": "string",
                        "enum": ["Edit", "Active", "Server", "Client"],
                        "description": "Which DataModel to save from (default: 'Edit', allowing clean saving of the Edit place while Play is running)."
                    },
                    "selection_only": {
                        "type": "boolean",
                        "description": "Save only the current selection as a .rbxm/.rbxmx model file (default: false)."
                    },
                    "targets": {
                        "type": "array",
                        "description": "Optional list of instance targets (paths or ref:<id>) to export as a .rbxm/.rbxmx model file.",
                        "items": {}
                    }
                }
            }),
        },
        ToolDef {
            name: "place_diagnostics".into(),
            description: "Return detailed place and engine diagnostics: instance stats summary, service list, script counts and total lines, recent crash reports, resident memory, and frame performance metrics.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": {
                        "type": "string",
                        "enum": ["Active", "Edit", "Server", "Client"],
                        "description": "Which DataModel to inspect (default: 'Active')."
                    }
                }
            }),
        },
        ToolDef {
            name: "studio_quit".into(),
            description: "Cleanly close the RuStudio process, flushing settings and optionally saving the place beforehand.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "save_before_quit": {
                        "type": "boolean",
                        "description": "Save the Edit place before quitting (default: false)."
                    },
                    "save_path": {
                        "type": "string",
                        "description": "Optional path to save to when save_before_quit is true and the place has no existing path."
                    },
                    "discard_unsaved": {
                        "type": "boolean",
                        "description": "Allow quitting without saving when there are unsaved changes (default: true)."
                    }
                }
            }),
        },
    ]
}

pub fn call(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    name: &str,
    args: &Value,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let result = match name {
        "studio_get_state" => studio_get_state(ed),
        "place_new" => place_new(ed, ctx, args),
        "place_open" => place_open(ed, ctx, args),
        "place_save" => place_save(ed, args),
        "place_diagnostics" => place_diagnostics(ed, args),
        "studio_quit" => studio_quit(ed, ctx, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(result))
}

fn mode_and_datamodels(ed: &crate::app::Editor) -> (&'static str, Vec<&'static str>, &'static str) {
    match ed.play.as_ref() {
        None => ("Edit", vec!["Edit"], "Edit"),
        Some(p) => match p.mode {
            RunMode::Play => ("Play", vec!["Edit", "Server", "Client"], p.showing.label()),
            RunMode::Run => ("Run", vec!["Edit", "Server"], "Server"),
            RunMode::Edit => ("Edit", vec!["Edit"], "Edit"),
        },
    }
}

fn studio_get_state(ed: &crate::app::Editor) -> ToolCallResult {
    let (mode, available_datamodels, active_datamodel) = mode_and_datamodels(ed);
    let showing_side = ed.play.as_ref().map(|p| p.showing.label());
    let play_time = ed.play.as_ref().map(|p| p.time).unwrap_or(0.0);
    let has_character = ed.play_has_character();
    let solver = if ed.play.is_some() {
        crate::solver::kind().name()
    } else {
        crate::rustudio_settings::solver_now().name()
    };
    let rig_type = match ed.play_rig {
        Some(RigType::R15) => "R15",
        Some(RigType::R6) => "R6",
        None => "Auto",
    };

    let selection: Vec<Value> = ed
        .doc
        .selection
        .iter()
        .map(|&r| {
            let inst = ed.doc.dom.get_by_ref(r);
            json!({
                "ref": ref_to_str(r),
                "path": instance_path(&ed.doc, r),
                "class": inst.map(|i| i.class.as_str()),
                "name": inst.map(|i| i.name.as_str()),
            })
        })
        .collect();

    let path = ed
        .doc
        .path
        .as_ref()
        .or_else(|| ed.play_saved_doc().and_then(|d| d.path.as_ref()))
        .map(|p| p.display().to_string());

    let edit_info = ed.play_saved_doc().map(|d| {
        json!({
            "title": d.title,
            "path": d.path.as_ref().map(|p| p.display().to_string()),
            "dirty": d.dirty(),
            "revision": d.revision,
            "tree_revision": d.tree_revision,
            "undo_len": d.undo_len(),
            "can_undo": d.can_undo(),
            "can_redo": d.can_redo(),
        })
    });

    let camera = json!({
        "pos": [ed.cam.pos.x, ed.cam.pos.y, ed.cam.pos.z],
        "yaw_deg": ed.cam.yaw.to_degrees(),
        "pitch_deg": ed.cam.pitch.to_degrees(),
        "fov_deg": ed.cam.fov_deg,
        "focus": [ed.cam.focus.x, ed.cam.focus.y, ed.cam.focus.z],
        "mode": match ed.cam.mode {
            CameraMode::Fly => "Fly",
            CameraMode::Orbit => "Orbit",
        },
    });

    let project = ed.project.as_ref().map(|p| {
        json!({
            "name": p.name,
            "file": p.file.display().to_string(),
            "root": p.root.display().to_string(),
            "place": p.place.as_ref().map(|pl| pl.display().to_string()),
            "synced_count": p.map.len(),
        })
    });

    ToolCallResult::json(&json!({
        "mode": mode,
        "available_datamodels": available_datamodels,
        "active_datamodel": active_datamodel,
        "showing_side": showing_side,
        "title": ed.doc.title,
        "path": path,
        "kind": match ed.doc.kind {
            DocKind::Place => "Place",
            DocKind::Model => "Model",
        },
        "dirty": ed.doc.dirty(),
        "revision": ed.doc.revision,
        "tree_revision": ed.doc.tree_revision,
        "undo_len": ed.doc.undo_len(),
        "can_undo": ed.doc.can_undo(),
        "can_redo": ed.doc.can_redo(),
        "edit_doc": edit_info,
        "selection": selection,
        "camera": camera,
        "fps": ed.fps,
        "last_work_ms": ed.last_work_ms,
        "frame_no": ed.frame_no,
        "resident_bytes": crate::platform::resident_bytes(),
        "solver": solver,
        "rig_type": rig_type,
        "play_time": play_time,
        "has_character": has_character,
        "start_page_visible": ed.start.visible,
        "instance_count": ed.doc.dom.descendants().count(),
        "log_len": ed.log.len(),
        "paused": ed.mcp.as_ref().is_some_and(|m| m.state.paused),
        "project": project,
    }))
}

fn reset_editor_caches(ed: &mut crate::app::Editor) {
    ed.start.visible = false;
    ed.confirm_discard = None;
    ed.project_choice = None;
    ed.highlight_index = None;
    ed.terrain_drawn = None;
    ed.terrain_brushed = None;
    ed.terrain_grass_chunks.clear();
    ed.terrain_grass_built.clear();
    ed.terrain_params_dirty = true;
    ed.vf_scene = crate::scene::Scene::viewport_frames();
    ed.asset_gen = ed.asset_gen.wrapping_add(1);
}

fn place_new(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let template = args
        .get("template")
        .and_then(|v| v.as_str())
        .unwrap_or("baseplate");

    let doc = match template {
        "baseplate" => Document::new_place(),
        "studio_baseplate" => crate::start_page::templates::baseplate(),
        "empty" => {
            let mut d = Document::empty();
            d.title = "Untitled place".into();
            d.ensure_services();
            d
        }
        other => {
            return ToolCallResult::error(format!(
                "unknown template '{other}'; expected baseplate, studio_baseplate, or empty"
            ));
        }
    };

    if ed.play.is_some() {
        ed.stop_play();
    }
    ed.set_document(doc);
    reset_editor_caches(ed);
    ed.info("New place created");
    ctx.request_repaint();

    ToolCallResult::json(&json!({
        "created": true,
        "template": template,
        "title": ed.doc.title,
        "mode": "Edit",
        "instance_count": ed.doc.dom.descendants().count(),
        "revision": ed.doc.revision,
        "tree_revision": ed.doc.tree_revision,
    }))
}

fn apply_origin_place_context(path: &Path) {
    if std::env::var_os("RUSTUDIO_PLACE_CONTEXT").is_none() {
        let origin = path.parent().map(|d| d.join("origin.json"));
        let place = origin
            .and_then(|o| std::fs::read_to_string(o).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("place_id")?.as_u64())
            .unwrap_or(0);
        rustudio_account::session::set_place_context(place);
    }
}

fn open_project_file(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    project_file: &Path,
) -> ToolCallResult {
    let (doc, mut st) = match crate::project_sync::open(project_file) {
        Ok(res) => res,
        Err(e) => return ToolCallResult::error(format!("Could not open project {}: {e}", project_file.display())),
    };

    if ed.play.is_some() {
        ed.stop_play();
    }

    let n = doc.dom.descendants().count();
    let warnings = std::mem::take(&mut st.warnings);
    let synced = st.map.len();
    let project_name = st.name.clone();
    let project_root = st.root.display().to_string();
    let place_str = st.place.as_ref().map(|p| p.display().to_string());
    let place_note = st
        .place
        .as_ref()
        .map(|p| format!(", place {}", p.display()))
        .unwrap_or_else(|| ", no place file (an empty DataModel under it)".into());

    ed.set_document(doc);
    reset_editor_caches(ed);
    crate::project_config::set_active(Some(st.config.clone()));
    ed.project = Some(st);
    ed.info(format!(
        "Opened project \"{project_name}\" from {} ({n} instances, {synced} from files{place_note})",
        project_file.display()
    ));
    for w in warnings.iter().take(20) {
        ed.warn(format!("Project: {w}"));
    }
    if warnings.len() > 20 {
        ed.warn(format!("Project: {} more warnings", warnings.len() - 20));
    }
    ctx.request_repaint();

    ToolCallResult::json(&json!({
        "opened": true,
        "is_project": true,
        "project_name": project_name,
        "project_file": project_file.display().to_string(),
        "project_root": project_root,
        "place_path": place_str,
        "title": ed.doc.title,
        "instance_count": n,
        "synced_count": synced,
        "warnings": warnings,
        "mode": "Edit",
    }))
}

fn place_open(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let Some(path_str) = args.get("path").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else {
        return ToolCallResult::error("place_open requires a non-empty 'path' string");
    };
    let path = PathBuf::from(path_str);
    if !path.exists() {
        return ToolCallResult::error(format!("path does not exist: {}", path.display()));
    }

    if path.is_dir() {
        let files = crate::project::project_files(&path);
        if files.is_empty() {
            return ToolCallResult::error(format!("{} has no *.project.json", path.display()));
        }
        let chosen: Result<std::path::PathBuf, String> =
            if let Some(pf) = args.get("project_file").and_then(|v| v.as_str()) {
                let candidate = path.join(pf);
                if candidate.is_file() {
                    Ok(candidate)
                } else {
                    files
                        .iter()
                        .find(|f| f.file_name().and_then(|n| n.to_str()) == Some(pf))
                        .cloned()
                        .ok_or_else(|| {
                            format!("project_file '{pf}' not found in {}", path.display())
                        })
                }
            } else {
                Ok(files[0].clone())
            };
        return match chosen {
            Ok(f) => open_project_file(ed, ctx, &f),
            Err(e) => ToolCallResult::error(e),
        };
    }

    if crate::project::name_of(&path).ends_with(".project.json") {
        return open_project_file(ed, ctx, &path);
    }

    apply_origin_place_context(&path);
    let doc = match Document::open(&path) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(format!("Could not open {}: {e}", path.display())),
    };

    if ed.play.is_some() {
        ed.stop_play();
    }

    let n = doc.dom.descendants().count();
    let kind = match doc.kind {
        DocKind::Place => "Place",
        DocKind::Model => "Model",
    };
    ed.set_document(doc);
    reset_editor_caches(ed);
    ed.info(format!("Opened {} ({n} instances)", path.display()));
    ctx.request_repaint();

    ToolCallResult::json(&json!({
        "opened": true,
        "is_project": false,
        "title": ed.doc.title,
        "path": path.display().to_string(),
        "kind": kind,
        "instance_count": n,
        "mode": "Edit",
    }))
}

fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
    }
    Ok(())
}

fn save_doc_to(doc: &mut Document, path: &Path) -> Result<u64, String> {
    ensure_parent_dir(path)?;
    doc.save_to(path)?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(bytes)
}

fn place_save(ed: &mut crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm = args
        .get("datamodel")
        .and_then(|v| v.as_str())
        .unwrap_or("Edit");
    let selection_only = args
        .get("selection_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let targets = args.get("targets").and_then(|v| v.as_array());
    let explicit_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    if selection_only || targets.is_some() {
        let Some(save_path) = explicit_path else {
            return ToolCallResult::error("saving selection/targets requires 'path' ending in .rbxm or .rbxmx");
        };
        if !matches!(Document::kind_for(&save_path), Some((DocKind::Model, _))) {
            return ToolCallResult::error("selection export requires a .rbxm or .rbxmx file extension");
        }
        let doc = match resolve_datamodel(ed, Some(dm)) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let refs: Vec<Ref> = if let Some(items) = targets {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match resolve_target(doc, item) {
                    Ok(r) => out.push(r),
                    Err(e) => return ToolCallResult::error(e),
                }
            }
            out
        } else {
            doc.selection.clone()
        };
        if refs.is_empty() {
            return ToolCallResult::error("no instances selected or specified to save");
        }
        if let Err(e) = ensure_parent_dir(&save_path) {
            return ToolCallResult::error(e);
        }
        if let Err(e) = doc.save_selection_to(&refs, &save_path) {
            return ToolCallResult::error(format!("Save selection failed: {e}"));
        }
        let bytes = std::fs::metadata(&save_path).map(|m| m.len()).unwrap_or(0);
        return ToolCallResult::json(&json!({
            "saved": true,
            "selection_only": true,
            "datamodel": dm,
            "count": refs.len(),
            "path": save_path.display().to_string(),
            "bytes": bytes,
        }));
    }

    let is_edit_target = dm.eq_ignore_ascii_case("edit")
        || (dm.eq_ignore_ascii_case("active") && ed.play.is_none());

    if explicit_path.is_none() && is_edit_target && ed.project.is_some() {
        let mut st = ed.project.take().expect("checked");
        let doc = match resolve_datamodel_mut(ed, Some("Edit")) {
            Ok(d) => d,
            Err(e) => {
                ed.project = Some(st);
                return ToolCallResult::error(e);
            }
        };
        if st.doc_id == doc.id {
            st.write_back(doc);
        }
        let wrote_files = std::mem::take(&mut st.log);
        let mut saved_place = None;
        let mut saved_bytes = 0u64;
        if let Some(place_path) = st.place.clone() {
            if st.place_dirty || !place_path.exists() {
                match save_doc_to(doc, &place_path) {
                    Ok(b) => {
                        st.place_dirty = false;
                        saved_place = Some(place_path.display().to_string());
                        saved_bytes = b;
                    }
                    Err(e) => {
                        ed.project = Some(st);
                        return ToolCallResult::error(format!("Save failed: {e}"));
                    }
                }
            }
        }
        let dirty = doc.dirty();
        ed.project = Some(st);
        return ToolCallResult::json(&json!({
            "saved": true,
            "is_project": true,
            "datamodel": dm,
            "path": saved_place,
            "bytes": saved_bytes,
            "synced_files_written": wrote_files,
            "dirty": dirty,
        }));
    }

    let doc = match resolve_datamodel_mut(ed, Some(dm)) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };

    let target_path = match explicit_path.or_else(|| doc.path.clone()) {
        Some(p) => p,
        None => {
            return ToolCallResult::error(
                "no path specified and document has no existing path; pass 'path' (.rbxl/.rbxlx/.rbxm/.rbxmx)",
            );
        }
    };

    match save_doc_to(doc, &target_path) {
        Ok(bytes) => {
            let dirty = doc.dirty();
            let title = doc.title.clone();
            ed.info(format!("Saved {}", target_path.display()));
            ToolCallResult::json(&json!({
                "saved": true,
                "datamodel": dm,
                "title": title,
                "path": target_path.display().to_string(),
                "dirty": dirty,
                "bytes": bytes,
            }))
        }
        Err(e) => ToolCallResult::error(format!("Save failed: {e}")),
    }
}

fn collect_crash_reports() -> Vec<Value> {
    let dir = crate::crash::dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".txt") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((mtime, p, meta.len()));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .take(20)
        .map(|(_, p, size)| {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let summary = text
                .lines()
                .find(|l| l.starts_with("panic in thread") || l.starts_with("rustudio crashed: fatal"))
                .unwrap_or("")
                .to_string();
            json!({
                "file": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "path": p.display().to_string(),
                "bytes": size,
                "summary": summary,
            })
        })
        .collect()
}

fn place_diagnostics(ed: &crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm = args
        .get("datamodel")
        .and_then(|v| v.as_str())
        .unwrap_or("Active");
    let doc = match resolve_datamodel(ed, Some(dm)) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };

    let stats_text = doc.stats();
    let root = doc.dom.root_ref();
    let services: Vec<Value> = doc
        .dom
        .root()
        .children()
        .iter()
        .filter_map(|&r| {
            let inst = doc.dom.get_by_ref(r)?;
            let desc = doc.dom.descendants_of(r).count().saturating_sub(1);
            Some(json!({
                "ref": ref_to_str(r),
                "name": inst.name,
                "class": inst.class.as_str(),
                "children": inst.children().len(),
                "descendants": desc,
            }))
        })
        .collect();

    let mut script_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut script_lines = 0usize;
    let mut script_bytes = 0usize;
    let mut disabled_scripts = 0usize;
    let mut total_instances = 0usize;

    for inst in doc.dom.descendants() {
        if inst.referent() == root {
            continue;
        }
        total_instances += 1;
        let cls = inst.class.as_str();
        if matches!(cls, "Script" | "LocalScript" | "ModuleScript") {
            let key = match cls {
                "Script" => "Script",
                "LocalScript" => "LocalScript",
                _ => "ModuleScript",
            };
            *script_counts.entry(key).or_default() += 1;
            if let Some(Variant::String(src)) = doc.prop(inst.referent(), "Source") {
                script_lines += src.lines().count();
                script_bytes += src.len();
            }
            let disabled = matches!(doc.prop(inst.referent(), "Disabled"), Some(Variant::Bool(true)))
                || matches!(doc.prop(inst.referent(), "Enabled"), Some(Variant::Bool(false)));
            if disabled {
                disabled_scripts += 1;
            }
        }
    }

    let script_total: usize = script_counts.values().sum();
    let resident = crate::platform::resident_bytes();
    let crash_reports = collect_crash_reports();

    let play_info = ed.play.as_ref().map(|p| {
        let (orphans, freed, lua_slots) = p.gc_counts();
        json!({
            "mode": match p.mode {
                RunMode::Play => "Play",
                RunMode::Run => "Run",
                RunMode::Edit => "Edit",
            },
            "time": p.time,
            "showing": p.showing.label(),
            "orphans": orphans,
            "freed": freed,
            "server_lua_slots": lua_slots,
        })
    });

    ToolCallResult::json(&json!({
        "datamodel": dm,
        "title": doc.title,
        "stats_text": stats_text,
        "total_instances": total_instances,
        "counts": {
            "base_parts": doc.count_class("BasePart"),
            "models": doc.count_class("Model"),
            "lights": doc.count_class("Light"),
            "guis": doc.count_class("GuiObject"),
            "sounds": doc.count_class("Sound"),
        },
        "services": services,
        "scripts": {
            "total": script_total,
            "by_class": script_counts,
            "disabled": disabled_scripts,
            "total_lines": script_lines,
            "total_bytes": script_bytes,
        },
        "memory": {
            "resident_bytes": resident,
            "resident_mb": resident.map(|b| b as f64 / (1024.0 * 1024.0)),
        },
        "performance": {
            "fps": ed.fps,
            "last_work_ms": ed.last_work_ms,
            "frame_no": ed.frame_no,
            "draw_stats": ed.draw_stats,
            "terrain_tris": ed.terrain_tris,
        },
        "crash_reports_dir": crate::crash::dir().display().to_string(),
        "crash_reports": crash_reports,
        "play": play_info,
    }))
}

fn studio_quit(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let save_before_quit = args
        .get("save_before_quit")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let discard_unsaved = args
        .get("discard_unsaved")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let save_path = args
        .get("save_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    let mut saved_to: Option<String> = None;

    if save_before_quit {
        let save_res = place_save(
            ed,
            &json!({
                "datamodel": "Edit",
                "path": save_path.as_ref().map(|p| p.display().to_string()),
            }),
        );
        if save_res.is_error {
            return save_res;
        }
        saved_to = save_path
            .map(|p| p.display().to_string())
            .or_else(|| {
                ed.play_saved_doc()
                    .unwrap_or(&ed.doc)
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
            });
    } else if !discard_unsaved {
        let any_dirty = ed.doc.dirty() || ed.play_saved_doc().is_some_and(|d| d.dirty());
        if any_dirty {
            return ToolCallResult::error(
                "unsaved changes present; pass save_before_quit: true or discard_unsaved: true",
            );
        }
    }

    if ed.play.is_some() {
        ed.stop_play();
    }
    ed.confirm_discard = None;
    // Mark current document non-dirty so app.rs's close_requested guard does
    // not cancel ViewportCommand::Close and show an interactive modal dialog.
    ed.doc.mark_saved_at(ed.doc.revision);
    crate::studio_settings::flush();
    crate::rustudio_settings::flush();
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    ctx.request_repaint();

    ToolCallResult::json(&json!({
        "quitting": true,
        "saved": save_before_quit,
        "saved_path": saved_to,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_cover_all_session_tools() {
        let names: Vec<String> = schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "studio_get_state",
                "place_new",
                "place_open",
                "place_save",
                "place_diagnostics",
                "studio_quit",
            ]
        );
    }

    #[test]
    fn crash_report_listing_handles_missing_or_empty_dir() {
        let reports = collect_crash_reports();
        assert!(reports.len() <= 20);
    }
}
