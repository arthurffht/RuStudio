//! Bidirectional `rbx_dom_weak::types::Variant` <-> `serde_json::Value`
//! converters, target path/ref resolution, and DataModel selectors for OmniMCP.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::str::FromStr;

use base64::Engine as _;
use glam::{EulerRot, Mat3, Vec3};
use rbx_dom_weak::types::{
    Attributes, Axes, BinaryString, BrickColor, CFrame, Color3, Color3uint8, ColorSequence,
    ColorSequenceKeypoint, Content, ContentId, ContentType, CustomPhysicalProperties, Enum,
    EnumItem, Faces, Font, FontStyle, FontWeight, MaterialColors, Matrix3, NetAssetRef,
    NumberRange, NumberSequence, NumberSequenceKeypoint, PhysicalProperties, Ray, Rect, Ref,
    Region3, Region3int16, SecurityCapabilities, SharedString, Tags, TerrainMaterials, UDim, UDim2,
    UniqueId, Variant, VariantType, Vector2, Vector2int16, Vector3, Vector3int16,
};
use serde_json::{json, Map, Value};

use crate::doc::Document;
use crate::math::Xform;

/// Formats a `Ref` as `"ref:<32-hex>"` (or `"null"` for `Ref::none()`).
pub fn ref_to_str(r: Ref) -> String {
    if r.is_none() {
        "null".into()
    } else {
        format!("ref:{r}")
    }
}

/// Parses `"ref:<32-hex>"`, raw `"<32-hex>"`, or `"null"`/`"nil"`/`"none"` into `Ref`.
pub fn str_to_ref(s: &str) -> Result<Ref, String> {
    let t = s.trim();
    if t.is_empty()
        || t.eq_ignore_ascii_case("null")
        || t.eq_ignore_ascii_case("nil")
        || t.eq_ignore_ascii_case("none")
    {
        return Ok(Ref::none());
    }
    let hex = t.strip_prefix("ref:").unwrap_or(t).trim();
    Ref::from_str(hex).map_err(|e| format!("invalid ref '{s}': {e}"))
}

/// Returns the dot-separated path of `r` from DataModel root (e.g. `"Workspace.Baseplate"`,
/// or `"game"` for the DataModel root itself).
pub fn instance_path(doc: &Document, r: Ref) -> String {
    if r.is_none() {
        return String::new();
    }
    if r == doc.dom.root_ref() {
        return "game".into();
    }
    let mut parts = Vec::new();
    let mut cur = r;
    while let Some(inst) = doc.dom.get_by_ref(cur) {
        if inst.parent().is_none() {
            break;
        }
        parts.push(inst.name.clone());
        cur = inst.parent();
    }
    parts.reverse();
    if parts.is_empty() {
        ref_to_str(r)
    } else {
        parts.join(".")
    }
}

/// Resolves an instance target from a JSON string or object:
/// - `"ref:<32-hex>"` or raw 32-char hex ID
/// - dot-path (`"Workspace.Part"`, `"game.Workspace.Part"`)
/// - slash-path (`"Workspace/Part"`, `"/Workspace/Part"`)
/// - `"game"`, `"Game"`, `"DataModel"`, `"."`, `"/"` -> `doc.dom.root_ref()`
/// - object `{"ref": "..."}` or `{"path": "..."}`
pub fn resolve_target(doc: &Document, target: &Value) -> Result<Ref, String> {
    let raw = match target {
        Value::String(s) => s.as_str(),
        Value::Object(map) => {
            if let Some(s) = map.get("ref").and_then(|v| v.as_str()) {
                s
            } else if let Some(s) = map.get("path").and_then(|v| v.as_str()) {
                s
            } else {
                return Err("target object must contain 'ref' or 'path'".into());
            }
        }
        _ => return Err("target must be a path string, 'ref:<id>', or {ref|path}".into()),
    };
    resolve_target_str(doc, raw)
}

/// String-based target resolution (`ref:<id>`, dot-path, or slash-path).
pub fn resolve_target_str(doc: &Document, raw: &str) -> Result<Ref, String> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix("ref:") {
        let r = str_to_ref(hex)?;
        if r.is_some() && doc.exists(r) {
            return Ok(r);
        }
        return Err(format!("instance '{raw}' not found in DataModel"));
    }
    if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(r) = Ref::from_str(s) {
            if r.is_some() && doc.exists(r) {
                return Ok(r);
            }
        }
    }
    if matches!(s, "" | "." | "/" | "game" | "Game" | "DataModel") {
        return Ok(doc.dom.root_ref());
    }

    let trimmed = s
        .strip_prefix("game.")
        .or_else(|| s.strip_prefix("Game."))
        .or_else(|| s.strip_prefix("DataModel."))
        .or_else(|| s.strip_prefix("game/"))
        .or_else(|| s.strip_prefix("Game/"))
        .or_else(|| s.strip_prefix("DataModel/"))
        .or_else(|| s.strip_prefix('/'))
        .or_else(|| s.strip_prefix('.'))
        .unwrap_or(s);

    if trimmed.is_empty() {
        return Ok(doc.dom.root_ref());
    }

    let sep = if trimmed.contains('/') { '/' } else { '.' };
    let segments: Vec<&str> = trimmed.split(sep).filter(|p| !p.is_empty()).collect();
    if segments.is_empty() {
        return Ok(doc.dom.root_ref());
    }

    let mut cur = doc.dom.root_ref();
    for (idx, seg) in segments.iter().enumerate() {
        let inst = doc
            .dom
            .get_by_ref(cur)
            .ok_or_else(|| format!("instance vanished along path '{raw}'"))?;
        let found = inst
            .children()
            .iter()
            .copied()
            .find(|c| doc.dom.get_by_ref(*c).is_some_and(|ch| ch.name == *seg))
            .or_else(|| {
                // Allow lowercase `"workspace"` at top level, or service lookup by class name.
                if idx == 0 {
                    if seg.eq_ignore_ascii_case("workspace") {
                        doc.workspace()
                    } else {
                        doc.find_child(cur, seg)
                    }
                } else {
                    None
                }
            });
        match found {
            Some(next) => cur = next,
            None => {
                return Err(format!(
                    "instance not found: '{raw}' (missing segment '{seg}' under '{}')",
                    instance_path(doc, cur)
                ));
            }
        }
    }
    Ok(cur)
}

/// Resolves a read-only DataModel by selector: `"Edit"`, `"Server"`, `"Client"`, or `"Active"` (default).
pub fn resolve_datamodel<'a>(
    ed: &'a crate::app::Editor,
    name: Option<&str>,
) -> Result<&'a Document, String> {
    let key = name.unwrap_or("active").trim().to_ascii_lowercase();
    match key.as_str() {
        "" | "active" => Ok(&ed.doc),
        "edit" => match ed.play.as_ref() {
            None => Ok(&ed.doc),
            Some(_) => ed
                .play_saved_doc()
                .ok_or_else(|| "Edit DataModel is unavailable".into()),
        },
        "server" => {
            let p = ed
                .play
                .as_ref()
                .ok_or_else(|| "Server DataModel requires an active Play or Run session".to_string())?;
            if p.showing == crate::runtime::Side::Server {
                Ok(&ed.doc)
            } else {
                ed.play_other_doc()
                    .ok_or_else(|| "Server DataModel is unavailable".into())
            }
        }
        "client" => {
            let p = ed
                .play
                .as_ref()
                .ok_or_else(|| "Client DataModel requires an active Play session".to_string())?;
            if p.mode == crate::runtime::RunMode::Run {
                return Err("Run mode has no Client DataModel (use Play mode)".into());
            }
            if p.showing == crate::runtime::Side::Client {
                Ok(&ed.doc)
            } else {
                ed.play_other_doc()
                    .ok_or_else(|| "Client DataModel is unavailable".into())
            }
        }
        other => Err(format!(
            "unknown datamodel '{other}'; expected Edit, Server, Client, or Active"
        )),
    }
}

/// Resolves a mutable DataModel by selector: `"Edit"` (even during Play via
/// `ed.play_saved_doc_mut()`), `"Server"`, `"Client"`, or `"Active"` (default).
pub fn resolve_datamodel_mut<'a>(
    ed: &'a mut crate::app::Editor,
    name: Option<&str>,
) -> Result<&'a mut Document, String> {
    let key = name.unwrap_or("active").trim().to_ascii_lowercase();
    match key.as_str() {
        "" | "active" => Ok(&mut ed.doc),
        "edit" => {
            if ed.play.is_none() {
                Ok(&mut ed.doc)
            } else {
                ed.play_saved_doc_mut()
                    .ok_or_else(|| "Edit DataModel is unavailable".into())
            }
        }
        "server" => {
            let showing = ed
                .play
                .as_ref()
                .map(|p| p.showing)
                .ok_or_else(|| "Server DataModel requires an active Play or Run session".to_string())?;
            if showing == crate::runtime::Side::Server {
                Ok(&mut ed.doc)
            } else {
                ed.play_other_doc_mut()
                    .ok_or_else(|| "Server DataModel is unavailable".into())
            }
        }
        "client" => {
            let (showing, mode) = ed
                .play
                .as_ref()
                .map(|p| (p.showing, p.mode))
                .ok_or_else(|| "Client DataModel requires an active Play session".to_string())?;
            if mode == crate::runtime::RunMode::Run {
                return Err("Run mode has no Client DataModel (use Play mode)".into());
            }
            if showing == crate::runtime::Side::Client {
                Ok(&mut ed.doc)
            } else {
                ed.play_other_doc_mut()
                    .ok_or_else(|| "Client DataModel is unavailable".into())
            }
        }
        other => Err(format!(
            "unknown datamodel '{other}'; expected Edit, Server, Client, or Active"
        )),
    }
}

