//! Unrestricted DOM inspection and mutation tools for OmniMCP.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::Sender;

use glam::{EulerRot, Mat3, Vec3};
use rbx_dom_weak::types::{
    Attributes, Color3uint8, Content, ContentId, Ref, Tags, Variant, VariantType, Vector3,
};
use rbx_dom_weak::{ustr, InstanceBuilder, Ustr, WeakDom};
use rbx_reflection::{DataType, PropertyKind, PropertyTag, Scriptability};
use serde_json::{json, Map, Value};

use crate::app::Editor;
use crate::doc::{self, Change, Document};
use crate::math::Xform;
use crate::runtime::{RepOp, RunMode, Runtime, Side};
use crate::scene;

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::variants;

/// Resolved DataModel destination for read/write dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetDm {
    Edit,
    Play(Side),
}

/// Resolves the `datamodel` selector argument (`"Active"`, `"Edit"`, `"Server"`, `"Client"`).
pub(super) fn resolve_target_dm(ed: &Editor, args: &Value) -> Result<TargetDm, String> {
    let key = args
        .get("datamodel")
        .and_then(|v| v.as_str())
        .unwrap_or("active")
        .trim()
        .to_ascii_lowercase();
    match key.as_str() {
        "" | "active" => match ed.play.as_ref() {
            Some(p) => Ok(TargetDm::Play(p.showing)),
            None => Ok(TargetDm::Edit),
        },
        "edit" => {
            if ed.play.is_none() || ed.play_saved_doc().is_some() {
                Ok(TargetDm::Edit)
            } else {
                Err("Edit DataModel is unavailable".into())
            }
        }
        "server" => {
            if ed.play.is_some() {
                Ok(TargetDm::Play(Side::Server))
            } else {
                Err("Server DataModel requires an active Play or Run session".into())
            }
        }
        "client" => {
            let p = ed
                .play
                .as_ref()
                .ok_or_else(|| "Client DataModel requires an active Play session".to_string())?;
            if p.mode == RunMode::Run {
                return Err("Run mode has no Client DataModel (use Play mode)".into());
            }
            Ok(TargetDm::Play(Side::Client))
        }
        other => Err(format!(
            "unknown datamodel '{other}'; expected Edit, Server, Client, or Active"
        )),
    }
}

pub(super) fn datamodel_arg(args: &Value) -> Option<&str> {
    args.get("datamodel").and_then(|v| v.as_str())
}

/// Runs a mutation on either the Edit DataModel (`variants::resolve_datamodel_mut`,
/// even during Play) or a live `PlaySession` side (`ed.with_play_side`).
pub(super) fn with_edit_or_play<T, FEdit, FPlay>(
    ed: &mut Editor,
    args: &Value,
    on_edit: FEdit,
    on_play: FPlay,
) -> Result<T, String>
where
    FEdit: FnOnce(&mut Document) -> Result<T, String>,
    FPlay: FnOnce(&Runtime) -> Result<T, String>,
{
    match resolve_target_dm(ed, args)? {
        TargetDm::Edit => {
            let doc = variants::resolve_datamodel_mut(ed, datamodel_arg(args))?;
            on_edit(doc)
        }
        TargetDm::Play(side) => ed
            .with_play_side(side, |rt, _, _| on_play(rt))
            .ok_or_else(|| format!("{} DataModel is unavailable", side.label()))?,
    }
}

/// Keeps open script editor tabs synchronized when `Source` changes in Edit mode.
pub(super) fn sync_script_tab_sources(ed: &mut Editor, dm: TargetDm, updated: &[(Ref, String)]) {
    if dm == TargetDm::Edit && !ed.is_playing() {
        for (r, src) in updated {
            if ed.scripts.bufs.get(r).is_some_and(|b| b.text != *src) {
                ed.scripts.bufs.remove(r);
            }
        }
    }
}

/// Applies a property in a live `PlaySession` runtime, including CollectionService
/// tag signals, `BaseScript` start/stop on `Disabled`/`Enabled`, and legacy content twins.
pub(super) fn play_apply_prop(rt: &Runtime, r: Ref, name: &str, v: Variant) {
    let class = crate::runtime::world(&rt.lua)
        .ok()
        .and_then(|w| w.d().class_of(r).map(|c| c.to_string()));

    let old_tags = if name == "Tags" {
        crate::runtime::world(&rt.lua)
            .ok()
            .map(|w| instance_tags(w.d(), r))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    rt.apply_prop(r, name, v.clone());

    if name.ends_with("Content") && name != "Attributes" {
        let uri = scene::content_string(Some(v.clone())).unwrap_or_default();
        for legacy in doc::content_legacy_names(name) {
            let has_prop = class
                .as_deref()
                .is_some_and(|c| variants::expected_variant_type(c, legacy).is_some());
            if has_prop {
                rt.apply_prop(r, legacy, Variant::ContentId(ContentId::from(uri.clone())));
            }
        }
    }

    if name == "Tags" {
        if let Variant::Tags(new_t) = &v {
            let new_tags: Vec<String> = new_t.iter().map(|s| s.to_string()).collect();
            if let Ok(mut w) = crate::runtime::world(&rt.lua) {
                let root = w.d().dom.root_ref();
                if w.d().is_ancestor(root, r) {
                    if let Some(cs) = w.d().find_child(root, "CollectionService") {
                        let inst_val = rt.inst(r);
                        for t in &new_tags {
                            if !old_tags.iter().any(|o| o == t) {
                                w.fire(cs, &format!("TagAdded:{t}"), vec![inst_val.clone()]);
                            }
                        }
                        for t in &old_tags {
                            if !new_tags.iter().any(|n| n == t) {
                                w.fire(cs, &format!("TagRemoved:{t}"), vec![inst_val.clone()]);
                            }
                        }
                    }
                }
            }
        }
    }

    if class
        .as_deref()
        .is_some_and(|c| c == "Script" || c == "LocalScript")
        && (name == "Disabled" || name == "Enabled")
    {
        let disabled = matches!(
            (name, &v),
            ("Disabled", Variant::Bool(true)) | ("Enabled", Variant::Bool(false))
        );
        if disabled {
            rt.stop_script(r);
        } else if let Ok(mut w) = crate::runtime::world(&rt.lua) {
            if !w.started.contains(&r) && w.runs_here(r) {
                w.to_start.push(r);
            }
        }
    }
}

/// Renames an instance in a live `PlaySession` runtime and queues replication.
pub(super) fn play_apply_name(rt: &Runtime, r: Ref, name: &str) {
    rt.apply_name(r, name);
    if let Ok(mut w) = crate::runtime::world(&rt.lua) {
        w.record(RepOp::Name(r));
    }
}

/// Returns all `Tags` strings stored on `r`.
pub(super) fn instance_tags(doc: &Document, r: Ref) -> Vec<String> {
    match doc.prop(r, "Tags") {
        Some(Variant::Tags(t)) => t.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Returns all `Attributes` on `r` as a JSON map.
pub(super) fn instance_attributes(doc: &Document, r: Ref) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Variant::Attributes(attrs)) = doc.prop(r, "Attributes") {
        let mut keys: Vec<_> = attrs.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in keys {
            out.insert(k.clone(), variants::variant_to_json_with_doc(doc, None, None, v));
        }
    }
    out
}

/// Normalizes a dot or slash path prefix (stripping `game.` / `DataModel.` / leading `/`).
pub(super) fn normalize_path_prefix(raw: &str) -> String {
    let s = raw.trim().replace('/', ".");
    let s = s
        .strip_prefix("game.")
        .or_else(|| s.strip_prefix("Game."))
        .or_else(|| s.strip_prefix("DataModel."))
        .unwrap_or(&s);
    s.trim_matches('.').to_string()
}

pub(super) fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty()
        || prefix.eq_ignore_ascii_case("game")
        || prefix.eq_ignore_ascii_case("datamodel")
    {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

pub(super) fn resolve_targets_arg(
    doc: &Document,
    args: &Value,
    single_key: &str,
    list_key: &str,
) -> Result<Vec<Ref>, String> {
    if let Some(arr) = args.get(list_key).and_then(|v| v.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for (idx, item) in arr.iter().enumerate() {
            let r = variants::resolve_target(doc, item)
                .map_err(|e| format!("{list_key}[{idx}]: {e}"))?;
            out.push(r);
        }
        return Ok(out);
    }
    if let Some(single) = args.get(single_key) {
        return Ok(vec![variants::resolve_target(doc, single)?]);
    }
    Err(format!("expected '{single_key}' or '{list_key}'"))
}

pub fn schemas() -> Vec<ToolDef> {
    let dm_prop = json!({
        "type": "string",
        "enum": ["Active", "Edit", "Server", "Client"],
        "description": "Target DataModel (default: Active; 'Edit' works even during Play)."
    });
    vec![
        ToolDef {
            name: "dom_get_tree".into(),
            description: "Read the DataModel instance hierarchy (hierarchical or flat) from any root with optional depth, class (IsA), name, tag filters, and properties.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "root": { "description": "Root instance path or ref:<id> (default: 'game')." },
                    "max_depth": { "type": "integer", "minimum": 0, "description": "Maximum traversal depth (default: unlimited)." },
                    "flat": { "type": "boolean", "description": "Return a flat list instead of nested children (default: false)." },
                    "class": { "type": "string", "description": "Filter instances by Roblox class using IsA inheritance." },
                    "name_contains": { "type": "string", "description": "Case-insensitive substring filter on instance Name." },
                    "tag": { "type": "string", "description": "Filter instances carrying this CollectionService tag." },
                    "include_properties": {
                        "description": "true for all stored properties, or an array of property names to include per node."
                    }
                }
            }),
        },
        ToolDef {
            name: "dom_inspect_instance".into(),
            description: "Inspect full details of one or more instances: ALL stored DOM properties + reflection defaults (including NotScriptable & RobloxScriptSecurity), property security metadata, Attributes, Tags, Archivable, children summary, and world CFrame/Position/Size.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "target": { "description": "Instance path, ref:<id>, or {ref|path}." },
                    "targets": { "type": "array", "description": "Multiple instance targets to inspect in one call." },
                    "include_defaults": { "type": "boolean", "description": "Include reflection defaults from the database (default: true)." },
                    "include_security": { "type": "boolean", "description": "Include per-property security & scriptability metadata (default: true)." }
                }
            }),
        },
        ToolDef {
            name: "dom_query".into(),
            description: "Unlimited DOM query across the DataModel (no 50-result cap). Supports class, inheritance (is_a), name substring, path prefix, tags, attributes, property equality, and 3D sphere/box spatial filters.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "root": { "description": "Subtree root to search under (default: 'game')." },
                    "is_a": { "type": "string", "description": "Match instances satisfying Instance:IsA(is_a)." },
                    "exact_class": { "type": "string", "description": "Match exact ClassName." },
                    "name_pattern": { "type": "string", "description": "Case-insensitive substring match against Name." },
                    "path_prefix": { "type": "string", "description": "Only match instances under this dot/slash path." },
                    "has_tag": { "type": "string", "description": "Match instances having this tag." },
                    "has_attribute": { "type": "string", "description": "Match instances having this attribute key." },
                    "attribute_equals": { "type": "object", "description": "Map of {attributeName: value} (or {name, value}) that must match." },
                    "property_equals": { "type": "object", "description": "Map of {propertyName: value} that must match." },
                    "within_sphere": {
                        "type": "object",
                        "properties": {
                            "center": { "type": "array", "items": { "type": "number" } },
                            "radius": { "type": "number" }
                        },
                        "required": ["center", "radius"]
                    },
                    "within_box": {
                        "type": "object",
                        "properties": {
                            "min": { "type": "array", "items": { "type": "number" } },
                            "max": { "type": "array", "items": { "type": "number" } }
                        },
                        "required": ["min", "max"]
                    },
                    "limit": { "type": "integer", "minimum": 1, "description": "Optional result cap (unlimited if omitted)." },
                    "select_properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include on each matched result." }
                }
            }),
        },
        ToolDef {
            name: "dom_create_instances".into(),
            description: "Create arbitrary recursive instance hierarchies with properties, attributes, tags, and cross-references (temp_id) in one atomic undoable batch.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "items": {
                        "type": "array",
                        "description": "Top-level creation specs { parent?, class, name?, temp_id?, properties?, attributes?, tags?, children?: [...] }.",
                        "items": { "type": "object" }
                    }
                },
                "required": ["items"]
            }),
        },
        ToolDef {
            name: "dom_set_properties".into(),
            description: "Batch-mutate properties, attributes, tags, and names on any instances, bypassing ALL NotScriptable and security capability restrictions.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": {},
                                "name": { "type": "string" },
                                "properties": { "type": "object" },
                                "attributes": { "type": "object", "description": "Map of attribute updates (set value to null to remove)." },
                                "add_tags": { "type": "array", "items": { "type": "string" } },
                                "remove_tags": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["target"]
                        }
                    }
                },
                "required": ["edits"]
            }),
        },
        ToolDef {
            name: "dom_reparent_rename_delete".into(),
            description: "Batch reparent, rename, delete, or clear children across any number of instances in one call.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "reparent": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "target": {}, "new_parent": {} },
                            "required": ["target", "new_parent"]
                        }
                    },
                    "rename": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "target": {}, "name": { "type": "string" } },
                            "required": ["target", "name"]
                        }
                    },
                    "delete": { "type": "array" },
                    "clear_children": { "type": "array" }
                }
            }),
        },
        ToolDef {
            name: "dom_clone_and_transform".into(),
            description: "Clone target instance(s) N times with optional parent, name_prefix, offset_step, rotation_step_deg, grid layout, or explicit transforms.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "target": {},
                    "targets": { "type": "array" },
                    "count": { "type": "integer", "minimum": 1 },
                    "parent": {},
                    "name_prefix": { "type": "string" },
                    "offset_step": { "type": "array", "items": { "type": "number" } },
                    "rotation_step_deg": { "type": "array", "items": { "type": "number" } },
                    "grid": {
                        "type": "object",
                        "properties": {
                            "cols": { "type": "integer", "minimum": 1 },
                            "rows": { "type": "integer", "minimum": 1 },
                            "spacing": { "type": "array", "items": { "type": "number" } }
                        },
                        "required": ["cols", "spacing"]
                    },
                    "transforms": { "type": "array" }
                }
            }),
        },
        ToolDef {
            name: "dom_group_ungroup".into(),
            description: "Group targets into a Model or Folder, or ungroup Model/Folder containers.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "action": { "type": "string", "enum": ["group", "ungroup"] },
                    "targets": { "type": "array" },
                    "target": {},
                    "class": { "type": "string", "enum": ["Model", "Folder"], "description": "Container class when grouping (default: Model)." },
                    "name": { "type": "string", "description": "Optional name for the created group." },
                    "parent": { "description": "Optional parent override when grouping." }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "dom_serialize_subtree".into(),
            description: "Export target subtree(s) to .rbxmx XML or base64 .rbxm binary, or import .rbxmx XML / base64 .rbxm / file_path under a parent.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "action": { "type": "string", "enum": ["export", "import"] },
                    "targets": { "type": "array" },
                    "target": {},
                    "format": { "type": "string", "enum": ["rbxmx", "rbxm", "xml", "binary"], "description": "Export format (default: rbxmx)." },
                    "save_path": { "type": "string", "description": "Optional file path to save exported bytes." },
                    "parent": { "description": "Parent for import (default: Workspace / root)." },
                    "xml": { "type": "string", "description": "Raw .rbxmx XML string for import." },
                    "base64": { "type": "string", "description": "Base64-encoded .rbxm/.rbxmx bytes for import." },
                    "data": { "type": "string", "description": "XML or base64 payload for import." },
                    "file_path": { "type": "string", "description": "Disk path to .rbxm/.rbxmx file for import." }
                },
                "required": ["action"]
            }),
        },
    ]
}

