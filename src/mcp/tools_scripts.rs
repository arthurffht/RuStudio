//! Unrestricted script listing, reading, atomic batch writing/patching,
//! unlimited grep, syntax checking, and editor tab control for OmniMCP.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::mpsc::Sender;

use md5::{Digest, Md5};
use rbx_dom_weak::types::{Enum, Ref, Variant};
use rbx_dom_weak::{ustr, InstanceBuilder};
use serde_json::{json, Map, Value};

use crate::app::Editor;
use crate::doc::{self, Change, Document};
use crate::runtime;

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::tools_dom::{
    datamodel_arg, normalize_path_prefix, path_matches_prefix, play_apply_prop, resolve_target_dm,
    resolve_targets_arg, sync_script_tab_sources, with_edit_or_play, TargetDm,
};
use super::variants;

pub fn schemas() -> Vec<ToolDef> {
    let dm_prop = json!({
        "type": "string",
        "enum": ["Active", "Edit", "Server", "Client"],
        "description": "Target DataModel (default: Active; 'Edit' works even during Play)."
    });
    vec![
        ToolDef {
            name: "script_list_all".into(),
            description: "List every Script, LocalScript, and ModuleScript (LuaSourceContainer) with ref_id, path, class, lines, bytes, disabled, run_context, and syntax check status.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "path_prefix": { "type": "string", "description": "Optional dot/slash path prefix filter." },
                    "class_filter": { "type": "string", "description": "Optional class filter (e.g. Script, LocalScript, ModuleScript)." },
                    "include_syntax_check": { "type": "boolean", "description": "Run Luau syntax check per script (default: true)." }
                }
            }),
        },
        ToolDef {
            name: "script_read_batch".into(),
            description: "Read one or many scripts in one call (full source or line ranges), returning line-numbered source, raw source, byte length, and MD5 digest.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "target": { "description": "Single script target (path, ref:<id>, or {ref|path})." },
                    "targets": {
                        "type": "array",
                        "description": "Array of script targets (strings/refs or {target, start_line?, end_line?})."
                    },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Default 1-based start line." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Default 1-based inclusive end line." },
                    "numbered": { "type": "boolean", "description": "Include line-numbered source text (default: true)." }
                }
            }),
        },
        ToolDef {
            name: "script_write_batch".into(),
            description: "Create or replace Source across multiple scripts atomically in one call, optionally auto-creating missing Folder parents.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "scripts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Dot or slash path for the script (e.g. ServerScriptService.Main)." },
                                "target": { "description": "Optional ref:<id> or path when updating an existing script." },
                                "class": { "type": "string", "enum": ["Script", "LocalScript", "ModuleScript"], "description": "Class for newly created scripts (default: Script)." },
                                "source": { "type": "string" },
                                "disabled": { "type": "boolean" },
                                "run_context": { "description": "Legacy, Server, Client, Plugin (or enum integer 0..3)." },
                                "create_parents": { "type": "boolean", "description": "Auto-create missing Folder parents along path (default: false)." }
                            },
                            "required": ["source"]
                        }
                    }
                },
                "required": ["scripts"]
            }),
        },
        ToolDef {
            name: "script_patch_batch".into(),
            description: "Atomically apply surgical search/replace or line-range edits across arbitrarily many scripts. Validates ALL patches before committing ANY edit.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "files": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": { "description": "Script path, ref:<id>, or {ref|path}." },
                                "edits": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "old_string": { "type": "string" },
                                            "new_string": { "type": "string" },
                                            "replace_all": { "type": "boolean" },
                                            "start_line": { "type": "integer", "minimum": 1 },
                                            "end_line": { "type": "integer", "minimum": 0 },
                                            "replacement": { "type": "string" }
                                        }
                                    }
                                }
                            },
                            "required": ["target", "edits"]
                        }
                    }
                },
                "required": ["files"]
            }),
        },
        ToolDef {
            name: "script_grep".into(),
            description: "Unlimited search across all scripts (no 50-match cap), returning path, ref_id, line, column, text, and optional context lines.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "query": { "type": "string" },
                    "case_sensitive": { "type": "boolean", "description": "Case-sensitive search (default: false)." },
                    "context_lines": { "type": "integer", "minimum": 0, "description": "Number of context lines before/after each match (default: 0)." },
                    "path_prefix": { "type": "string" },
                    "class_filter": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Optional cap (unlimited if omitted)." }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "script_check_syntax".into(),
            description: "Compile-check Luau syntax on one script, multiple scripts, or all scripts in the DataModel.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "target": {},
                    "targets": { "type": "array" },
                    "path_prefix": { "type": "string" },
                    "errors_only": { "type": "boolean", "description": "Only list scripts with syntax errors (default: false)." }
                }
            }),
        },
        ToolDef {
            name: "script_open_tab".into(),
            description: "Open or focus script tab(s) inside RuStudio's code editor.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_prop,
                    "target": {},
                    "targets": { "type": "array" }
                }
            }),
        },
    ]
}