/// Looks up the expected `(VariantType, Option<enum_name>)` for `class.prop`
/// in the reflection database.
pub fn expected_variant_type(class: &str, prop: &str) -> Option<(VariantType, Option<String>)> {
    let db = crate::doc::db();
    let desc = db.classes.get(class)?;
    db.superclasses_iter(desc).find_map(|c| {
        let p = c.properties.get(prop)?;
        match &p.data_type {
            rbx_reflection::DataType::Value(vt) => Some((*vt, None)),
            rbx_reflection::DataType::Enum(e) => Some((VariantType::Enum, Some((*e).to_string()))),
            _ => None,
        }
    })
}

/// Resolves an enum item name (or `"Enum.Type.Item"` / `"Type.Item"`) to its numeric value.
pub fn enum_name_to_u32(enum_name: Option<&str>, item: &str) -> Option<u32> {
    let trimmed = item.trim();
    if let Ok(n) = trimmed.parse::<u32>() {
        return Some(n);
    }
    let parts: Vec<&str> = trimmed.split('.').collect();
    let (inferred_enum, item_name) = match parts.as_slice() {
        ["Enum", e, i] => (Some(*e), *i),
        [e, i] => (Some(*e), *i),
        [i] => (enum_name, *i),
        _ => (enum_name, trimmed),
    };
    let ename = inferred_enum.or(enum_name)?;

    let db = crate::doc::db();
    if let Some(ed) = db.enums.get(ename) {
        if let Some(&v) = ed.items.get(item_name) {
            return Some(v);
        }
        for (k, &v) in &ed.items {
            if k.eq_ignore_ascii_case(item_name) {
                return Some(v);
            }
        }
    }
    if let Some(api) = crate::api::get() {
        if let Some(items) = api.enums.get(ename) {
            if let Some((_, v)) = items.iter().find(|(k, _)| k.eq_ignore_ascii_case(item_name)) {
                return Some(*v);
            }
        }
        if let Some(modern) = api.legacy_enum_items.get(&(ename.to_string(), item_name.to_string())) {
            if let Some(ed) = db.enums.get(ename) {
                if let Some(&v) = ed.items.get(modern.as_str()) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Resolves an enum numeric value to its item name using `doc::db()` and `api::get()`.
pub fn enum_u32_to_name(enum_name: &str, value: u32) -> Option<String> {
    if let Some(ed) = crate::doc::db().enums.get(enum_name) {
        if let Some((k, _)) = ed.items.iter().find(|(_, v)| **v == value) {
            return Some((*k).to_string());
        }
    }
    if let Some(api) = crate::api::get() {
        if let Some(items) = api.enums.get(enum_name) {
            if let Some((k, _)) = items.iter().find(|(_, v)| *v == value) {
                return Some(k.clone());
            }
        }
    }
    None
}

/// Serializes any `Variant` to JSON without document/class context.
pub fn variant_to_json(v: &Variant) -> Value {
    variant_to_json_inner(None, None, None, v)
}

/// Serializes any `Variant` to JSON with optional `Document`, `class`, and `prop`
/// context (resolving `Enum` names and `Ref` paths).
pub fn variant_to_json_with_doc(
    doc: &Document,
    class: Option<&str>,
    prop: Option<&str>,
    v: &Variant,
) -> Value {
    variant_to_json_inner(Some(doc), class, prop, v)
}

fn json_num_f32(v: f32) -> Value {
    if v.is_finite() {
        json!(v)
    } else if v.is_nan() {
        json!("NaN")
    } else if v.is_sign_positive() {
        json!("Infinity")
    } else {
        json!("-Infinity")
    }
}

fn json_num_f64(v: f64) -> Value {
    if v.is_finite() {
        json!(v)
    } else if v.is_nan() {
        json!("NaN")
    } else if v.is_sign_positive() {
        json!("Infinity")
    } else {
        json!("-Infinity")
    }
}

fn cframe_to_json(cf: &CFrame) -> Value {
    let xf = Xform::from_cframe(cf);
    let deg = xf.orientation_deg();
    let r = xf.rot.x_axis;
    let u = xf.rot.y_axis;
    let b = xf.rot.z_axis;
    json!({
        "pos": [xf.pos.x, xf.pos.y, xf.pos.z],
        "angles_deg": [deg.x, deg.y, deg.z],
        "right": [r.x, r.y, r.z],
        "up": [u.x, u.y, u.z],
        "back": [b.x, b.y, b.z],
    })
}

fn font_weight_str(w: FontWeight) -> &'static str {
    match w {
        FontWeight::Thin => "Thin",
        FontWeight::ExtraLight => "ExtraLight",
        FontWeight::Light => "Light",
        FontWeight::Regular => "Regular",
        FontWeight::Medium => "Medium",
        FontWeight::SemiBold => "SemiBold",
        FontWeight::Bold => "Bold",
        FontWeight::ExtraBold => "ExtraBold",
        FontWeight::Heavy => "Heavy",
        _ => "Regular",
    }
}

fn font_style_str(s: FontStyle) -> &'static str {
    match s {
        FontStyle::Normal => "Normal",
        FontStyle::Italic => "Italic",
        _ => "Normal",
    }
}

fn variant_to_json_inner(
    doc: Option<&Document>,
    class: Option<&str>,
    prop: Option<&str>,
    v: &Variant,
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    match v {
        Variant::Bool(b) => Value::Bool(*b),
        Variant::Int32(n) => json!(n),
        Variant::Int64(n) => json!(n),
        Variant::Float32(n) => json_num_f32(*n),
        Variant::Float64(n) => json_num_f64(*n),
        Variant::String(s) => Value::String(s.clone()),
        Variant::BinaryString(bs) => json!({
            "type": "BinaryString",
            "base64": b64.encode(bs.as_ref() as &[u8]),
        }),
        Variant::SharedString(ss) => json!({
            "type": "SharedString",
            "base64": b64.encode(ss.data()),
            "hash": ss.hash().to_string(),
        }),
        Variant::NetAssetRef(nar) => json!({
            "type": "NetAssetRef",
            "base64": b64.encode(nar.data()),
            "hash": nar.hash().to_string(),
        }),
        Variant::ContentId(c) => Value::String(c.as_str().to_string()),
        Variant::Content(c) => match c.value() {
            ContentType::None => Value::Null,
            ContentType::Uri(u) => Value::String(u.clone()),
            ContentType::Object(r) => {
                let mut obj = Map::new();
                obj.insert("object".into(), Value::String(ref_to_str(*r)));
                if let Some(d) = doc {
                    obj.insert("path".into(), Value::String(instance_path(d, *r)));
                }
                Value::Object(obj)
            }
            _ => Value::Null,
        },
        Variant::Vector2(v2) => json!([v2.x, v2.y]),
        Variant::Vector2int16(v2) => json!([v2.x, v2.y]),
        Variant::Vector3(v3) => json!([v3.x, v3.y, v3.z]),
        Variant::Vector3int16(v3) => json!([v3.x, v3.y, v3.z]),
        Variant::CFrame(cf) => cframe_to_json(cf),
        Variant::OptionalCFrame(opt) => match opt {
            Some(cf) => cframe_to_json(cf),
            None => Value::Null,
        },
        Variant::Color3(c) => json!([c.r, c.g, c.b]),
        Variant::Color3uint8(c) => json!([c.r, c.g, c.b]),
        Variant::BrickColor(bc) => json!({
            "name": bc.to_string(),
            "number": *bc as u16,
        }),
        Variant::UDim(u) => json!([u.scale, u.offset]),
        Variant::UDim2(u) => json!([[u.x.scale, u.x.offset], [u.y.scale, u.y.offset]]),
        Variant::Rect(r) => json!([[r.min.x, r.min.y], [r.max.x, r.max.y]]),
        Variant::Region3(r) => json!([[r.min.x, r.min.y, r.min.z], [r.max.x, r.max.y, r.max.z]]),
        Variant::Region3int16(r) => {
            json!([[r.min.x, r.min.y, r.min.z], [r.max.x, r.max.y, r.max.z]])
        }
        Variant::Ray(ray) => json!({
            "origin": [ray.origin.x, ray.origin.y, ray.origin.z],
            "direction": [ray.direction.x, ray.direction.y, ray.direction.z],
        }),
        Variant::NumberRange(nr) => json!([nr.min, nr.max]),
        Variant::NumberSequence(ns) => Value::Array(
            ns.keypoints
                .iter()
                .map(|kp| json!({"time": kp.time, "value": kp.value, "envelope": kp.envelope}))
                .collect(),
        ),
        Variant::ColorSequence(cs) => Value::Array(
            cs.keypoints
                .iter()
                .map(|kp| json!({"time": kp.time, "color": [kp.color.r, kp.color.g, kp.color.b]}))
                .collect(),
        ),
        Variant::PhysicalProperties(pp) => match pp {
            PhysicalProperties::Default => json!("Default"),
            PhysicalProperties::Custom(c) => json!({
                "density": c.density(),
                "friction": c.friction(),
                "elasticity": c.elasticity(),
                "frictionWeight": c.friction_weight(),
                "elasticityWeight": c.elasticity_weight(),
                "acousticAbsorption": c.acoustic_absorption(),
            }),
        },
        Variant::Enum(e) => {
            let enum_name = class
                .zip(prop)
                .and_then(|(c, p)| expected_variant_type(c, p))
                .and_then(|(_, en)| en);
            if let Some(en) = enum_name {
                let item = enum_u32_to_name(&en, e.to_u32());
                json!({
                    "enum": en,
                    "name": item,
                    "value": e.to_u32(),
                })
            } else {
                json!(e.to_u32())
            }
        }
        Variant::EnumItem(ei) => {
            let item = enum_u32_to_name(&ei.ty, ei.value);
            json!({
                "enum": ei.ty,
                "name": item,
                "value": ei.value,
            })
        }
        Variant::Font(f) => json!({
            "family": f.family,
            "weight": font_weight_str(f.weight),
            "style": font_style_str(f.style),
            "cachedFaceId": f.cached_face_id,
        }),
        Variant::UniqueId(u) => Value::String(u.to_string()),
        Variant::SecurityCapabilities(sc) => json!(sc.bits()),
        Variant::Attributes(attrs) => {
            let mut map = Map::new();
            for (k, val) in attrs.iter() {
                map.insert(k.clone(), variant_to_json_inner(doc, None, None, val));
            }
            Value::Object(map)
        }
        Variant::Tags(tags) => Value::Array(tags.iter().map(|t| Value::String(t.to_string())).collect()),
        Variant::MaterialColors(mc) => serde_json::to_value(mc).unwrap_or(Value::Null),
        Variant::Ref(r) => {
            if r.is_none() {
                Value::Null
            } else if let Some(d) = doc {
                json!({
                    "ref": ref_to_str(*r),
                    "path": instance_path(d, *r),
                })
            } else {
                Value::String(ref_to_str(*r))
            }
        }
        Variant::Axes(axes) => serde_json::to_value(axes).unwrap_or_else(|_| json!([])),
        Variant::Faces(faces) => serde_json::to_value(faces).unwrap_or_else(|_| json!([])),
        _ => json!(format!("{v:?}")),
    }
}

/// Converts a JSON value into a `rbx_dom_weak::types::Variant` using explicit `hint`,
/// reflection metadata `(class, prop)`, or tagged/inferred JSON structure.
pub fn json_to_variant(
    doc: Option<&Document>,
    class: Option<&str>,
    prop: Option<&str>,
    val: &Value,
    hint: Option<VariantType>,
) -> Result<Variant, String> {
    let (resolved_type, enum_name, inner_val) = resolve_type_and_value(class, prop, val, hint)?;
    convert_typed(doc, enum_name.as_deref(), resolved_type, inner_val)
}

fn resolve_type_and_value<'a>(
    class: Option<&str>,
    prop: Option<&str>,
    val: &'a Value,
    hint: Option<VariantType>,
) -> Result<(VariantType, Option<String>, &'a Value), String> {
    // Allow explicitly tagged payloads like `{"type": "Vector3", "value": [1, 2, 3]}`
    // when no fixed reflection type overrides it (e.g. Attributes).
    if let Value::Object(map) = val {
        if let (Some(ty_str), Some(inner)) = (map.get("type").and_then(|v| v.as_str()), map.get("value")) {
            if let Some(vt) = parse_variant_type_name(ty_str) {
                let ename = map.get("enum").and_then(|v| v.as_str()).map(str::to_string);
                return Ok((vt, ename, inner));
            }
        }
    }

    if let Some(vt) = hint {
        let ename = class
            .zip(prop)
            .and_then(|(c, p)| expected_variant_type(c, p))
            .and_then(|(_, en)| en);
        return Ok((vt, ename, val));
    }

    if let Some((c, p)) = class.zip(prop) {
        if let Some((vt, ename)) = expected_variant_type(c, p) {
            return Ok((vt, ename, val));
        }
        // Special Roblox properties that may not be listed in older reflection DB entries.
        match p {
            "Tags" => return Ok((VariantType::Tags, None, val)),
            "Attributes" | "AttributesSerialize" => return Ok((VariantType::Attributes, None, val)),
            "MaterialColors" => return Ok((VariantType::MaterialColors, None, val)),
            "UniqueId" => return Ok((VariantType::UniqueId, None, val)),
            _ => {}
        }
    }

    // Heuristic inference (e.g., for Attributes or untyped property values).
    let vt = match val {
        Value::Bool(_) => VariantType::Bool,
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                VariantType::Float64
            } else {
                VariantType::Float64
            }
        }
        Value::String(s) => {
            if s.starts_with('#')
                && (s.len() == 7 || s.len() == 4)
                && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                VariantType::Color3
            } else {
                VariantType::String
            }
        }
        Value::Array(arr) => match arr.len() {
            2 if arr.iter().all(|x| x.is_number()) => VariantType::Vector2,
            3 if arr.iter().all(|x| x.is_number()) => VariantType::Vector3,
            12 if arr.iter().all(|x| x.is_number()) => VariantType::CFrame,
            _ => return Err("cannot infer Roblox Variant type from JSON array; specify type".into()),
        },
        Value::Object(map) => {
            if map.contains_key("pos") || map.contains_key("position") {
                VariantType::CFrame
            } else if map.contains_key("scale") && map.contains_key("offset") {
                VariantType::UDim
            } else if map.contains_key("x") && map.contains_key("y") && map.contains_key("z") {
                VariantType::Vector3
            } else if map.contains_key("x") && map.contains_key("y") {
                VariantType::Vector2
            } else if map.contains_key("r") && map.contains_key("g") && map.contains_key("b") {
                VariantType::Color3
            } else if map.contains_key("family") {
                VariantType::Font
            } else if map.contains_key("base64") {
                VariantType::BinaryString
            } else if map.contains_key("ref") || map.contains_key("path") {
                VariantType::Ref
            } else {
                return Err("cannot infer Roblox Variant type from JSON object".into());
            }
        }
        Value::Null => return Err("cannot infer Variant type from null without property type".into()),
    };
    Ok((vt, None, val))
}