pub fn call(
    ed: &mut Editor,
    _ctx: &egui::Context,
    name: &str,
    args: &Value,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let res = match name {
        "dom_get_tree" => handle_get_tree(ed, args),
        "dom_inspect_instance" => handle_inspect_instance(ed, args),
        "dom_query" => handle_query(ed, args),
        "dom_create_instances" => handle_create_instances(ed, args),
        "dom_set_properties" => handle_set_properties(ed, args),
        "dom_reparent_rename_delete" => handle_reparent_rename_delete(ed, args),
        "dom_clone_and_transform" => handle_clone_and_transform(ed, args),
        "dom_group_ungroup" => handle_group_ungroup(ed, args),
        "dom_serialize_subtree" => handle_serialize_subtree(ed, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(match res {
        Ok(v) => ToolCallResult::json(&v),
        Err(e) => ToolCallResult::error(e),
    }))
}

// =========================================================================
// Read helpers: world transforms, property extraction, and matching
// =========================================================================

fn world_aabb_of_obb(xf: &Xform, size: Vec3) -> (Vec3, Vec3) {
    let h = size * 0.5;
    let ex = xf.rot.x_axis.abs() * h.x + xf.rot.y_axis.abs() * h.y + xf.rot.z_axis.abs() * h.z;
    (xf.pos - ex, xf.pos + ex)
}

pub(super) fn world_spatial_info(doc: &Document, r: Ref) -> Option<Value> {
    let class = doc.class_of(r)?;
    let c = class.as_str();
    if c != "Terrain" && doc::is_a(c, "BasePart") {
        let xf = scene::xform_of(doc, r)?;
        let size = scene::vec3_of(doc.prop(r, "Size")).unwrap_or(Vec3::ONE);
        let cf_json = variants::variant_to_json(&Variant::CFrame(xf.to_cframe()));
        return Some(json!({
            "cframe": cf_json,
            "position": [xf.pos.x, xf.pos.y, xf.pos.z],
            "size": [size.x, size.y, size.z],
        }));
    }
    if doc::is_a(c, "Attachment") {
        let xf = crate::attachment::world(doc, r, true)
            .unwrap_or_else(|| crate::attachment::local(doc, r, true));
        let cf_json = variants::variant_to_json(&Variant::CFrame(xf.to_cframe()));
        return Some(json!({
            "cframe": cf_json,
            "position": [xf.pos.x, xf.pos.y, xf.pos.z],
            "size": [0.0, 0.0, 0.0],
        }));
    }
    if c == "Model" {
        let pivot = crate::part_drag::editor_model_pivot(doc, r)
            .unwrap_or(Xform { pos: Vec3::ZERO, rot: Mat3::IDENTITY });
        let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        for desc in doc.dom.descendants_of(r) {
            if desc.class != "Terrain" && doc::is_a(&desc.class, "BasePart") {
                if let Some(xf) = scene::xform_of(doc, desc.referent()) {
                    let sz = scene::vec3_of(doc.prop(desc.referent(), "Size")).unwrap_or(Vec3::ONE);
                    let (a, b) = world_aabb_of_obb(&xf, sz);
                    lo = lo.min(a);
                    hi = hi.max(b);
                }
            }
        }
        let cf_json = variants::variant_to_json(&Variant::CFrame(pivot.to_cframe()));
        if lo.is_finite() {
            let sz = hi - lo;
            let center = (lo + hi) * 0.5;
            return Some(json!({
                "cframe": cf_json,
                "position": [pivot.pos.x, pivot.pos.y, pivot.pos.z],
                "size": [sz.x, sz.y, sz.z],
                "bounds_center": [center.x, center.y, center.z],
                "bounds_min": [lo.x, lo.y, lo.z],
                "bounds_max": [hi.x, hi.y, hi.z],
            }));
        }
        return Some(json!({
            "cframe": cf_json,
            "position": [pivot.pos.x, pivot.pos.y, pivot.pos.z],
            "size": [0.0, 0.0, 0.0],
        }));
    }
    if let Some(xf) = scene::xform_of(doc, r) {
        let cf_json = variants::variant_to_json(&Variant::CFrame(xf.to_cframe()));
        return Some(json!({
            "cframe": cf_json,
            "position": [xf.pos.x, xf.pos.y, xf.pos.z],
        }));
    }
    None
}

fn world_position_of(doc: &Document, r: Ref) -> Option<Vec3> {
    let class = doc.class_of(r)?;
    let c = class.as_str();
    if c != "Terrain" && doc::is_a(c, "BasePart") {
        return scene::xform_of(doc, r).map(|x| x.pos);
    }
    if doc::is_a(c, "Attachment") {
        return Some(
            crate::attachment::world(doc, r, true)
                .unwrap_or_else(|| crate::attachment::local(doc, r, true))
                .pos,
        );
    }
    if c == "Model" {
        return crate::part_drag::editor_model_pivot(doc, r).map(|x| x.pos);
    }
    scene::xform_of(doc, r).map(|x| x.pos)
}

fn read_effective_prop(doc: &Document, r: Ref, prop: &str) -> Option<Variant> {
    let inst = doc.dom.get_by_ref(r)?;
    let class = inst.class.as_str();
    match prop {
        "Name" => return Some(Variant::String(inst.name.clone())),
        "ClassName" => return Some(Variant::String(class.to_string())),
        "Parent" => return Some(Variant::Ref(inst.parent())),
        "Archivable" => {
            return Some(match doc.prop(r, "Archivable") {
                Some(v) => v,
                None => Variant::Bool(true),
            });
        }
        "Enabled" if doc::is_a(class, "BaseScript") => {
            let on = !matches!(doc.prop(r, "Disabled"), Some(Variant::Bool(true)))
                && !matches!(doc.prop(r, "Enabled"), Some(Variant::Bool(false)));
            return Some(Variant::Bool(on));
        }
        "Disabled" if doc::is_a(class, "BaseScript") => {
            let off = matches!(doc.prop(r, "Disabled"), Some(Variant::Bool(true)))
                || matches!(doc.prop(r, "Enabled"), Some(Variant::Bool(false)));
            return Some(Variant::Bool(off));
        }
        "Position" if class != "Terrain" && doc::is_a(class, "BasePart") => {
            if let Some(xf) = scene::xform_of(doc, r) {
                return Some(Variant::Vector3(Vector3::new(xf.pos.x, xf.pos.y, xf.pos.z)));
            }
        }
        "Orientation" if class != "Terrain" && doc::is_a(class, "BasePart") => {
            if let Some(xf) = scene::xform_of(doc, r) {
                let d = xf.orientation_deg();
                return Some(Variant::Vector3(Vector3::new(d.x, d.y, d.z)));
            }
        }
        "WorldCFrame" if doc::is_a(class, "Attachment") => {
            let xf = crate::attachment::world(doc, r, true)
                .unwrap_or_else(|| crate::attachment::local(doc, r, true));
            return Some(Variant::CFrame(xf.to_cframe()));
        }
        "WorldPosition" if doc::is_a(class, "Attachment") => {
            let xf = crate::attachment::world(doc, r, true)
                .unwrap_or_else(|| crate::attachment::local(doc, r, true));
            return Some(Variant::Vector3(Vector3::new(xf.pos.x, xf.pos.y, xf.pos.z)));
        }
        "WorldOrientation" if doc::is_a(class, "Attachment") => {
            let xf = crate::attachment::world(doc, r, true)
                .unwrap_or_else(|| crate::attachment::local(doc, r, true));
            let d = xf.orientation_deg();
            return Some(Variant::Vector3(Vector3::new(d.x, d.y, d.z)));
        }
        "TransformedWorldCFrame" if class == "Bone" => {
            let xf = crate::attachment::world(doc, r, true)
                .unwrap_or_else(|| crate::attachment::local(doc, r, true));
            return Some(Variant::CFrame(xf.to_cframe()));
        }
        "WorldPivot" if class == "Model" => {
            if let Some(xf) = crate::part_drag::editor_model_pivot(doc, r) {
                return Some(Variant::CFrame(xf.to_cframe()));
            }
        }
        _ => {}
    }
    doc.prop(r, prop)
}

fn select_props_json(doc: &Document, r: Ref, spec: Option<&Value>) -> Option<Map<String, Value>> {
    let inst = doc.dom.get_by_ref(r)?;
    let class = inst.class.as_str();
    match spec {
        None | Some(Value::Bool(false)) | Some(Value::Null) => None,
        Some(Value::Bool(true)) => {
            let mut map = Map::new();
            let mut keys: Vec<_> = inst.properties.keys().copied().collect();
            keys.sort_by_key(|k| k.as_str());
            for k in keys {
                if let Some(v) = inst.properties.get(&k) {
                    map.insert(
                        k.as_str().to_string(),
                        variants::variant_to_json_with_doc(doc, Some(class), Some(k.as_str()), v),
                    );
                }
            }
            Some(map)
        }
        Some(Value::Array(arr)) => {
            let mut map = Map::new();
            for item in arr {
                if let Some(p) = item.as_str() {
                    if let Some(v) = read_effective_prop(doc, r, p) {
                        map.insert(
                            p.to_string(),
                            variants::variant_to_json_with_doc(doc, Some(class), Some(p), &v),
                        );
                    }
                }
            }
            Some(map)
        }
        _ => None,
    }
}

fn parse_vec3_json(v: &Value) -> Result<Vec3, String> {
    match v {
        Value::Array(a) if a.len() == 3 => Ok(Vec3::new(
            a[0].as_f64().ok_or("expected number in Vec3")? as f32,
            a[1].as_f64().ok_or("expected number in Vec3")? as f32,
            a[2].as_f64().ok_or("expected number in Vec3")? as f32,
        )),
        Value::Object(m) => {
            let x = m.get("x").or_else(|| m.get("X")).and_then(|n| n.as_f64()).ok_or("Vec3 missing x")?;
            let y = m.get("y").or_else(|| m.get("Y")).and_then(|n| n.as_f64()).ok_or("Vec3 missing y")?;
            let z = m.get("z").or_else(|| m.get("Z")).and_then(|n| n.as_f64()).ok_or("Vec3 missing z")?;
            Ok(Vec3::new(x as f32, y as f32, z as f32))
        }
        _ => Err(format!("expected [x, y, z] or {{x, y, z}}, got {v}")),
    }
}

// =========================================================================
// 1. dom_get_tree
// =========================================================================

struct TreeFilter<'a> {
    class: Option<&'a str>,
    name_lower: Option<String>,
    tag: Option<&'a str>,
    include_props: Option<&'a Value>,
    max_depth: Option<usize>,
}

impl<'a> TreeFilter<'a> {
    fn has_filter(&self) -> bool {
        self.class.is_some() || self.name_lower.is_some() || self.tag.is_some()
    }

    fn matches(&self, doc: &Document, r: Ref) -> bool {
        let Some(inst) = doc.dom.get_by_ref(r) else {
            return false;
        };
        if let Some(cls) = self.class {
            if !doc::is_a(&inst.class, cls) {
                return false;
            }
        }
        if let Some(nl) = &self.name_lower {
            if !inst.name.to_ascii_lowercase().contains(nl) {
                return false;
            }
        }
        if let Some(t) = self.tag {
            if !instance_tags(doc, r).iter().any(|x| x == t) {
                return false;
            }
        }
        true
    }
}

fn node_summary_json(
    doc: &Document,
    r: Ref,
    depth: usize,
    include_props: Option<&Value>,
) -> Option<Map<String, Value>> {
    let inst = doc.dom.get_by_ref(r)?;
    let mut map = Map::new();
    map.insert("ref_id".into(), Value::String(variants::ref_to_str(r)));
    map.insert("path".into(), Value::String(variants::instance_path(doc, r)));
    map.insert("name".into(), Value::String(inst.name.clone()));
    map.insert("class".into(), Value::String(inst.class.to_string()));
    map.insert("child_count".into(), json!(inst.children().len()));
    map.insert("depth".into(), json!(depth));
    let tags = instance_tags(doc, r);
    map.insert("tags".into(), json!(tags));
    if let Some(props) = select_props_json(doc, r, include_props) {
        map.insert("properties".into(), Value::Object(props));
    }
    Some(map)
}

fn build_tree_node(
    doc: &Document,
    r: Ref,
    depth: usize,
    filter: &TreeFilter<'_>,
) -> Option<(Value, bool)> {
    let inst = doc.dom.get_by_ref(r)?;
    let direct_match = filter.matches(doc, r);
    let at_limit = filter.max_depth.is_some_and(|m| depth >= m);

    let mut children_json = Vec::new();
    let mut any_descendant_match = false;
    if !at_limit {
        for &c in inst.children() {
            if let Some((cj, cm)) = build_tree_node(doc, c, depth + 1, filter) {
                if cm {
                    any_descendant_match = true;
                }
                if !filter.has_filter() || cm {
                    children_json.push(cj);
                }
            }
        }
    }

    let subtree_match = direct_match || any_descendant_match;
    if filter.has_filter() && !subtree_match {
        return None;
    }

    let mut map = node_summary_json(doc, r, depth, filter.include_props)?;
    if filter.has_filter() {
        map.insert("matched".into(), json!(direct_match));
    }
    if at_limit && !inst.children().is_empty() {
        map.insert("truncated_children".into(), json!(true));
    } else {
        map.insert("children".into(), Value::Array(children_json));
    }
    Some((Value::Object(map), subtree_match))
}

fn collect_flat_tree(
    doc: &Document,
    r: Ref,
    depth: usize,
    filter: &TreeFilter<'_>,
    out: &mut Vec<Value>,
) {
    let Some(inst) = doc.dom.get_by_ref(r) else {
        return;
    };
    if filter.matches(doc, r) {
        if let Some(mut map) = node_summary_json(doc, r, depth, filter.include_props) {
            let parent = inst.parent();
            map.insert(
                "parent_ref".into(),
                if parent.is_none() {
                    Value::Null
                } else {
                    Value::String(variants::ref_to_str(parent))
                },
            );
            out.push(Value::Object(map));
        }
    }
    if filter.max_depth.is_some_and(|m| depth >= m) {
        return;
    }
    for &c in inst.children() {
        collect_flat_tree(doc, c, depth + 1, filter, out);
    }
}

