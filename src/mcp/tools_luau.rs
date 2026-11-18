//! OmniMCP Unrestricted Luau Execution, Signal Firing & VM Diagnostics tools:
//! `luau_execute`, `luau_fire_signal`, `luau_vm_stats`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::Instant;

use base64::Engine as _;
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, LuaString, MultiValue, ObjectLike, Table, Thread, Value};
use rbx_dom_weak::types::Variant;
use serde_json::{json, Map, Value as JsonValue};

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::variants::{instance_path, json_to_variant, ref_to_str, resolve_target, variant_to_json};
use crate::app::Level;
use crate::doc::Document;
use crate::ltypes::{
    datatype_to_variant, enum_value, LBrickColor, LCFrame, LColor3, LDateTime, LEnum, LEnumItem,
    LRay, LRaycastResult, LTweenInfo, LUDim, LUDim2, LVector2, LVector3,
};
use crate::runtime::{
    describe_error, inst_value, security, variant_to_lua, world, LInst, LObj, RunMode, Runtime,
    Side, World,
};

struct FallbackEditVm {
    rt: Runtime,
    world: Option<World>,
    doc_id: u64,
    bound: bool,
}

thread_local! {
    static FALLBACK_EDIT_VM: RefCell<Option<FallbackEditVm>> = const { RefCell::new(None) };
}

pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "luau_execute".into(),
            description: "Execute arbitrary Luau code inside the live Edit, Server, or Client DataModel VM with selectable thread security identity (default: 8 RobloxScript / maximum unrestricted access). Captures print/warn output, errors with stack tracebacks, execution duration in microseconds, and converts all Luau/Roblox return values to JSON.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Luau source code to execute (may return multiple values)."
                    },
                    "datamodel": {
                        "type": "string",
                        "enum": ["Active", "Edit", "Server", "Client"],
                        "description": "Target VM / DataModel (default: 'Active' -> Edit when stopped, or currently viewed Server/Client side during Play/Run)."
                    },
                    "identity": {
                        "type": "integer",
                        "enum": [2, 4, 6, 8],
                        "description": "Luau thread security identity: 2 (Game script), 4 (CoreScript), 6 (Elevated Plugin), 8 (RobloxScript / maximum unrestricted access, default)."
                    },
                    "chunk_name": {
                        "type": "string",
                        "description": "Optional chunk label for tracebacks."
                    }
                },
                "required": ["code"]
            }),
        },
        ToolDef {
            name: "luau_fire_signal".into(),
            description: "Fire any signal/event on a target Instance inside Edit, Server, or Client (Runtime::fire + dispatch), converting a JSON args array into Luau/Roblox values.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "description": "Target instance path (e.g. 'Workspace.Button.ClickDetector') or 'ref:<id>'.",
                        "oneOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "signal": {
                        "type": "string",
                        "description": "Signal / event name (e.g. 'MouseClick', 'Triggered', 'Activated', 'Event', 'Changed')."
                    },
                    "args": {
                        "type": "array",
                        "description": "Optional JSON arguments converted to Luau values (supports primitives, tables, {\"ref\": ...}, {\"type\": \"Vector3\", \"value\": [x,y,z]}, etc.).",
                        "items": {}
                    },
                    "datamodel": {
                        "type": "string",
                        "enum": ["Active", "Edit", "Server", "Client"],
                        "description": "Target VM / DataModel (default: 'Active')."
                    }
                },
                "required": ["target", "signal"]
            }),
        },
        ToolDef {
            name: "luau_vm_stats".into(),
            description: "Return live Luau VM statistics (lua_slots, mem_stats, gc_counts, heap memory, and SimClock state) for Edit, Server, and Client VMs.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": {
                        "type": "string",
                        "enum": ["All", "Edit", "Server", "Client"],
                        "description": "Which VM(s) to inspect (default: 'All')."
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
    args: &JsonValue,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let result = match name {
        "luau_execute" => luau_execute(ed, ctx, args),
        "luau_fire_signal" => luau_fire_signal(ed, ctx, args),
        "luau_vm_stats" => luau_vm_stats(ed, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(result))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetVm {
    Edit,
    Server,
    Client,
}

impl TargetVm {
    fn label(self) -> &'static str {
        match self {
            TargetVm::Edit => "Edit",
            TargetVm::Server => "Server",
            TargetVm::Client => "Client",
        }
    }
}

fn resolve_target_vm(ed: &crate::app::Editor, dm: Option<&str>) -> Result<TargetVm, String> {
    let key = dm.unwrap_or("active").trim().to_ascii_lowercase();
    match key.as_str() {
        "" | "active" => match ed.play.as_ref() {
            None => Ok(TargetVm::Edit),
            Some(p) => match p.showing {
                Side::Server => Ok(TargetVm::Server),
                Side::Client if p.mode == RunMode::Play && ed.play_side_lua(Side::Client).is_some() => {
                    Ok(TargetVm::Client)
                }
                Side::Client => Ok(TargetVm::Server),
            },
        },
        "edit" => {
            if ed.play.is_some() && ed.play_saved_doc().is_none() {
                return Err("Edit DataModel is unavailable".into());
            }
            Ok(TargetVm::Edit)
        }
        "server" => {
            if ed.play.is_none() {
                return Err("Server VM requires an active Play or Run session".into());
            }
            Ok(TargetVm::Server)
        }
        "client" => {
            let p = ed
                .play
                .as_ref()
                .ok_or_else(|| "Client VM requires an active Play session".to_string())?;
            if p.mode == RunMode::Run || ed.play_side_lua(Side::Client).is_none() {
                return Err("Run mode has no Client VM (use Play mode)".into());
            }
            Ok(TargetVm::Client)
        }
        other => Err(format!(
            "unknown datamodel '{other}'; expected Active, Edit, Server, or Client"
        )),
    }
}

fn with_edit_runtime<R>(
    ed: &mut crate::app::Editor,
    f: impl FnOnce(&Runtime) -> R,
) -> Result<R, String> {
    // 1. When plugins are active with an Edit VM matching the Edit document,
    //    share that live Edit VM directly.
    let mut f_opt = Some(f);
    if ed.plugins.edit_lua().is_some() {
        if ed.play.is_none() {
            if let Some(res) = ed
                .plugins
                .with_edit(&mut ed.doc, |rt| (f_opt.take().expect("once"))(rt))
            {
                ed.plugin_output();
                FALLBACK_EDIT_VM.with(|cell| *cell.borrow_mut() = None);
                return Ok(res);
            }
        } else if let Some(saved_doc) = ed.play_saved_doc_mut() {
            // SAFETY: `saved_doc` lives inside `ed.play`, which `ed.plugins.with_edit`
            // never touches (mirrors `src/plugin_host/glue.rs:76-78`).
            let doc_ptr: *mut Document = saved_doc;
            if let Some(res) = ed
                .plugins
                .with_edit(unsafe { &mut *doc_ptr }, |rt| (f_opt.take().expect("once"))(rt))
            {
                ed.plugin_output();
                FALLBACK_EDIT_VM.with(|cell| *cell.borrow_mut() = None);
                return Ok(res);
            }
        }
    }

    let f_rem = f_opt
        .take()
        .ok_or_else(|| "Edit VM closure consumed unexpectedly".to_string())?;
    with_fallback_edit_vm(ed, f_rem)
}

fn with_fallback_edit_vm<R>(
    ed: &mut crate::app::Editor,
    f: impl FnOnce(&Runtime) -> R,
) -> Result<R, String> {
    let mut logged_output: Vec<(Level, String)> = Vec::new();
    let result = FALLBACK_EDIT_VM.with(|cell| -> Result<R, String> {
        let doc = if ed.play.is_some() {
            ed.play_saved_doc_mut()
                .ok_or_else(|| "Edit DataModel is unavailable".to_string())?
        } else {
            &mut ed.doc
        };

        let needs_new = cell
            .borrow()
            .as_ref()
            .is_none_or(|vm| vm.doc_id != doc.id || vm.world.is_none());
        if needs_new {
            let rt = Runtime::new()?;
            rt.install_side().map_err(|e| describe_error(&e))?;
            let mut w = World::new(Document::empty(), RunMode::Edit);
            w.doc = None;
            *cell.borrow_mut() = Some(FallbackEditVm {
                rt,
                world: Some(w),
                doc_id: doc.id,
                bound: false,
            });
        }

        let mut borrow = cell.borrow_mut();
        let vm = borrow.as_mut().expect("fallback edit vm initialized");
        let mut w = vm.world.take().expect("fallback edit world present");
        w.signal_mode = crate::runtime::signal_timing::SignalMode::of(doc);
        w.doc = Some(std::mem::replace(doc, Document::placeholder()));
        vm.rt.enter(w);
        if !vm.bound {
            vm.bound = true;
            if let Err(e) = vm.rt.bind_globals() {
                if let Ok(mut w) = world(&vm.rt.lua) {
                    w.output.push((Level::Error, describe_error(&e)));
                }
            }
        }

        let r = f(&vm.rt);
        vm.rt.dispatch();

        let mut w = vm.rt.leave();
        *doc = w.doc.take().expect("edit document returned");
        logged_output = std::mem::take(&mut w.output);
        vm.world = Some(w);
        Ok(r)
    });

    for (level, text) in logged_output {
        ed.push_log(level, text);
    }
    result
}

fn with_target_runtime<R>(
    ed: &mut crate::app::Editor,
    vm: TargetVm,
    f: impl FnOnce(&Runtime) -> R,
) -> Result<R, String> {
    match vm {
        TargetVm::Edit => with_edit_runtime(ed, f),
        TargetVm::Server => ed
            .with_play_side(Side::Server, |rt, _, _| f(rt))
            .ok_or_else(|| "Server VM is unavailable".into()),
        TargetVm::Client => ed
            .with_play_side(Side::Client, |rt, _, _| f(rt))
            .ok_or_else(|| "Client VM is unavailable".into()),
    }
}

struct IdentityRestore {
    th: Thread,
    prev: u8,
}

impl Drop for IdentityRestore {
    fn drop(&mut self) {
        security::set(&self.th, self.prev);
    }
}

fn call_with_identity<R>(lua: &Lua, temp_id: u8, f: impl FnOnce() -> R) -> R {
    let th = lua.current_thread();
    let prev = security::of(&th);
    security::set(&th, temp_id);
    let _guard = IdentityRestore { th, prev };
    f()
}

fn build_exec_env(lua: &Lua, identity: u8) -> mlua::Result<Table> {
    if identity == security::GAME {
        return crate::globals_parity::new_env(lua, None);
    }
    let env = crate::globals_parity::harness_env(lua)?;
    if matches!(identity, 6 | 8) {
        // Wrap global capability-checked Studio singletons in this local env so
        // printidentity() still reports 6/8 while settings()/PluginManager()/DebuggerManager()
        // succeed without altering shared VM globals.
        for gname in ["settings", "PluginManager", "DebuggerManager"] {
            if let Ok(Value::Function(orig)) = lua.globals().raw_get::<Value>(gname) {
                let wrapped = lua.create_function(move |lua, args: MultiValue| {
                    call_with_identity(lua, security::CORE, || orig.call::<MultiValue>(args))
                })?;
                env.raw_set(gname, wrapped)?;
            }
        }
    }
    if identity == 8 {
        if let Ok(Value::Table(orig_inst)) = lua.globals().raw_get::<Value>("Instance") {
            let proxy = lua.create_table()?;
            if let Ok(Value::Function(orig_new)) = orig_inst.raw_get::<Value>("new") {
                let new_fn = lua.create_function(move |lua, args: MultiValue| {
                    call_with_identity(lua, security::CORE, || orig_new.call::<MultiValue>(args))
                })?;
                proxy.raw_set("new", new_fn)?;
            }
            if let Ok(Value::Function(orig_fe)) = orig_inst.raw_get::<Value>("fromExisting") {
                let fe_fn = lua.create_function(move |lua, args: MultiValue| {
                    call_with_identity(lua, security::CORE, || orig_fe.call::<MultiValue>(args))
                })?;
                proxy.raw_set("fromExisting", fe_fn)?;
            }
            let mt = lua.create_table()?;
            mt.raw_set("__index", orig_inst)?;
            proxy.set_metatable(Some(mt))?;
            env.raw_set("Instance", proxy)?;
        }

        let exec_env_ref = env.clone();
        let ls = lua.create_function(move |lua, (src, name): (LuaString, Option<String>)| {
            let src_s = src.to_str()?.to_string();
            let cname = match name {
                Some(n) if n.starts_with("=rustudio.") => n,
                Some(n) => format!("=rustudio.omnimcp.loadstring:{n}"),
                None => "=rustudio.omnimcp.loadstring".into(),
            };
            match lua
                .load(&src_s)
                .set_name(cname)
                .set_environment(exec_env_ref.clone())
                .into_function()
            {
                Ok(f) => Ok((Value::Function(f), Value::Nil)),
                Err(e) => Ok((
                    Value::Nil,
                    Value::String(lua.create_string(describe_error(&e))?),
                )),
            }
        })?;
        env.raw_set("loadstring", ls)?;
    }
    Ok(env)
}

fn json_f64(n: f64) -> JsonValue {
    if n.is_finite() {
        json!(n)
    } else if n.is_nan() {
        json!("NaN")
    } else if n.is_sign_positive() {
        json!("Infinity")
    } else {
        json!("-Infinity")
    }
}

fn lua_value_to_json(
    lua: &Lua,
    val: &Value,
    visited: &mut HashSet<usize>,
    depth: usize,
) -> JsonValue {
    match val {
        Value::Nil => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(i) => json!(*i),
        Value::Number(n) => json_f64(*n),
        Value::String(s) => match s.to_str() {
            Ok(st) => JsonValue::String(st.to_string()),
            Err(_) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
                json!({
                    "type": "BinaryString",
                    "base64": b64,
                })
            }
        },
        Value::Vector(v) => json!([v.x(), v.y(), v.z()]),
        Value::Buffer(b) => {
            let bytes = b.to_vec();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            json!({
                "type": "buffer",
                "bytes": bytes.len(),
                "base64": b64,
            })
        }
        Value::Function(f) => json!(format!("function: {:?}", f.to_pointer())),
        Value::Thread(t) => json!(format!("thread: {:?} ({:?})", t.to_pointer(), t.status())),
        Value::LightUserData(p) => json!(format!("lightuserdata: {:?}", p.0)),
        Value::Error(e) => json!({ "error": describe_error(e) }),
        Value::Table(t) => {
            if depth >= 32 {
                return json!("<max table depth>");
            }
            let ptr = t.to_pointer() as usize;
            if !visited.insert(ptr) {
                return json!("<cyclic table>");
            }
            let out = table_to_json(lua, t, visited, depth);
            visited.remove(&ptr);
            out
        }
        Value::UserData(ud) => userdata_to_json(lua, val, ud, visited, depth),
        _ => json!(format!("{val:?}")),
    }
}

fn table_to_json(
    lua: &Lua,
    t: &Table,
    visited: &mut HashSet<usize>,
    depth: usize,
) -> JsonValue {
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for pair in t.pairs::<Value, Value>() {
        match pair {
            Ok((k, v)) => entries.push((k, v)),
            Err(_) => break,
        }
    }

    if entries.is_empty() {
        return JsonValue::Object(Map::new());
    }

    // Detect clean 1-based dense array `1..=N`.
    let len = entries.len();
    let mut array_items: Vec<Option<JsonValue>> = vec![None; len];
    let mut is_array = true;
    for (k, v) in &entries {
        let idx = match k {
            Value::Integer(i) if *i >= 1 && (*i as usize) <= len => Some(*i as usize),
            Value::Number(n)
                if n.is_finite()
                    && *n >= 1.0
                    && n.fract() == 0.0
                    && (*n as usize) <= len =>
            {
                Some(*n as usize)
            }
            _ => None,
        };
        if let Some(pos) = idx {
            if array_items[pos - 1].is_some() {
                is_array = false;
                break;
            }
            array_items[pos - 1] = Some(lua_value_to_json(lua, v, visited, depth + 1));
        } else {
            is_array = false;
            break;
        }
    }
    if is_array && array_items.iter().all(Option::is_some) {
        return JsonValue::Array(array_items.into_iter().flatten().collect());
    }

    let mut map = Map::new();
    for (k, v) in entries {
        let key_str = match &k {
            Value::String(s) => s.to_string_lossy(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => b.to_string(),
            other => lua_value_to_json(lua, other, visited, depth + 1).to_string(),
        };
        map.insert(key_str, lua_value_to_json(lua, &v, visited, depth + 1));
    }
    JsonValue::Object(map)
}

fn userdata_to_json(
    lua: &Lua,
    val: &Value,
    ud: &mlua::AnyUserData,
    visited: &mut HashSet<usize>,
    depth: usize,
) -> JsonValue {
    if let Some(r) = ud
        .borrow::<LInst>()
        .ok()
        .map(|x| x.0)
        .or_else(|| ud.borrow::<LObj>().ok().map(|x| x.0))
    {
        if let Ok(w) = world(lua) {
            let d = w.d();
            let inst = d.dom.get_by_ref(r);
            return json!({
                "ref": ref_to_str(r),
                "path": instance_path(d, r),
                "class": inst.map(|i| i.class.as_str()),
                "name": inst.map(|i| i.name.as_str()),
            });
        }
        return json!({
            "ref": ref_to_str(r),
            "path": null,
            "class": null,
            "name": null,
        });
    }

    if let Ok(v) = ud.borrow::<LVector3>() {
        return json!([v.0.x, v.0.y, v.0.z]);
    }
    if let Ok(v) = ud.borrow::<LVector2>() {
        return json!([v.0.x, v.0.y]);
    }
    if let Ok(cf) = ud.borrow::<LCFrame>() {
        return variant_to_json(&Variant::CFrame(cf.0.to_cframe()));
    }
    if let Ok(c) = ud.borrow::<LColor3>() {
        return json!([c.0[0], c.0[1], c.0[2]]);
    }
    if let Ok(bc) = ud.borrow::<LBrickColor>() {
        return variant_to_json(&Variant::BrickColor(bc.0));
    }
    if let Ok(u) = ud.borrow::<LUDim>() {
        return json!([u.scale, u.offset]);
    }
    if let Ok(u) = ud.borrow::<LUDim2>() {
        return json!([[u.x.scale, u.x.offset], [u.y.scale, u.y.offset]]);
    }
    if let Ok(e) = ud.borrow::<LEnumItem>() {
        return json!({
            "enum": e.enum_type,
            "name": e.name,
            "value": e.value,
        });
    }
    if let Ok(e) = ud.borrow::<LEnum>() {
        return json!({ "enum": e.0 });
    }
    if let Ok(ray) = ud.borrow::<LRay>() {
        return json!({
            "origin": [ray.origin.x, ray.origin.y, ray.origin.z],
            "direction": [ray.direction.x, ray.direction.y, ray.direction.z],
        });
    }
    if let Ok(rr) = ud.borrow::<LRaycastResult>() {
        return json!({
            "instance": lua_value_to_json(lua, &rr.instance, visited, depth + 1),
            "name": rr.name,
            "position": [rr.position.x, rr.position.y, rr.position.z],
            "normal": [rr.normal.x, rr.normal.y, rr.normal.z],
            "distance": rr.distance,
            "material": rr.material.as_ref().map(|m| m.name.as_str()),
        });
    }
    if let Ok(dt) = ud.borrow::<LDateTime>() {
        return json!({
            "type": "DateTime",
            "unix_timestamp_millis": dt.millis,
        });
    }
    if let Ok(ti) = ud.borrow::<LTweenInfo>() {
        return json!({
            "type": "TweenInfo",
            "time": ti.time,
            "easing_style": ti.easing_style.name,
            "easing_direction": ti.easing_direction.name,
            "repeat_count": ti.repeat_count,
            "reverses": ti.reverses,
            "delay_time": ti.delay_time,
        });
    }
    if let Some(variant) = datatype_to_variant(val) {
        return variant_to_json(&variant);
    }

    match ud.to_string() {
        Ok(s) => JsonValue::String(s),
        Err(_) => json!("<userdata>"),
    }
}

fn json_arg_to_lua(lua: &Lua, doc: &Document, val: &JsonValue) -> Result<Value, String> {
    match val {
        JsonValue::Null => Ok(Value::Nil),
        JsonValue::Bool(b) => Ok(Value::Boolean(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else {
                Ok(Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        JsonValue::String(s) => lua
            .create_string(s)
            .map(Value::String)
            .map_err(|e| e.to_string()),
        JsonValue::Array(arr) => {
            let t = lua.create_table().map_err(|e| e.to_string())?;
            for (i, item) in arr.iter().enumerate() {
                t.raw_set(i + 1, json_arg_to_lua(lua, doc, item)?)
                    .map_err(|e| e.to_string())?;
            }
            Ok(Value::Table(t))
        }
        JsonValue::Object(map) => {
            // Explicit Instance reference: {"type": "Instance", "value": ...} or {"ref": "..."}
            if map.get("type").and_then(|v| v.as_str()) == Some("Instance") {
                let inner = map
                    .get("value")
                    .ok_or_else(|| "Instance object requires 'value'".to_string())?;
                let r = resolve_target(doc, inner)?;
                return inst_value(lua, r).map_err(|e| describe_error(&e));
            }
            if map.contains_key("ref") {
                let r = resolve_target(doc, val)?;
                return inst_value(lua, r).map_err(|e| describe_error(&e));
            }
            if map.contains_key("type") && map.contains_key("value") {
                let variant = json_to_variant(Some(doc), None, None, val, None)?;
                return variant_to_lua(lua, &variant, None).map_err(|e| describe_error(&e));
            }
            if let Some(en) = map.get("enum").and_then(|v| v.as_str()) {
                let item = if let Some(name) = map.get("name").and_then(|v| v.as_str()) {
                    LEnumItem::from_name(en, name)
                        .ok_or_else(|| format!("unknown Enum.{en}.{name}"))?
                } else if let Some(num) = map.get("value").and_then(|v| v.as_u64()) {
                    LEnumItem::from_value(en, num as u32)
                        .ok_or_else(|| format!("unknown Enum.{en} value {num}"))?
                } else {
                    return Err("EnumItem object requires 'name' or 'value'".into());
                };
                return enum_value(lua, item).map_err(|e| describe_error(&e));
            }
            if map.contains_key("pos")
                && (map.contains_key("look_at")
                    || map.contains_key("lookAt")
                    || map.contains_key("angles_deg")
                    || map.contains_key("anglesDeg")
                    || (map.contains_key("right")
                        && map.contains_key("up")
                        && map.contains_key("back")))
            {
                let variant = json_to_variant(
                    Some(doc),
                    None,
                    None,
                    val,
                    Some(rbx_dom_weak::types::VariantType::CFrame),
                )?;
                return variant_to_lua(lua, &variant, None).map_err(|e| describe_error(&e));
            }

            let t = lua.create_table().map_err(|e| e.to_string())?;
            for (k, v) in map {
                t.raw_set(k.as_str(), json_arg_to_lua(lua, doc, v)?)
                    .map_err(|e| e.to_string())?;
            }
            Ok(Value::Table(t))
        }
    }
}

fn capture_world_output(lua: &Lua, start_index: usize) -> Vec<JsonValue> {
    world(lua)
        .map(|w| {
            w.output
                .iter()
                .skip(start_index)
                .map(|(level, text)| {
                    json!({
                        "level": format!("{level:?}"),
                        "text": text.replace(crate::output_panel::STACK_MARK, ""),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn luau_execute(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &JsonValue) -> ToolCallResult {
    let Some(code) = args.get("code").and_then(|v| v.as_str()) else {
        return ToolCallResult::error("luau_execute requires 'code' string");
    };
    let identity_raw = args
        .get("identity")
        .and_then(|v| v.as_u64())
        .unwrap_or(8);
    if !matches!(identity_raw, 1 | 2 | 4 | 5 | 6 | 8) {
        return ToolCallResult::error(format!(
            "unsupported security identity {identity_raw}; expected 2, 4, 6, or 8"
        ));
    }
    let identity = identity_raw as u8;
    let custom_name = args.get("chunk_name").and_then(|v| v.as_str());

    let vm = match resolve_target_vm(ed, args.get("datamodel").and_then(|v| v.as_str())) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };

    let chunk_name = match (identity, custom_name) {
        (6 | 8, None) => "=rustudio.omnimcp".to_string(),
        (6 | 8, Some(n)) => {
            if n.starts_with("=rustudio.") {
                n.to_string()
            } else {
                format!("=rustudio.omnimcp:{n}")
            }
        }
        (_, Some(n)) => {
            if n.starts_with('=') || n.starts_with('@') {
                n.to_string()
            } else {
                format!("={n}")
            }
        }
        (2, None) => "=omnimcp.game".to_string(),
        (4, None) => "=omnimcp.core".to_string(),
        (_, None) => "=omnimcp".to_string(),
    };

    let code_owned = code.to_string();
    let dm_label = vm.label();

    let exec_res = with_target_runtime(ed, vm, |rt| {
        let lua = &rt.lua;
        let t0 = Instant::now();
        let out_start = world(lua).map(|w| w.output.len()).unwrap_or(0);

        let compiled = security::with_main_identity(lua, identity, || -> mlua::Result<Function> {
            let env = build_exec_env(lua, identity)?;
            lua.load(&code_owned)
                .set_name(&chunk_name)
                .set_environment(env)
                .into_function()
        });

        let func = match compiled {
            Ok(f) => f,
            Err(e) => {
                let duration_us = t0.elapsed().as_micros() as u64;
                let mut res = ToolCallResult::json(&json!({
                    "ok": false,
                    "phase": "compile",
                    "datamodel": dm_label,
                    "identity": identity,
                    "error": describe_error(&e),
                    "traceback": JsonValue::Null,
                    "output": [],
                    "duration_us": duration_us,
                }));
                res.is_error = true;
                return res;
            }
        };

        let run_res: Result<(Vec<JsonValue>, bool), (String, Option<String>)> =
            security::with_main_identity(lua, identity, || {
                let th = crate::runtime::domains::assets_cloud::scene::create_thread(lua, func)
                    .map_err(|e| (describe_error(&e), None))?;
                security::set(&th, identity);
                match th.resume::<MultiValue>(()) {
                    Ok(vals) => {
                        let yielded = th.status() == ThreadStatus::Resumable;
                        let mut visited = HashSet::new();
                        let converted: Vec<JsonValue> = vals
                            .iter()
                            .map(|v| lua_value_to_json(lua, v, &mut visited, 0))
                            .collect();
                        Ok((converted, yielded))
                    }
                    Err(e) => {
                        let tb = lua
                            .globals()
                            .get::<Table>("debug")
                            .and_then(|d| d.get::<Function>("traceback"))
                            .and_then(|f| f.call::<String>(th.clone()))
                            .ok();
                        Err((describe_error(&e), tb))
                    }
                }
            });

        rt.dispatch();
        let duration_us = t0.elapsed().as_micros() as u64;
        let output = capture_world_output(lua, out_start);

        match run_res {
            Ok((results, yielded)) => {
                let first = results.first().cloned().unwrap_or(JsonValue::Null);
                ToolCallResult::json(&json!({
                    "ok": true,
                    "datamodel": dm_label,
                    "identity": identity,
                    "yielded": yielded,
                    "result": first,
                    "results": results,
                    "output": output,
                    "duration_us": duration_us,
                }))
            }
            Err((err_msg, traceback)) => {
                let mut res = ToolCallResult::json(&json!({
                    "ok": false,
                    "phase": "runtime",
                    "datamodel": dm_label,
                    "identity": identity,
                    "error": err_msg,
                    "traceback": traceback,
                    "output": output,
                    "duration_us": duration_us,
                }));
                res.is_error = true;
                res
            }
        }
    });

    ctx.request_repaint();
    match exec_res {
        Ok(res) => res,
        Err(e) => ToolCallResult::error(e),
    }
}

fn luau_fire_signal(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &JsonValue,
) -> ToolCallResult {
    let Some(target_val) = args.get("target") else {
        return ToolCallResult::error("luau_fire_signal requires 'target'");
    };
    let Some(signal_name) = args
        .get("signal")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return ToolCallResult::error("luau_fire_signal requires non-empty 'signal' string");
    };
    let arg_items = args
        .get("args")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let vm = match resolve_target_vm(ed, args.get("datamodel").and_then(|v| v.as_str())) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };
    let dm_label = vm.label();
    let target_cloned = target_val.clone();
    let signal_owned = signal_name.to_string();

    let res = with_target_runtime(ed, vm, |rt| -> Result<ToolCallResult, String> {
        let lua = &rt.lua;
        let (target_ref, target_path, target_class, target_name) = {
            let w = world(lua).map_err(|e| describe_error(&e))?;
            let d = w.d();
            let r = resolve_target(d, &target_cloned)?;
            let inst = d
                .dom
                .get_by_ref(r)
                .ok_or_else(|| format!("target instance {} vanished", ref_to_str(r)))?;
            (
                r,
                instance_path(d, r),
                inst.class.as_str().to_string(),
                inst.name.clone(),
            )
        };

        let lua_args: Vec<Value> = {
            let w = world(lua).map_err(|e| describe_error(&e))?;
            let d = w.d();
            let mut converted = Vec::with_capacity(arg_items.len());
            for item in &arg_items {
                converted.push(json_arg_to_lua(lua, d, item)?);
            }
            converted
        };

        let out_start = world(lua).map(|w| w.output.len()).unwrap_or(0);
        let had_listeners = world(lua)
            .map(|w| w.listening.contains(&target_ref))
            .unwrap_or(false);

        rt.fire(target_ref, &signal_owned, lua_args);
        rt.dispatch();

        let output = capture_world_output(lua, out_start);
        Ok(ToolCallResult::json(&json!({
            "fired": true,
            "datamodel": dm_label,
            "target": {
                "ref": ref_to_str(target_ref),
                "path": target_path,
                "class": target_class,
                "name": target_name,
            },
            "signal": signal_owned,
            "args_count": arg_items.len(),
            "had_listeners": had_listeners,
            "output": output,
        })))
    });

    ctx.request_repaint();
    match res {
        Ok(Ok(r)) => r,
        Ok(Err(e)) | Err(e) => ToolCallResult::error(e),
    }
}

fn parse_mem_stats(raw: &str) -> JsonValue {
    let mut map = Map::new();
    for tok in raw.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if let Ok(n) = v.parse::<u64>() {
                map.insert(k.to_string(), json!(n));
            } else {
                map.insert(k.to_string(), json!(v));
            }
        }
    }
    JsonValue::Object(map)
}

fn luau_vm_stats(ed: &mut crate::app::Editor, args: &JsonValue) -> ToolCallResult {
    let selector = args
        .get("datamodel")
        .and_then(|v| v.as_str())
        .unwrap_or("All")
        .trim()
        .to_ascii_lowercase();

    let want_edit = matches!(selector.as_str(), "all" | "edit");
    let want_server = matches!(selector.as_str(), "all" | "server");
    let want_client = matches!(selector.as_str(), "all" | "client");
    if !want_edit && !want_server && !want_client {
        return ToolCallResult::error(format!(
            "unknown datamodel '{selector}'; expected All, Edit, Server, or Client"
        ));
    }

    let mut vms = Map::new();

    if want_edit {
        if let Some(lua) = ed.plugins.edit_lua() {
            let mem = lua.used_memory();
            let edit_doc = ed.play_saved_doc().unwrap_or(&ed.doc);
            let tree = edit_doc.dom.descendants().count();
            vms.insert(
                "edit".into(),
                json!({
                    "initialized": true,
                    "source": "plugin_host",
                    "used_memory_bytes": mem,
                    "used_memory_kb": mem / 1024,
                    "tree_instances": tree,
                    "loaded_plugins": ed.plugins.loaded().len(),
                }),
            );
        } else {
            let fb = FALLBACK_EDIT_VM.with(|cell| {
                let borrow = cell.borrow();
                borrow.as_ref().map(|vm| {
                    let slots = vm.rt.lua_slots();
                    let mem = vm.rt.lua.used_memory();
                    let orphans = vm.world.as_ref().map(|w| w.orphans.len()).unwrap_or(0);
                    let freed = vm.world.as_ref().map(|w| w.freed).unwrap_or(0);
                    json!({
                        "initialized": true,
                        "source": "omnimcp_fallback",
                        "doc_id": vm.doc_id,
                        "lua_slots": slots,
                        "used_memory_bytes": mem,
                        "used_memory_kb": mem / 1024,
                        "gc_counts": {
                            "orphans": orphans,
                            "freed": freed,
                            "lua_slots": slots,
                        },
                    })
                })
            });
            vms.insert(
                "edit".into(),
                fb.unwrap_or_else(|| json!({ "initialized": false })),
            );
        }
    }

    if want_server {
        if let Some((orphans, freed, gc_slots)) = ed.play.as_ref().map(|p| p.gc_counts()) {
            let (slots, mem_str, used_bytes) = ed
                .with_play_side(Side::Server, |rt, _, _| {
                    (rt.lua_slots(), rt.mem_stats(), rt.lua.used_memory())
                })
                .unwrap_or((gc_slots, String::new(), 0));
            vms.insert(
                "server".into(),
                json!({
                    "initialized": true,
                    "lua_slots": slots,
                    "used_memory_bytes": used_bytes,
                    "used_memory_kb": used_bytes / 1024,
                    "mem_stats": mem_str,
                    "parsed": parse_mem_stats(&mem_str),
                    "gc_counts": {
                        "orphans": orphans,
                        "freed": freed,
                        "lua_slots": gc_slots,
                    },
                }),
            );
        } else {
            vms.insert("server".into(), json!({ "initialized": false }));
        }
    }

    if want_client {
        let has_client = ed
            .play
            .as_ref()
            .is_some_and(|p| p.mode == RunMode::Play && ed.play_side_lua(Side::Client).is_some());
        if has_client {
            let (slots, mem_str, used_bytes, orphans, freed) = ed
                .with_play_side(Side::Client, |rt, _, _| {
                    let (o, fr) = world(&rt.lua)
                        .map(|w| (w.orphans.len(), w.freed))
                        .unwrap_or_default();
                    (rt.lua_slots(), rt.mem_stats(), rt.lua.used_memory(), o, fr)
                })
                .unwrap_or_default();
            vms.insert(
                "client".into(),
                json!({
                    "initialized": true,
                    "lua_slots": slots,
                    "used_memory_bytes": used_bytes,
                    "used_memory_kb": used_bytes / 1024,
                    "mem_stats": mem_str,
                    "parsed": parse_mem_stats(&mem_str),
                    "gc_counts": {
                        "orphans": orphans,
                        "freed": freed,
                        "lua_slots": slots,
                    },
                }),
            );
        } else {
            vms.insert("client".into(), json!({ "initialized": false }));
        }
    }

    ToolCallResult::json(&json!({
        "sim_clock": {
            "enabled": crate::sim_clock::on(),
            "dt": crate::sim_clock::dt(),
            "seconds": crate::sim_clock::seconds(),
            "epoch_secs": crate::sim_clock::epoch_secs(),
        },
        "vms": vms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn schemas_cover_all_luau_tools() {
        let names: Vec<String> = schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["luau_execute", "luau_fire_signal", "luau_vm_stats"]);
    }

    #[test]
    fn converts_all_luau_return_types_with_cycle_guard() {
        let mut doc = Document::new_place();
        let ws = doc.workspace().unwrap();
        let part = doc.insert(ws, InstanceBuilder::new("Part").with_name("TestPart"));

        let rt = Runtime::new().unwrap();
        rt.install_side().unwrap();
        let mut w = World::new(doc, RunMode::Edit);
        w.side = Side::Server;
        rt.enter(w);
        rt.bind_globals().unwrap();

        let code = r#"
            local cyc = { a = 1 }
            cyc.self = cyc
            local arr = { 10, 20, 30 }
            local p = workspace:FindFirstChild("TestPart")
            return nil, true, 42, 3.5, "hello", Vector3.new(1, 2, 3),
                CFrame.new(4, 5, 6), Color3.new(1, 0.5, 0), UDim2.new(0.5, 10, 1, -4),
                Enum.Material.Neon, p, arr, cyc
        "#;
        let lua = &rt.lua;
        let out: MultiValue = security::with_main_identity(lua, 8, || {
            let env = build_exec_env(lua, 8).unwrap();
            lua.load(code)
                .set_name("=rustudio.omnimcp")
                .set_environment(env)
                .call(())
                .unwrap()
        });
        let mut visited = HashSet::new();
        let vals: Vec<JsonValue> = out
            .iter()
            .map(|v| lua_value_to_json(lua, v, &mut visited, 0))
            .collect();

        assert_eq!(vals[0], JsonValue::Null);
        assert_eq!(vals[1], json!(true));
        assert_eq!(vals[2], json!(42));
        assert_eq!(vals[3], json!(3.5));
        assert_eq!(vals[4], json!("hello"));
        assert_eq!(vals[5], json!([1.0, 2.0, 3.0]));
        assert_eq!(vals[6]["pos"], json!([4.0, 5.0, 6.0]));
        assert_eq!(vals[7], json!([1.0, 0.5, 0.0]));
        assert_eq!(vals[8], json!([[0.5, 10], [1.0, -4]]));
        assert_eq!(vals[9]["name"], json!("Neon"));
        assert_eq!(vals[10]["ref"], json!(ref_to_str(part)));
        assert_eq!(vals[10]["path"], json!("Workspace.TestPart"));
        assert_eq!(vals[10]["class"], json!("Part"));
        assert_eq!(vals[10]["name"], json!("TestPart"));
        assert_eq!(vals[11], json!([10, 20, 30]));
        assert_eq!(vals[12]["a"], json!(1));
        assert_eq!(vals[12]["self"], json!("<cyclic table>"));

        let _ = rt.leave();
    }

    #[test]
    fn security_identities_enforce_or_grant_capabilities() {
        let doc = Document::new_place();
        let rt = Runtime::new().unwrap();
        rt.install_side().unwrap();
        rt.enter(World::new(doc, RunMode::Edit));
        rt.bind_globals().unwrap();

        let probe = r#"
            local ok_settings = pcall(settings)
            local ok_coregui = pcall(function() return game:GetService("CoreGui") ~= nil end)
            local ok_alias = pcall(function() return workspace.SignalBehaviorAlias end)
            return ok_settings, ok_coregui, ok_alias
        "#;

        let lua = &rt.lua;
        let res2: (bool, bool, bool) = security::with_main_identity(lua, 2, || {
            let env = build_exec_env(lua, 2).unwrap();
            let f = lua.load(probe).set_name("=omnimcp.game").set_environment(env).into_function().unwrap();
            let th = crate::runtime::domains::assets_cloud::scene::create_thread(lua, f).unwrap();
            security::set(&th, 2);
            th.resume(()).unwrap()
        });
        assert!(!res2.0, "identity 2 must not access settings()");
        assert!(!res2.2, "identity 2 must not access NotScriptable SignalBehaviorAlias");

        let res8: (bool, bool, bool) = security::with_main_identity(lua, 8, || {
            let env = build_exec_env(lua, 8).unwrap();
            let f = lua.load(probe).set_name("=rustudio.omnimcp").set_environment(env).into_function().unwrap();
            let th = crate::runtime::domains::assets_cloud::scene::create_thread(lua, f).unwrap();
            security::set(&th, 8);
            th.resume(()).unwrap()
        });
        assert!(res8.0, "identity 8 must access settings()");
        assert!(res8.1, "identity 8 must access CoreGui");
        assert!(res8.2, "identity 8 must access NotScriptable SignalBehaviorAlias");

        let _ = rt.leave();
    }

    #[test]
    fn json_arg_to_lua_converts_instances_vectors_and_fires_signal() {
        let mut doc = Document::new_place();
        let ws = doc.workspace().unwrap();
        let be = doc.insert(ws, InstanceBuilder::new("BindableEvent").with_name("Ping"));

        let rt = Runtime::new().unwrap();
        rt.install_side().unwrap();
        rt.enter(World::new(doc, RunMode::Edit));
        rt.bind_globals().unwrap();

        let lua = &rt.lua;
        security::with_main_identity(lua, 8, || {
            let env = build_exec_env(lua, 8).unwrap();
            lua.load(r#"
                _G.got = nil
                workspace.Ping.Event:Connect(function(msg, vec, obj)
                    _G.got = { msg, vec.X + vec.Y + vec.Z, obj.Name }
                end)
            "#)
            .set_name("=rustudio.omnimcp")
            .set_environment(env)
            .exec()
            .unwrap();
        });

        let args_lua = {
            let w = world(lua).unwrap();
            let d = w.d();
            vec![
                json_arg_to_lua(lua, d, &json!("hi")).unwrap(),
                json_arg_to_lua(lua, d, &json!({"type": "Vector3", "value": [1, 2, 3]})).unwrap(),
                json_arg_to_lua(lua, d, &json!({"ref": ref_to_str(be)})).unwrap(),
            ]
        };
        rt.fire(be, "Event", args_lua);
        rt.dispatch();

        let got: (String, f64, String) = security::with_main_identity(lua, 8, || {
            let env = build_exec_env(lua, 8).unwrap();
            lua.load("return _G.got[1], _G.got[2], _G.got[3]")
                .set_name("=rustudio.omnimcp")
                .set_environment(env)
                .eval()
                .unwrap()
        });
        assert_eq!(got, ("hi".to_string(), 6.0, "Ping".to_string()));
        let _ = rt.leave();
    }
}