fn parse_variant_type_name(s: &str) -> Option<VariantType> {
    Some(match s {
        "Bool" | "boolean" => VariantType::Bool,
        "Int32" | "int" => VariantType::Int32,
        "Int64" => VariantType::Int64,
        "Float32" | "float" => VariantType::Float32,
        "Float64" | "number" | "double" => VariantType::Float64,
        "String" | "string" => VariantType::String,
        "BinaryString" => VariantType::BinaryString,
        "SharedString" => VariantType::SharedString,
        "NetAssetRef" => VariantType::NetAssetRef,
        "Content" => VariantType::Content,
        "ContentId" => VariantType::ContentId,
        "Vector2" => VariantType::Vector2,
        "Vector2int16" => VariantType::Vector2int16,
        "Vector3" => VariantType::Vector3,
        "Vector3int16" => VariantType::Vector3int16,
        "CFrame" => VariantType::CFrame,
        "OptionalCFrame" => VariantType::OptionalCFrame,
        "Color3" => VariantType::Color3,
        "Color3uint8" => VariantType::Color3uint8,
        "BrickColor" => VariantType::BrickColor,
        "UDim" => VariantType::UDim,
        "UDim2" => VariantType::UDim2,
        "Rect" => VariantType::Rect,
        "Region3" => VariantType::Region3,
        "Region3int16" => VariantType::Region3int16,
        "Ray" => VariantType::Ray,
        "NumberRange" => VariantType::NumberRange,
        "NumberSequence" => VariantType::NumberSequence,
        "ColorSequence" => VariantType::ColorSequence,
        "PhysicalProperties" => VariantType::PhysicalProperties,
        "Enum" => VariantType::Enum,
        "EnumItem" => VariantType::EnumItem,
        "Font" => VariantType::Font,
        "UniqueId" => VariantType::UniqueId,
        "SecurityCapabilities" => VariantType::SecurityCapabilities,
        "Attributes" => VariantType::Attributes,
        "Tags" => VariantType::Tags,
        "MaterialColors" => VariantType::MaterialColors,
        "Ref" => VariantType::Ref,
        "Axes" => VariantType::Axes,
        "Faces" => VariantType::Faces,
        _ => return None,
    })
}