pub(crate) fn dom_get_tree_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let root = match args.get("root") {
        Some(v) if !v.is_null() => variants::resolve_target(doc, v)?,
        _ => doc.dom.root_ref(),
    };
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let flat = args.get("flat").and_then(|v| v.as_bool()).unwrap_or(false);
    let class = args.get("class").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let name_lower = args
        .get("name_contains")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let tag = args.get("tag").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let include_props = args.get("include_properties");

    let filter = TreeFilter {
        class,
        name_lower,
        tag,
        include_props,
        max_depth,
    };

    if flat {
        let mut nodes = Vec::new();
        collect_flat_tree(doc, root, 0, &filter, &mut nodes);
        Ok(json!({
            "flat": true,
            "root": variants::instance_path(doc, root),
            "count": nodes.len(),
            "nodes": nodes,
        }))
    } else {
        let tree = build_tree_node(doc, root, 0, &filter)
            .map(|(v, _)| v)
            .unwrap_or_else(|| json!(null));
        Ok(json!({
            "flat": false,
            "root": variants::instance_path(doc, root),
            "tree": tree,
        }))
    }
}

fn handle_get_tree(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    dom_get_tree_in(doc, args)
}

// =========================================================================
// 2. dom_inspect_instance
// =========================================================================

fn data_type_label(dt: &DataType<'_>) -> String {
    match dt {
        DataType::Value(vt) => format!("{vt:?}"),
        DataType::Enum(e) => format!("Enum.{e}"),
        _ => "Unknown".into(),
    }
}

fn scriptability_label(s: Scriptability) -> &'static str {
    match s {
        Scriptability::None => "None",
        Scriptability::ReadWrite => "ReadWrite",
        Scriptability::Read => "Read",
        Scriptability::Write => "Write",
        Scriptability::Custom => "Custom",
        _ => "Unknown",
    }
}

pub(crate) fn inspect_one_in(
    doc: &Document,
    r: Ref,
    include_defaults: bool,
    include_security: bool,
) -> Result<Value, String> {
    let inst = doc
        .dom
        .get_by_ref(r)
        .ok_or_else(|| format!("instance '{}' not found", variants::ref_to_str(r)))?;
    let class = inst.class.as_str();

    let mut stored_map = Map::new();
    let mut stored_keys: Vec<_> = inst.properties.keys().copied().collect();
    stored_keys.sort_by_key(|k| k.as_str());
    for k in stored_keys {
        if let Some(v) = inst.properties.get(&k) {
            stored_map.insert(
                k.as_str().to_string(),
                variants::variant_to_json_with_doc(doc, Some(class), Some(k.as_str()), v),
            );
        }
    }

    let mut all_props: BTreeMap<String, Value> = BTreeMap::new();
    for (k, v) in &stored_map {
        all_props.insert(k.clone(), v.clone());
    }

    let mut prop_meta: BTreeMap<String, Value> = BTreeMap::new();
    let db = doc::db();
    let api = crate::api::get();

    if let Some(desc) = db.classes.get(class) {
        for cls in db.superclasses_iter(desc) {
            for (pname, pdesc) in &cls.properties {
                if include_defaults && !all_props.contains_key(*pname) {
                    let resolved_val = read_effective_prop(doc, r, pname).or_else(|| {
                        if let PropertyKind::Alias { alias_for } = &pdesc.kind {
                            read_effective_prop(doc, r, alias_for)
                        } else {
                            None
                        }
                    });
                    if let Some(v) = resolved_val {
                        all_props.insert(
                            (*pname).to_string(),
                            variants::variant_to_json_with_doc(doc, Some(class), Some(pname), &v),
                        );
                    }
                }
                if include_security && !prop_meta.contains_key(*pname) {
                    let api_member = api.and_then(|a| a.lookup(class, pname).map(|(_, m)| m));
                    let security = api_member
                        .map(|m| m.security.clone())
                        .unwrap_or_else(|| "Unknown".into());
                    prop_meta.insert(
                        (*pname).to_string(),
                        json!({
                            "data_type": data_type_label(&pdesc.data_type),
                            "scriptability": scriptability_label(pdesc.scriptability),
                            "security": security,
                            "not_scriptable": pdesc.tags.contains(&PropertyTag::NotScriptable)
                                || matches!(pdesc.scriptability, Scriptability::None),
                            "read_only": pdesc.tags.contains(&PropertyTag::ReadOnly)
                                || matches!(pdesc.scriptability, Scriptability::Read | Scriptability::None),
                            "hidden": pdesc.tags.contains(&PropertyTag::Hidden)
                                || pdesc.tags.contains(&PropertyTag::NotBrowsable),
                            "deprecated": pdesc.tags.contains(&PropertyTag::Deprecated),
                            "not_replicated": pdesc.tags.contains(&PropertyTag::NotReplicated),
                        }),
                    );
                }
            }
        }
    }

    if include_security {
        if let Some(a) = api {
            let mut cur = Some(class);
            let mut guard = 0;
            while let Some(cname) = cur {
                let Some(ac) = a.classes.get(cname) else {
                    break;
                };
                for (mname, m) in &ac.members {
                    if m.kind == crate::api::Kind::Property && !prop_meta.contains_key(mname) {
                        prop_meta.insert(
                            mname.clone(),
                            json!({
                                "data_type": "ApiDump",
                                "scriptability": if m.read_only { "Read" } else { "ReadWrite" },
                                "security": m.security,
                                "not_scriptable": false,
                                "read_only": m.read_only,
                                "hidden": m.hidden,
                                "deprecated": m.deprecated,
                                "not_replicated": !m.can_load,
                            }),
                        );
                    }
                }
                cur = ac.superclass.as_deref();
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
        }
    }

    if include_defaults {
        for derived in [
            "Name",
            "ClassName",
            "Parent",
            "Archivable",
            "Position",
            "Orientation",
            "WorldCFrame",
            "WorldPosition",
            "WorldOrientation",
            "TransformedWorldCFrame",
            "WorldPivot",
            "Enabled",
            "Disabled",
        ] {
            if !all_props.contains_key(derived) {
                if let Some(v) = read_effective_prop(doc, r, derived) {
                    all_props.insert(
                        derived.to_string(),
                        variants::variant_to_json_with_doc(doc, Some(class), Some(derived), &v),
                    );
                }
            }
        }
    }

    let parent_json = if inst.parent().is_none() {
        Value::Null
    } else {
        let p = inst.parent();
        json!({
            "ref_id": variants::ref_to_str(p),
            "path": variants::instance_path(doc, p),
            "name": doc.dom.get_by_ref(p).map(|pi| pi.name.as_str()).unwrap_or(""),
            "class": doc.dom.get_by_ref(p).map(|pi| pi.class.as_str()).unwrap_or(""),
        })
    };

    let children: Vec<Value> = inst
        .children()
        .iter()
        .filter_map(|&c| {
            let ci = doc.dom.get_by_ref(c)?;
            Some(json!({
                "ref_id": variants::ref_to_str(c),
                "name": ci.name,
                "class": ci.class.as_str(),
                "child_count": ci.children().len(),
            }))
        })
        .collect();

    let archivable = match doc.prop(r, "Archivable") {
        Some(Variant::Bool(b)) => b,
        _ => true,
    };

    let mut props_obj = Map::new();
    for (k, v) in all_props {
        props_obj.insert(k, v);
    }

    let mut out = Map::new();
    out.insert("ref_id".into(), Value::String(variants::ref_to_str(r)));
    out.insert("path".into(), Value::String(variants::instance_path(doc, r)));
    out.insert("name".into(), Value::String(inst.name.clone()));
    out.insert("class".into(), Value::String(class.to_string()));
    out.insert("parent".into(), parent_json);
    out.insert("archivable".into(), json!(archivable));
    out.insert("tags".into(), json!(instance_tags(doc, r)));
    out.insert("attributes".into(), Value::Object(instance_attributes(doc, r)));
    if let Some(w) = world_spatial_info(doc, r) {
        out.insert("world".into(), w);
    }
    out.insert("child_count".into(), json!(children.len()));
    out.insert("children".into(), Value::Array(children));
    out.insert("stored_properties".into(), Value::Object(stored_map));
    out.insert("properties".into(), Value::Object(props_obj));
    if include_security {
        let mut meta_obj = Map::new();
        for (k, v) in prop_meta {
            meta_obj.insert(k, v);
        }
        out.insert("property_security".into(), Value::Object(meta_obj));
    }
    Ok(Value::Object(out))
}

pub(crate) fn dom_inspect_instance_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let refs = resolve_targets_arg(doc, args, "target", "targets")?;
    let include_defaults = args
        .get("include_defaults")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let include_security = args
        .get("include_security")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let single_mode = args.get("targets").is_none() && args.get("target").is_some();
    if single_mode {
        inspect_one_in(doc, refs[0], include_defaults, include_security)
    } else {
        let mut instances = Vec::with_capacity(refs.len());
        for r in refs {
            instances.push(inspect_one_in(doc, r, include_defaults, include_security)?);
        }
        Ok(json!({
            "count": instances.len(),
            "instances": instances,
        }))
    }
}

fn handle_inspect_instance(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    dom_inspect_instance_in(doc, args)
}

// =========================================================================
// 3. dom_query
// =========================================================================

fn variants_match(actual: &Variant, expected: &Variant) -> bool {
    if actual == expected {
        return true;
    }
    let as_f64 = |v: &Variant| -> Option<f64> {
        match v {
            Variant::Float32(n) => Some(*n as f64),
            Variant::Float64(n) => Some(*n),
            Variant::Int32(n) => Some(*n as f64),
            Variant::Int64(n) => Some(*n as f64),
            _ => None,
        }
    };
    if let (Some(a), Some(b)) = (as_f64(actual), as_f64(expected)) {
        return (a - b).abs() <= 1e-4;
    }
    match (actual, expected) {
        (Variant::Vector3(a), Variant::Vector3(b)) => {
            (a.x - b.x).abs() <= 1e-4 && (a.y - b.y).abs() <= 1e-4 && (a.z - b.z).abs() <= 1e-4
        }
        (Variant::Vector2(a), Variant::Vector2(b)) => {
            (a.x - b.x).abs() <= 1e-4 && (a.y - b.y).abs() <= 1e-4
        }
        (Variant::Color3(a), Variant::Color3(b)) => {
            (a.r - b.r).abs() <= 1e-3 && (a.g - b.g).abs() <= 1e-3 && (a.b - b.b).abs() <= 1e-3
        }
        (Variant::Color3uint8(a), Variant::Color3(b))
        | (Variant::Color3(b), Variant::Color3uint8(a)) => {
            ((a.r as f32 / 255.0) - b.r).abs() <= 0.01
                && ((a.g as f32 / 255.0) - b.g).abs() <= 0.01
                && ((a.b as f32 / 255.0) - b.b).abs() <= 0.01
        }
        (Variant::Enum(a), Variant::EnumItem(b)) => a.to_u32() == b.value,
        (Variant::EnumItem(a), Variant::Enum(b)) => a.value == b.to_u32(),
        (Variant::Content(c), Variant::ContentId(cid))
        | (Variant::ContentId(cid), Variant::Content(c)) => {
            c.as_uri().unwrap_or("") == cid.as_str()
        }
        (Variant::Content(c), Variant::String(s))
        | (Variant::String(s), Variant::Content(c)) => c.as_uri().unwrap_or("") == s.as_str(),
        (Variant::ContentId(c), Variant::String(s))
        | (Variant::String(s), Variant::ContentId(c)) => c.as_str() == s.as_str(),
        (Variant::BinaryString(b), Variant::String(s))
        | (Variant::String(s), Variant::BinaryString(b)) => {
            std::str::from_utf8(b.as_ref()).is_ok_and(|t| t == s)
        }
        _ => false,
    }
}

fn json_matches_variant(
    doc: &Document,
    class: Option<&str>,
    prop: Option<&str>,
    actual: Option<&Variant>,
    expected: &Value,
) -> bool {
    if expected.is_null() {
        return match actual {
            None => true,
            Some(Variant::Ref(r)) => r.is_none(),
            Some(Variant::OptionalCFrame(None)) => true,
            Some(Variant::Content(c)) => c.as_uri().is_none() && c.as_object().is_none(),
            _ => false,
        };
    }
    let Some(actual) = actual else {
        return false;
    };
    if let Ok(exp_var) = variants::json_to_variant(Some(doc), class, prop, expected, None) {
        if variants_match(actual, &exp_var) {
            return true;
        }
    }
    variants::variant_to_json_with_doc(doc, class, prop, actual) == *expected
}