pub fn call(
    ed: &mut Editor,
    ctx: &egui::Context,
    name: &str,
    args: &Value,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let res = match name {
        "script_list_all" => handle_list_all(ed, args),
        "script_read_batch" => handle_read_batch(ed, args),
        "script_write_batch" => handle_write_batch(ed, args),
        "script_patch_batch" => handle_patch_batch(ed, args),
        "script_grep" => handle_grep(ed, args),
        "script_check_syntax" => handle_check_syntax(ed, args),
        "script_open_tab" => handle_open_tab(ed, ctx, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(match res {
        Ok(v) => ToolCallResult::json(&v),
        Err(e) => ToolCallResult::error(e),
    }))
}

// =========================================================================
// Shared script utilities
// =========================================================================

fn is_script_class(class: &str) -> bool {
    doc::is_a(class, "LuaSourceContainer")
}

fn script_source(doc: &Document, r: Ref) -> String {
    match doc.prop(r, "Source") {
        Some(Variant::String(s)) => s,
        _ => String::new(),
    }
}

fn script_disabled(doc: &Document, r: Ref, class: &str) -> bool {
    if !doc::is_a(class, "BaseScript") {
        return false;
    }
    matches!(doc.prop(r, "Disabled"), Some(Variant::Bool(true)))
        || matches!(doc.prop(r, "Enabled"), Some(Variant::Bool(false)))
}

fn script_run_context(doc: &Document, r: Ref, class: &str) -> Value {
    if !doc::is_a(class, "BaseScript") {
        return Value::Null;
    }
    let val = crate::scene::enum_u32(doc.prop(r, "RunContext")).unwrap_or(0);
    let label = variants::enum_u32_to_name("RunContext", val).unwrap_or_else(|| match val {
        0 => "Legacy".into(),
        1 => "Server".into(),
        2 => "Client".into(),
        3 => "Plugin".into(),
        other => other.to_string(),
    });
    Value::String(label)
}

fn md5_hex(data: &str) -> String {
    let digest = Md5::digest(data.as_bytes());
    let mut hex = String::with_capacity(32);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn line_count(source: &str) -> usize {
    if source.is_empty() {
        0
    } else {
        source.split('\n').count()
    }
}

fn split_lines_owned(source: &str) -> (Vec<String>, bool) {
    if source.is_empty() {
        return (Vec::new(), false);
    }
    let had_trailing_nl = source.ends_with('\n');
    let body = if had_trailing_nl {
        &source[..source.len() - 1]
    } else {
        source
    };
    let lines = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    (lines, had_trailing_nl)
}

fn split_lines_ref(source: &str) -> Vec<&str> {
    if source.is_empty() {
        return Vec::new();
    }
    source
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

fn syntax_check_json(source: &str, path: &str) -> (bool, Value) {
    match runtime::check_syntax(source, path) {
        Some((line, message)) => (
            true,
            json!({
                "line": line,
                "message": message,
            }),
        ),
        None => (false, Value::Null),
    }
}

fn collect_scripts(
    doc: &Document,
    path_prefix: Option<&str>,
    class_filter: Option<&str>,
) -> Vec<(Ref, String, String)> {
    let norm_prefix = path_prefix.filter(|s| !s.is_empty()).map(normalize_path_prefix);
    let mut out = Vec::new();
    for inst in doc.dom.descendants() {
        let cls = inst.class.as_str();
        if !is_script_class(cls) {
            continue;
        }
        if let Some(cf) = class_filter {
            if !doc::is_a(cls, cf) {
                continue;
            }
        }
        let r = inst.referent();
        let path = variants::instance_path(doc, r);
        if let Some(prefix) = &norm_prefix {
            if !path_matches_prefix(&path, prefix) {
                continue;
            }
        }
        out.push((r, path, cls.to_string()));
    }
    out
}

fn parse_run_context(val: &Value) -> Result<u32, String> {
    match val {
        Value::Number(n) => n
            .as_u64()
            .and_then(|u| u32::try_from(u).ok())
            .ok_or_else(|| format!("invalid RunContext number {val}")),
        Value::String(s) => variants::enum_name_to_u32(Some("RunContext"), s).or_else(|| {
            match s.trim().to_ascii_lowercase().as_str() {
                "legacy" => Some(0),
                "server" => Some(1),
                "client" => Some(2),
                "plugin" => Some(3),
                _ => None,
            }
        }).ok_or_else(|| format!("unknown RunContext '{s}'")),
        _ => Err(format!("expected RunContext name or integer, got {val}")),
    }
}

// =========================================================================
// 1. script_list_all
// =========================================================================

pub(crate) fn script_list_all_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let class_filter = args
        .get("class_filter")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let include_syntax = args
        .get("include_syntax_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let list = collect_scripts(doc, path_prefix, class_filter);
    let mut scripts = Vec::with_capacity(list.len());
    for (r, path, class) in list {
        let src = script_source(doc, r);
        let (has_err, err_val) = if include_syntax {
            syntax_check_json(&src, &path)
        } else {
            (false, Value::Null)
        };
        let mut item = json!({
            "ref_id": variants::ref_to_str(r),
            "path": path,
            "name": doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
            "class": class,
            "lines": line_count(&src),
            "bytes": src.len(),
            "disabled": script_disabled(doc, r, &class),
            "run_context": script_run_context(doc, r, &class),
        });
        if include_syntax {
            item["has_syntax_error"] = json!(has_err);
            item["syntax_error"] = err_val;
        }
        scripts.push(item);
    }

    Ok(json!({
        "count": scripts.len(),
        "scripts": scripts,
    }))
}

fn handle_list_all(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    script_list_all_in(doc, args)
}

// =========================================================================
// 2. script_read_batch
// =========================================================================

struct ReadTargetSpec {
    r: Ref,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

pub(crate) fn script_read_batch_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let default_start = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let default_end = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let numbered = args
        .get("numbered")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut specs = Vec::new();
    if let Some(arr) = args.get("targets").and_then(|v| v.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let (tv, sl, el) = if let Some(obj) = item.as_object() {
                if let Some(t) = obj.get("target") {
                    let s = obj.get("start_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                    let e = obj.get("end_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                    (t, s.or(default_start), e.or(default_end))
                } else {
                    (item, default_start, default_end)
                }
            } else {
                (item, default_start, default_end)
            };
            let r = variants::resolve_target(doc, tv)
                .map_err(|e| format!("targets[{idx}]: {e}"))?;
            specs.push(ReadTargetSpec {
                r,
                start_line: sl,
                end_line: el,
            });
        }
    } else if let Some(tv) = args.get("target") {
        let r = variants::resolve_target(doc, tv)?;
        specs.push(ReadTargetSpec {
            r,
            start_line: default_start,
            end_line: default_end,
        });
    } else {
        return Err("script_read_batch requires 'target' or 'targets'".into());
    }

    let mut scripts = Vec::with_capacity(specs.len());
    for spec in specs {
        let class = doc
            .class_of(spec.r)
            .ok_or_else(|| format!("instance '{}' not found", variants::ref_to_str(spec.r)))?
            .to_string();
        if !is_script_class(&class) {
            return Err(format!(
                "'{}' is a {class}, not a Script/LocalScript/ModuleScript",
                variants::instance_path(doc, spec.r)
            ));
        }
        let path = variants::instance_path(doc, spec.r);
        let full_src = script_source(doc, spec.r);
        let lines = split_lines_ref(&full_src);
        let total_lines = lines.len();
        let digest = md5_hex(&full_src);

        let (start, end, sliced_src, numbered_src) = if total_lines == 0 {
            if let Some(s) = spec.start_line {
                if s > 1 {
                    return Err(format!("{path}: start_line {s} exceeds total_lines 0"));
                }
            }
            (0usize, 0usize, String::new(), String::new())
        } else {
            let s = spec.start_line.unwrap_or(1);
            if s == 0 {
                return Err(format!("{path}: start_line must be >= 1"));
            }
            if s > total_lines {
                return Err(format!(
                    "{path}: start_line {s} exceeds total_lines {total_lines}"
                ));
            }
            let e = spec.end_line.unwrap_or(total_lines).min(total_lines);
            if e < s {
                return Err(format!("{path}: end_line {e} < start_line {s}"));
            }
            let slice = &lines[(s - 1)..e];
            let raw = slice.join("\n");
            let num = if numbered {
                let width = e.to_string().len().max(1);
                let mut buf = String::new();
                for (i, line) in slice.iter().enumerate() {
                    if i > 0 {
                        buf.push('\n');
                    }
                    let _ = write!(buf, "{:>width$} | {line}", s + i);
                }
                buf
            } else {
                String::new()
            };
            (s, e, raw, num)
        };

        let mut item = json!({
            "ref_id": variants::ref_to_str(spec.r),
            "path": path,
            "name": doc.dom.get_by_ref(spec.r).map(|i| i.name.as_str()).unwrap_or(""),
            "class": class,
            "disabled": script_disabled(doc, spec.r, &class),
            "run_context": script_run_context(doc, spec.r, &class),
            "total_lines": total_lines,
            "start_line": start,
            "end_line": end,
            "bytes": full_src.len(),
            "md5": digest,
            "source": sliced_src,
        });
        if numbered {
            item["numbered_source"] = Value::String(numbered_src);
        }
        scripts.push(item);
    }

    Ok(json!({
        "count": scripts.len(),
        "scripts": scripts,
    }))
}

fn handle_read_batch(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    script_read_batch_in(doc, args)
}

// =========================================================================
// 3. script_write_batch
// =========================================================================

#[derive(Clone)]
enum NodeHandle {
    Existing(Ref),
    Planned(usize),
}

struct PlannedFolder {
    id: usize,
    parent: NodeHandle,
    class: String,
    name: String,
}

enum WriteAction {
    UpdateExisting {
        r: Ref,
        class: String,
        source: String,
        disabled: Option<bool>,
        run_context: Option<u32>,
    },
    CreateNew {
        parent: NodeHandle,
        name: String,
        class: String,
        source: String,
        disabled: Option<bool>,
        run_context: Option<u32>,
    },
}

struct PlannedWriteBatch {
    folders: Vec<PlannedFolder>,
    actions: Vec<WriteAction>,
}

fn split_script_path(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw
        .trim()
        .strip_prefix("game.")
        .or_else(|| raw.trim().strip_prefix("Game."))
        .or_else(|| raw.trim().strip_prefix("DataModel."))
        .or_else(|| raw.trim().strip_prefix("game/"))
        .or_else(|| raw.trim().strip_prefix("Game/"))
        .or_else(|| raw.trim().strip_prefix("DataModel/"))
        .unwrap_or(raw.trim());
    let norm = trimmed.trim_matches(|c| c == '.' || c == '/');
    let sep = if norm.contains('/') { '/' } else { '.' };
    let segs: Vec<String> = norm
        .split(sep)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if segs.is_empty() {
        return Err(format!("invalid empty script path '{raw}'"));
    }
    Ok(segs)
}

fn find_child_in_doc(doc: &Document, parent: Ref, seg: &str, is_root_child: bool) -> Option<Ref> {
    let pinst = doc.dom.get_by_ref(parent)?;
    pinst
        .children()
        .iter()
        .copied()
        .find(|c| doc.dom.get_by_ref(*c).is_some_and(|ch| ch.name == seg))
        .or_else(|| {
            if is_root_child {
                if seg.eq_ignore_ascii_case("workspace") {
                    doc.workspace()
                } else {
                    doc.find_child(parent, seg)
                }
            } else {
                None
            }
        })
}

fn plan_write_batch(doc: &Document, args: &Value) -> Result<PlannedWriteBatch, String> {
    let entries = args
        .get("scripts")
        .and_then(|v| v.as_array())
        .ok_or("script_write_batch requires 'scripts' array")?;

    let mut folders: Vec<PlannedFolder> = Vec::new();
    let mut planned_folder_lookup: HashMap<(Option<Ref>, Option<usize>, String), usize> =
        HashMap::new();
    // Track scripts created earlier in the same batch so duplicate paths in one batch are caught.
    let mut planned_script_paths: HashSet<String> = HashSet::new();
    let mut actions = Vec::with_capacity(entries.len());

    for (idx, entry) in entries.iter().enumerate() {
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("scripts[{idx}] must be an object"))?;
        let source = obj
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("scripts[{idx}] missing string 'source'"))?
            .to_string();
        let req_class = obj
            .get("class")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());
        if let Some(c) = &req_class {
            if !is_script_class(c) {
                return Err(format!(
                    "scripts[{idx}]: class '{c}' is not a Script, LocalScript, or ModuleScript"
                ));
            }
        }
        let disabled = match obj.get("disabled") {
            Some(Value::Bool(b)) => Some(*b),
            Some(v) if !v.is_null() => {
                return Err(format!("scripts[{idx}].disabled must be boolean"))
            }
            _ => None,
        };
        let run_context = match obj.get("run_context") {
            Some(v) if !v.is_null() => {
                Some(parse_run_context(v).map_err(|e| format!("scripts[{idx}].run_context: {e}"))?)
            }
            _ => None,
        };
        let create_parents = obj
            .get("create_parents")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let target_val = obj.get("target").or_else(|| obj.get("path")).ok_or_else(|| {
            format!("scripts[{idx}] requires 'path' or 'target'")
        })?;

        if let Ok(existing_r) = variants::resolve_target(doc, target_val) {
            let actual_class = doc
                .class_of(existing_r)
                .ok_or_else(|| format!("scripts[{idx}]: instance disappeared"))?
                .to_string();
            if !is_script_class(&actual_class) {
                return Err(format!(
                    "scripts[{idx}]: target '{}' is a {actual_class}, not a script",
                    variants::instance_path(doc, existing_r)
                ));
            }
            if let Some(c) = &req_class {
                if c != &actual_class {
                    return Err(format!(
                        "scripts[{idx}]: target '{}' already exists as {actual_class} (requested {c})",
                        variants::instance_path(doc, existing_r)
                    ));
                }
            }
            actions.push(WriteAction::UpdateExisting {
                r: existing_r,
                class: actual_class,
                source,
                disabled,
                run_context,
            });
            continue;
        }

        let raw_path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| target_val.as_str())
            .ok_or_else(|| {
                format!("scripts[{idx}]: target does not exist and no string 'path' was provided")
            })?;

        if raw_path.trim().starts_with("ref:")
            || (raw_path.trim().len() == 32 && raw_path.trim().bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(format!("scripts[{idx}]: ref '{raw_path}' does not exist"));
        }

        let segs = split_script_path(raw_path).map_err(|e| format!("scripts[{idx}]: {e}"))?;
        let canonical = segs.join(".");
        if !planned_script_paths.insert(canonical.clone()) {
            return Err(format!(
                "scripts[{idx}]: duplicate creation path '{canonical}' in same batch"
            ));
        }

        let mut cur = NodeHandle::Existing(doc.dom.root_ref());
        for (si, seg) in segs[..segs.len() - 1].iter().enumerate() {
            match &cur {
                NodeHandle::Existing(parent_r) => {
                    if let Some(found) = find_child_in_doc(doc, *parent_r, seg, si == 0) {
                        cur = NodeHandle::Existing(found);
                    } else {
                        if !create_parents {
                            return Err(format!(
                                "scripts[{idx}]: missing parent segment '{seg}' along '{raw_path}' (set create_parents: true)"
                            ));
                        }
                        let key = (Some(*parent_r), None, seg.clone());
                        let fid = if let Some(&id) = planned_folder_lookup.get(&key) {
                            id
                        } else {
                            let id = folders.len();
                            let cls = if si == 0 && doc::is_service(seg) {
                                seg.clone()
                            } else {
                                "Folder".to_string()
                            };
                            folders.push(PlannedFolder {
                                id,
                                parent: NodeHandle::Existing(*parent_r),
                                class: cls,
                                name: seg.clone(),
                            });
                            planned_folder_lookup.insert(key, id);
                            id
                        };
                        cur = NodeHandle::Planned(fid);
                    }
                }
                NodeHandle::Planned(pid) => {
                    let key = (None, Some(*pid), seg.clone());
                    let fid = if let Some(&id) = planned_folder_lookup.get(&key) {
                        id
                    } else {
                        let id = folders.len();
                        folders.push(PlannedFolder {
                            id,
                            parent: NodeHandle::Planned(*pid),
                            class: "Folder".to_string(),
                            name: seg.clone(),
                        });
                        planned_folder_lookup.insert(key, id);
                        id
                    };
                    cur = NodeHandle::Planned(fid);
                }
            }
        }

        let class = req_class.unwrap_or_else(|| "Script".to_string());
        let final_name = segs.last().unwrap().clone();
        actions.push(WriteAction::CreateNew {
            parent: cur,
            name: final_name,
            class,
            source,
            disabled,
            run_context,
        });
    }

    Ok(PlannedWriteBatch { folders, actions })
}