fn as_f64(v: &Value) -> Result<f64, String> {
    match v {
        Value::Number(n) => n.as_f64().ok_or_else(|| "invalid number".into()),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "nan" => Ok(f64::NAN),
            "inf" | "infinity" | "+inf" | "+infinity" => Ok(f64::INFINITY),
            "-inf" | "-infinity" => Ok(f64::NEG_INFINITY),
            other => other
                .parse::<f64>()
                .map_err(|_| format!("expected number, got '{s}'")),
        },
        _ => Err(format!("expected number, got {v}")),
    }
}

fn as_f32(v: &Value) -> Result<f32, String> {
    Ok(as_f64(v)? as f32)
}

fn as_i64(v: &Value) -> Result<i64, String> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            .or_else(|| n.as_f64().map(|f| f.round() as i64))
            .ok_or_else(|| format!("expected integer, got {v}")),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("expected integer, got '{s}'")),
        _ => Err(format!("expected integer, got {v}")),
    }
}

fn parse_vec2(v: &Value) -> Result<Vector2, String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok(Vector2::new(as_f32(&a[0])?, as_f32(&a[1])?)),
        Value::Object(m) => Ok(Vector2::new(
            as_f32(m.get("x").or_else(|| m.get("X")).ok_or("Vector2 missing x")?)?,
            as_f32(m.get("y").or_else(|| m.get("Y")).ok_or("Vector2 missing y")?)?,
        )),
        _ => Err(format!("expected Vector2 [x, y] or {{x, y}}, got {v}")),
    }
}

fn parse_vec3(v: &Value) -> Result<Vector3, String> {
    match v {
        Value::Array(a) if a.len() == 3 => {
            Ok(Vector3::new(as_f32(&a[0])?, as_f32(&a[1])?, as_f32(&a[2])?))
        }
        Value::Object(m) => Ok(Vector3::new(
            as_f32(m.get("x").or_else(|| m.get("X")).ok_or("Vector3 missing x")?)?,
            as_f32(m.get("y").or_else(|| m.get("Y")).ok_or("Vector3 missing y")?)?,
            as_f32(m.get("z").or_else(|| m.get("Z")).ok_or("Vector3 missing z")?)?,
        )),
        _ => Err(format!("expected Vector3 [x, y, z] or {{x, y, z}}, got {v}")),
    }
}

fn parse_glam_vec3(v: &Value) -> Result<Vec3, String> {
    let p = parse_vec3(v)?;
    Ok(Vec3::new(p.x, p.y, p.z))
}

fn parse_hex_color_u8(s: &str) -> Result<[u8; 3], String> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| format!("invalid hex color '{s}'"))?;
            let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| format!("invalid hex color '{s}'"))?;
            let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| format!("invalid hex color '{s}'"))?;
            Ok([r, g, b])
        }
        3 => {
            let d = |i| u8::from_str_radix(&hex[i..i + 1], 16).map(|n| n * 17);
            Ok([
                d(0).map_err(|_| format!("invalid hex color '{s}'"))?,
                d(1).map_err(|_| format!("invalid hex color '{s}'"))?,
                d(2).map_err(|_| format!("invalid hex color '{s}'"))?,
            ])
        }
        _ => Err(format!("expected #RRGGBB hex color, got '{s}'")),
    }
}

fn parse_color3(v: &Value) -> Result<Color3, String> {
    match v {
        Value::String(s) => {
            let [r, g, b] = parse_hex_color_u8(s)?;
            Ok(Color3::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
        }
        Value::Array(a) if a.len() == 3 => {
            let r = as_f32(&a[0])?;
            let g = as_f32(&a[1])?;
            let b = as_f32(&a[2])?;
            Ok(Color3::new(r, g, b))
        }
        Value::Object(m) => {
            let r = as_f32(m.get("r").or_else(|| m.get("R")).ok_or("Color3 missing r")?)?;
            let g = as_f32(m.get("g").or_else(|| m.get("G")).ok_or("Color3 missing g")?)?;
            let b = as_f32(m.get("b").or_else(|| m.get("B")).ok_or("Color3 missing b")?)?;
            Ok(Color3::new(r, g, b))
        }
        _ => Err(format!("expected Color3 [r, g, b] or '#RRGGBB', got {v}")),
    }
}

fn parse_color3_uint8(v: &Value) -> Result<Color3uint8, String> {
    match v {
        Value::String(s) => {
            let [r, g, b] = parse_hex_color_u8(s)?;
            Ok(Color3uint8::new(r, g, b))
        }
        Value::Array(a) if a.len() == 3 => {
            let all_int = a.iter().all(|x| x.is_i64() || x.is_u64());
            let r = as_f64(&a[0])?;
            let g = as_f64(&a[1])?;
            let b = as_f64(&a[2])?;
            if !all_int && r <= 1.0 && g <= 1.0 && b <= 1.0 && r >= 0.0 && g >= 0.0 && b >= 0.0 {
                Ok(Color3uint8::new(
                    (r * 255.0).round() as u8,
                    (g * 255.0).round() as u8,
                    (b * 255.0).round() as u8,
                ))
            } else {
                Ok(Color3uint8::new(
                    r.clamp(0.0, 255.0).round() as u8,
                    g.clamp(0.0, 255.0).round() as u8,
                    b.clamp(0.0, 255.0).round() as u8,
                ))
            }
        }
        Value::Object(m) => {
            let rv = m.get("r").or_else(|| m.get("R")).ok_or("Color3uint8 missing r")?;
            let gv = m.get("g").or_else(|| m.get("G")).ok_or("Color3uint8 missing g")?;
            let bv = m.get("b").or_else(|| m.get("B")).ok_or("Color3uint8 missing b")?;
            parse_color3_uint8(&Value::Array(vec![rv.clone(), gv.clone(), bv.clone()]))
        }
        _ => Err(format!("expected Color3uint8 [r, g, b] or '#RRGGBB', got {v}")),
    }
}

fn parse_cframe(v: &Value) -> Result<CFrame, String> {
    match v {
        Value::Array(a) if a.len() == 12 => {
            let mut c = [0f32; 12];
            for (i, item) in a.iter().enumerate() {
                c[i] = as_f32(item)?;
            }
            Ok(CFrame::new(
                Vector3::new(c[0], c[1], c[2]),
                Matrix3::new(
                    Vector3::new(c[3], c[4], c[5]),
                    Vector3::new(c[6], c[7], c[8]),
                    Vector3::new(c[9], c[10], c[11]),
                ),
            ))
        }
        Value::Array(a) if a.len() == 3 => {
            let p = parse_vec3(v)?;
            Ok(CFrame::new(p, Matrix3::identity()))
        }
        Value::Object(m) => {
            let pos = match m.get("pos").or_else(|| m.get("position")) {
                Some(pv) => parse_glam_vec3(pv)?,
                None => Vec3::ZERO,
            };
            if let (Some(rv), Some(uv), Some(bv)) = (m.get("right"), m.get("up"), m.get("back")) {
                let r = parse_glam_vec3(rv)?;
                let u = parse_glam_vec3(uv)?;
                let b = parse_glam_vec3(bv)?;
                return Ok(Xform {
                    pos,
                    rot: Mat3::from_cols(r, u, b),
                }
                .to_cframe());
            }
            if let Some(target_v) = m.get("look_at").or_else(|| m.get("lookAt")) {
                let target = parse_glam_vec3(target_v)?;
                let up = match m.get("up") {
                    Some(uv) => parse_glam_vec3(uv)?,
                    None => Vec3::Y,
                };
                return Ok(crate::ltypes::look_at(pos, target, up).to_cframe());
            }
            if let Some(ang) = m.get("angles_deg").or_else(|| m.get("anglesDeg")) {
                let deg = parse_glam_vec3(ang)?;
                return Ok(Xform::from_orientation_deg(pos, deg).to_cframe());
            }
            if let Some(ang) = m.get("angles_rad").or_else(|| m.get("anglesRad")) {
                let rad = parse_glam_vec3(ang)?;
                return Ok(Xform {
                    pos,
                    rot: Mat3::from_euler(EulerRot::XYZ, rad.x, rad.y, rad.z),
                }
                .to_cframe());
            }
            if let Some(ori) = m.get("orientation") {
                let mat: Matrix3 = serde_json::from_value(ori.clone())
                    .map_err(|e| format!("invalid CFrame orientation: {e}"))?;
                return Ok(CFrame::new(Vector3::new(pos.x, pos.y, pos.z), mat));
            }
            Ok(CFrame::new(
                Vector3::new(pos.x, pos.y, pos.z),
                Matrix3::identity(),
            ))
        }
        _ => Err(format!(
            "expected CFrame (12 floats or {{pos, look_at|angles_deg|right+up+back}}), got {v}"
        )),
    }
}

fn parse_udim(v: &Value) -> Result<UDim, String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok(UDim::new(as_f32(&a[0])?, as_i64(&a[1])? as i32)),
        Value::Object(m) => Ok(UDim::new(
            m.get("scale").map(as_f32).transpose()?.unwrap_or(0.0),
            m.get("offset").map(as_i64).transpose()?.unwrap_or(0) as i32,
        )),
        _ => Err(format!("expected UDim [scale, offset], got {v}")),
    }
}