fn parse_kv_filter(obj: &Map<String, Value>) -> Vec<(String, Value)> {
    if let (Some(Value::String(name)), Some(val)) = (obj.get("name"), obj.get("value")) {
        if obj.len() == 2 {
            return vec![(name.clone(), val.clone())];
        }
    }
    obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

pub(crate) fn dom_query_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let root = match args.get("root") {
        Some(v) if !v.is_null() => variants::resolve_target(doc, v)?,
        _ => doc.dom.root_ref(),
    };
    let is_a = args.get("is_a").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let exact_class = args
        .get("exact_class")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let name_pattern = args
        .get("name_pattern")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let path_prefix = args
        .get("path_prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(normalize_path_prefix);
    let has_tag = args
        .get("has_tag")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let has_attribute = args
        .get("has_attribute")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let attr_eq = match args.get("attribute_equals") {
        Some(Value::Object(m)) => parse_kv_filter(m),
        Some(v) if !v.is_null() => return Err("attribute_equals must be an object".into()),
        _ => Vec::new(),
    };
    let prop_eq = match args.get("property_equals") {
        Some(Value::Object(m)) => parse_kv_filter(m),
        Some(v) if !v.is_null() => return Err("property_equals must be an object".into()),
        _ => Vec::new(),
    };
    let sphere = match args.get("within_sphere") {
        Some(Value::Object(m)) => {
            let c = parse_vec3_json(m.get("center").ok_or("within_sphere missing center")?)?;
            let r = m
                .get("radius")
                .and_then(|v| v.as_f64())
                .ok_or("within_sphere missing radius")? as f32;
            Some((c, r))
        }
        Some(v) if !v.is_null() => return Err("within_sphere must be an object".into()),
        _ => None,
    };
    let bbox = match args.get("within_box") {
        Some(Value::Object(m)) => {
            let bmin = parse_vec3_json(m.get("min").ok_or("within_box missing min")?)?;
            let bmax = parse_vec3_json(m.get("max").ok_or("within_box missing max")?)?;
            Some((bmin.min(bmax), bmin.max(bmax)))
        }
        Some(v) if !v.is_null() => return Err("within_box must be an object".into()),
        _ => None,
    };
    let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
    let select_props = args.get("select_properties");

    let mut total_matched = 0usize;
    let mut results = Vec::new();

    for inst in doc.dom.descendants_of(root) {
        let r = inst.referent();
        let class = inst.class.as_str();
        if let Some(base) = is_a {
            if !doc::is_a(class, base) {
                continue;
            }
        }
        if let Some(exact) = exact_class {
            if class != exact {
                continue;
            }
        }
        if let Some(pat) = &name_pattern {
            if !inst.name.to_ascii_lowercase().contains(pat) {
                continue;
            }
        }
        let mut cached_path: Option<String> = None;
        if let Some(prefix) = &path_prefix {
            let p = variants::instance_path(doc, r);
            if !path_matches_prefix(&p, prefix) {
                continue;
            }
            cached_path = Some(p);
        }
        if let Some(tag) = has_tag {
            if !instance_tags(doc, r).iter().any(|t| t == tag) {
                continue;
            }
        }
        if let Some(attr) = has_attribute {
            let present = matches!(
                doc.prop(r, "Attributes"),
                Some(Variant::Attributes(a)) if a.get(attr).is_some()
            );
            if !present {
                continue;
            }
        }
        if !attr_eq.is_empty() {
            let attrs = match doc.prop(r, "Attributes") {
                Some(Variant::Attributes(a)) => Some(a),
                _ => None,
            };
            let ok = attr_eq.iter().all(|(k, exp)| {
                let actual = attrs.as_ref().and_then(|a| a.get(k.as_str()));
                json_matches_variant(doc, None, None, actual, exp)
            });
            if !ok {
                continue;
            }
        }
        if !prop_eq.is_empty() {
            let ok = prop_eq.iter().all(|(k, exp)| {
                let actual = read_effective_prop(doc, r, k);
                json_matches_variant(doc, Some(class), Some(k), actual.as_ref(), exp)
            });
            if !ok {
                continue;
            }
        }
        if sphere.is_some() || bbox.is_some() {
            let Some(pos) = world_position_of(doc, r) else {
                continue;
            };
            if let Some((center, radius)) = sphere {
                if (pos - center).length() > radius {
                    continue;
                }
            }
            if let Some((bmin, bmax)) = bbox {
                if pos.x < bmin.x
                    || pos.y < bmin.y
                    || pos.z < bmin.z
                    || pos.x > bmax.x
                    || pos.y > bmax.y
                    || pos.z > bmax.z
                {
                    continue;
                }
            }
        }

        total_matched += 1;
        if limit.is_none_or(|lim| results.len() < lim) {
            let path = cached_path.unwrap_or_else(|| variants::instance_path(doc, r));
            let mut item = Map::new();
            item.insert("ref_id".into(), Value::String(variants::ref_to_str(r)));
            item.insert("path".into(), Value::String(path));
            item.insert("name".into(), Value::String(inst.name.clone()));
            item.insert("class".into(), Value::String(class.to_string()));
            item.insert("tags".into(), json!(instance_tags(doc, r)));
            if let Some(props) = select_props_json(doc, r, select_props) {
                item.insert("properties".into(), Value::Object(props));
            }
            results.push(Value::Object(item));
        }
    }

    Ok(json!({
        "total_matched": total_matched,
        "returned": results.len(),
        "truncated": results.len() < total_matched,
        "results": results,
    }))
}

fn handle_query(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    dom_query_in(doc, args)
}

// =========================================================================
// Shared property preparation for create & set_properties
// =========================================================================

enum PlannedProp {
    Static(Ustr, Variant),
    TempRef(Ustr, String),
}

fn parse_temp_ref(val: &Value) -> Option<&str> {
    match val {
        Value::String(s) => {
            let t = s.trim();
            t.strip_prefix('$').or_else(|| t.strip_prefix("temp:"))
        }
        Value::Object(m) => m
            .get("temp_id")
            .and_then(|v| v.as_str())
            .or_else(|| {
                m.get("ref")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.strip_prefix('$').or_else(|| s.strip_prefix("temp:")))
            }),
        _ => None,
    }
}

fn prepare_props_map(
    doc: &Document,
    class: &str,
    existing_ref: Option<Ref>,
    props: &Map<String, Value>,
    known_temp_ids: Option<&HashSet<String>>,
) -> Result<(Option<String>, Vec<PlannedProp>), String> {
    let mut out = Vec::new();
    let mut name_override: Option<String> = None;

    let has_cframe = props.contains_key("CFrame");
    let has_pos = props.contains_key("Position");
    let has_ori = props.contains_key("Orientation");

    for (k, v) in props {
        if k == "Parent" {
            return Err("cannot set 'Parent' via properties; use parent field or dom_reparent_rename_delete".into());
        }
        if k == "Name" {
            let s = v
                .as_str()
                .ok_or_else(|| format!("property 'Name' must be a string, got {v}"))?;
            name_override = Some(s.to_string());
            continue;
        }
        if !has_cframe && (k == "Position" || k == "Orientation") && class != "Terrain" && doc::is_a(class, "BasePart") {
            continue;
        }

        if let Some(tid) = parse_temp_ref(v) {
            if let Some(set) = known_temp_ids {
                if !set.contains(tid) {
                    return Err(format!("unknown temp_id '${tid}' for property '{k}'"));
                }
                out.push(PlannedProp::TempRef(ustr(k), tid.to_string()));
                continue;
            }
        }

        let variant = variants::json_to_variant(Some(doc), Some(class), Some(k), v, None)
            .map_err(|e| format!("{class}.{k}: {e}"))?;

        if k == "Enabled" && doc::is_a(class, "BaseScript") {
            if let Variant::Bool(on) = variant {
                out.push(PlannedProp::Static(ustr("Enabled"), Variant::Bool(on)));
                if !props.contains_key("Disabled") {
                    out.push(PlannedProp::Static(ustr("Disabled"), Variant::Bool(!on)));
                }
                continue;
            }
        }
        if k == "Disabled" && doc::is_a(class, "BaseScript") {
            if let Variant::Bool(off) = variant {
                out.push(PlannedProp::Static(ustr("Disabled"), Variant::Bool(off)));
                if !props.contains_key("Enabled") {
                    out.push(PlannedProp::Static(ustr("Enabled"), Variant::Bool(!off)));
                }
                continue;
            }
        }

        if k == "BrickColor" && doc::is_a(class, "BasePart") {
            if let Variant::BrickColor(bc) = variant {
                out.push(PlannedProp::Static(ustr("BrickColor"), variant));
                if !props.contains_key("Color") {
                    let rgb = scene::brick_rgb(bc);
                    out.push(PlannedProp::Static(
                        ustr("Color"),
                        Variant::Color3uint8(Color3uint8::new(rgb[0], rgb[1], rgb[2])),
                    ));
                }
                continue;
            }
        }

        if k == "WorldPivot" && doc::is_a(class, "Model") {
            if let Variant::CFrame(cf) = variant {
                out.push(PlannedProp::Static(ustr("WorldPivot"), variant));
                if !props.contains_key("WorldPivotData") {
                    out.push(PlannedProp::Static(
                        ustr("WorldPivotData"),
                        Variant::OptionalCFrame(Some(cf)),
                    ));
                }
                continue;
            }
        }

        if let Some(twin) = doc::content_twin(k) {
            if !props.contains_key(twin) {
                let uri = scene::content_string(Some(variant.clone())).unwrap_or_default();
                let c = if uri.is_empty() {
                    Content::none()
                } else {
                    Content::from_uri(uri)
                };
                out.push(PlannedProp::Static(ustr(twin), Variant::Content(c)));
            }
        } else if k.ends_with("Content") && k != "Attributes" {
            let uri = scene::content_string(Some(variant.clone())).unwrap_or_default();
            for legacy in doc::content_legacy_names(k) {
                if !props.contains_key(legacy)
                    && variants::expected_variant_type(class, legacy).is_some()
                {
                    out.push(PlannedProp::Static(
                        ustr(legacy),
                        Variant::ContentId(ContentId::from(uri.clone())),
                    ));
                }
            }
        }

        out.push(PlannedProp::Static(ustr(k), variant));
    }

    if !has_cframe && (has_pos || has_ori) && class != "Terrain" && doc::is_a(class, "BasePart") {
        let mut xf = existing_ref
            .and_then(|r| scene::xform_of(doc, r))
            .unwrap_or(Xform {
                pos: Vec3::ZERO,
                rot: Mat3::IDENTITY,
            });
        if let Some(pv) = props.get("Position") {
            xf.pos = parse_vec3_json(pv).map_err(|e| format!("{class}.Position: {e}"))?;
        }
        if let Some(ov) = props.get("Orientation") {
            let deg = parse_vec3_json(ov).map_err(|e| format!("{class}.Orientation: {e}"))?;
            xf = Xform::from_orientation_deg(xf.pos, deg);
        }
        out.push(PlannedProp::Static(
            ustr("CFrame"),
            Variant::CFrame(xf.to_cframe()),
        ));
    }

    Ok((name_override, out))
}

fn parse_attributes_into(
    doc: &Document,
    existing: Option<Attributes>,
    map: &Map<String, Value>,
) -> Result<Attributes, String> {
    let mut attrs = existing.unwrap_or_default();
    for (k, v) in map {
        if v.is_null() {
            attrs.remove(k.as_str());
        } else {
            let av = variants::json_to_variant(Some(doc), None, None, v, None)
                .map_err(|e| format!("attribute '{k}': {e}"))?;
            attrs.insert(k.clone(), av);
        }
    }
    Ok(attrs)
}

fn parse_tags_array(v: &Value, field: &str) -> Result<Vec<String>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| format!("'{field}' must be an array of strings"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let s = item
            .as_str()
            .ok_or_else(|| format!("{field}[{i}] must be a string"))?;
        out.push(s.to_string());
    }
    Ok(out)
}

// =========================================================================
// 4. dom_create_instances
// =========================================================================

struct PlannedNode {
    class: String,
    name: String,
    temp_id: Option<String>,
    props: Vec<PlannedProp>,
    children: Vec<PlannedNode>,
}

struct PlannedRootItem {
    parent: Ref,
    node: PlannedNode,
}

fn collect_temp_ids(val: &Value, out: &mut HashSet<String>) -> Result<(), String> {
    let obj = val
        .as_object()
        .ok_or("each item in dom_create_instances must be an object")?;
    if let Some(tid) = obj.get("temp_id").and_then(|v| v.as_str()) {
        if !out.insert(tid.to_string()) {
            return Err(format!("duplicate temp_id '{tid}'"));
        }
    }
    if let Some(children) = obj.get("children") {
        let arr = children
            .as_array()
            .ok_or("'children' must be an array")?;
        for c in arr {
            collect_temp_ids(c, out)?;
        }
    }
    Ok(())
}