fn apply_script_attrs_builder(
    builder: &mut InstanceBuilder,
    class: &str,
    source: &str,
    disabled: Option<bool>,
    run_context: Option<u32>,
) {
    builder.add_property("Source", Variant::String(source.to_string()));
    if doc::is_a(class, "BaseScript") {
        if let Some(d) = disabled {
            builder.add_property("Disabled", Variant::Bool(d));
            builder.add_property("Enabled", Variant::Bool(!d));
        }
        if let Some(rc) = run_context {
            builder.add_property("RunContext", Variant::Enum(Enum::from_u32(rc)));
        }
    }
}

pub(crate) fn script_write_batch_in(
    doc: &mut Document,
    args: &Value,
) -> Result<(Value, Vec<(Ref, String)>), String> {
    let batch = plan_write_batch(doc, args)?;
    let mut folder_refs: Vec<Option<Ref>> = vec![None; batch.folders.len()];
    let mut create_changes: Vec<Change> = Vec::new();
    let mut prop_edits: Vec<(Ref, rbx_dom_weak::Ustr, Variant)> = Vec::new();
    let mut touched_scripts: Vec<(Ref, String)> = Vec::new();

    for f in batch.folders {
        let parent_r = match f.parent {
            NodeHandle::Existing(r) => r,
            NodeHandle::Planned(pid) => folder_refs[pid].expect("parent folder created first"),
        };
        let is_top_new = matches!(f.parent, NodeHandle::Existing(_));
        let r = doc.dom.insert(parent_r, InstanceBuilder::new(&f.class).with_name(&f.name));
        folder_refs[f.id] = Some(r);
        if is_top_new {
            create_changes.push(Change::Create { r, parent: parent_r });
        }
    }

    for act in batch.actions {
        match act {
            WriteAction::UpdateExisting {
                r,
                class,
                source,
                disabled,
                run_context,
            } => {
                prop_edits.push((r, ustr("Source"), Variant::String(source.clone())));
                if doc::is_a(&class, "BaseScript") {
                    if let Some(d) = disabled {
                        prop_edits.push((r, ustr("Disabled"), Variant::Bool(d)));
                        prop_edits.push((r, ustr("Enabled"), Variant::Bool(!d)));
                    }
                    if let Some(rc) = run_context {
                        prop_edits.push((r, ustr("RunContext"), Variant::Enum(Enum::from_u32(rc))));
                    }
                }
                touched_scripts.push((r, source));
            }
            WriteAction::CreateNew {
                parent,
                name,
                class,
                source,
                disabled,
                run_context,
            } => {
                let (parent_r, parent_was_existing) = match parent {
                    NodeHandle::Existing(r) => (r, true),
                    NodeHandle::Planned(pid) => (folder_refs[pid].expect("folder created"), false),
                };
                let mut builder = InstanceBuilder::new(&class).with_name(&name);
                apply_script_attrs_builder(&mut builder, &class, &source, disabled, run_context);
                let r = doc.dom.insert(parent_r, builder);
                if parent_was_existing {
                    create_changes.push(Change::Create { r, parent: parent_r });
                }
                touched_scripts.push((r, source));
            }
        }
    }

    if !create_changes.is_empty() {
        doc.record_changes("Write scripts (create)", create_changes);
    }
    if !prop_edits.is_empty() {
        doc.set_props("Write scripts", prop_edits, None);
    }

    let mut results = Vec::with_capacity(touched_scripts.len());
    for (r, src) in &touched_scripts {
        let path = variants::instance_path(doc, *r);
        let class = doc.class_of(*r).map(|c| c.to_string()).unwrap_or_default();
        let (has_err, err_val) = syntax_check_json(src, &path);
        results.push(json!({
            "ref_id": variants::ref_to_str(*r),
            "path": path,
            "class": class,
            "lines": line_count(src),
            "bytes": src.len(),
            "md5": md5_hex(src),
            "has_syntax_error": has_err,
            "syntax_error": err_val,
        }));
    }

    Ok((
        json!({
            "count": results.len(),
            "scripts": results,
        }),
        touched_scripts,
    ))
}