fn parse_udim2(v: &Value) -> Result<UDim2, String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok(UDim2::new(parse_udim(&a[0])?, parse_udim(&a[1])?)),
        Value::Array(a) if a.len() == 4 => Ok(UDim2::new(
            UDim::new(as_f32(&a[0])?, as_i64(&a[1])? as i32),
            UDim::new(as_f32(&a[2])?, as_i64(&a[3])? as i32),
        )),
        Value::Object(m) => Ok(UDim2::new(
            parse_udim(m.get("x").or_else(|| m.get("X")).ok_or("UDim2 missing x")?)?,
            parse_udim(m.get("y").or_else(|| m.get("Y")).ok_or("UDim2 missing y")?)?,
        )),
        _ => Err(format!("expected UDim2 [[xs, xo], [ys, yo]], got {v}")),
    }
}

fn parse_bytes(v: &Value) -> Result<Vec<u8>, String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    match v {
        Value::String(s) => b64
            .decode(s.trim())
            .map_err(|e| format!("invalid base64: {e}")),
        Value::Object(m) => {
            if let Some(s) = m.get("base64").and_then(|x| x.as_str()) {
                b64.decode(s.trim())
                    .map_err(|e| format!("invalid base64: {e}"))
            } else {
                Err("expected 'base64' field".into())
            }
        }
        Value::Array(a) => a
            .iter()
            .map(|x| {
                as_i64(x).and_then(|n| u8::try_from(n).map_err(|_| format!("byte out of range: {n}")))
            })
            .collect(),
        _ => Err(format!("expected base64 string or byte array, got {v}")),
    }
}

fn parse_font_weight(v: &Value) -> Result<FontWeight, String> {
    match v {
        Value::Number(n) => {
            let w = n.as_u64().and_then(|x| u16::try_from(x).ok()).ok_or("invalid FontWeight")?;
            FontWeight::from_u16(w).ok_or_else(|| format!("unknown FontWeight {w}"))
        }
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "thin" => Ok(FontWeight::Thin),
            "extralight" => Ok(FontWeight::ExtraLight),
            "light" => Ok(FontWeight::Light),
            "regular" => Ok(FontWeight::Regular),
            "medium" => Ok(FontWeight::Medium),
            "semibold" => Ok(FontWeight::SemiBold),
            "bold" => Ok(FontWeight::Bold),
            "extrabold" => Ok(FontWeight::ExtraBold),
            "heavy" => Ok(FontWeight::Heavy),
            other => other
                .parse::<u16>()
                .ok()
                .and_then(FontWeight::from_u16)
                .ok_or_else(|| format!("unknown FontWeight '{s}'")),
        },
        _ => Err(format!("expected FontWeight name or number, got {v}")),
    }
}

fn parse_font_style(v: &Value) -> Result<FontStyle, String> {
    match v {
        Value::Number(n) => {
            let s = n.as_u64().and_then(|x| u8::try_from(x).ok()).ok_or("invalid FontStyle")?;
            FontStyle::from_u8(s).ok_or_else(|| format!("unknown FontStyle {s}"))
        }
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Ok(FontStyle::Normal),
            "italic" => Ok(FontStyle::Italic),
            _ => Err(format!("unknown FontStyle '{s}'")),
        },
        _ => Err(format!("expected FontStyle name or number, got {v}")),
    }
}