fn plan_node(
    doc: &Document,
    val: &Value,
    temp_ids: &HashSet<String>,
) -> Result<PlannedNode, String> {
    let obj = val
        .as_object()
        .ok_or("creation item must be an object")?;
    let class = obj
        .get("class")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("creation item missing non-empty 'class'")?
        .to_string();
    let temp_id = obj
        .get("temp_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let (prop_name, mut props) = match obj.get("properties") {
        Some(Value::Object(m)) => prepare_props_map(doc, &class, None, m, Some(temp_ids))?,
        Some(v) if !v.is_null() => return Err("'properties' must be an object".into()),
        _ => (None, Vec::new()),
    };

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(prop_name)
        .unwrap_or_else(|| class.clone());

    if let Some(attr_val) = obj.get("attributes") {
        if !attr_val.is_null() {
            let amap = attr_val
                .as_object()
                .ok_or("'attributes' must be an object")?;
            let existing = props.iter().find_map(|p| match p {
                PlannedProp::Static(k, Variant::Attributes(a)) if k.as_str() == "Attributes" => {
                    Some(a.clone())
                }
                _ => None,
            });
            let attrs = parse_attributes_into(doc, existing, amap)?;
            props.retain(|p| !matches!(p, PlannedProp::Static(k, _) if k.as_str() == "Attributes"));
            props.push(PlannedProp::Static(
                ustr("Attributes"),
                Variant::Attributes(attrs),
            ));
        }
    }

    if let Some(tags_val) = obj.get("tags") {
        if !tags_val.is_null() {
            let mut list: Vec<String> = props
                .iter()
                .find_map(|p| match p {
                    PlannedProp::Static(k, Variant::Tags(t)) if k.as_str() == "Tags" => {
                        Some(t.iter().map(|s| s.to_string()).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            for t in parse_tags_array(tags_val, "tags")? {
                if !list.contains(&t) {
                    list.push(t);
                }
            }
            props.retain(|p| !matches!(p, PlannedProp::Static(k, _) if k.as_str() == "Tags"));
            props.push(PlannedProp::Static(
                ustr("Tags"),
                Variant::Tags(Tags::from(list)),
            ));
        }
    }

    let mut children = Vec::new();
    if let Some(cv) = obj.get("children") {
        if !cv.is_null() {
            let arr = cv.as_array().ok_or("'children' must be an array")?;
            for child in arr {
                children.push(plan_node(doc, child, temp_ids)?);
            }
        }
    }

    Ok(PlannedNode {
        class,
        name,
        temp_id,
        props,
        children,
    })
}

fn insert_planned_subtree(
    dom: &mut WeakDom,
    parent: Ref,
    node: PlannedNode,
    temp_map: &mut HashMap<String, Ref>,
    pending_refs: &mut Vec<(Ref, Ustr, String)>,
    all_created: &mut Vec<Ref>,
) {
    let mut builder = InstanceBuilder::new(node.class).with_name(node.name);
    let mut deferred_props = Vec::new();
    for p in node.props {
        match p {
            PlannedProp::Static(k, v) => builder.add_property(k, v),
            PlannedProp::TempRef(k, tid) => deferred_props.push((k, tid)),
        }
    }
    let r = dom.insert(parent, builder);
    all_created.push(r);
    if let Some(tid) = node.temp_id {
        temp_map.insert(tid, r);
    }
    for (k, tid) in deferred_props {
        pending_refs.push((r, k, tid));
    }
    for child in node.children {
        insert_planned_subtree(dom, r, child, temp_map, pending_refs, all_created);
    }
}

fn build_planned_into_doc(
    doc: &mut Document,
    planned: Vec<PlannedRootItem>,
    attach_parentless_then_reparent: bool,
) -> (Vec<(Ref, Ref)>, Vec<Ref>) {
    let mut temp_map = HashMap::new();
    let mut pending_refs = Vec::new();
    let mut all_created = Vec::new();
    let mut top_roots = Vec::with_capacity(planned.len());

    for item in planned {
        let insert_under = if attach_parentless_then_reparent {
            doc.dom.root_ref()
        } else {
            item.parent
        };
        let mut builder = InstanceBuilder::new(item.node.class).with_name(item.node.name);
        let mut deferred_props = Vec::new();
        for p in item.node.props {
            match p {
                PlannedProp::Static(k, v) => builder.add_property(k, v),
                PlannedProp::TempRef(k, tid) => deferred_props.push((k, tid)),
            }
        }
        let r = doc.dom.insert(insert_under, builder);
        if attach_parentless_then_reparent {
            doc.dom.transfer_within(r, Ref::none());
        }
        all_created.push(r);
        top_roots.push((r, item.parent));
        if let Some(tid) = item.node.temp_id {
            temp_map.insert(tid, r);
        }
        for (k, tid) in deferred_props {
            pending_refs.push((r, k, tid));
        }
        for child in item.node.children {
            insert_planned_subtree(
                &mut doc.dom,
                r,
                child,
                &mut temp_map,
                &mut pending_refs,
                &mut all_created,
            );
        }
    }

    for (r, k, tid) in pending_refs {
        if let Some(&target_r) = temp_map.get(&tid) {
            if let Some(inst) = doc.dom.get_by_ref_mut(r) {
                inst.properties.insert(k, Variant::Ref(target_r));
            }
        }
    }
    (top_roots, all_created)
}

pub(crate) fn dom_create_instances_in(doc: &mut Document, args: &Value) -> Result<Value, String> {
    let items = args
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or("dom_create_instances requires 'items' array")?;

    let mut temp_ids = HashSet::new();
    for (idx, item) in items.iter().enumerate() {
        collect_temp_ids(item, &mut temp_ids).map_err(|e| format!("items[{idx}]: {e}"))?;
    }

    let mut planned = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        let parent = match item.get("parent") {
            Some(pv) if !pv.is_null() => variants::resolve_target(doc, pv)
                .map_err(|e| format!("items[{idx}].parent: {e}"))?,
            _ => doc.default_parent(),
        };
        let node = plan_node(doc, item, &temp_ids).map_err(|e| format!("items[{idx}]: {e}"))?;
        planned.push(PlannedRootItem { parent, node });
    }

    let (top_roots, all_created) = build_planned_into_doc(doc, planned, false);
    let changes: Vec<Change> = top_roots
        .iter()
        .map(|&(r, parent)| Change::Create { r, parent })
        .collect();
    doc.record_changes("Create instances", changes);

    let created: Vec<Value> = all_created
        .iter()
        .map(|&r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
                "name": doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                "class": doc.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(json!({
        "created_count": created.len(),
        "created": created,
    }))
}

fn handle_create_instances(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    with_edit_or_play(
        ed,
        args,
        |doc| dom_create_instances_in(doc, args),
        |rt| {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or("dom_create_instances requires 'items' array")?;

            let (planned, all_created) = {
                let mut w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                let d = w.d();
                let mut temp_ids = HashSet::new();
                for (idx, item) in items.iter().enumerate() {
                    collect_temp_ids(item, &mut temp_ids)
                        .map_err(|e| format!("items[{idx}]: {e}"))?;
                }
                let mut planned = Vec::with_capacity(items.len());
                for (idx, item) in items.iter().enumerate() {
                    let parent = match item.get("parent") {
                        Some(pv) if !pv.is_null() => variants::resolve_target(d, pv)
                            .map_err(|e| format!("items[{idx}].parent: {e}"))?,
                        _ => d.default_parent(),
                    };
                    let node = plan_node(d, item, &temp_ids)
                        .map_err(|e| format!("items[{idx}]: {e}"))?;
                    planned.push(PlannedRootItem { parent, node });
                }
                let (top_roots, all_created) = build_planned_into_doc(w.dm(), planned, true);
                (top_roots, all_created)
            };

            for (r, parent) in planned {
                rt.apply_parent(r, parent);
            }

            let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
            let d = w.d();
            let created: Vec<Value> = all_created
                .iter()
                .map(|&r| {
                    json!({
                        "ref_id": variants::ref_to_str(r),
                        "path": variants::instance_path(d, r),
                        "name": d.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                        "class": d.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                    })
                })
                .collect();

            Ok(json!({
                "created_count": created.len(),
                "created": created,
            }))
        },
    )
}

// =========================================================================
// 5. dom_set_properties
// =========================================================================

struct StagedEdit {
    r: Ref,
    rename: Option<String>,
    props: Vec<(Ustr, Variant)>,
}

fn stage_property_edits(doc: &Document, args: &Value) -> Result<Vec<StagedEdit>, String> {
    let raw_edits: Vec<&Value> = if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
        arr.iter().collect()
    } else if args.get("target").is_some() {
        vec![args]
    } else {
        return Err("dom_set_properties requires 'edits' array".into());
    };

    let mut staged = Vec::with_capacity(raw_edits.len());
    for (idx, item) in raw_edits.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("edits[{idx}] must be an object"))?;
        let tv = obj
            .get("target")
            .ok_or_else(|| format!("edits[{idx}] missing 'target'"))?;
        let r = variants::resolve_target(doc, tv).map_err(|e| format!("edits[{idx}]: {e}"))?;
        let class = doc
            .class_of(r)
            .ok_or_else(|| format!("edits[{idx}]: instance missing"))?
            .to_string();

        let explicit_name = match obj.get("name") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(v) if !v.is_null() => return Err(format!("edits[{idx}].name must be a string")),
            _ => None,
        };

        let mut props_vec = Vec::new();
        let mut prop_name = None;
        if let Some(pv) = obj.get("properties") {
            if !pv.is_null() {
                let pmap = pv
                    .as_object()
                    .ok_or_else(|| format!("edits[{idx}].properties must be an object"))?;
                let (pn, planned) = prepare_props_map(doc, &class, Some(r), pmap, None)
                    .map_err(|e| format!("edits[{idx}]: {e}"))?;
                prop_name = pn;
                for p in planned {
                    if let PlannedProp::Static(k, v) = p {
                        props_vec.push((k, v));
                    }
                }
            }
        }

        if let Some(av) = obj.get("attributes") {
            if !av.is_null() {
                let amap = av
                    .as_object()
                    .ok_or_else(|| format!("edits[{idx}].attributes must be an object"))?;
                let base_attrs = props_vec
                    .iter()
                    .find_map(|(k, v)| match (k.as_str(), v) {
                        ("Attributes", Variant::Attributes(a)) => Some(a.clone()),
                        _ => None,
                    })
                    .or_else(|| match doc.prop(r, "Attributes") {
                        Some(Variant::Attributes(a)) => Some(a),
                        _ => None,
                    });
                let merged = parse_attributes_into(doc, base_attrs, amap)
                    .map_err(|e| format!("edits[{idx}]: {e}"))?;
                props_vec.retain(|(k, _)| k.as_str() != "Attributes");
                props_vec.push((ustr("Attributes"), Variant::Attributes(merged)));
            }
        }

        let add_tags = match obj.get("add_tags") {
            Some(v) if !v.is_null() => Some(parse_tags_array(v, &format!("edits[{idx}].add_tags"))?),
            _ => None,
        };
        let remove_tags = match obj.get("remove_tags") {
            Some(v) if !v.is_null() => Some(parse_tags_array(v, &format!("edits[{idx}].remove_tags"))?),
            _ => None,
        };
        if add_tags.is_some() || remove_tags.is_some() {
            let mut tags = props_vec
                .iter()
                .find_map(|(k, v)| match (k.as_str(), v) {
                    ("Tags", Variant::Tags(t)) => {
                        Some(t.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| instance_tags(doc, r));
            if let Some(rem) = remove_tags {
                tags.retain(|t| !rem.iter().any(|r| r == t));
            }
            if let Some(add) = add_tags {
                for t in add {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
            }
            props_vec.retain(|(k, _)| k.as_str() != "Tags");
            props_vec.push((ustr("Tags"), Variant::Tags(Tags::from(tags))));
        }

        staged.push(StagedEdit {
            r,
            rename: explicit_name.or(prop_name),
            props: props_vec,
        });
    }
    Ok(staged)
}

pub(crate) fn dom_set_properties_in(doc: &mut Document, args: &Value) -> Result<Value, String> {
    let staged = stage_property_edits(doc, args)?;
    let mut changes = Vec::new();
    let mut prop_edits = Vec::new();
    let mut updated_refs = Vec::with_capacity(staged.len());

    for item in staged {
        updated_refs.push(item.r);
        if let Some(new_name) = item.rename {
            if let Some(inst) = doc.dom.get_by_ref_mut(item.r) {
                if inst.name != new_name {
                    let old = std::mem::replace(&mut inst.name, new_name.clone());
                    changes.push(Change::Name {
                        r: item.r,
                        old,
                        new: new_name,
                    });
                }
            }
        }
        for (k, v) in item.props {
            prop_edits.push((item.r, k, v));
        }
    }

    if !changes.is_empty() && prop_edits.is_empty() {
        doc.record_changes("Set properties", changes);
    } else if !changes.is_empty() {
        let twins: Vec<_> = prop_edits
            .iter()
            .filter_map(|(r, name, value)| {
                let class = doc.class_of(*r)?;
                doc::screen_insets_twin(class.as_str(), name.as_str(), value)
                    .map(|(n, v)| (*r, ustr(n), v))
            })
            .collect();
        prop_edits.extend(twins);
        for (r, name, value) in prop_edits {
            if let Some(inst) = doc.dom.get_by_ref_mut(r) {
                let old = inst.properties.insert(name, value.clone());
                if old.as_ref() != Some(&value) {
                    changes.push(Change::Prop {
                        r,
                        name,
                        old,
                        new: Some(value),
                    });
                }
            }
        }
        doc.record_changes("Set properties", changes);
    } else if !prop_edits.is_empty() {
        doc.set_props("Set properties", prop_edits, None);
    }

    let updated: Vec<Value> = updated_refs
        .into_iter()
        .map(|r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
            })
        })
        .collect();

    Ok(json!({
        "updated_count": updated.len(),
        "updated": updated,
    }))
}

fn handle_set_properties(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let dm = resolve_target_dm(ed, args)?;
    let mut updated_sources: Vec<(Ref, String)> = Vec::new();

    let res = with_edit_or_play(
        ed,
        args,
        |doc| {
            let out = dom_set_properties_in(doc, args)?;
            if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
                for item in arr {
                    if let (Some(tv), Some(pv)) = (item.get("target"), item.get("properties")) {
                        if pv.get("Source").is_some() {
                            if let Ok(r) = variants::resolve_target(doc, tv) {
                                if let Some(Variant::String(s)) = doc.prop(r, "Source") {
                                    updated_sources.push((r, s));
                                }
                            }
                        }
                    }
                }
            }
            Ok(out)
        },
        |rt| {
            let staged = {
                let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                stage_property_edits(w.d(), args)?
            };
            let mut refs = Vec::with_capacity(staged.len());
            for item in staged {
                refs.push(item.r);
                if let Some(new_name) = item.rename {
                    play_apply_name(rt, item.r, &new_name);
                }
                for (k, v) in item.props {
                    play_apply_prop(rt, item.r, k.as_str(), v);
                }
            }
            let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
            let d = w.d();
            let updated: Vec<Value> = refs
                .into_iter()
                .map(|r| {
                    json!({
                        "ref_id": variants::ref_to_str(r),
                        "path": variants::instance_path(d, r),
                    })
                })
                .collect();
            Ok(json!({
                "updated_count": updated.len(),
                "updated": updated,
            }))
        },
    )?;

    sync_script_tab_sources(ed, dm, &updated_sources);
    Ok(res)
}

// =========================================================================
// 6. dom_reparent_rename_delete
// =========================================================================

struct StagedTreeOps {
    reparent: Vec<(Ref, Ref)>,
    rename: Vec<(Ref, String)>,
    delete: Vec<Ref>,
}

fn stage_reparent_rename_delete(
    doc: &Document,
    args: &Value,
    allow_null_parent: bool,
) -> Result<StagedTreeOps, String> {
    let mut reparent = Vec::new();
    if let Some(arr) = args.get("reparent").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("reparent[{idx}] must be an object"))?;
            let r = variants::resolve_target(
                doc,
                obj.get("target")
                    .ok_or_else(|| format!("reparent[{idx}] missing 'target'"))?,
            )
            .map_err(|e| format!("reparent[{idx}]: {e}"))?;
            if !doc.can_edit_structure(r) {
                return Err(format!(
                    "reparent[{idx}]: cannot reparent DataModel root or service '{}'",
                    variants::instance_path(doc, r)
                ));
            }
            let npv = obj
                .get("new_parent")
                .ok_or_else(|| format!("reparent[{idx}] missing 'new_parent'"))?;
            let new_parent = if npv.is_null()
                || npv
                    .as_str()
                    .is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "null" | "nil" | "none"))
            {
                if !allow_null_parent {
                    return Err(format!(
                        "reparent[{idx}]: new_parent cannot be null in Edit DataModel; use delete"
                    ));
                }
                Ref::none()
            } else {
                variants::resolve_target(doc, npv)
                    .map_err(|e| format!("reparent[{idx}].new_parent: {e}"))?
            };
            if new_parent.is_some() && (r == new_parent || doc.is_ancestor(r, new_parent)) {
                return Err(format!(
                    "reparent[{idx}]: cannot reparent an instance to itself or its descendant"
                ));
            }
            reparent.push((r, new_parent));
        }
    }

    let mut rename = Vec::new();
    if let Some(arr) = args.get("rename").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("rename[{idx}] must be an object"))?;
            let r = variants::resolve_target(
                doc,
                obj.get("target")
                    .ok_or_else(|| format!("rename[{idx}] missing 'target'"))?,
            )
            .map_err(|e| format!("rename[{idx}]: {e}"))?;
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("rename[{idx}] missing string 'name'"))?
                .to_string();
            rename.push((r, name));
        }
    }

    let mut delete_set = HashSet::new();
    let mut delete = Vec::new();
    if let Some(arr) = args.get("delete").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let r = variants::resolve_target(doc, item)
                .map_err(|e| format!("delete[{idx}]: {e}"))?;
            if !doc.can_edit_structure(r) {
                return Err(format!(
                    "delete[{idx}]: cannot delete DataModel root or service '{}'",
                    variants::instance_path(doc, r)
                ));
            }
            if delete_set.insert(r) {
                delete.push(r);
            }
        }
    }
    if let Some(arr) = args.get("clear_children").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let parent = variants::resolve_target(doc, item)
                .map_err(|e| format!("clear_children[{idx}]: {e}"))?;
            if let Some(inst) = doc.dom.get_by_ref(parent) {
                for &c in inst.children().iter().rev() {
                    if doc.can_edit_structure(c) && delete_set.insert(c) {
                        delete.push(c);
                    }
                }
            }
        }
    }

    Ok(StagedTreeOps {
        reparent,
        rename,
        delete,
    })
}