fn handle_write_batch(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let dm = resolve_target_dm(ed, args)?;
    let mut updated_tabs = Vec::new();
    let res = with_edit_or_play(
        ed,
        args,
        |doc| {
            let (v, touched) = script_write_batch_in(doc, args)?;
            updated_tabs = touched;
            Ok(v)
        },
        |rt| {
            let batch = {
                let w = runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                plan_write_batch(w.d(), args)?
            };

            let mut folder_refs: Vec<Option<Ref>> = vec![None; batch.folders.len()];
            let mut top_new: Vec<(Ref, Ref)> = Vec::new();
            let mut updates: Vec<(Ref, String, String, Option<bool>, Option<u32>)> = Vec::new();
            let mut touched: Vec<(Ref, String)> = Vec::new();

            {
                let mut w = runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                let root_ref = w.d().dom.root_ref();
                for f in batch.folders {
                    let (parent_r, attach_parentless) = match f.parent {
                        NodeHandle::Existing(r) => (r, true),
                        NodeHandle::Planned(pid) => (folder_refs[pid].expect("folder exists"), false),
                    };
                    let r = w.dm().dom.insert(
                        if attach_parentless { root_ref } else { parent_r },
                        InstanceBuilder::new(&f.class).with_name(&f.name),
                    );
                    if attach_parentless {
                        w.dm().dom.transfer_within(r, Ref::none());
                        top_new.push((r, parent_r));
                    }
                    folder_refs[f.id] = Some(r);
                }

                for act in batch.actions {
                    match act {
                        WriteAction::UpdateExisting {
                            r,
                            class,
                            source,
                            disabled,
                            run_context,
                        } => {
                            touched.push((r, source.clone()));
                            updates.push((r, class, source, disabled, run_context));
                        }
                        WriteAction::CreateNew {
                            parent,
                            name,
                            class,
                            source,
                            disabled,
                            run_context,
                        } => {
                            let (parent_r, attach_parentless) = match parent {
                                NodeHandle::Existing(r) => (r, true),
                                NodeHandle::Planned(pid) => {
                                    (folder_refs[pid].expect("folder exists"), false)
                                }
                            };
                            let mut builder = InstanceBuilder::new(&class).with_name(&name);
                            apply_script_attrs_builder(
                                &mut builder,
                                &class,
                                &source,
                                disabled,
                                run_context,
                            );
                            let r = w.dm().dom.insert(
                                if attach_parentless { root_ref } else { parent_r },
                                builder,
                            );
                            if attach_parentless {
                                w.dm().dom.transfer_within(r, Ref::none());
                                top_new.push((r, parent_r));
                            }
                            touched.push((r, source));
                        }
                    }
                }
            }

            for (r, parent) in top_new {
                rt.apply_parent(r, parent);
            }
            for (r, class, source, disabled, run_context) in updates {
                play_apply_prop(rt, r, "Source", Variant::String(source));
                if doc::is_a(&class, "BaseScript") {
                    if let Some(d) = disabled {
                        play_apply_prop(rt, r, "Disabled", Variant::Bool(d));
                        play_apply_prop(rt, r, "Enabled", Variant::Bool(!d));
                    }
                    if let Some(rc) = run_context {
                        play_apply_prop(rt, r, "RunContext", Variant::Enum(Enum::from_u32(rc)));
                    }
                }
            }

            let w = runtime::world(&rt.lua).map_err(|e| e.to_string())?;
            let d = w.d();
            let mut results = Vec::with_capacity(touched.len());
            for (r, src) in &touched {
                let path = variants::instance_path(d, *r);
                let class = d.class_of(*r).map(|c| c.to_string()).unwrap_or_default();
                let (has_err, err_val) = syntax_check_json(src, &path);
                results.push(json!({
                    "ref_id": variants::ref_to_str(*r),
                    "path": path,
                    "class": class,
                    "lines": line_count(src),
                    "bytes": src.len(),
                    "md5": md5_hex(src),
                    "has_syntax_error": has_err,
                    "syntax_error": err_val,
                }));
            }
            Ok(json!({
                "count": results.len(),
                "scripts": results,
            }))
        },
    )?;
    sync_script_tab_sources(ed, dm, &updated_tabs);
    Ok(res)
}