fn convert_typed(
    doc: Option<&Document>,
    enum_name: Option<&str>,
    vt: VariantType,
    v: &Value,
) -> Result<Variant, String> {
    match vt {
        VariantType::Bool => match v {
            Value::Bool(b) => Ok(Variant::Bool(*b)),
            _ => Err(format!("expected boolean, got {v}")),
        },
        VariantType::Int32 => {
            let n = as_i64(v)?;
            let i = i32::try_from(n).map_err(|_| format!("Int32 out of range: {n}"))?;
            Ok(Variant::Int32(i))
        }
        VariantType::Int64 => Ok(Variant::Int64(as_i64(v)?)),
        VariantType::Float32 => Ok(Variant::Float32(as_f32(v)?)),
        VariantType::Float64 => Ok(Variant::Float64(as_f64(v)?)),
        VariantType::String => match v {
            Value::String(s) => Ok(Variant::String(s.clone())),
            _ => Err(format!("expected string, got {v}")),
        },
        VariantType::BinaryString => Ok(Variant::BinaryString(BinaryString::from(parse_bytes(v)?))),
        VariantType::SharedString => Ok(Variant::SharedString(SharedString::new(parse_bytes(v)?))),
        VariantType::NetAssetRef => Ok(Variant::NetAssetRef(NetAssetRef::new(parse_bytes(v)?))),
        VariantType::ContentId => match v {
            Value::String(s) => Ok(Variant::ContentId(ContentId::from(s.clone()))),
            Value::Null => Ok(Variant::ContentId(ContentId::new())),
            Value::Object(m) => {
                let uri = m.get("uri").and_then(|u| u.as_str()).unwrap_or_default();
                Ok(Variant::ContentId(ContentId::from(uri.to_string())))
            }
            _ => Err(format!("expected ContentId string, got {v}")),
        },
        VariantType::Content => match v {
            Value::Null => Ok(Variant::Content(Content::none())),
            Value::String(s) => {
                if s.is_empty() {
                    Ok(Variant::Content(Content::none()))
                } else {
                    Ok(Variant::Content(Content::from_uri(s.clone())))
                }
            }
            Value::Object(m) => {
                if m.get("none").and_then(|x| x.as_bool()) == Some(true) {
                    return Ok(Variant::Content(Content::none()));
                }
                if let Some(u) = m.get("uri").and_then(|x| x.as_str()) {
                    return Ok(Variant::Content(if u.is_empty() {
                        Content::none()
                    } else {
                        Content::from_uri(u.to_string())
                    }));
                }
                if let Some(obj) = m.get("object") {
                    let r = resolve_ref_value(doc, obj)?;
                    return Ok(Variant::Content(Content::from_referent(r)));
                }
                Err("Content object must contain 'uri', 'object', or 'none'".into())
            }
            _ => Err(format!("expected Content string/null/object, got {v}")),
        },
        VariantType::Vector2 => Ok(Variant::Vector2(parse_vec2(v)?)),
        VariantType::Vector2int16 => {
            let v2 = parse_vec2(v)?;
            Ok(Variant::Vector2int16(Vector2int16::new(
                v2.x.round() as i16,
                v2.y.round() as i16,
            )))
        }
        VariantType::Vector3 => Ok(Variant::Vector3(parse_vec3(v)?)),
        VariantType::Vector3int16 => {
            let v3 = parse_vec3(v)?;
            Ok(Variant::Vector3int16(Vector3int16::new(
                v3.x.round() as i16,
                v3.y.round() as i16,
                v3.z.round() as i16,
            )))
        }
        VariantType::CFrame => Ok(Variant::CFrame(parse_cframe(v)?)),
        VariantType::OptionalCFrame => {
            if v.is_null() {
                Ok(Variant::OptionalCFrame(None))
            } else {
                Ok(Variant::OptionalCFrame(Some(parse_cframe(v)?)))
            }
        }
        VariantType::Color3 => Ok(Variant::Color3(parse_color3(v)?)),
        VariantType::Color3uint8 => Ok(Variant::Color3uint8(parse_color3_uint8(v)?)),
        VariantType::BrickColor => match v {
            Value::Number(n) => {
                let id = n.as_u64().and_then(|x| u16::try_from(x).ok()).ok_or("invalid BrickColor number")?;
                BrickColor::from_number(id)
                    .map(Variant::BrickColor)
                    .ok_or_else(|| format!("unknown BrickColor number {id}"))
            }
            Value::String(s) => BrickColor::from_name(s.trim())
                .map(Variant::BrickColor)
                .ok_or_else(|| format!("unknown BrickColor name '{s}'")),
            Value::Object(m) => {
                if let Some(id) = m.get("number").and_then(|x| x.as_u64()).and_then(|x| u16::try_from(x).ok()) {
                    BrickColor::from_number(id)
                        .map(Variant::BrickColor)
                        .ok_or_else(|| format!("unknown BrickColor number {id}"))
                } else if let Some(name) = m.get("name").and_then(|x| x.as_str()) {
                    BrickColor::from_name(name.trim())
                        .map(Variant::BrickColor)
                        .ok_or_else(|| format!("unknown BrickColor name '{name}'"))
                } else {
                    Err("BrickColor object must have 'name' or 'number'".into())
                }
            }
            _ => Err(format!("expected BrickColor name or number, got {v}")),
        },
        VariantType::UDim => Ok(Variant::UDim(parse_udim(v)?)),
        VariantType::UDim2 => Ok(Variant::UDim2(parse_udim2(v)?)),
        VariantType::Rect => match v {
            Value::Array(a) if a.len() == 2 => {
                Ok(Variant::Rect(Rect::new(parse_vec2(&a[0])?, parse_vec2(&a[1])?)))
            }
            Value::Array(a) if a.len() == 4 => Ok(Variant::Rect(Rect::new(
                Vector2::new(as_f32(&a[0])?, as_f32(&a[1])?),
                Vector2::new(as_f32(&a[2])?, as_f32(&a[3])?),
            ))),
            Value::Object(m) => Ok(Variant::Rect(Rect::new(
                parse_vec2(m.get("min").ok_or("Rect missing min")?)?,
                parse_vec2(m.get("max").ok_or("Rect missing max")?)?,
            ))),
            _ => Err(format!("expected Rect [[x0,y0],[x1,y1]], got {v}")),
        },
        VariantType::Region3 => match v {
            Value::Array(a) if a.len() == 2 => {
                Ok(Variant::Region3(Region3::new(parse_vec3(&a[0])?, parse_vec3(&a[1])?)))
            }
            Value::Object(m) => Ok(Variant::Region3(Region3::new(
                parse_vec3(m.get("min").ok_or("Region3 missing min")?)?,
                parse_vec3(m.get("max").ok_or("Region3 missing max")?)?,
            ))),
            _ => Err(format!("expected Region3 [[x0,y0,z0],[x1,y1,z1]], got {v}")),
        },
        VariantType::Region3int16 => match v {
            Value::Array(a) if a.len() == 2 => {
                let min = parse_vec3(&a[0])?;
                let max = parse_vec3(&a[1])?;
                Ok(Variant::Region3int16(Region3int16::new(
                    Vector3int16::new(min.x.round() as i16, min.y.round() as i16, min.z.round() as i16),
                    Vector3int16::new(max.x.round() as i16, max.y.round() as i16, max.z.round() as i16),
                )))
            }
            Value::Object(m) => {
                let min = parse_vec3(m.get("min").ok_or("Region3int16 missing min")?)?;
                let max = parse_vec3(m.get("max").ok_or("Region3int16 missing max")?)?;
                Ok(Variant::Region3int16(Region3int16::new(
                    Vector3int16::new(min.x.round() as i16, min.y.round() as i16, min.z.round() as i16),
                    Vector3int16::new(max.x.round() as i16, max.y.round() as i16, max.z.round() as i16),
                )))
            }
            _ => Err(format!("expected Region3int16, got {v}")),
        },
        VariantType::Ray => match v {
            Value::Object(m) => Ok(Variant::Ray(Ray::new(
                parse_vec3(m.get("origin").ok_or("Ray missing origin")?)?,
                parse_vec3(m.get("direction").ok_or("Ray missing direction")?)?,
            ))),
            Value::Array(a) if a.len() == 2 => {
                Ok(Variant::Ray(Ray::new(parse_vec3(&a[0])?, parse_vec3(&a[1])?)))
            }
            _ => Err(format!("expected Ray {{origin, direction}}, got {v}")),
        },
        VariantType::NumberRange => match v {
            Value::Number(_) => {
                let n = as_f32(v)?;
                Ok(Variant::NumberRange(NumberRange::new(n, n)))
            }
            Value::Array(a) if a.len() == 2 => {
                Ok(Variant::NumberRange(NumberRange::new(as_f32(&a[0])?, as_f32(&a[1])?)))
            }
            Value::Object(m) => Ok(Variant::NumberRange(NumberRange::new(
                as_f32(m.get("min").ok_or("NumberRange missing min")?)?,
                as_f32(m.get("max").ok_or("NumberRange missing max")?)?,
            ))),
            _ => Err(format!("expected NumberRange [min, max], got {v}")),
        },
        VariantType::NumberSequence => match v {
            Value::Number(_) => {
                let val = as_f32(v)?;
                Ok(Variant::NumberSequence(NumberSequence {
                    keypoints: vec![
                        NumberSequenceKeypoint::new(0.0, val, 0.0),
                        NumberSequenceKeypoint::new(1.0, val, 0.0),
                    ],
                }))
            }
            Value::Array(a) => {
                let mut keypoints = Vec::with_capacity(a.len());
                for item in a {
                    match item {
                        Value::Object(m) => {
                            let time = as_f32(m.get("time").ok_or("NumberSequence keypoint missing time")?)?;
                            let value = as_f32(m.get("value").ok_or("NumberSequence keypoint missing value")?)?;
                            let env = m.get("envelope").map(as_f32).transpose()?.unwrap_or(0.0);
                            keypoints.push(NumberSequenceKeypoint::new(time, value, env));
                        }
                        Value::Array(pair) if pair.len() == 2 || pair.len() == 3 => {
                            let time = as_f32(&pair[0])?;
                            let value = as_f32(&pair[1])?;
                            let env = if pair.len() == 3 { as_f32(&pair[2])? } else { 0.0 };
                            keypoints.push(NumberSequenceKeypoint::new(time, value, env));
                        }
                        _ => return Err(format!("invalid NumberSequence keypoint: {item}")),
                    }
                }
                Ok(Variant::NumberSequence(NumberSequence { keypoints }))
            }
            _ => Err(format!("expected NumberSequence array or number, got {v}")),
        },
        VariantType::ColorSequence => match v {
            Value::String(_) => {
                let c = parse_color3(v)?;
                Ok(Variant::ColorSequence(ColorSequence {
                    keypoints: vec![
                        ColorSequenceKeypoint::new(0.0, c),
                        ColorSequenceKeypoint::new(1.0, c),
                    ],
                }))
            }
            Value::Array(a) if a.len() == 3 && a.iter().all(|x| x.is_number()) => {
                let c = parse_color3(v)?;
                Ok(Variant::ColorSequence(ColorSequence {
                    keypoints: vec![
                        ColorSequenceKeypoint::new(0.0, c),
                        ColorSequenceKeypoint::new(1.0, c),
                    ],
                }))
            }
            Value::Array(a) => {
                let mut keypoints = Vec::with_capacity(a.len());
                for item in a {
                    match item {
                        Value::Object(m) => {
                            let time = as_f32(m.get("time").ok_or("ColorSequence keypoint missing time")?)?;
                            let color = parse_color3(m.get("color").ok_or("ColorSequence keypoint missing color")?)?;
                            keypoints.push(ColorSequenceKeypoint::new(time, color));
                        }
                        Value::Array(pair) if pair.len() == 2 => {
                            let time = as_f32(&pair[0])?;
                            let color = parse_color3(&pair[1])?;
                            keypoints.push(ColorSequenceKeypoint::new(time, color));
                        }
                        _ => return Err(format!("invalid ColorSequence keypoint: {item}")),
                    }
                }
                Ok(Variant::ColorSequence(ColorSequence { keypoints }))
            }
            _ => Err(format!("expected ColorSequence array or Color3, got {v}")),
        },
        VariantType::PhysicalProperties => match v {
            Value::Null => Ok(Variant::PhysicalProperties(PhysicalProperties::Default)),
            Value::String(s) if s.eq_ignore_ascii_case("default") => {
                Ok(Variant::PhysicalProperties(PhysicalProperties::Default))
            }
            Value::Object(m) => {
                let density = m.get("density").map(as_f32).transpose()?.unwrap_or(0.7);
                let friction = m.get("friction").map(as_f32).transpose()?.unwrap_or(0.3);
                let elasticity = m.get("elasticity").map(as_f32).transpose()?.unwrap_or(0.5);
                let friction_weight = m
                    .get("frictionWeight")
                    .or_else(|| m.get("friction_weight"))
                    .map(as_f32)
                    .transpose()?
                    .unwrap_or(1.0);
                let elasticity_weight = m
                    .get("elasticityWeight")
                    .or_else(|| m.get("elasticity_weight"))
                    .map(as_f32)
                    .transpose()?
                    .unwrap_or(1.0);
                let acoustic_absorption = m
                    .get("acousticAbsorption")
                    .or_else(|| m.get("acoustic_absorption"))
                    .map(as_f32)
                    .transpose()?
                    .unwrap_or(1.0);
                Ok(Variant::PhysicalProperties(PhysicalProperties::Custom(
                    CustomPhysicalProperties::new(
                        density,
                        friction,
                        elasticity,
                        friction_weight,
                        elasticity_weight,
                        acoustic_absorption,
                    ),
                )))
            }
            _ => Err(format!("expected PhysicalProperties 'Default' or object, got {v}")),
        },
        VariantType::Enum => match v {
            Value::Number(_) => {
                let n = as_i64(v)?;
                let u = u32::try_from(n).map_err(|_| format!("invalid enum integer {n}"))?;
                Ok(Variant::Enum(Enum::from_u32(u)))
            }
            Value::String(s) => {
                let val = enum_name_to_u32(enum_name, s)
                    .ok_or_else(|| format!("unknown enum item '{s}' (enum: {enum_name:?})"))?;
                Ok(Variant::Enum(Enum::from_u32(val)))
            }
            Value::Object(m) => {
                let en = m.get("enum").and_then(|x| x.as_str()).or(enum_name);
                if let Some(name) = m.get("name").and_then(|x| x.as_str()) {
                    if let Some(val) = enum_name_to_u32(en, name) {
                        return Ok(Variant::Enum(Enum::from_u32(val)));
                    }
                }
                if let Some(val) = m.get("value") {
                    let n = as_i64(val)?;
                    let u = u32::try_from(n).map_err(|_| format!("invalid enum integer {n}"))?;
                    return Ok(Variant::Enum(Enum::from_u32(u)));
                }
                Err("Enum object must contain 'name' or 'value'".into())
            }
            _ => Err(format!("expected Enum name or integer, got {v}")),
        },
        VariantType::EnumItem => match v {
            Value::Object(m) => {
                let ty = m
                    .get("type")
                    .or_else(|| m.get("enum"))
                    .and_then(|x| x.as_str())
                    .or(enum_name)
                    .ok_or("EnumItem missing 'type'")?
                    .to_string();
                let value = if let Some(val) = m.get("value") {
                    as_i64(val)? as u32
                } else if let Some(name) = m.get("name").and_then(|x| x.as_str()) {
                    enum_name_to_u32(Some(&ty), name)
                        .ok_or_else(|| format!("unknown EnumItem '{ty}.{name}'"))?
                } else {
                    return Err("EnumItem missing 'value' or 'name'".into());
                };
                Ok(Variant::EnumItem(EnumItem { ty, value }))
            }
            _ => Err(format!("expected EnumItem object, got {v}")),
        },
        VariantType::Font => match v {
            Value::String(s) => Ok(Variant::Font(Font::regular(s))),
            Value::Object(m) => {
                let family = m
                    .get("family")
                    .and_then(|x| x.as_str())
                    .unwrap_or("rbxasset://fonts/families/SourceSansPro.json");
                let weight = m
                    .get("weight")
                    .map(parse_font_weight)
                    .transpose()?
                    .unwrap_or_default();
                let style = m
                    .get("style")
                    .map(parse_font_style)
                    .transpose()?
                    .unwrap_or_default();
                let mut font = Font::new(family, weight, style);
                font.cached_face_id = m
                    .get("cachedFaceId")
                    .or_else(|| m.get("cached_face_id"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                Ok(Variant::Font(font))
            }
            _ => Err(format!("expected Font object or family string, got {v}")),
        },
        VariantType::UniqueId => match v {
            Value::String(s) => UniqueId::from_str(s.trim())
                .map(Variant::UniqueId)
                .map_err(|e| format!("invalid UniqueId '{s}': {e}")),
            Value::Null => Ok(Variant::UniqueId(UniqueId::nil())),
            _ => Err(format!("expected UniqueId hex string, got {v}")),
        },
        VariantType::SecurityCapabilities => {
            let bits = match v {
                Value::Number(n) => n.as_u64().ok_or("invalid SecurityCapabilities")?,
                Value::String(s) => s
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| format!("invalid SecurityCapabilities '{s}': {e}"))?,
                _ => return Err(format!("expected u64 for SecurityCapabilities, got {v}")),
            };
            Ok(Variant::SecurityCapabilities(SecurityCapabilities::from_bits(bits)))
        }
        VariantType::Attributes => match v {
            Value::Object(m) => {
                let mut attrs = Attributes::new();
                for (k, val) in m {
                    let av = json_to_variant(doc, None, None, val, None)?;
                    attrs.insert(k.clone(), av);
                }
                Ok(Variant::Attributes(attrs))
            }
            _ => Err(format!("expected object for Attributes, got {v}")),
        },
        VariantType::Tags => match v {
            Value::Array(a) => {
                let mut tags = Tags::new();
                for item in a {
                    let s = item.as_str().ok_or("Tags array elements must be strings")?;
                    tags.push(s);
                }
                Ok(Variant::Tags(tags))
            }
            _ => Err(format!("expected string array for Tags, got {v}")),
        },
        VariantType::MaterialColors => match v {
            Value::Object(m) => {
                let mut map = BTreeMap::new();
                for (k, cv) in m {
                    let mat = TerrainMaterials::from_str(k.trim())
                        .map_err(|e| format!("unknown TerrainMaterial '{k}': {e}"))?;
                    let c = parse_color3_uint8(cv)?;
                    map.insert(mat, c);
                }
                Ok(Variant::MaterialColors(MaterialColors::from(map)))
            }
            _ => Err(format!("expected object for MaterialColors, got {v}")),
        },
        VariantType::Ref => Ok(Variant::Ref(resolve_ref_value(doc, v)?)),
        VariantType::Axes => serde_json::from_value::<Axes>(v.clone())
            .map(Variant::Axes)
            .map_err(|e| format!("invalid Axes: {e}")),
        VariantType::Faces => serde_json::from_value::<Faces>(v.clone())
            .map(Variant::Faces)
            .map_err(|e| format!("invalid Faces: {e}")),
        other => Err(format!("unsupported VariantType {other:?}")),
    }
}

fn resolve_ref_value(doc: Option<&Document>, v: &Value) -> Result<Ref, String> {
    if v.is_null() {
        return Ok(Ref::none());
    }
    if let Value::String(s) = v {
        let t = s.trim();
        if t.is_empty()
            || t.eq_ignore_ascii_case("null")
            || t.eq_ignore_ascii_case("nil")
            || t.eq_ignore_ascii_case("none")
        {
            return Ok(Ref::none());
        }
        if t.starts_with("ref:") || (t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit())) {
            let r = str_to_ref(t)?;
            if let Some(d) = doc {
                if r.is_some() && !d.exists(r) {
                    return Err(format!("ref '{t}' does not exist in DataModel"));
                }
            }
            return Ok(r);
        }
    }
    if let Some(d) = doc {
        resolve_target(d, v)
    } else {
        match v {
            Value::String(s) => str_to_ref(s),
            _ => Err("cannot resolve path Ref without a Document".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn resolves_paths_and_refs() {
        let mut doc = Document::new_place();
        let ws = doc.workspace().unwrap();
        let folder = doc.insert(ws, InstanceBuilder::new("Folder").with_name("Enemies"));
        let part = doc.insert(folder, InstanceBuilder::new("Part").with_name("Boss"));

        assert_eq!(
            resolve_target(&doc, &json!("Workspace.Enemies.Boss")).unwrap(),
            part
        );
        assert_eq!(
            resolve_target(&doc, &json!("game/Workspace/Enemies/Boss")).unwrap(),
            part
        );
        assert_eq!(
            resolve_target(&doc, &json!("/Workspace/Enemies/Boss")).unwrap(),
            part
        );
        assert_eq!(
            resolve_target(&doc, &json!(ref_to_str(part))).unwrap(),
            part
        );
        assert_eq!(instance_path(&doc, part), "Workspace.Enemies.Boss");
        assert_eq!(
            resolve_target(&doc, &json!("game")).unwrap(),
            doc.dom.root_ref()
        );
    }

    #[test]
    fn converts_all_major_variants() {
        let mut doc = Document::new_place();
        let ws = doc.workspace().unwrap();
        let part = doc.insert(ws, InstanceBuilder::new("Part").with_name("Probe"));

        // Primitives
        assert_eq!(
            json_to_variant(None, None, None, &json!(true), Some(VariantType::Bool)).unwrap(),
            Variant::Bool(true)
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!(-42), Some(VariantType::Int32)).unwrap(),
            Variant::Int32(-42)
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!(9_000_000_000i64), Some(VariantType::Int64)).unwrap(),
            Variant::Int64(9_000_000_000i64)
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!(1.5), Some(VariantType::Float32)).unwrap(),
            Variant::Float32(1.5)
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!(2.25), Some(VariantType::Float64)).unwrap(),
            Variant::Float64(2.25)
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!("hello"), Some(VariantType::String)).unwrap(),
            Variant::String("hello".into())
        );

        // BinaryString & SharedString
        let bs = json_to_variant(None, None, None, &json!("aGVsbG8="), Some(VariantType::BinaryString)).unwrap();
        assert_eq!(bs, Variant::BinaryString(BinaryString::from(b"hello".as_slice())));
        let ss = json_to_variant(None, None, None, &json!("d29ybGQ="), Some(VariantType::SharedString)).unwrap();
        assert_eq!(ss, Variant::SharedString(SharedString::new(b"world".to_vec())));

        // Content & ContentId
        let cid = json_to_variant(None, Some("MeshPart"), Some("MeshId"), &json!("rbxassetid://123"), None).unwrap();
        assert_eq!(cid, Variant::ContentId(ContentId::from("rbxassetid://123")));
        let cnt = json_to_variant(None, Some("MeshPart"), Some("MeshContent"), &json!("rbxassetid://456"), None).unwrap();
        assert_eq!(cnt, Variant::Content(Content::from_uri("rbxassetid://456")));

        // Vectors
        assert_eq!(
            json_to_variant(None, None, None, &json!([1, 2]), Some(VariantType::Vector2)).unwrap(),
            Variant::Vector2(Vector2::new(1.0, 2.0))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([10, 20]), Some(VariantType::Vector2int16)).unwrap(),
            Variant::Vector2int16(Vector2int16::new(10, 20))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([1, 2, 3]), Some(VariantType::Vector3)).unwrap(),
            Variant::Vector3(Vector3::new(1.0, 2.0, 3.0))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([4, 5, 6]), Some(VariantType::Vector3int16)).unwrap(),
            Variant::Vector3int16(Vector3int16::new(4, 5, 6))
        );

        // CFrame modes
        let cf_look = json_to_variant(
            None,
            None,
            None,
            &json!({"pos": [0, 5, 10], "look_at": [0, 5, 0]}),
            Some(VariantType::CFrame),
        )
        .unwrap();
        assert!(matches!(cf_look, Variant::CFrame(cf) if (cf.position.z - 10.0).abs() < 1e-4));

        let cf_deg = json_to_variant(
            None,
            None,
            None,
            &json!({"pos": [1, 2, 3], "angles_deg": [0, 90, 0]}),
            Some(VariantType::CFrame),
        )
        .unwrap();
        let back_deg = variant_to_json(&cf_deg);
        let cf_rub = json_to_variant(None, None, None, &back_deg, Some(VariantType::CFrame)).unwrap();
        assert_eq!(cf_deg, cf_rub);

        let cf_12 = json_to_variant(
            None,
            None,
            None,
            &json!([7, 8, 9, 1, 0, 0, 0, 1, 0, 0, 0, 1]),
            Some(VariantType::CFrame),
        )
        .unwrap();
        assert_eq!(
            cf_12,
            Variant::CFrame(CFrame::new(Vector3::new(7.0, 8.0, 9.0), Matrix3::identity()))
        );

        // Colors & BrickColor
        let c3 = json_to_variant(None, None, None, &json!("#ff8000"), Some(VariantType::Color3)).unwrap();
        assert!(matches!(c3, Variant::Color3(c) if (c.r - 1.0).abs() < 1e-3 && c.b == 0.0));
        let c8 = json_to_variant(None, None, None, &json!("#336699"), Some(VariantType::Color3uint8)).unwrap();
        assert_eq!(c8, Variant::Color3uint8(Color3uint8::new(0x33, 0x66, 0x99)));
        let c8_arr = json_to_variant(None, None, None, &json!([10, 20, 30]), Some(VariantType::Color3uint8)).unwrap();
        assert_eq!(c8_arr, Variant::Color3uint8(Color3uint8::new(10, 20, 30)));
        let bc = json_to_variant(None, None, None, &json!("Bright red"), Some(VariantType::BrickColor)).unwrap();
        assert_eq!(bc, Variant::BrickColor(BrickColor::BrightRed));

        // UDim, UDim2, Rect, Region3, Ranges, Sequences, PhysicalProperties
        assert_eq!(
            json_to_variant(None, None, None, &json!([0.5, 10]), Some(VariantType::UDim)).unwrap(),
            Variant::UDim(UDim::new(0.5, 10))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([[0.5, 10], [1.0, -20]]), Some(VariantType::UDim2)).unwrap(),
            Variant::UDim2(UDim2::new(UDim::new(0.5, 10), UDim::new(1.0, -20)))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([[0, 0], [100, 50]]), Some(VariantType::Rect)).unwrap(),
            Variant::Rect(Rect::new(Vector2::new(0.0, 0.0), Vector2::new(100.0, 50.0)))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([[-1, -2, -3], [4, 5, 6]]), Some(VariantType::Region3)).unwrap(),
            Variant::Region3(Region3::new(Vector3::new(-1.0, -2.0, -3.0), Vector3::new(4.0, 5.0, 6.0)))
        );
        assert_eq!(
            json_to_variant(None, None, None, &json!([2, 8]), Some(VariantType::NumberRange)).unwrap(),
            Variant::NumberRange(NumberRange::new(2.0, 8.0))
        );
        let ns = json_to_variant(
            None,
            None,
            None,
            &json!([{"time": 0, "value": 1, "envelope": 0}, {"time": 1, "value": 0}]),
            Some(VariantType::NumberSequence),
        )
        .unwrap();
        assert!(matches!(ns, Variant::NumberSequence(s) if s.keypoints.len() == 2));
        let cs = json_to_variant(
            None,
            None,
            None,
            &json!([{"time": 0, "color": [1, 0, 0]}, {"time": 1, "color": "#00ff00"}]),
            Some(VariantType::ColorSequence),
        )
        .unwrap();
        assert!(matches!(cs, Variant::ColorSequence(s) if s.keypoints.len() == 2));
        assert_eq!(
            json_to_variant(None, None, None, &json!("Default"), Some(VariantType::PhysicalProperties)).unwrap(),
            Variant::PhysicalProperties(PhysicalProperties::Default)
        );
        let pp = json_to_variant(
            None,
            None,
            None,
            &json!({"density": 2.0, "friction": 0.8}),
            Some(VariantType::PhysicalProperties),
        )
        .unwrap();
        assert!(matches!(pp, Variant::PhysicalProperties(PhysicalProperties::Custom(_))));

        // Enum by name and integer
        let mat_name = json_to_variant(None, Some("Part"), Some("Material"), &json!("Neon"), None).unwrap();
        let mat_num = json_to_variant(None, Some("Part"), Some("Material"), &json!(288), None).unwrap();
        assert_eq!(mat_name, mat_num);
        assert_eq!(mat_name, Variant::Enum(Enum::from_u32(288)));

        // Font, UniqueId, SecurityCapabilities, Attributes, Tags, MaterialColors, Ref
        let font = json_to_variant(
            None,
            None,
            None,
            &json!({"family": "rbxasset://fonts/families/GothamSSm.json", "weight": "Bold", "style": "Italic"}),
            Some(VariantType::Font),
        )
        .unwrap();
        assert!(matches!(font, Variant::Font(f) if f.weight == FontWeight::Bold && f.style == FontStyle::Italic));

        let uid = json_to_variant(
            None,
            None,
            None,
            &json!("1020304050607080facade0013370000"),
            Some(VariantType::UniqueId),
        )
        .unwrap();
        assert!(matches!(uid, Variant::UniqueId(_)));

        let sc = json_to_variant(None, None, None, &json!(15), Some(VariantType::SecurityCapabilities)).unwrap();
        assert_eq!(sc, Variant::SecurityCapabilities(SecurityCapabilities::from_bits(15)));

        let attrs = json_to_variant(
            None,
            None,
            None,
            &json!({"Health": 100, "Boss": true, "Tag": "Alpha"}),
            Some(VariantType::Attributes),
        )
        .unwrap();
        assert!(matches!(attrs, Variant::Attributes(a) if a.len() == 3));

        let tags = json_to_variant(None, None, None, &json!(["Enemy", "Damageable"]), Some(VariantType::Tags)).unwrap();
        assert!(matches!(tags, Variant::Tags(t) if t.len() == 2));

        let mc = json_to_variant(
            None,
            None,
            None,
            &json!({"Grass": [10, 20, 30]}),
            Some(VariantType::MaterialColors),
        )
        .unwrap();
        assert!(matches!(mc, Variant::MaterialColors(_)));

        let r_path = json_to_variant(Some(&doc), None, None, &json!("Workspace.Probe"), Some(VariantType::Ref)).unwrap();
        let r_id = json_to_variant(Some(&doc), None, None, &json!(ref_to_str(part)), Some(VariantType::Ref)).unwrap();
        assert_eq!(r_path, Variant::Ref(part));
        assert_eq!(r_id, Variant::Ref(part));
    }
}