pub(crate) fn dom_reparent_rename_delete_in(
    doc: &mut Document,
    args: &Value,
) -> Result<Value, String> {
    let ops = stage_reparent_rename_delete(doc, args, false)?;

    let mut changes = Vec::new();
    let mut reparented = Vec::new();
    for (r, new_parent) in &ops.reparent {
        if !doc.exists(*r) || !doc.exists(*new_parent) || *r == *new_parent || doc.is_ancestor(*r, *new_parent) {
            return Err("reparent would create an invalid cycle or target missing instance".into());
        }
        let old = doc.dom.get_by_ref(*r).map(|i| i.parent()).unwrap_or_else(Ref::none);
        if old != *new_parent {
            doc.dom.transfer_within(*r, *new_parent);
            changes.push(Change::Parent {
                r: *r,
                old,
                new: *new_parent,
            });
        }
        reparented.push(*r);
    }

    let mut renamed = Vec::new();
    for (r, name) in &ops.rename {
        if let Some(inst) = doc.dom.get_by_ref_mut(*r) {
            if inst.name != *name {
                let old = std::mem::replace(&mut inst.name, name.clone());
                changes.push(Change::Name {
                    r: *r,
                    old,
                    new: name.clone(),
                });
            }
            renamed.push(*r);
        }
    }

    if !changes.is_empty() {
        doc.record_changes("Reparent / Rename", changes);
    }

    let deleted_paths: Vec<Value> = ops
        .delete
        .iter()
        .map(|&r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
            })
        })
        .collect();
    let deleted_count = if ops.delete.is_empty() {
        0
    } else {
        doc.remove(&ops.delete)
    };

    let reparented_json: Vec<Value> = reparented
        .into_iter()
        .map(|r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
            })
        })
        .collect();
    let renamed_json: Vec<Value> = renamed
        .into_iter()
        .map(|r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
            })
        })
        .collect();

    Ok(json!({
        "reparented_count": reparented_json.len(),
        "renamed_count": renamed_json.len(),
        "deleted_count": deleted_count,
        "reparented": reparented_json,
        "renamed": renamed_json,
        "deleted": deleted_paths,
    }))
}

fn handle_reparent_rename_delete(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    with_edit_or_play(
        ed,
        args,
        |doc| dom_reparent_rename_delete_in(doc, args),
        |rt| {
            let ops = {
                let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                stage_reparent_rename_delete(w.d(), args, true)?
            };

            let mut reparented = Vec::new();
            for (r, new_parent) in &ops.reparent {
                rt.apply_parent(*r, *new_parent);
                reparented.push(*r);
            }
            let mut renamed = Vec::new();
            for (r, name) in &ops.rename {
                play_apply_name(rt, *r, name);
                renamed.push(*r);
            }
            let deleted_info: Vec<Value> = {
                let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                ops.delete
                    .iter()
                    .map(|&r| {
                        json!({
                            "ref_id": variants::ref_to_str(r),
                            "path": variants::instance_path(w.d(), r),
                        })
                    })
                    .collect()
            };
            let top_delete = {
                let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                w.d().top_level(&ops.delete)
            };
            for r in &top_delete {
                rt.apply_destroy(*r);
            }

            let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
            let d = w.d();
            Ok(json!({
                "reparented_count": reparented.len(),
                "renamed_count": renamed.len(),
                "deleted_count": top_delete.len(),
                "reparented": reparented.into_iter().map(|r| json!({
                    "ref_id": variants::ref_to_str(r),
                    "path": variants::instance_path(d, r),
                })).collect::<Vec<_>>(),
                "renamed": renamed.into_iter().map(|r| json!({
                    "ref_id": variants::ref_to_str(r),
                    "path": variants::instance_path(d, r),
                })).collect::<Vec<_>>(),
                "deleted": deleted_info,
            }))
        },
    )
}

// =========================================================================
// 7. dom_clone_and_transform
// =========================================================================

enum CloneTransformSpec {
    Delta { offset: Vec3, rot_deg: Vec3 },
    Absolute { pos: Vec3, rot: Option<Mat3> },
}

fn parse_clone_transform_entry(v: &Value) -> Result<CloneTransformSpec, String> {
    if let Value::Object(m) = v {
        if m.contains_key("offset") && !m.contains_key("pos") && !m.contains_key("position") {
            let offset = parse_vec3_json(m.get("offset").unwrap())?;
            let rot_deg = match m.get("rotation_deg").or_else(|| m.get("angles_deg")) {
                Some(rv) => parse_vec3_json(rv)?,
                None => Vec3::ZERO,
            };
            return Ok(CloneTransformSpec::Delta { offset, rot_deg });
        }
        if let Some(pv) = m.get("position").or_else(|| m.get("pos")) {
            let has_rot = m.contains_key("angles_deg")
                || m.contains_key("anglesDeg")
                || m.contains_key("rotation_deg")
                || m.contains_key("angles_rad")
                || m.contains_key("anglesRad")
                || m.contains_key("look_at")
                || m.contains_key("lookAt")
                || m.contains_key("right")
                || m.contains_key("orientation");
            if !has_rot {
                let pos = parse_vec3_json(pv)?;
                return Ok(CloneTransformSpec::Absolute { pos, rot: None });
            }
            if let Some(rv) = m.get("rotation_deg") {
                let pos = parse_vec3_json(pv)?;
                let deg = parse_vec3_json(rv)?;
                let xf = Xform::from_orientation_deg(pos, deg);
                return Ok(CloneTransformSpec::Absolute {
                    pos: xf.pos,
                    rot: Some(xf.rot),
                });
            }
        }
    }
    let Variant::CFrame(cf) = variants::json_to_variant(None, None, None, v, Some(VariantType::CFrame))? else {
        return Err("expected CFrame transform".into());
    };
    let xf = Xform::from_cframe(&cf);
    Ok(CloneTransformSpec::Absolute {
        pos: xf.pos,
        rot: Some(xf.rot),
    })
}

fn transform_subtree_in_dom(dom: &mut WeakDom, root_r: Ref, spec: &CloneTransformSpec) {
    let pivot = subtree_pivot_in_weak_dom(dom, root_r);
    let new_pivot = match spec {
        CloneTransformSpec::Delta { offset, rot_deg } => {
            let rot = Mat3::from_euler(
                EulerRot::YXZ,
                rot_deg.y.to_radians(),
                rot_deg.x.to_radians(),
                rot_deg.z.to_radians(),
            );
            Xform {
                pos: pivot.pos + *offset,
                rot: rot * pivot.rot,
            }
        }
        CloneTransformSpec::Absolute { pos, rot } => Xform {
            pos: *pos,
            rot: rot.unwrap_or(pivot.rot),
        },
    };
    if (new_pivot.pos - pivot.pos).length_squared() < 1e-10
        && (new_pivot.rot - pivot.rot).abs_diff_eq(Mat3::ZERO, 1e-6)
    {
        return;
    }
    let inv = pivot.inverse();
    let delta = Xform {
        pos: new_pivot.pos + new_pivot.rot * inv.pos,
        rot: new_pivot.rot * inv.rot,
    };

    let all_refs: Vec<Ref> = dom.descendants_of(root_r).map(|i| i.referent()).collect();
    for r in all_refs {
        let Some(inst) = dom.get_by_ref_mut(r) else {
            continue;
        };
        let cls = inst.class.as_str();
        if cls != "Terrain" && doc::is_a(cls, "BasePart") || cls == "Camera" {
            let cur = match inst.properties.get(&ustr("CFrame")) {
                Some(Variant::CFrame(cf)) => Xform::from_cframe(cf),
                _ => Xform {
                    pos: Vec3::ZERO,
                    rot: Mat3::IDENTITY,
                },
            };
            let moved = Xform {
                pos: delta.apply(cur.pos),
                rot: delta.rot * cur.rot,
            };
            inst.properties
                .insert(ustr("CFrame"), Variant::CFrame(moved.to_cframe()));
        } else if cls == "Model" {
            if let Some(Variant::OptionalCFrame(Some(cf))) =
                inst.properties.get(&ustr("WorldPivotData")).cloned()
            {
                let cur = Xform::from_cframe(&cf);
                let moved = Xform {
                    pos: delta.apply(cur.pos),
                    rot: delta.rot * cur.rot,
                };
                inst.properties.insert(
                    ustr("WorldPivotData"),
                    Variant::OptionalCFrame(Some(moved.to_cframe())),
                );
            }
        }
    }
}

fn subtree_pivot_in_weak_dom(dom: &WeakDom, r: Ref) -> Xform {
    let Some(inst) = dom.get_by_ref(r) else {
        return Xform { pos: Vec3::ZERO, rot: Mat3::IDENTITY };
    };
    let cls = inst.class.as_str();
    if cls != "Terrain" && doc::is_a(cls, "BasePart") || cls == "Camera" {
        if let Some(Variant::CFrame(cf)) = inst.properties.get(&ustr("CFrame")) {
            return Xform::from_cframe(cf).orthonormalized();
        }
        return Xform { pos: Vec3::ZERO, rot: Mat3::IDENTITY };
    }
    if cls == "Model" {
        if let Some(Variant::Ref(pp)) = inst.properties.get(&ustr("PrimaryPart")) {
            if let Some(Variant::CFrame(cf)) =
                dom.get_by_ref(*pp).and_then(|pi| pi.properties.get(&ustr("CFrame")))
            {
                let base = Xform::from_cframe(cf);
                let offset = match dom
                    .get_by_ref(*pp)
                    .and_then(|pi| pi.properties.get(&ustr("PivotOffset")))
                {
                    Some(Variant::CFrame(ocf)) => Xform::from_cframe(ocf),
                    _ => Xform { pos: Vec3::ZERO, rot: Mat3::IDENTITY },
                };
                return Xform {
                    pos: base.apply(offset.pos),
                    rot: base.rot * offset.rot,
                };
            }
        }
        if let Some(Variant::OptionalCFrame(Some(cf))) = inst.properties.get(&ustr("WorldPivotData")) {
            return Xform::from_cframe(cf);
        }
    }
    let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for desc in dom.descendants_of(r) {
        if desc.class != "Terrain" && doc::is_a(&desc.class, "BasePart") {
            let xf = match desc.properties.get(&ustr("CFrame")) {
                Some(Variant::CFrame(cf)) => Xform::from_cframe(cf),
                _ => continue,
            };
            let sz = match desc.properties.get(&ustr("Size")) {
                Some(Variant::Vector3(v)) => Vec3::new(v.x, v.y, v.z),
                _ => Vec3::ONE,
            };
            let (a, b) = world_aabb_of_obb(&xf, sz);
            lo = lo.min(a);
            hi = hi.max(b);
        }
    }
    Xform {
        pos: if lo.is_finite() { (lo + hi) * 0.5 } else { Vec3::ZERO },
        rot: Mat3::IDENTITY,
    }
}

struct PlannedCloneBatch {
    sources: Vec<(Ref, Ref, Vec<u8>)>,
    count: usize,
    name_prefix: Option<String>,
    specs: Vec<CloneTransformSpec>,
}

fn prepare_clone_batch(doc: &Document, args: &Value) -> Result<PlannedCloneBatch, String> {
    let raw_targets = resolve_targets_arg(doc, args, "target", "targets")?;
    let targets = doc.top_level(&raw_targets);
    if targets.is_empty() {
        return Err("no valid targets to clone".into());
    }
    for &r in &targets {
        if !doc.can_edit_structure(r) {
            return Err(format!(
                "cannot clone DataModel root or service '{}'",
                variants::instance_path(doc, r)
            ));
        }
    }

    let parent_override = match args.get("parent") {
        Some(v) if !v.is_null() => Some(variants::resolve_target(doc, v)?),
        _ => None,
    };

    let name_prefix = args
        .get("name_prefix")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let explicit_transforms = match args.get("transforms") {
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (idx, item) in arr.iter().enumerate() {
                out.push(
                    parse_clone_transform_entry(item)
                        .map_err(|e| format!("transforms[{idx}]: {e}"))?,
                );
            }
            Some(out)
        }
        Some(v) if !v.is_null() => return Err("'transforms' must be an array".into()),
        _ => None,
    };

    let grid = match args.get("grid") {
        Some(Value::Object(m)) => {
            let cols = m
                .get("cols")
                .and_then(|v| v.as_u64())
                .filter(|&n| n >= 1)
                .ok_or("grid.cols must be >= 1")? as usize;
            let rows = m.get("rows").and_then(|v| v.as_u64()).map(|n| n as usize);
            let sp = m
                .get("spacing")
                .and_then(|v| v.as_array())
                .ok_or("grid.spacing must be [sx, sz] or [sx, sy, sz]")?;
            let spacing = match sp.len() {
                2 => Vec3::new(
                    sp[0].as_f64().ok_or("grid.spacing[0] must be a number")? as f32,
                    0.0,
                    sp[1].as_f64().ok_or("grid.spacing[1] must be a number")? as f32,
                ),
                3 => Vec3::new(
                    sp[0].as_f64().ok_or("grid.spacing[0] must be a number")? as f32,
                    sp[1].as_f64().ok_or("grid.spacing[1] must be a number")? as f32,
                    sp[2].as_f64().ok_or("grid.spacing[2] must be a number")? as f32,
                ),
                _ => return Err("grid.spacing must have 2 or 3 numbers".into()),
            };
            Some((cols, rows, spacing))
        }
        Some(v) if !v.is_null() => return Err("'grid' must be an object".into()),
        _ => None,
    };

    let count = match args.get("count").and_then(|v| v.as_u64()) {
        Some(0) => return Err("count must be >= 1".into()),
        Some(n) => n as usize,
        None => {
            if let Some(t) = &explicit_transforms {
                if t.is_empty() {
                    return Err("transforms array cannot be empty when count is omitted".into());
                }
                t.len()
            } else if let Some((cols, rows, _)) = grid {
                cols * rows.unwrap_or(1)
            } else {
                1
            }
        }
    };

    let offset_step = match args.get("offset_step") {
        Some(v) if !v.is_null() => parse_vec3_json(v)?,
        _ => Vec3::ZERO,
    };
    let rot_step = match args.get("rotation_step_deg") {
        Some(v) if !v.is_null() => parse_vec3_json(v)?,
        _ => Vec3::ZERO,
    };

    let mut specs = Vec::with_capacity(count);
    if let Some(t) = explicit_transforms {
        if t.len() < count {
            return Err(format!(
                "transforms length ({}) is less than count ({count})",
                t.len()
            ));
        }
        specs.extend(t.into_iter().take(count));
    } else if let Some((cols, _, spacing)) = grid {
        for i in 0..count {
            let col = (i % cols) as f32;
            let row = (i / cols) as f32;
            let grid_off = Vec3::new(col * spacing.x, (i as f32) * spacing.y, row * spacing.z);
            let step_off = offset_step * (i as f32);
            let rot_deg = rot_step * (i as f32);
            specs.push(CloneTransformSpec::Delta {
                offset: grid_off + step_off,
                rot_deg,
            });
        }
    } else {
        for i in 0..count {
            let step = (i + 1) as f32;
            specs.push(CloneTransformSpec::Delta {
                offset: offset_step * step,
                rot_deg: rot_step * step,
            });
        }
    }

    let mut sources = Vec::with_capacity(targets.len());
    for r in targets {
        let dest_parent = parent_override.unwrap_or_else(|| {
            doc.dom
                .get_by_ref(r)
                .map(|i| i.parent())
                .unwrap_or_else(|| doc.default_parent())
        });
        let bytes = doc.encode(&[r], false)?;
        sources.push((r, dest_parent, bytes));
    }

    Ok(PlannedCloneBatch {
        sources,
        count,
        name_prefix,
        specs,
    })
}