// =========================================================================
// 4. script_patch_batch
// =========================================================================

fn apply_single_patch_edit(
    source: &mut String,
    edit: &Map<String, Value>,
    ctx_label: &str,
) -> Result<(), String> {
    if let Some(old_val) = edit.get("old_string") {
        let old_str = old_val
            .as_str()
            .ok_or_else(|| format!("{ctx_label}: 'old_string' must be a string"))?;
        if old_str.is_empty() {
            return Err(format!("{ctx_label}: 'old_string' cannot be empty"));
        }
        let new_str = edit
            .get("new_string")
            .or_else(|| edit.get("replacement"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{ctx_label}: missing 'new_string' (or 'replacement')"))?;
        let replace_all = edit
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (needle, repl) = if source.contains(old_str) {
            (old_str.to_string(), new_str.to_string())
        } else if source.contains("\r\n") && !old_str.contains("\r\n") {
            let crlf_old = old_str.replace('\n', "\r\n");
            let crlf_new = new_str.replace('\n', "\r\n");
            (crlf_old, crlf_new)
        } else {
            (old_str.to_string(), new_str.to_string())
        };

        let count = source.matches(&needle).count();
        if count == 0 {
            return Err(format!("{ctx_label}: old_string not found in script"));
        }
        if !replace_all && count > 1 {
            return Err(format!(
                "{ctx_label}: old_string matched {count} times; set replace_all: true or provide more surrounding context"
            ));
        }
        if replace_all {
            *source = source.replace(&needle, &repl);
        } else {
            *source = source.replacen(&needle, &repl, 1);
        }
        return Ok(());
    }

    if let Some(sl_val) = edit.get("start_line") {
        let start_line = sl_val
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| format!("{ctx_label}: 'start_line' must be an integer"))?;
        if start_line == 0 {
            return Err(format!("{ctx_label}: start_line is 1-based and must be >= 1"));
        }
        let replacement = edit
            .get("replacement")
            .or_else(|| edit.get("new_string"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{ctx_label}: missing 'replacement' (or 'new_string')"))?;

        let (mut lines, had_trailing_nl) = split_lines_owned(source);
        let total = lines.len();
        let end_line = match edit.get("end_line").and_then(|v| v.as_u64()) {
            Some(e) => e as usize,
            None => {
                if total == 0 && start_line == 1 {
                    0
                } else {
                    start_line
                }
            }
        };

        if start_line > total + 1 {
            return Err(format!(
                "{ctx_label}: start_line {start_line} exceeds total_lines + 1 ({})",
                total + 1
            ));
        }
        if end_line + 1 < start_line {
            return Err(format!(
                "{ctx_label}: end_line ({end_line}) cannot be less than start_line - 1 ({})",
                start_line - 1
            ));
        }
        if end_line > total {
            return Err(format!(
                "{ctx_label}: end_line {end_line} exceeds total_lines {total}"
            ));
        }

        let repl_lines: Vec<String> = if replacement.is_empty() {
            Vec::new()
        } else {
            let trimmed = replacement
                .strip_suffix("\r\n")
                .or_else(|| replacement.strip_suffix('\n'))
                .unwrap_or(replacement);
            trimmed
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect()
        };

        let start_idx = start_line - 1;
        let end_idx_excl = end_line;
        lines.splice(start_idx..end_idx_excl, repl_lines);

        let line_sep = if source.contains("\r\n") { "\r\n" } else { "\n" };
        let mut out = lines.join(line_sep);
        if had_trailing_nl && !lines.is_empty() {
            out.push_str(line_sep);
        }
        *source = out;
        return Ok(());
    }

    Err(format!(
        "{ctx_label}: edit must specify 'old_string'+'new_string' or 'start_line'+'replacement'"
    ))
}

struct PlannedPatchFile {
    r: Ref,
    path: String,
    class: String,
    new_source: String,
    edits_applied: usize,
}

fn plan_patch_batch(doc: &Document, args: &Value) -> Result<Vec<PlannedPatchFile>, String> {
    let files = args
        .get("files")
        .or_else(|| args.get("scripts"))
        .and_then(|v| v.as_array())
        .ok_or("script_patch_batch requires 'files' array")?;

    let mut staged_sources: HashMap<Ref, String> = HashMap::new();
    let mut order: Vec<Ref> = Vec::new();
    let mut edit_counts: HashMap<Ref, usize> = HashMap::new();

    for (fidx, fval) in files.iter().enumerate() {
        let obj = fval
            .as_object()
            .ok_or_else(|| format!("files[{fidx}] must be an object"))?;
        let tv = obj
            .get("target")
            .ok_or_else(|| format!("files[{fidx}] missing 'target'"))?;
        let r = variants::resolve_target(doc, tv).map_err(|e| format!("files[{fidx}]: {e}"))?;
        let class = doc
            .class_of(r)
            .ok_or_else(|| format!("files[{fidx}]: instance disappeared"))?
            .to_string();
        let path = variants::instance_path(doc, r);
        if !is_script_class(&class) {
            return Err(format!(
                "files[{fidx}]: '{path}' is a {class}, not a script"
            ));
        }

        let edits = obj
            .get("edits")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("files[{fidx}] missing 'edits' array"))?;

        if !staged_sources.contains_key(&r) {
            staged_sources.insert(r, script_source(doc, r));
            order.push(r);
        }
        let buf = staged_sources.get_mut(&r).unwrap();

        for (eidx, eval) in edits.iter().enumerate() {
            let eobj = eval
                .as_object()
                .ok_or_else(|| format!("files[{fidx}].edits[{eidx}] must be an object"))?;
            apply_single_patch_edit(
                buf,
                eobj,
                &format!("files[{fidx}]({path}).edits[{eidx}]"),
            )?;
        }
        *edit_counts.entry(r).or_default() += edits.len();
    }

    let mut out = Vec::with_capacity(order.len());
    for r in order {
        let path = variants::instance_path(doc, r);
        let class = doc.class_of(r).map(|c| c.to_string()).unwrap_or_default();
        let new_source = staged_sources.remove(&r).unwrap();
        let edits_applied = edit_counts[&r];
        out.push(PlannedPatchFile {
            r,
            path,
            class,
            new_source,
            edits_applied,
        });
    }
    Ok(out)
}

pub(crate) fn script_patch_batch_in(
    doc: &mut Document,
    args: &Value,
) -> Result<(Value, Vec<(Ref, String)>), String> {
    let planned = plan_patch_batch(doc, args)?;
    let edits: Vec<(Ref, rbx_dom_weak::Ustr, Variant)> = planned
        .iter()
        .map(|p| (p.r, ustr("Source"), Variant::String(p.new_source.clone())))
        .collect();
    doc.set_props("Patch scripts", edits, None);

    let mut touched = Vec::with_capacity(planned.len());
    let mut results = Vec::with_capacity(planned.len());
    for p in planned {
        let (has_err, err_val) = syntax_check_json(&p.new_source, &p.path);
        results.push(json!({
            "ref_id": variants::ref_to_str(p.r),
            "path": p.path,
            "class": p.class,
            "edits_applied": p.edits_applied,
            "lines": line_count(&p.new_source),
            "bytes": p.new_source.len(),
            "md5": md5_hex(&p.new_source),
            "has_syntax_error": has_err,
            "syntax_error": err_val,
        }));
        touched.push((p.r, p.new_source));
    }

    Ok((
        json!({
            "count": results.len(),
            "files": results,
        }),
        touched,
    ))
}

fn handle_patch_batch(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let dm = resolve_target_dm(ed, args)?;
    let mut updated_tabs = Vec::new();
    let res = with_edit_or_play(
        ed,
        args,
        |doc| {
            let (v, touched) = script_patch_batch_in(doc, args)?;
            updated_tabs = touched;
            Ok(v)
        },
        |rt| {
            let planned = {
                let w = runtime::world(&rt.lua).map_err(|e| e.to_string())?;
                plan_patch_batch(w.d(), args)?
            };
            let mut results = Vec::with_capacity(planned.len());
            for p in planned {
                play_apply_prop(rt, p.r, "Source", Variant::String(p.new_source.clone()));
                let (has_err, err_val) = syntax_check_json(&p.new_source, &p.path);
                results.push(json!({
                    "ref_id": variants::ref_to_str(p.r),
                    "path": p.path,
                    "class": p.class,
                    "edits_applied": p.edits_applied,
                    "lines": line_count(&p.new_source),
                    "bytes": p.new_source.len(),
                    "md5": md5_hex(&p.new_source),
                    "has_syntax_error": has_err,
                    "syntax_error": err_val,
                }));
            }
            Ok(json!({
                "count": results.len(),
                "files": results,
            }))
        },
    )?;
    sync_script_tab_sources(ed, dm, &updated_tabs);
    Ok(res)
}

// =========================================================================
// 5. script_grep
// =========================================================================

pub(crate) fn script_grep_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("script_grep requires non-empty string 'query'")?;
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let class_filter = args
        .get("class_filter")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);

    let needle_lower = if case_sensitive {
        String::new()
    } else {
        query.to_lowercase()
    };

    let list = collect_scripts(doc, path_prefix, class_filter);
    let mut total_matches = 0usize;
    let mut scripts_matched = 0usize;
    let mut matches = Vec::new();

    for (r, path, class) in list {
        let src = script_source(doc, r);
        let lines = split_lines_ref(&src);
        let ref_str = variants::ref_to_str(r);
        let mut script_hit = false;

        for (li, line_text) in lines.iter().enumerate() {
            let col = if case_sensitive {
                line_text.find(query)
            } else {
                line_text.to_lowercase().find(&needle_lower)
            };
            let Some(byte_col) = col else {
                continue;
            };
            let column = line_text[..byte_col].chars().count() + 1;
            total_matches += 1;
            if !script_hit {
                script_hit = true;
                scripts_matched += 1;
            }
            if limit.is_none_or(|lim| matches.len() < lim) {
                let mut item = json!({
                    "ref_id": ref_str,
                    "path": path,
                    "class": class,
                    "line": li + 1,
                    "column": column,
                    "text": *line_text,
                });
                if context_lines > 0 {
                    let before_start = li.saturating_sub(context_lines);
                    let before: Vec<Value> = (before_start..li)
                        .map(|idx| json!({ "line": idx + 1, "text": lines[idx] }))
                        .collect();
                    let after_end = (li + 1 + context_lines).min(lines.len());
                    let after: Vec<Value> = ((li + 1)..after_end)
                        .map(|idx| json!({ "line": idx + 1, "text": lines[idx] }))
                        .collect();
                    item["context_before"] = Value::Array(before);
                    item["context_after"] = Value::Array(after);
                }
                matches.push(item);
            }
        }
    }

    Ok(json!({
        "query": query,
        "case_sensitive": case_sensitive,
        "total_matches": total_matches,
        "scripts_matched": scripts_matched,
        "returned": matches.len(),
        "truncated": matches.len() < total_matches,
        "matches": matches,
    }))
}