fn instantiate_clones(
    dom: &mut WeakDom,
    batch: &PlannedCloneBatch,
    attach_parentless: bool,
) -> Result<Vec<(Ref, Ref)>, String> {
    let mut created = Vec::new();
    for i in 0..batch.count {
        let spec = &batch.specs[i];
        for (_, dest_parent, bytes) in &batch.sources {
            let mut tmp = Document::decode(bytes)?;
            let roots = tmp.root().children().to_vec();
            for nr in roots {
                if let Some(prefix) = &batch.name_prefix {
                    if let Some(inst) = tmp.get_by_ref_mut(nr) {
                        inst.name = if batch.count == 1 {
                            format!("{prefix}{}", inst.name)
                        } else {
                            format!("{prefix}{}_{}", inst.name, i + 1)
                        };
                    }
                }
                transform_subtree_in_dom(&mut tmp, nr, spec);
                if attach_parentless {
                    let root_ref = dom.root_ref();
                    tmp.transfer(nr, dom, root_ref);
                    dom.transfer_within(nr, Ref::none());
                } else {
                    tmp.transfer(nr, dom, *dest_parent);
                }
                created.push((nr, *dest_parent));
            }
        }
    }
    Ok(created)
}

pub(crate) fn dom_clone_and_transform_in(
    doc: &mut Document,
    args: &Value,
) -> Result<Value, String> {
    let batch = prepare_clone_batch(doc, args)?;
    let created_pairs = instantiate_clones(&mut doc.dom, &batch, false)?;
    let changes: Vec<Change> = created_pairs
        .iter()
        .map(|&(r, parent)| Change::Create { r, parent })
        .collect();
    doc.record_changes("Clone and transform", changes);

    let created: Vec<Value> = created_pairs
        .into_iter()
        .map(|(r, _)| {
            let pos = world_position_of(doc, r).map(|p| [p.x, p.y, p.z]);
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
                "name": doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                "class": doc.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                "position": pos,
            })
        })
        .collect();

    Ok(json!({
        "created_count": created.len(),
        "created": created,
    }))
}

fn handle_clone_and_transform(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    with_edit_or_play(
        ed,
        args,
        |doc| dom_clone_and_transform_in(doc, args),
        |rt| {
            let created_pairs = {
                let mut w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                let batch = prepare_clone_batch(w.d(), args)?;
                instantiate_clones(&mut w.dm().dom, &batch, true)?
            };
            for &(r, dest_parent) in &created_pairs {
                rt.apply_parent(r, dest_parent);
            }
            let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
            let d = w.d();
            let created: Vec<Value> = created_pairs
                .into_iter()
                .map(|(r, _)| {
                    let pos = world_position_of(d, r).map(|p| [p.x, p.y, p.z]);
                    json!({
                        "ref_id": variants::ref_to_str(r),
                        "path": variants::instance_path(d, r),
                        "name": d.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                        "class": d.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                        "position": pos,
                    })
                })
                .collect();
            Ok(json!({
                "created_count": created.len(),
                "created": created,
            }))
        },
    )
}

// =========================================================================
// 8. dom_group_ungroup
// =========================================================================

pub(crate) fn dom_group_ungroup_in(doc: &mut Document, args: &Value) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("dom_group_ungroup requires 'action' ('group' or 'ungroup')")?
        .trim()
        .to_ascii_lowercase();
    let refs = resolve_targets_arg(doc, args, "target", "targets")?;

    match action.as_str() {
        "group" => {
            let class = args
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("Model");
            if !matches!(class, "Model" | "Folder") {
                return Err(format!(
                    "group class must be 'Model' or 'Folder', got '{class}'"
                ));
            }
            let parent_override = match args.get("parent") {
                Some(v) if !v.is_null() => Some(variants::resolve_target(doc, v)?),
                _ => None,
            };
            let name_override = args.get("name").and_then(|v| v.as_str());
            let top: Vec<Ref> = doc
                .top_level(&refs)
                .into_iter()
                .filter(|r| doc.can_edit_structure(*r))
                .collect();
            if top.is_empty() {
                return Err("no groupable instances found".into());
            }
            if let Some(p) = parent_override {
                if top.iter().any(|r| *r == p || doc.is_ancestor(*r, p)) {
                    return Err("cannot group into one of the grouped instances or its descendants".into());
                }
            }
            let group = doc
                .group_as(&top, class)
                .ok_or("failed to create group")?;
            if let Some(p) = parent_override {
                doc.reparent(&[group], p);
            }
            if let Some(n) = name_override {
                doc.rename(group, n);
            }
            Ok(json!({
                "action": "group",
                "group": {
                    "ref_id": variants::ref_to_str(group),
                    "path": variants::instance_path(doc, group),
                    "name": doc.dom.get_by_ref(group).map(|i| i.name.as_str()).unwrap_or(""),
                    "class": class,
                },
                "grouped_count": top.len(),
            }))
        }
        "ungroup" => {
            let top: Vec<Ref> = doc
                .top_level(&refs)
                .into_iter()
                .filter(|r| {
                    doc.can_edit_structure(*r)
                        && doc
                            .class_of(*r)
                            .is_some_and(|c| c == "Model" || c == "Folder")
                })
                .collect();
            if top.is_empty() {
                return Err("no Model or Folder targets found to ungroup".into());
            }
            let released = doc.ungroup(&top);
            let children: Vec<Value> = released
                .iter()
                .map(|&r| {
                    json!({
                        "ref_id": variants::ref_to_str(r),
                        "path": variants::instance_path(doc, r),
                        "name": doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                        "class": doc.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                    })
                })
                .collect();
            Ok(json!({
                "action": "ungroup",
                "ungrouped_count": top.len(),
                "released_count": children.len(),
                "released": children,
            }))
        }
        other => Err(format!(
            "unknown action '{other}'; expected 'group' or 'ungroup'"
        )),
    }
}

fn handle_group_ungroup(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    with_edit_or_play(
        ed,
        args,
        |doc| dom_group_ungroup_in(doc, args),
        |rt| {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or("dom_group_ungroup requires 'action'")?
                .trim()
                .to_ascii_lowercase();

            match action.as_str() {
                "group" => {
                    let class = args
                        .get("class")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Model");
                    if !matches!(class, "Model" | "Folder") {
                        return Err(format!(
                            "group class must be 'Model' or 'Folder', got '{class}'"
                        ));
                    }
                    let name = args
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(class)
                        .to_string();

                    let (group_r, group_parent, top) = {
                        let mut w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                        let refs = resolve_targets_arg(w.d(), args, "target", "targets")?;
                        let top: Vec<Ref> = w
                            .d()
                            .top_level(&refs)
                            .into_iter()
                            .filter(|r| w.d().can_edit_structure(*r))
                            .collect();
                        if top.is_empty() {
                            return Err("no groupable instances found".into());
                        }
                        let parent = match args.get("parent") {
                            Some(v) if !v.is_null() => variants::resolve_target(w.d(), v)?,
                            _ => w
                                .d()
                                .dom
                                .get_by_ref(top[0])
                                .map(|i| i.parent())
                                .unwrap_or_else(|| w.d().default_parent()),
                        };
                        if top.iter().any(|r| *r == parent || w.d().is_ancestor(*r, parent)) {
                            return Err("cannot group into one of the grouped instances".into());
                        }
                        let root_ref = w.d().dom.root_ref();
                        let g = w
                            .dm()
                            .dom
                            .insert(root_ref, InstanceBuilder::new(class).with_name(&name));
                        w.dm().dom.transfer_within(g, Ref::none());
                        (g, parent, top)
                    };

                    rt.apply_parent(group_r, group_parent);
                    for &r in &top {
                        rt.apply_parent(r, group_r);
                    }
                    let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                    Ok(json!({
                        "action": "group",
                        "group": {
                            "ref_id": variants::ref_to_str(group_r),
                            "path": variants::instance_path(w.d(), group_r),
                            "name": name,
                            "class": class,
                        },
                        "grouped_count": top.len(),
                    }))
                }
                "ungroup" => {
                    let plan: Vec<(Ref, Ref, Vec<Ref>)> = {
                        let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                        let d = w.d();
                        let refs = resolve_targets_arg(d, args, "target", "targets")?;
                        let top: Vec<Ref> = d
                            .top_level(&refs)
                            .into_iter()
                            .filter(|r| {
                                d.can_edit_structure(*r)
                                    && d.class_of(*r)
                                        .is_some_and(|c| c == "Model" || c == "Folder")
                            })
                            .collect();
                        if top.is_empty() {
                            return Err("no Model or Folder targets found to ungroup".into());
                        }
                        top.into_iter()
                            .filter_map(|r| {
                                let i = d.dom.get_by_ref(r)?;
                                Some((r, i.parent(), i.children().to_vec()))
                            })
                            .collect()
                    };
                    let mut released = Vec::new();
                    for (container, parent, kids) in &plan {
                        for &c in kids {
                            rt.apply_parent(c, *parent);
                            released.push(c);
                        }
                        rt.apply_destroy(*container);
                    }
                    let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                    let d = w.d();
                    let children: Vec<Value> = released
                        .into_iter()
                        .map(|r| {
                            json!({
                                "ref_id": variants::ref_to_str(r),
                                "path": variants::instance_path(d, r),
                                "name": d.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                                "class": d.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                            })
                        })
                        .collect();
                    Ok(json!({
                        "action": "ungroup",
                        "ungrouped_count": plan.len(),
                        "released_count": children.len(),
                        "released": children,
                    }))
                }
                other => Err(format!(
                    "unknown action '{other}'; expected 'group' or 'ungroup'"
                )),
            }
        },
    )
}

// =========================================================================
// 9. dom_serialize_subtree
// =========================================================================

fn read_import_bytes(args: &Value) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
        return std::fs::read(fp).map_err(|e| format!("failed to read '{fp}': {e}"));
    }
    if let Some(xml) = args.get("xml").and_then(|v| v.as_str()) {
        return Ok(xml.as_bytes().to_vec());
    }
    if let Some(b64_str) = args.get("base64").and_then(|v| v.as_str()) {
        return b64
            .decode(b64_str.trim())
            .map_err(|e| format!("invalid base64 payload: {e}"));
    }
    if let Some(data) = args.get("data").and_then(|v| v.as_str()) {
        let trimmed = data.trim_start();
        if trimmed.starts_with("<roblox") && !trimmed.starts_with("<roblox!") {
            return Ok(data.as_bytes().to_vec());
        }
        return b64
            .decode(data.trim())
            .map_err(|e| format!("invalid base64 payload: {e}"));
    }
    Err("import requires 'xml', 'base64', 'data', or 'file_path'".into())
}

pub(crate) fn dom_export_in(doc: &Document, args: &Value) -> Result<Value, String> {
    use base64::Engine as _;
    let refs = resolve_targets_arg(doc, args, "target", "targets")?;
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("rbxmx")
        .trim()
        .to_ascii_lowercase();
    let is_xml = match format.as_str() {
        "rbxmx" | "xml" => true,
        "rbxm" | "binary" => false,
        other => {
            return Err(format!(
                "unknown export format '{other}'; expected 'rbxmx' or 'rbxm'"
            ));
        }
    };

    let root = doc.dom.root_ref();
    let (bytes, exported_refs) = if refs.contains(&root) {
        (doc.encode_all(is_xml)?, vec![root])
    } else {
        let top = doc.top_level(&refs);
        if top.is_empty() {
            return Err("no valid targets to export".into());
        }
        (doc.encode(&top, is_xml)?, top)
    };

    let saved_path = if let Some(path) = args.get("save_path").and_then(|v| v.as_str()) {
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create_dir_all '{}': {e}", parent.display()))?;
            }
        }
        std::fs::write(p, &bytes).map_err(|e| format!("write '{}': {e}", p.display()))?;
        Some(path.to_string())
    } else {
        None
    };

    let exported: Vec<Value> = exported_refs
        .iter()
        .map(|&r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
            })
        })
        .collect();

    let mut out = json!({
        "action": "export",
        "format": if is_xml { "rbxmx" } else { "rbxm" },
        "bytes": bytes.len(),
        "exported": exported,
        "saved_path": saved_path,
    });
    if is_xml {
        let xml = String::from_utf8(bytes)
            .map_err(|e| format!("encoded XML is not valid UTF-8: {e}"))?;
        out["xml"] = Value::String(xml);
    } else {
        out["base64"] =
            Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes));
    }
    Ok(out)
}

pub(crate) fn dom_import_in(doc: &mut Document, args: &Value) -> Result<Value, String> {
    let bytes = read_import_bytes(args)?;
    let check = Document::decode(&bytes)?;
    if check.root().children().is_empty() {
        return Err("serialized payload contains no root instances".into());
    }
    drop(check);
    let parent = match args.get("parent") {
        Some(v) if !v.is_null() => variants::resolve_target(doc, v)?,
        _ => doc.default_parent(),
    };
    let created = doc.paste(&bytes, parent, "Import subtree");
    if created.is_empty() {
        return Err("failed to import serialized payload".into());
    }
    let items: Vec<Value> = created
        .iter()
        .map(|&r| {
            json!({
                "ref_id": variants::ref_to_str(r),
                "path": variants::instance_path(doc, r),
                "name": doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                "class": doc.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
            })
        })
        .collect();
    Ok(json!({
        "action": "import",
        "imported_count": items.len(),
        "imported": items,
    }))
}

fn handle_serialize_subtree(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("dom_serialize_subtree requires 'action' ('export' or 'import')")?
        .trim()
        .to_ascii_lowercase();

    match action.as_str() {
        "export" => {
            let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
            dom_export_in(doc, args)
        }
        "import" => with_edit_or_play(
            ed,
            args,
            |doc| dom_import_in(doc, args),
            |rt| {
                let bytes = read_import_bytes(args)?;
                let mut tmp = Document::decode(&bytes)?;
                let roots = tmp.root().children().to_vec();
                if roots.is_empty() {
                    return Err("serialized payload contains no root instances".into());
                }
                let parent = {
                    let mut w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                    let parent = match args.get("parent") {
                        Some(v) if !v.is_null() => variants::resolve_target(w.d(), v)?,
                        _ => w.d().default_parent(),
                    };
                    let root_ref = w.d().dom.root_ref();
                    for &r in &roots {
                        tmp.transfer(r, &mut w.dm().dom, root_ref);
                        w.dm().dom.transfer_within(r, Ref::none());
                    }
                    parent
                };
                for &r in &roots {
                    rt.apply_parent(r, parent);
                }
                let w = crate::runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                let d = w.d();
                let items: Vec<Value> = roots
                    .iter()
                    .map(|&r| {
                        json!({
                            "ref_id": variants::ref_to_str(r),
                            "path": variants::instance_path(d, r),
                            "name": d.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
                            "class": d.class_of(r).map(|c| c.to_string()).unwrap_or_default(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "action": "import",
                    "imported_count": items.len(),
                    "imported": items,
                }))
            },
        ),
        other => Err(format!(
            "unknown action '{other}'; expected 'export' or 'import'"
        )),
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn schemas_expose_all_9_dom_tools() {
        let names: Vec<String> = schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "dom_get_tree",
                "dom_inspect_instance",
                "dom_query",
                "dom_create_instances",
                "dom_set_properties",
                "dom_reparent_rename_delete",
                "dom_clone_and_transform",
                "dom_group_ungroup",
                "dom_serialize_subtree",
            ]
        );
    }

    #[test]
    fn create_inspect_set_and_bypass_not_scriptable() {
        let mut doc = Document::new_place();
        let undo_before = doc.undo_len();

        let created = dom_create_instances_in(
            &mut doc,
            &json!({
                "items": [{
                    "parent": "Workspace",
                    "class": "Model",
                    "name": "PropRig",
                    "temp_id": "rig",
                    "tags": ["Interactive"],
                    "attributes": { "Tier": 3, "BossName": "Golem" },
                    "children": [{
                        "class": "Part",
                        "name": "Core",
                        "temp_id": "core",
                        "properties": {
                            "Position": [10, 5, -4],
                            "Size": [4, 4, 4],
                            "Anchored": true,
                            "BrickColor": "Bright red",
                            "CollisionGroup": "Enemies",
                            "RobloxLocked": true,
                            "SourceAssetId": 987654321
                        }
                    }],
                    "properties": {
                        "PrimaryPart": "$core",
                        "WorldPivotData": [1, 2, 3, 1, 0, 0, 0, 1, 0, 0, 0, 1]
                    }
                }]
            }),
        )
        .unwrap();
        assert_eq!(created["created_count"], 2);
        // Single undoable batch
        assert_eq!(doc.undo_len(), undo_before + 1);

        let core_ref = variants::resolve_target_str(&doc, "Workspace.PropRig.Core").unwrap();
        let rig_ref = variants::resolve_target_str(&doc, "Workspace.PropRig").unwrap();
        assert_eq!(doc.prop(rig_ref, "PrimaryPart"), Some(Variant::Ref(core_ref)));

        let inspected = dom_inspect_instance_in(
            &doc,
            &json!({ "target": "Workspace.PropRig.Core" }),
        )
        .unwrap();
        assert_eq!(inspected["class"], "Part");
        assert_eq!(inspected["properties"]["RobloxLocked"], json!(true));
        assert_eq!(inspected["properties"]["SourceAssetId"], json!(987654321));
        assert_eq!(inspected["properties"]["CollisionGroup"], json!("Enemies"));
        assert_eq!(inspected["world"]["position"], json!([10.0, 5.0, -4.0]));
        assert_eq!(inspected["world"]["size"], json!([4.0, 4.0, 4.0]));
        assert_eq!(
            inspected["property_security"]["RobloxLocked"]["security"],
            json!("PluginSecurity")
        );
        assert_eq!(
            inspected["property_security"]["SourceAssetId"]["security"],
            json!("RobloxScriptSecurity")
        );
        assert_eq!(
            inspected["property_security"]["Color3uint8"]["not_scriptable"],
            json!(true)
        );

        let rig_insp = dom_inspect_instance_in(
            &doc,
            &json!({ "target": "Workspace.PropRig" }),
        )
        .unwrap();
        assert_eq!(rig_insp["tags"], json!(["Interactive"]));
        assert_eq!(rig_insp["attributes"]["Tier"], json!(3.0));
        assert_eq!(rig_insp["attributes"]["BossName"], json!("Golem"));
        assert_eq!(
            rig_insp["property_security"]["WorldPivotData"]["not_scriptable"],
            json!(true)
        );

        // Mutate properties + attributes + tags + name + NotScriptable/RobloxScriptSecurity
        dom_set_properties_in(
            &mut doc,
            &json!({
                "edits": [{
                    "target": "Workspace.PropRig.Core",
                    "name": "RenamedCore",
                    "properties": {
                        "Position": [20, 8, 0],
                        "Transparency": 0.25,
                        "RobloxLocked": false,
                        "SourceAssetId": 111
                    },
                    "attributes": { "Damage": 50, "RemoveMe": "bye" },
                    "add_tags": ["Targetable", "Metal"]
                }]
            }),
        )
        .unwrap();

        let renamed_ref = variants::resolve_target_str(&doc, "Workspace.PropRig.RenamedCore").unwrap();
        assert_eq!(renamed_ref, core_ref);
        assert_eq!(doc.prop(core_ref, "RobloxLocked"), Some(Variant::Bool(false)));
        assert_eq!(doc.prop(core_ref, "SourceAssetId"), Some(Variant::Int64(111)));
        assert_eq!(
            scene::xform_of(&doc, core_ref).unwrap().pos,
            Vec3::new(20.0, 8.0, 0.0)
        );

        // Combined rename + props was strictly one undo step
        assert_eq!(doc.undo_len(), undo_before + 2);
        doc.undo();
        doc.undo();
        assert!(variants::resolve_target_str(&doc, "Workspace.PropRig").is_err());
    }

    #[test]
    fn unlimited_query_and_spatial_filters() {
        let mut doc = Document::new_place();
        let mut items = Vec::new();
        for i in 0..75 {
            items.push(json!({
                "parent": "Workspace",
                "class": "Part",
                "name": format!("Cube_{i:02}"),
                "tags": if i % 2 == 0 { json!(["Even"]) } else { json!([]) },
                "attributes": { "Index": i },
                "properties": {
                    "Position": [i as f32 * 2.0, 3.0, 0.0],
                    "Anchored": i < 60
                }
            }));
        }
        dom_create_instances_in(&mut doc, &json!({ "items": items })).unwrap();

        // No 50-result cap!
        let all_cubes = dom_query_in(
            &doc,
            &json!({
                "is_a": "BasePart",
                "name_pattern": "cube_",
                "property_equals": { "Anchored": true },
                "select_properties": ["Position"]
            }),
        )
        .unwrap();
        assert_eq!(all_cubes["total_matched"], 60);
        assert_eq!(all_cubes["returned"], 60);
        assert_eq!(all_cubes["truncated"], false);

        // Spatial sphere + tag + attribute
        let near_origin = dom_query_in(
            &doc,
            &json!({
                "has_tag": "Even",
                "within_sphere": { "center": [0, 3, 0], "radius": 8.5 }
            }),
        )
        .unwrap();
        // Indices 0, 2, 4 -> x = 0, 4, 8 (distance <= 8.5)
        assert_eq!(near_origin["total_matched"], 3);

        let in_box = dom_query_in(
            &doc,
            &json!({
                "name_pattern": "Cube_",
                "within_box": { "min": [19.0, 0.0, -1.0], "max": [25.0, 5.0, 1.0] }
            }),
        )
        .unwrap();
        // x = 20, 22, 24 -> indices 10, 11, 12
        assert_eq!(in_box["total_matched"], 3);
    }

    #[test]
    fn tree_clone_group_reparent_and_serialize() {
        let mut doc = Document::new_place();
        dom_create_instances_in(
            &mut doc,
            &json!({
                "items": [{
                    "parent": "Workspace",
                    "class": "Part",
                    "name": "Pillar",
                    "properties": { "Position": [0, 5, 0], "Size": [2, 10, 2] }
                }]
            }),
        )
        .unwrap();

        let tree = dom_get_tree_in(
            &doc,
            &json!({
                "root": "Workspace",
                "class": "Part",
                "name_contains": "pillar",
                "include_properties": ["Size"]
            }),
        )
        .unwrap();
        assert_eq!(tree["tree"]["children"].as_array().unwrap().len(), 1);
        assert_eq!(
            tree["tree"]["children"][0]["properties"]["Size"],
            json!([2.0, 10.0, 2.0])
        );

        // Clone 3 times along +X
        let cloned = dom_clone_and_transform_in(
            &mut doc,
            &json!({
                "target": "Workspace.Pillar",
                "count": 3,
                "name_prefix": "Row_",
                "offset_step": [10, 0, 0]
            }),
        )
        .unwrap();
        assert_eq!(cloned["created_count"], 3);
        let c1 = variants::resolve_target_str(&doc, "Workspace.Row_Pillar_1").unwrap();
        let c3 = variants::resolve_target_str(&doc, "Workspace.Row_Pillar_3").unwrap();
        assert_eq!(scene::xform_of(&doc, c1).unwrap().pos, Vec3::new(10.0, 5.0, 0.0));
        assert_eq!(scene::xform_of(&doc, c3).unwrap().pos, Vec3::new(30.0, 5.0, 0.0));

        // Group into Model, serialize to rbxmx, delete, and re-import
        let grp = dom_group_ungroup_in(
            &mut doc,
            &json!({
                "action": "group",
                "targets": [
                    "Workspace.Row_Pillar_1",
                    "Workspace.Row_Pillar_2",
                    "Workspace.Row_Pillar_3"
                ],
                "class": "Model",
                "name": "PillarRow"
            }),
        )
        .unwrap();
        assert_eq!(grp["grouped_count"], 3);

        let exported = dom_serialize_subtree_in_test(&doc, "Workspace.PillarRow");
        let xml = exported["xml"].as_str().unwrap().to_string();
        assert!(xml.contains("PillarRow"));

        dom_reparent_rename_delete_in(
            &mut doc,
            &json!({ "delete": ["Workspace.PillarRow"] }),
        )
        .unwrap();
        assert!(variants::resolve_target_str(&doc, "Workspace.PillarRow").is_err());

        let imported = dom_import_in(
            &mut doc,
            &json!({ "parent": "Workspace", "xml": xml }),
        )
        .unwrap();
        assert_eq!(imported["imported_count"], 1);
        assert!(variants::resolve_target_str(&doc, "Workspace.PillarRow.Row_Pillar_2").is_ok());

        // Ungroup back to Workspace
        let ungrp = dom_group_ungroup_in(
            &mut doc,
            &json!({ "action": "ungroup", "targets": ["Workspace.PillarRow"] }),
        )
        .unwrap();
        assert_eq!(ungrp["released_count"], 3);
        assert!(variants::resolve_target_str(&doc, "Workspace.Row_Pillar_2").is_ok());
    }

    fn dom_serialize_subtree_in_test(doc: &Document, target: &str) -> Value {
        dom_export_in(doc, &json!({ "target": target, "format": "rbxmx" })).unwrap()
    }

    #[cfg(test)]
    pub(crate) fn test_app_headless(ctx: &egui::Context) -> crate::app::RustudioApp {
        let harness = crate::app::Harness {
            shot_after: None,
            log_out: None,
            actions: Vec::new(),
            pointer: None,
            started: std::time::Instant::now(),
            held: 0.0,
            screenshot: None,
            selftest: None,
            ui_shots: None,
            screenshot_requested: false,
            start_play: false,
            camera: None,
            shift: false,
            shots: Vec::new(),
            shot_next: 0,
            shot_lines: Vec::new(),
            from_play: false,
            play_started: None,
            play_game_time: None,
            frame_ms: Vec::new(),
            last_frame: None,
            open_roblox: None,
            team_create: None,
            roblox_picker: None,
            roblox_settings: false,
            ui_due: Vec::new(),
            no_ui: true,
            overlays: false,
            no_render: true,
            end_after: None,
            fail_on_error: false,
            result: None,
            plugin_files: Vec::new(),
            installed_plugins: Vec::new(),
            plugin_dirs: Vec::new(),
            no_plugins: true,
            mcp_port: None,
            no_mcp: true,
            plugins_started: false,
            end_value: None,
            run_frames: Vec::new(),
            run_last: None,
            run_playing: false,
            finished: false,
            bench: Some(crate::bench::Bench::new(None, "mcp-test", true, 1.0)),
        };
        crate::app::RustudioApp::new_in(ctx, None, None, harness, false)
    }

    fn call_json(ed: &mut Editor, ctx: &egui::Context, tool: &str, args: Value) -> Value {
        let (tx, _rx) = std::sync::mpsc::channel();
        let Some(CallOutcome::Immediate(res)) = call(ed, ctx, tool, &args, &tx) else {
            panic!("expected immediate outcome for {tool}");
        };
        assert!(!res.is_error, "{tool} errored: {:?}", res.content);
        let super::super::protocol::ToolContent::Text { text } = &res.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn editor_play_edit_isolation_and_live_server_sync() {
        let ctx = egui::Context::default();
        let mut app = test_app_headless(&ctx);
        let ed = &mut app.ed;

        call_json(
            ed,
            &ctx,
            "dom_create_instances",
            json!({
                "items": [{
                    "parent": "Workspace",
                    "class": "Part",
                    "name": "TargetPart",
                    "properties": { "Position": [0, 5, 0] }
                }]
            }),
        );

        ed.start_play(RunMode::Run);
        assert!(ed.is_playing());

        // Mutate Edit DataModel while Play is running
        call_json(
            ed,
            &ctx,
            "dom_set_properties",
            json!({
                "datamodel": "Edit",
                "edits": [{
                    "target": "Workspace.TargetPart",
                    "properties": { "Position": [100, 10, 0] }
                }]
            }),
        );

        // Mutate Server DataModel live while Play is running
        call_json(
            ed,
            &ctx,
            "dom_set_properties",
            json!({
                "datamodel": "Server",
                "edits": [{
                    "target": "Workspace.TargetPart",
                    "properties": { "Position": [-50, 20, 0] }
                }]
            }),
        );

        let edit_view = call_json(
            ed,
            &ctx,
            "dom_inspect_instance",
            json!({ "datamodel": "Edit", "target": "Workspace.TargetPart" }),
        );
        let server_view = call_json(
            ed,
            &ctx,
            "dom_inspect_instance",
            json!({ "datamodel": "Server", "target": "Workspace.TargetPart" }),
        );
        assert_eq!(edit_view["world"]["position"], json!([100.0, 10.0, 0.0]));
        assert_eq!(server_view["world"]["position"], json!([-50.0, 20.0, 0.0]));

        ed.stop_play();
        assert!(!ed.is_playing());

        // After Play stops, the Edit-during-Play change is preserved!
        let post_stop = call_json(
            ed,
            &ctx,
            "dom_inspect_instance",
            json!({ "target": "Workspace.TargetPart" }),
        );
        assert_eq!(post_stop["world"]["position"], json!([100.0, 10.0, 0.0]));
    }
}