fn handle_grep(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    script_grep_in(doc, args)
}

// =========================================================================
// 6. script_check_syntax
// =========================================================================

pub(crate) fn script_check_syntax_in(doc: &Document, args: &Value) -> Result<Value, String> {
    let errors_only = args
        .get("errors_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let targets: Vec<(Ref, String, String)> =
        if args.get("target").is_some() || args.get("targets").is_some() {
            let refs = resolve_targets_arg(doc, args, "target", "targets")?;
            let mut out = Vec::with_capacity(refs.len());
            for r in refs {
                let class = doc
                    .class_of(r)
                    .ok_or_else(|| format!("instance '{}' not found", variants::ref_to_str(r)))?
                    .to_string();
                let path = variants::instance_path(doc, r);
                if !is_script_class(&class) {
                    return Err(format!("'{path}' is a {class}, not a script"));
                }
                out.push((r, path, class));
            }
            out
        } else {
            let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
            collect_scripts(doc, path_prefix, None)
        };

    let checked = targets.len();
    let mut error_count = 0usize;
    let mut results = Vec::new();

    for (r, path, class) in targets {
        let src = script_source(doc, r);
        let (has_err, err_val) = syntax_check_json(&src, &path);
        if has_err {
            error_count += 1;
        }
        if !errors_only || has_err {
            results.push(json!({
                "ref_id": variants::ref_to_str(r),
                "path": path,
                "class": class,
                "ok": !has_err,
                "error": err_val,
            }));
        }
    }

    Ok(json!({
        "checked": checked,
        "error_count": error_count,
        "ok": error_count == 0,
        "results": results,
    }))
}

fn handle_check_syntax(ed: &mut Editor, args: &Value) -> Result<Value, String> {
    let doc = variants::resolve_datamodel(ed, datamodel_arg(args))?;
    script_check_syntax_in(doc, args)
}

// =========================================================================
// 7. script_open_tab
// =========================================================================

fn handle_open_tab(ed: &mut Editor, ctx: &egui::Context, args: &Value) -> Result<Value, String> {
    let dm = resolve_target_dm(ed, args)?;
    let is_shown = match (ed.play.as_ref(), dm) {
        (None, TargetDm::Edit) => true,
        (Some(p), TargetDm::Play(side)) => p.showing == side,
        _ => false,
    };
    if !is_shown {
        return Err(
            "script_open_tab can only open scripts in the currently shown DataModel".into(),
        );
    }

    let refs = resolve_targets_arg(&ed.doc, args, "target", "targets")?;
    let mut opened = Vec::with_capacity(refs.len());
    for r in refs {
        let class = ed
            .doc
            .class_of(r)
            .ok_or_else(|| format!("instance '{}' not found", variants::ref_to_str(r)))?
            .to_string();
        let path = variants::instance_path(&ed.doc, r);
        if !is_script_class(&class) {
            return Err(format!("'{path}' is a {class}, not a script"));
        }
        if !ed.open_script.contains(&r) {
            ed.open_script.push(r);
        }
        opened.push(json!({
            "ref_id": variants::ref_to_str(r),
            "path": path,
            "name": ed.doc.dom.get_by_ref(r).map(|i| i.name.as_str()).unwrap_or(""),
            "class": class,
        }));
    }
    ctx.request_repaint();

    Ok(json!({
        "opened_count": opened.len(),
        "opened": opened,
    }))
}

// =========================================================================
// Unit tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_expose_all_7_script_tools() {
        let names: Vec<String> = schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "script_list_all",
                "script_read_batch",
                "script_write_batch",
                "script_patch_batch",
                "script_grep",
                "script_check_syntax",
                "script_open_tab",
            ]
        );
    }

    #[test]
    fn write_read_list_and_create_parents_deduped() {
        let mut doc = Document::new_place();

        let written = script_write_batch_in(
            &mut doc,
            &json!({
                "scripts": [
                    {
                        "path": "ServerScriptService.Systems.Combat.Hitbox",
                        "class": "ModuleScript",
                        "create_parents": true,
                        "source": "local M = {}\nfunction M.hit(dmg)\n    return dmg * 2\nend\nreturn M"
                    },
                    {
                        "path": "ServerScriptService.Systems.Combat.Runner",
                        "class": "Script",
                        "create_parents": true,
                        "disabled": false,
                        "run_context": "Server",
                        "source": "local Hitbox = require(script.Parent.Hitbox)\nprint(Hitbox.hit(10))"
                    },
                    {
                        "path": "StarterPlayer.StarterPlayerScripts.ClientInit",
                        "class": "LocalScript",
                        "source": "print('client ready')"
                    }
                ]
            }),
        )
        .unwrap()
        .0;
        assert_eq!(written["count"], 3);

        // Shared intermediate folders were created once (not duplicated)
        let sss = variants::resolve_target_str(&doc, "ServerScriptService").unwrap();
        let systems_count = doc
            .dom
            .get_by_ref(sss)
            .unwrap()
            .children()
            .iter()
            .filter(|c| doc.dom.get_by_ref(**c).is_some_and(|i| i.name == "Systems"))
            .count();
        assert_eq!(systems_count, 1);

        let listed = script_list_all_in(
            &doc,
            &json!({ "path_prefix": "ServerScriptService/Systems" }),
        )
        .unwrap();
        assert_eq!(listed["count"], 2);
        assert_eq!(listed["scripts"][0]["has_syntax_error"], false);

        let read = script_read_batch_in(
            &doc,
            &json!({
                "targets": [
                    {
                        "target": "ServerScriptService.Systems.Combat.Hitbox",
                        "start_line": 2,
                        "end_line": 4
                    },
                    "StarterPlayer.StarterPlayerScripts.ClientInit"
                ]
            }),
        )
        .unwrap();
        assert_eq!(read["count"], 2);
        assert_eq!(
            read["scripts"][0]["source"],
            "function M.hit(dmg)\n    return dmg * 2\nend"
        );
        assert!(read["scripts"][0]["numbered_source"]
            .as_str()
            .unwrap()
            .contains("2 | function M.hit(dmg)"));
        assert_eq!(
            read["scripts"][1]["md5"].as_str().unwrap().len(),
            32
        );
    }

    #[test]
    fn atomic_patch_batch_validates_all_before_committing() {
        let mut doc = Document::new_place();
        script_write_batch_in(
            &mut doc,
            &json!({
                "scripts": [
                    {
                        "path": "ServerScriptService.Alpha",
                        "class": "Script",
                        "source": "local a = 1\nlocal b = 2\nprint(a + b)"
                    },
                    {
                        "path": "ServerScriptService.Beta",
                        "class": "Script",
                        "source": "local x = 10\nprint(x)"
                    }
                ]
            }),
        )
        .unwrap();

        // Patch 1 succeeds in staging, Patch 2 fails -> ZERO edits committed!
        let err = script_patch_batch_in(
            &mut doc,
            &json!({
                "files": [
                    {
                        "target": "ServerScriptService.Alpha",
                        "edits": [{ "old_string": "local a = 1", "new_string": "local a = 99" }]
                    },
                    {
                        "target": "ServerScriptService.Beta",
                        "edits": [{ "old_string": "NONEXISTENT", "new_string": "oops" }]
                    }
                ]
            }),
        )
        .unwrap_err();
        assert!(err.contains("not found"), "{err}");

        let alpha_r = variants::resolve_target_str(&doc, "ServerScriptService.Alpha").unwrap();
        assert_eq!(
            script_source(&doc, alpha_r),
            "local a = 1\nlocal b = 2\nprint(a + b)"
        );

        // Now valid multi-script patch with both old_string and start_line/end_line
        let patched = script_patch_batch_in(
            &mut doc,
            &json!({
                "files": [
                    {
                        "target": "ServerScriptService.Alpha",
                        "edits": [
                            { "old_string": "local a = 1", "new_string": "local a = 99" },
                            { "start_line": 2, "end_line": 2, "replacement": "local b = 200" }
                        ]
                    },
                    {
                        "target": "ServerScriptService.Beta",
                        "edits": [
                            { "old_string": "local x = 10", "new_string": "local x = 42" }
                        ]
                    }
                ]
            }),
        )
        .unwrap()
        .0;
        assert_eq!(patched["count"], 2);
        assert_eq!(
            script_source(&doc, alpha_r),
            "local a = 99\nlocal b = 200\nprint(a + b)"
        );
        let beta_r = variants::resolve_target_str(&doc, "ServerScriptService.Beta").unwrap();
        assert_eq!(script_source(&doc, beta_r), "local x = 42\nprint(x)");
    }

    #[test]
    fn unlimited_grep_and_syntax_check() {
        let mut doc = Document::new_place();
        let mut scripts = Vec::new();
        for i in 0..70 {
            scripts.push(json!({
                "path": format!("ReplicatedStorage.Mod_{i:02}"),
                "class": "ModuleScript",
                "source": format!("local TAG = 'TARGET_TOKEN'\nlocal val = {i}\nreturn val")
            }));
        }
        scripts.push(json!({
            "path": "ServerScriptService.Broken",
            "class": "Script",
            "source": "local ok = 1\nlocal = 2\nprint(ok)"
        }));
        script_write_batch_in(&mut doc, &json!({ "scripts": scripts })).unwrap();

        let grep = script_grep_in(
            &doc,
            &json!({
                "query": "target_token",
                "case_sensitive": false,
                "context_lines": 1,
                "path_prefix": "ReplicatedStorage"
            }),
        )
        .unwrap();
        assert_eq!(grep["total_matches"], 70);
        assert_eq!(grep["returned"], 70);
        assert_eq!(grep["truncated"], false);
        assert_eq!(grep["matches"][0]["line"], 1);
        assert_eq!(grep["matches"][0]["context_after"][0]["line"], 2);

        let syntax = script_check_syntax_in(&doc, &json!({})).unwrap();
        assert_eq!(syntax["checked"], 71);
        assert_eq!(syntax["error_count"], 1);
        assert_eq!(syntax["ok"], false);

        let errors_only = script_check_syntax_in(
            &doc,
            &json!({ "errors_only": true }),
        )
        .unwrap();
        assert_eq!(errors_only["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            errors_only["results"][0]["path"],
            "ServerScriptService.Broken"
        );
        assert_eq!(errors_only["results"][0]["error"]["line"], 2);
    }

    #[test]
    fn editor_open_tab_and_play_edit_isolation() {
        let ctx = egui::Context::default();
        let mut app = super::super::tools_dom::tests::test_app_headless(&ctx);
        let ed = &mut app.ed;
        let (tx, _rx) = std::sync::mpsc::channel();

        let Some(CallOutcome::Immediate(wres)) = call(
            ed,
            &ctx,
            "script_write_batch",
            &json!({
                "scripts": [{
                    "path": "ServerScriptService.LiveScript",
                    "class": "Script",
                    "source": "print('v1')"
                }]
            }),
            &tx,
        ) else {
            panic!("expected immediate");
        };
        assert!(!wres.is_error);

        let Some(CallOutcome::Immediate(ores)) = call(
            ed,
            &ctx,
            "script_open_tab",
            &json!({ "target": "ServerScriptService.LiveScript" }),
            &tx,
        ) else {
            panic!("expected immediate");
        };
        assert!(!ores.is_error);
        let sr = variants::resolve_target_str(&ed.doc, "ServerScriptService.LiveScript").unwrap();
        assert_eq!(ed.open_script, vec![sr]);

        ed.start_play(crate::runtime::RunMode::Run);
        let Some(CallOutcome::Immediate(edit_write)) = call(
            ed,
            &ctx,
            "script_write_batch",
            &json!({
                "datamodel": "Edit",
                "scripts": [{
                    "path": "ServerScriptService.LiveScript",
                    "source": "print('v_edit')"
                }]
            }),
            &tx,
        ) else {
            panic!("expected immediate");
        };
        assert!(!edit_write.is_error);

        let Some(CallOutcome::Immediate(srv_write)) = call(
            ed,
            &ctx,
            "script_write_batch",
            &json!({
                "datamodel": "Server",
                "scripts": [{
                    "path": "ServerScriptService.LiveScript",
                    "source": "print('v_server')"
                }]
            }),
            &tx,
        ) else {
            panic!("expected immediate");
        };
        assert!(!srv_write.is_error);

        let srv_r = variants::resolve_target_str(
            variants::resolve_datamodel(ed, Some("Server")).unwrap(),
            "ServerScriptService.LiveScript",
        )
        .unwrap();
        assert_eq!(
            script_source(variants::resolve_datamodel(ed, Some("Server")).unwrap(), srv_r),
            "print('v_server')"
        );
        let edit_doc = variants::resolve_datamodel(ed, Some("Edit")).unwrap();
        let edit_r = variants::resolve_target_str(edit_doc, "ServerScriptService.LiveScript").unwrap();
        assert_eq!(script_source(edit_doc, edit_r), "print('v_edit')");

        ed.stop_play();
        assert_eq!(script_source(&ed.doc, sr), "print('v_edit')");
    }
}
