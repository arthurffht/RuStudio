//! Terrain voxel inspection, procedural generation, sculpting & filling tools.

use std::sync::mpsc::Sender;

use glam::{Mat3, Vec3};
use rbx_dom_weak::types::{BinaryString, Ref, Variant, VariantType};
use rbx_dom_weak::{ustr, InstanceBuilder};
use serde_json::{json, Value};

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::variants::{
    self, instance_path, ref_to_str, resolve_datamodel, resolve_datamodel_mut, variant_to_json,
};
use crate::doc::Document;
use crate::math::Xform;
use crate::mech;
use crate::terrain::{self, Voxels, WATER, VOXEL};
use crate::terrain_brush::{
    self, BrushOp, FlattenMode, Params as BrushParams, Pivot, Shape as BrushShape, StrokeState,
};
use crate::terrain_editor::RegionBox;
use crate::terrain_gen::{self, Biome, Options as GenOptions};
use crate::terrain_region;

const MAX_INSPECT_SAMPLES: usize = 8192;
const MAX_GENERATE_VOXELS: usize = 8_388_608;
const MAX_CLEAR_VOXELS: usize = 16_777_216;
const MAX_FILL_EXTENT_STUDS: f32 = 2048.0;

pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "terrain_inspect".into(),
            description: "Inspect Workspace.Terrain properties (water look, Decoration, GrassLength, MaterialColors), chunk/voxel counts, world/voxel bounds, present materials, and optional 3D voxel sampling in a region.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": {
                        "type": "string",
                        "enum": ["Active", "Edit", "Server", "Client"],
                        "description": "Target DataModel to inspect (defaults to Active)."
                    },
                    "region": {
                        "type": "object",
                        "description": "Optional box to sample non-Air voxels within.",
                        "properties": {
                            "min": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                            "max": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                            "unit": { "type": "string", "enum": ["studs", "voxels"], "description": "Coordinate units for min/max (default: studs)." },
                            "step": { "type": "integer", "minimum": 1, "description": "Sampling stride in voxels (default: 1)." },
                            "include_air": { "type": "boolean", "description": "Include Air voxels in samples (default: false)." },
                            "max_samples": { "type": "integer", "minimum": 1, "maximum": 8192, "description": "Max voxel samples returned (default: 4096)." }
                        },
                        "required": ["min", "max"]
                    }
                }
            }),
        },
        ToolDef {
            name: "terrain_fill_shape".into(),
            description: "Fill or carve (material: 'Air') SmoothGrid terrain voxels using block, ball, cylinder, wedge, or water_level shapes. Undoable via History.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "shape": {
                        "type": "string",
                        "enum": ["block", "ball", "cylinder", "wedge", "water_level"],
                        "description": "Shape primitive to fill or carve."
                    },
                    "material": {
                        "type": "string",
                        "description": "Terrain Enum.Material name (e.g. Grass, Rock, Sand, Water, Air to carve). Defaults to Grass (or Water for water_level)."
                    },
                    "cframe": { "description": "CFrame ({position, orientation} or 12-float matrix) for block, cylinder, or wedge." },
                    "position": { "description": "Center [x, y, z] in studs (alternative to cframe)." },
                    "center": { "description": "Alias for position [x, y, z]." },
                    "orientation": { "description": "Optional Euler angles [rx, ry, rz] in degrees." },
                    "size": { "description": "Dimensions [sx, sy, sz] in studs for block, wedge, or water_level." },
                    "min": { "description": "Minimum corner [x, y, z] in studs (alternative bounds for block or water_level)." },
                    "max": { "description": "Maximum corner [x, y, z] in studs (alternative bounds for block or water_level)." },
                    "radius": { "type": "number", "description": "Radius in studs for ball or cylinder." },
                    "height": { "type": "number", "description": "Height in studs for cylinder." },
                    "evaporate": { "type": "boolean", "description": "For water_level: true replaces Water with Air; false replaces Air with Water (default: false, or true if material == Air)." }
                },
                "required": ["shape"]
            }),
        },
        ToolDef {
            name: "terrain_sculpt_stroke".into(),
            description: "Apply Terrain Brush strokes (Add, Subtract, Grow, Erode, Smooth, Flatten, Paint, SeaLevel) at a point or interpolated along a sequence of world points. Undoable.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "op": {
                        "type": "string",
                        "enum": ["Add", "Subtract", "Grow", "Erode", "Smooth", "Flatten", "Paint", "SeaLevel"],
                        "description": "Brush operation."
                    },
                    "point": { "description": "Single world hit position [x, y, z] in studs." },
                    "points": {
                        "type": "array",
                        "items": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                        "description": "Sequence of world positions [[x, y, z], ...] to stroke across."
                    },
                    "brush_size": { "type": "number", "description": "Brush diameter in voxels (default: 6)." },
                    "brush_height": { "type": "number", "description": "Brush height in voxels for Cube/Cylinder (default: brush_size)." },
                    "shape": { "type": "string", "enum": ["Sphere", "Cube", "Cylinder"], "description": "Brush shape (default: Sphere)." },
                    "pivot": { "type": "string", "enum": ["Center", "Top", "Bottom"], "description": "Brush vertical pivot (default: Center)." },
                    "strength": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Brush strength 0..1 (default: 0.5)." },
                    "material": { "type": "string", "description": "Material name (default: Grass)." },
                    "target_material": { "type": "string", "description": "Optional material to replace when op == Paint." },
                    "flatten_mode": { "type": "string", "enum": ["Grow", "Erode", "Both"], "description": "Flatten direction (default: Both)." },
                    "target_plane_y": { "type": "number", "description": "Fixed plane Y in studs for Flatten or SeaLevel (auto-detected from first point if omitted)." },
                    "ignore_water": { "type": "boolean", "description": "Ignore water voxels during Grow/Erode/Smooth/Flatten (default: false)." },
                    "snap_to_voxels": { "type": "boolean", "description": "Snap brush center to 4-stud voxel grid (default: false)." },
                    "interpolate": { "type": "boolean", "description": "Interpolate brush steps between consecutive points (default: true)." },
                    "steps_per_point": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Brush applications per point (default: 1)." }
                },
                "required": ["op"]
            }),
        },
        ToolDef {
            name: "terrain_generate".into(),
            description: "Procedurally generate multi-biome SmoothGrid terrain into Workspace.Terrain using RuStudio's Studio-matched noise generator. Undoable.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "position": { "description": "Center of generation region [x, y, z] in studs (default: [0, 0, 0])." },
                    "size": { "description": "Dimensions [sx, sy, sz] of generation region in studs (default: [256, 128, 256])." },
                    "seed": { "description": "Seed string or integer (default: 'rustudio')." },
                    "biomes": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["Arctic", "Dunes", "Canyons", "Lavascape", "Water", "Mountains", "Hills", "Plains", "Marsh"] },
                        "description": "Biomes to include (default: ['Mountains', 'Hills', 'Plains'])."
                    },
                    "biome_size": { "type": "number", "description": "Biome patch size in studs (default: 100)." },
                    "blending": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Biome blending factor 0..1 (default: 0.25)." },
                    "caves": { "type": "boolean", "description": "Carve 3D caves (default: false)." },
                    "clear_existing": { "type": "boolean", "description": "Clear existing terrain before generating (default: false)." }
                }
            }),
        },
        ToolDef {
            name: "terrain_clear".into(),
            description: "Clear all SmoothGrid terrain voxels or clear a specific world/voxel box region. Undoable.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "region": {
                        "type": "object",
                        "description": "Optional sub-region box to clear (if omitted, clears all terrain).",
                        "properties": {
                            "min": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                            "max": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                            "unit": { "type": "string", "enum": ["studs", "voxels"], "description": "Coordinate units for min/max (default: studs)." }
                        },
                        "required": ["min", "max"]
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
        "terrain_inspect" => handle_inspect(ed, args),
        "terrain_fill_shape" => handle_fill_shape(ed, ctx, args),
        "terrain_sculpt_stroke" => handle_sculpt_stroke(ed, ctx, args),
        "terrain_generate" => handle_generate(ed, ctx, args),
        "terrain_clear" => handle_clear(ed, ctx, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(result))
}

fn handle_inspect(ed: &crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let doc = match resolve_datamodel(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    let Some(ws) = doc.workspace() else {
        return ToolCallResult::error("Workspace not found in DataModel");
    };
    let Some(tref) = doc.find_child(ws, "Terrain") else {
        return ToolCallResult::json(&json!({
            "exists": false,
            "voxel_count": 0,
            "chunk_count": 0
        }));
    };
    let voxels = match decode_voxels(doc) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };
    let props = terrain_props_json(doc, tref);
    let mats_mask = terrain::materials_present(&voxels);
    let materials_present: Vec<Value> = mats_mask
        .iter()
        .enumerate()
        .filter(|(i, present)| **present && *i != 0)
        .map(|(i, _)| {
            let m = i as u8;
            json!({ "id": m, "name": terrain::material_name(m) })
        })
        .collect();
    let voxel_bounds = voxels
        .bounds()
        .map(|(lo, hi)| json!({ "min": lo, "max": hi }));
    let world_bounds = voxels
        .world_bounds()
        .map(|(lo, hi)| json!({ "min": lo, "max": hi }));

    let samples = if let Some(reg) = args.get("region").filter(|v| !v.is_null()) {
        match sample_region(&voxels, reg) {
            Ok(s) => Some(s),
            Err(e) => return ToolCallResult::error(e),
        }
    } else {
        None
    };

    let mut out = json!({
        "exists": true,
        "ref": ref_to_str(tref),
        "path": instance_path(doc, tref),
        "properties": props,
        "voxel_count": voxels.count(),
        "chunk_count": voxels.chunk_count(),
        "voxel_bounds": voxel_bounds,
        "world_bounds": world_bounds,
        "materials_present": materials_present,
    });
    if let Some(s) = samples {
        out["samples"] = s;
    }
    ToolCallResult::json(&out)
}

fn terrain_props_json(doc: &Document, tref: Ref) -> Value {
    let water_color = match doc.prop(tref, "WaterColor") {
        Some(Variant::Color3(c)) => json!([c.r, c.g, c.b]),
        Some(Variant::Color3uint8(c)) => {
            json!([c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0])
        }
        _ => json!(mech::WATER_COLOR_DEFAULT.map(|c| c as f32 / 255.0)),
    };
    let num = |name: &str, default: f32| match doc.prop(tref, name) {
        Some(Variant::Float32(f)) => f,
        Some(Variant::Float64(f)) => f as f32,
        _ => default,
    };
    let decoration = matches!(doc.prop(tref, "Decoration"), Some(Variant::Bool(true)));
    let mat_colors = doc.prop(tref, "MaterialColors").map(|v| variant_to_json(&v));
    json!({
        "WaterColor": water_color,
        "WaterWaveSize": num("WaterWaveSize", mech::WATER_WAVE_SIZE_DEFAULT),
        "WaterWaveSpeed": num("WaterWaveSpeed", mech::WATER_WAVE_SPEED_DEFAULT),
        "WaterReflectance": num("WaterReflectance", mech::WATER_REFLECTANCE_DEFAULT),
        "WaterTransparency": num("WaterTransparency", mech::WATER_TRANSPARENCY_DEFAULT),
        "Decoration": decoration,
        "GrassLength": num("GrassLength", mech::GRASS_LENGTH_DEFAULT),
        "MaterialColors": mat_colors,
    })
}

fn sample_region(v: &Voxels, reg: &Value) -> Result<Value, String> {
    let (lo, hi, unit) = parse_voxel_bounds(reg)?;
    let step = reg
        .get("step")
        .and_then(|s| s.as_u64())
        .unwrap_or(1)
        .max(1) as i32;
    let include_air = reg
        .get("include_air")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let max_samples = (reg
        .get("max_samples")
        .and_then(|n| n.as_u64())
        .unwrap_or(4096) as usize)
        .clamp(1, MAX_INSPECT_SAMPLES);

    let span = [0, 1, 2].map(|k| ((hi[k] - lo[k]) / step + 1).max(0) as u64);
    let total_steps = span[0].saturating_mul(span[1]).saturating_mul(span[2]);
    if total_steps > 2_000_000 {
        return Err(format!(
            "inspect region has {total_steps} sample grid points (exceeds 2,000,000); pass a smaller box or larger step"
        ));
    }

    let mut cells = Vec::new();
    let mut truncated = false;
    'outer: for x in (lo[0]..=hi[0]).step_by(step as usize) {
        for y in (lo[1]..=hi[1]).step_by(step as usize) {
            for z in (lo[2]..=hi[2]).step_by(step as usize) {
                let (mat, occ) = v.get(x, y, z);
                if !include_air && (mat == 0 || occ <= 0.0) {
                    continue;
                }
                if cells.len() >= max_samples {
                    truncated = true;
                    break 'outer;
                }
                let center = [
                    (x as f32 + 0.5) * VOXEL,
                    (y as f32 + 0.5) * VOXEL,
                    (z as f32 + 0.5) * VOXEL,
                ];
                cells.push(json!({
                    "cell": [x, y, z],
                    "world_center": center,
                    "material": terrain::material_name(mat),
                    "material_id": mat,
                    "occupancy": occ,
                }));
            }
        }
    }
    Ok(json!({
        "unit": unit,
        "voxel_min": lo,
        "voxel_max": hi,
        "step": step,
        "count": cells.len(),
        "truncated": truncated,
        "voxels": cells,
    }))
}

fn handle_fill_shape(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let shape = match args.get("shape").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("missing required parameter 'shape'"),
    };
    let shape_l = shape.to_ascii_lowercase();
    let default_mat = if shape_l == "water_level" {
        "Water"
    } else {
        "Grass"
    };
    let mat_id = match parse_material_arg(args.get("material"), default_mat) {
        Ok(m) => m,
        Err(e) => return ToolCallResult::error(e),
    };

    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let (doc_id, tref, mut voxels) = {
        let doc = match resolve_datamodel_mut(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let tref = match ensure_terrain_ref(doc) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(e),
        };
        let voxels = match decode_voxels(doc) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        (doc.id, tref, voxels)
    };

    let apply_res = match shape_l.as_str() {
        "block" | "wedge" => fill_box_or_wedge(&mut voxels, args, mat_id, shape_l == "wedge"),
        "ball" => fill_ball_shape(&mut voxels, args, mat_id),
        "cylinder" => fill_cylinder_shape(&mut voxels, args, mat_id),
        "water_level" => fill_water_level(&mut voxels, args, mat_id),
        other => Err(format!("unknown fill shape '{other}'")),
    };
    if let Err(e) = apply_res {
        return ToolCallResult::error(e);
    }

    commit_terrain_voxels(
        ed,
        ctx,
        dm,
        doc_id,
        tref,
        voxels,
        "Terrain Fill",
        json!({
            "shape": shape_l,
            "material": terrain::material_name(mat_id),
            "material_id": mat_id,
        }),
    )
}

fn fill_box_or_wedge(v: &mut Voxels, args: &Value, mat: u8, wedge: bool) -> Result<(), String> {
    let (xf, size) = if let (Some(vmin), Some(vmax)) = (args.get("min"), args.get("max")) {
        let a = parse_vec3(vmin, "min")?;
        let b = parse_vec3(vmax, "max")?;
        let lo = a.min(b);
        let hi = a.max(b);
        let size = (hi - lo).max(Vec3::splat(0.1));
        (
            Xform {
                pos: (lo + hi) * 0.5,
                rot: Mat3::IDENTITY,
            },
            size,
        )
    } else {
        let xf = parse_center_or_cframe(args)?;
        let size = args
            .get("size")
            .ok_or_else(|| "missing 'size' (or 'min'/'max')".to_string())
            .and_then(|s| parse_vec3(s, "size"))?
            .abs();
        (xf, size)
    };
    if size.max_element() > MAX_FILL_EXTENT_STUDS {
        return Err(format!(
            "shape size component {} exceeds {MAX_FILL_EXTENT_STUDS} studs limit",
            size.max_element()
        ));
    }
    let h = size * 0.5;
    let e = xf.rot.x_axis.abs() * h.x + xf.rot.y_axis.abs() * h.y + xf.rot.z_axis.abs() * h.z;
    let (lo, hi) = ((xf.pos - e).to_array(), (xf.pos + e).to_array());
    v.fill(lo, hi, mat, |c| {
        let p = xf.rot.transpose() * (Vec3::from(c) - xf.pos);
        let d = box_distance(p, h);
        if wedge {
            let n = Vec3::new(0.0, h.z, -h.y).normalize_or_zero();
            d.max(n.dot(p))
        } else {
            d
        }
    });
    Ok(())
}

fn fill_ball_shape(v: &mut Voxels, args: &Value, mat: u8) -> Result<(), String> {
    let center = parse_center_or_cframe(args)?.pos;
    let radius = if let Some(r) = args.get("radius").and_then(|x| x.as_f64()) {
        (r as f32).abs()
    } else if let Some(s) = args.get("size") {
        parse_vec3(s, "size")?.max_element() * 0.5
    } else {
        return Err("ball fill requires 'radius' (or 'size')".into());
    };
    if radius * 2.0 > MAX_FILL_EXTENT_STUDS {
        return Err(format!(
            "ball diameter {} exceeds {MAX_FILL_EXTENT_STUDS} studs limit",
            radius * 2.0
        ));
    }
    v.fill((center - radius).to_array(), (center + radius).to_array(), mat, |c| {
        (Vec3::from(c) - center).length() - radius
    });
    Ok(())
}

fn fill_cylinder_shape(v: &mut Voxels, args: &Value, mat: u8) -> Result<(), String> {
    let xf = parse_center_or_cframe(args)?;
    let (radius, height) = if let Some(s) = args.get("size") {
        let sz = parse_vec3(s, "size")?.abs();
        (
            args.get("radius")
                .and_then(|x| x.as_f64())
                .map(|r| r as f32)
                .unwrap_or(sz.x.max(sz.z) * 0.5),
            args.get("height")
                .and_then(|x| x.as_f64())
                .map(|h| h as f32)
                .unwrap_or(sz.y),
        )
    } else {
        let r = args
            .get("radius")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| "cylinder fill requires 'radius'".to_string())? as f32;
        let h = args
            .get("height")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| "cylinder fill requires 'height'".to_string())? as f32;
        (r.abs(), h.abs())
    };
    if radius * 2.0 > MAX_FILL_EXTENT_STUDS || height > MAX_FILL_EXTENT_STUDS {
        return Err(format!(
            "cylinder dimensions exceed {MAX_FILL_EXTENT_STUDS} studs limit"
        ));
    }
    let half = Vec3::new(radius, height * 0.5, radius);
    let e = xf.rot.x_axis.abs() * half.x + xf.rot.y_axis.abs() * half.y + xf.rot.z_axis.abs() * half.z;
    let (lo, hi) = ((xf.pos - e).to_array(), (xf.pos + e).to_array());
    v.fill(lo, hi, mat, |c| {
        let p = xf.rot.transpose() * (Vec3::from(c) - xf.pos);
        let q = glam::Vec2::new(
            glam::Vec2::new(p.x, p.z).length() - radius,
            p.y.abs() - height * 0.5,
        );
        q.max(glam::Vec2::ZERO).length() + q.max_element().min(0.0)
    });
    Ok(())
}

fn fill_water_level(v: &mut Voxels, args: &Value, mat: u8) -> Result<(), String> {
    let rb = if let (Some(vmin), Some(vmax)) = (args.get("min"), args.get("max")) {
        let a = parse_vec3(vmin, "min")?;
        let b = parse_vec3(vmax, "max")?;
        let lo = a.min(b);
        let hi = a.max(b);
        RegionBox::from_studs(((lo + hi) * 0.5).to_array(), (hi - lo).to_array())
    } else if let Some(s) = args.get("size") {
        let center = parse_center_or_cframe(args)?.pos;
        let size = parse_vec3(s, "size")?.abs();
        RegionBox::from_studs(center.to_array(), size.to_array())
    } else {
        return Err("water_level requires 'min'+'max' or 'position'+'size'".into());
    };
    let vol = rb.size[0] as u64 * rb.size[1] as u64 * rb.size[2] as u64;
    if vol > MAX_GENERATE_VOXELS as u64 {
        return Err(format!(
            "water_level region spans {vol} voxels (exceeds {MAX_GENERATE_VOXELS})"
        ));
    }
    let evaporate = args
        .get("evaporate")
        .and_then(|b| b.as_bool())
        .unwrap_or(mat == 0);
    terrain_gen::sea_level(v, rb.lo, rb.size, evaporate);
    Ok(())
}

fn box_distance(p: Vec3, h: Vec3) -> f32 {
    let q = p.abs() - h;
    q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
}

fn handle_sculpt_stroke(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let op_str = match args.get("op").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("missing required parameter 'op'"),
    };
    let mut points: Vec<Vec3> = Vec::new();
    if let Some(arr) = args.get("points").and_then(|v| v.as_array()) {
        for (idx, p) in arr.iter().enumerate() {
            match parse_vec3(p, &format!("points[{idx}]")) {
                Ok(v) => points.push(v),
                Err(e) => return ToolCallResult::error(e),
            }
        }
    } else if let Some(p) = args.get("point") {
        match parse_vec3(p, "point") {
            Ok(v) => points.push(v),
            Err(e) => return ToolCallResult::error(e),
        }
    }
    if points.is_empty() {
        return ToolCallResult::error("terrain_sculpt_stroke requires 'point' or non-empty 'points'");
    }

    let op_l = op_str.to_ascii_lowercase();
    let mut sea_level_mode = false;
    let (brush_op, subtract) = match op_l.as_str() {
        "add" => (BrushOp::Draw, false),
        "subtract" => (BrushOp::Draw, true),
        "grow" => (BrushOp::Sculpt, false),
        "erode" => (BrushOp::Sculpt, true),
        "smooth" => (BrushOp::Smooth, false),
        "flatten" => (BrushOp::Flatten, false),
        "paint" => (BrushOp::Paint, false),
        "sealevel" | "sea_level" => {
            sea_level_mode = true;
            (BrushOp::Flatten, false)
        }
        other => return ToolCallResult::error(format!("unknown sculpt op '{other}'")),
    };

    let mat = match parse_material_arg(args.get("material"), "Grass") {
        Ok(m) => m,
        Err(e) => return ToolCallResult::error(e),
    };
    let target_mat = match args.get("target_material").filter(|v| !v.is_null()) {
        Some(v) => match parse_material_arg(Some(v), "Grass") {
            Ok(m) => Some(m),
            Err(e) => return ToolCallResult::error(e),
        },
        None => None,
    };

    let size = args
        .get("brush_size")
        .or_else(|| args.get("size"))
        .and_then(|v| v.as_f64())
        .map(|s| s as f32)
        .unwrap_or(terrain_brush::DEFAULT_SIZE)
        .clamp(1.0, 64.0);
    let height = args
        .get("brush_height")
        .or_else(|| args.get("height"))
        .and_then(|v| v.as_f64())
        .map(|h| h as f32)
        .unwrap_or(size)
        .clamp(1.0, 64.0);
    let strength = args
        .get("strength")
        .and_then(|v| v.as_f64())
        .map(|s| s as f32)
        .unwrap_or(terrain_brush::DEFAULT_STRENGTH)
        .clamp(0.0, 1.0);
    let shape = match args
        .get("shape")
        .and_then(|v| v.as_str())
        .unwrap_or("Sphere")
        .to_ascii_lowercase()
        .as_str()
    {
        "sphere" | "ball" => BrushShape::Sphere,
        "cube" | "box" | "block" => BrushShape::Cube,
        "cylinder" => BrushShape::Cylinder,
        other => return ToolCallResult::error(format!("unknown brush shape '{other}'")),
    };
    let pivot = match args
        .get("pivot")
        .and_then(|v| v.as_str())
        .unwrap_or("Center")
        .to_ascii_lowercase()
        .as_str()
    {
        "top" => Pivot::Top,
        "center" => Pivot::Center,
        "bottom" => Pivot::Bottom,
        other => return ToolCallResult::error(format!("unknown brush pivot '{other}'")),
    };
    let flatten = match args
        .get("flatten_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("Both")
        .to_ascii_lowercase()
        .as_str()
    {
        "grow" => FlattenMode::Grow,
        "erode" => FlattenMode::Erode,
        "both" => FlattenMode::Both,
        other => return ToolCallResult::error(format!("unknown flatten_mode '{other}'")),
    };
    let plane_y = args
        .get("target_plane_y")
        .and_then(|v| v.as_f64())
        .map(|y| y as f32)
        .unwrap_or(points[0].y);
    let ignore_water = args
        .get("ignore_water")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let snap = args
        .get("snap_to_voxels")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let interpolate = args
        .get("interpolate")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let steps_per_point = args
        .get("steps_per_point")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 20) as usize;

    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let (doc_id, tref, mut voxels) = {
        let doc = match resolve_datamodel_mut(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let tref = match ensure_terrain_ref(doc) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(e),
        };
        let voxels = match decode_voxels(doc) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        (doc.id, tref, voxels)
    };

    let mut dirty_lo: Option<[i32; 3]> = None;
    let mut dirty_hi: Option<[i32; 3]> = None;
    let mut processed = 0usize;

    if sea_level_mode {
        let radius = size * 0.5 * VOXEL;
        let half_h = height * 0.5 * VOXEL;
        for pt in &points {
            let y_top = args
                .get("target_plane_y")
                .and_then(|v| v.as_f64())
                .map(|y| y as f32)
                .unwrap_or(pt.y);
            let min_s = Vec3::new(pt.x - radius, y_top - half_h * 2.0, pt.z - radius);
            let max_s = Vec3::new(pt.x + radius, y_top, pt.z + radius);
            let rb = RegionBox::from_studs(((min_s + max_s) * 0.5).to_array(), (max_s - min_s).to_array());
            terrain_gen::sea_level(&mut voxels, rb.lo, rb.size, subtract);
            processed += 1;
            let hi = [rb.lo[0] + rb.size[0] - 1, rb.lo[1] + rb.size[1] - 1, rb.lo[2] + rb.size[2] - 1];
            merge_ibox(&mut dirty_lo, &mut dirty_hi, rb.lo, hi);
        }
    } else {
        let mut params = BrushParams::new(brush_op);
        params.shape = shape;
        params.size = size;
        params.height = height;
        params.strength = strength;
        params.pivot = pivot;
        params.subtract = subtract;
        params.ignore_water = ignore_water;
        params.material = mat;
        params.target = target_mat;
        params.flatten = flatten;
        params.plane_y = plane_y;

        let mut st = StrokeState::default();
        let mut prev_center: Option<Vec3> = None;
        for pt in &points {
            let hit = terrain_brush::brush_center(*pt, &params, snap);
            if !interpolate {
                st.position = None;
            } else if prev_center.is_none() {
                st.position = Some(hit);
            }
            for _ in 0..steps_per_point {
                if let Some((a, b)) = terrain_brush::step(&mut voxels, &params, hit, &mut st) {
                    merge_ibox(&mut dirty_lo, &mut dirty_hi, a, b);
                }
                processed += 1;
                if !interpolate {
                    st.position = None;
                }
            }
            prev_center = Some(hit);
        }
    }

    let dirty_bounds = dirty_lo.zip(dirty_hi).map(|(lo, hi)| json!({ "min": lo, "max": hi }));
    commit_terrain_voxels(
        ed,
        ctx,
        dm,
        doc_id,
        tref,
        voxels,
        "Terrain Sculpt",
        json!({
            "op": op_str,
            "material": terrain::material_name(mat),
            "points_processed": processed,
            "dirty_voxel_bounds": dirty_bounds,
        }),
    )
}

fn handle_generate(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let pos = match args.get("position") {
        Some(v) => match parse_vec3(v, "position") {
            Ok(p) => p,
            Err(e) => return ToolCallResult::error(e),
        },
        None => Vec3::ZERO,
    };
    let size = match args.get("size") {
        Some(v) => match parse_vec3(v, "size") {
            Ok(s) => s.abs().max(Vec3::splat(VOXEL)),
            Err(e) => return ToolCallResult::error(e),
        },
        None => Vec3::new(256.0, 128.0, 256.0),
    };
    let rb = RegionBox::from_studs(pos.to_array(), size.to_array());
    let vol = rb.size[0] as u64 * rb.size[1] as u64 * rb.size[2] as u64;
    if vol > MAX_GENERATE_VOXELS as u64 {
        return ToolCallResult::error(format!(
            "terrain_generate volume {vol} voxels exceeds max {MAX_GENERATE_VOXELS} (roughly 200x100x200 voxels / 800x400x800 studs)"
        ));
    }

    let biomes = match parse_biomes(args.get("biomes")) {
        Ok(b) => b,
        Err(e) => return ToolCallResult::error(e),
    };
    let biome_size = args
        .get("biome_size")
        .and_then(|v| v.as_f64())
        .map(|x| x as f32)
        .unwrap_or(100.0)
        .clamp(terrain_gen::BIOME_SIZE_MIN, terrain_gen::BIOME_SIZE_MAX);
    let blending = args
        .get("blending")
        .and_then(|v| v.as_f64())
        .map(|x| x as f32)
        .unwrap_or(0.25)
        .clamp(0.0, 1.0);
    let caves = args.get("caves").and_then(|v| v.as_bool()).unwrap_or(false);
    let seed = parse_seed(args.get("seed"));
    let clear_existing = args
        .get("clear_existing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let (doc_id, tref, mut voxels) = {
        let doc = match resolve_datamodel_mut(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let tref = match ensure_terrain_ref(doc) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(e),
        };
        let voxels = if clear_existing {
            Voxels::empty()
        } else {
            match decode_voxels(doc) {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(e),
            }
        };
        (doc.id, tref, voxels)
    };

    let opts = GenOptions {
        biomes: biomes.clone(),
        blending,
        caves,
        biome_size,
        seed,
    };
    terrain_gen::generate(&mut voxels, rb.lo, rb.size, &opts);

    let biome_names: Vec<&str> = biomes.iter().map(|b| b.name()).collect();
    commit_terrain_voxels(
        ed,
        ctx,
        dm,
        doc_id,
        tref,
        voxels,
        "Generate Terrain",
        json!({
            "voxel_min": rb.lo,
            "voxel_size": rb.size,
            "seed": seed,
            "biomes": biome_names,
            "caves": caves,
        }),
    )
}

fn handle_clear(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let region_bounds = if let Some(reg) = args.get("region").filter(|v| !v.is_null()) {
        match parse_voxel_bounds(reg) {
            Ok((lo, hi, unit)) => {
                let sz = [0, 1, 2].map(|k| (hi[k] - lo[k] + 1).max(0));
                let vol = sz[0] as u64 * sz[1] as u64 * sz[2] as u64;
                if vol > MAX_CLEAR_VOXELS as u64 {
                    return ToolCallResult::error(format!(
                        "clear region spans {vol} voxels (exceeds {MAX_CLEAR_VOXELS})"
                    ));
                }
                Some((lo, sz, unit))
            }
            Err(e) => return ToolCallResult::error(e),
        }
    } else {
        None
    };

    let (doc_id, tref, mut voxels) = {
        let doc = match resolve_datamodel(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let Some(ws) = doc.workspace() else {
            return ToolCallResult::error("Workspace not found in DataModel");
        };
        let Some(tref) = doc.find_child(ws, "Terrain") else {
            return ToolCallResult::json(&json!({ "cleared": true, "voxel_count": 0, "chunk_count": 0 }));
        };
        let voxels = match decode_voxels(doc) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        (doc.id, tref, voxels)
    };

    let mode = if let Some((lo, sz, unit)) = region_bounds {
        terrain_region::set_box(&mut voxels, lo, sz, 0);
        json!({ "scope": "region", "unit": unit, "voxel_lo": lo, "voxel_size": sz })
    } else {
        voxels.clear();
        json!({ "scope": "all" })
    };

    commit_terrain_voxels(
        ed,
        ctx,
        dm,
        doc_id,
        tref,
        voxels,
        "Clear Terrain",
        mode,
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_terrain_voxels(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    dm: Option<&str>,
    doc_id: u64,
    tref: Ref,
    voxels: Voxels,
    label: &str,
    extra: Value,
) -> ToolCallResult {
    let encoded = voxels.encode();
    let voxel_count = voxels.count();
    let chunk_count = voxels.chunk_count();
    let world_bounds = voxels
        .world_bounds()
        .map(|(lo, hi)| json!({ "min": lo, "max": hi }));
    let (path, dirty) = {
        let doc = match resolve_datamodel_mut(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let old_voxels = decode_voxels(doc).ok();
        let dirty = terrain::dirty_chunks(old_voxels.as_ref(), &voxels).len();
        let path = instance_path(doc, tref);
        doc.set_props(
            label,
            vec![(
                tref,
                ustr("SmoothGrid"),
                Variant::BinaryString(BinaryString::from(encoded)),
            )],
            None,
        );
        (path, dirty)
    };
    if doc_id == ed.doc.id {
        ed.scene.terrain_dirty = true;
    }
    ctx.request_repaint();

    let mut out = json!({
        "ref": ref_to_str(tref),
        "path": path,
        "dirty_chunks": dirty,
        "voxel_count": voxel_count,
        "chunk_count": chunk_count,
        "world_bounds": world_bounds,
    });
    if let (Value::Object(dst), Value::Object(src)) = (&mut out, extra) {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }
    ToolCallResult::json(&out)
}

fn ensure_terrain_ref(doc: &mut Document) -> Result<Ref, String> {
    let ws = doc
        .workspace()
        .ok_or_else(|| "Workspace not found in DataModel".to_string())?;
    if let Some(t) = doc.find_child(ws, "Terrain") {
        return Ok(t);
    }
    Ok(doc.insert(ws, InstanceBuilder::new("Terrain").with_name("Terrain")))
}

fn decode_voxels(doc: &Document) -> Result<Voxels, String> {
    match Voxels::from_dom(&doc.dom) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(format!("failed to decode Terrain.SmoothGrid: {e}")),
        None => Ok(Voxels::empty()),
    }
}

fn parse_material_arg(val: Option<&Value>, default_name: &str) -> Result<u8, String> {
    let Some(v) = val.filter(|v| !v.is_null()) else {
        return terrain::material_index(default_name)
            .ok_or_else(|| format!("unknown default material '{default_name}'"));
    };
    if let Some(n) = v.as_u64() {
        let id = u8::try_from(n).map_err(|_| format!("invalid terrain material id {n}"))?;
        if id == 0 || id == WATER || terrain::material_name(id) != "Air" {
            return Ok(id);
        }
        return Err(format!("unknown terrain material id {n}"));
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        let tail = trimmed.rsplit('.').next().unwrap_or(trimmed);
        if let Some(id) = terrain::material_index(tail) {
            return Ok(id);
        }
        for i in 0u8..=22 {
            if terrain::material_name(i).eq_ignore_ascii_case(tail) {
                return Ok(i);
            }
        }
        return Err(format!("unknown terrain material '{s}'"));
    }
    Err(format!("expected string or integer terrain material, got {v}"))
}

fn parse_vec3(v: &Value, field: &str) -> Result<Vec3, String> {
    match v {
        Value::Array(a) if a.len() == 3 => Ok(Vec3::new(
            as_f32(&a[0], field)?,
            as_f32(&a[1], field)?,
            as_f32(&a[2], field)?,
        )),
        Value::Object(m) => Ok(Vec3::new(
            as_f32(m.get("x").or_else(|| m.get("X")).ok_or_else(|| format!("{field}.x missing"))?, field)?,
            as_f32(m.get("y").or_else(|| m.get("Y")).ok_or_else(|| format!("{field}.y missing"))?, field)?,
            as_f32(m.get("z").or_else(|| m.get("Z")).ok_or_else(|| format!("{field}.z missing"))?, field)?,
        )),
        _ => Err(format!("expected [x, y, z] or {{x,y,z}} for '{field}', got {v}")),
    }
}

fn as_f32(v: &Value, field: &str) -> Result<f32, String> {
    let f = v
        .as_f64()
        .ok_or_else(|| format!("expected finite number for '{field}', got {v}"))?;
    let f = f as f32;
    if !f.is_finite() {
        return Err(format!("non-finite number for '{field}'"));
    }
    Ok(f)
}

fn parse_center_or_cframe(args: &Value) -> Result<Xform, String> {
    if let Some(cfv) = args.get("cframe").filter(|v| !v.is_null()) {
        if let Ok(Variant::CFrame(cf)) =
            variants::json_to_variant(None, None, None, cfv, Some(VariantType::CFrame))
        {
            return Ok(Xform::from_cframe(&cf));
        }
        if let Ok(pos) = parse_vec3(cfv, "cframe") {
            return Ok(Xform {
                pos,
                rot: Mat3::IDENTITY,
            });
        }
        return Err(format!("invalid 'cframe': {cfv}"));
    }
    let pos = args
        .get("position")
        .or_else(|| args.get("center"))
        .ok_or_else(|| "missing 'cframe' or 'position'/'center'".to_string())
        .and_then(|v| parse_vec3(v, "position"))?;
    let rot = if let Some(ov) = args.get("orientation").filter(|v| !v.is_null()) {
        let deg = parse_vec3(ov, "orientation")?;
        Xform::from_orientation_deg(Vec3::ZERO, deg).rot
    } else {
        Mat3::IDENTITY
    };
    Ok(Xform { pos, rot })
}

fn parse_voxel_bounds(reg: &Value) -> Result<([i32; 3], [i32; 3], &'static str), String> {
    let a = parse_vec3(
        reg.get("min").ok_or("region.min is required")?,
        "region.min",
    )?;
    let b = parse_vec3(
        reg.get("max").ok_or("region.max is required")?,
        "region.max",
    )?;
    let unit = match reg
        .get("unit")
        .and_then(|u| u.as_str())
        .unwrap_or("studs")
        .to_ascii_lowercase()
        .as_str()
    {
        "studs" => "studs",
        "voxels" | "cells" => "voxels",
        other => return Err(format!("unknown region unit '{other}' (expected studs or voxels)")),
    };
    let lo_f = a.min(b);
    let hi_f = a.max(b);
    if unit == "voxels" {
        Ok((
            [lo_f.x.floor() as i32, lo_f.y.floor() as i32, lo_f.z.floor() as i32],
            [hi_f.x.ceil() as i32, hi_f.y.ceil() as i32, hi_f.z.ceil() as i32],
            "voxels",
        ))
    } else {
        Ok((
            [0, 1, 2].map(|k| (lo_f[k] / VOXEL).floor() as i32),
            [0, 1, 2].map(|k| ((hi_f[k] / VOXEL) - 1e-4).floor() as i32),
            "studs",
        ))
    }
}

fn parse_biomes(v: Option<&Value>) -> Result<Vec<Biome>, String> {
    let Some(arr) = v.filter(|v| !v.is_null()).and_then(|v| v.as_array()) else {
        return Ok(vec![Biome::Mountains, Biome::Hills, Biome::Plains]);
    };
    let mut out = Vec::new();
    for item in arr {
        let name = item
            .as_str()
            .ok_or_else(|| format!("expected biome name string, got {item}"))?;
        let found = Biome::ALL
            .iter()
            .copied()
            .find(|b| b.name().eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| format!("unknown biome '{name}'"))?;
        if !out.contains(&found) {
            out.push(found);
        }
    }
    if out.is_empty() {
        return Err("biomes array must contain at least one biome".into());
    }
    Ok(out)
}

fn parse_seed(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_u64().map(|u| u as i64).unwrap_or(0)),
        Some(Value::String(s)) => {
            if let Ok(i) = s.trim().parse::<i64>() {
                i
            } else {
                terrain_gen::master_seed(s) as i64
            }
        }
        _ => 618033988,
    }
}

fn merge_ibox(lo: &mut Option<[i32; 3]>, hi: &mut Option<[i32; 3]>, a: [i32; 3], b: [i32; 3]) {
    *lo = Some(match *lo {
        Some(prev) => [prev[0].min(a[0]), prev[1].min(a[1]), prev[2].min(a[2])],
        None => a,
    });
    *hi = Some(match *hi {
        Some(prev) => [prev[0].max(b[0]), prev[1].max(b[1]), prev[2].max(b[2])],
        None => b,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Harness, RustudioApp};

    fn test_ed() -> crate::app::Editor {
        let harness = Harness {
            shot_after: None,
            log_out: None,
            actions: Vec::new(),
            pointer: None,
            started: crate::sim_clock::now(),
            screenshot: None,
            selftest: None,
            ui_shots: None,
            screenshot_requested: false,
            start_play: false,
            bench: None,
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
            held: 0.0,
        };
        RustudioApp::new_in(&egui::Context::default(), None, None, None, harness, false).ed
    }

    fn call_tool(ed: &mut crate::app::Editor, name: &str, args: Value) -> ToolCallResult {
        let ctx = egui::Context::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        match call(ed, &ctx, name, &args, &tx) {
            Some(CallOutcome::Immediate(res)) => res,
            Some(CallOutcome::Deferred) => panic!("unexpected deferred outcome"),
            None => panic!("unhandled tool {name}"),
        }
    }

    fn parse_json(res: ToolCallResult) -> Value {
        assert!(!res.is_error, "tool returned error: {:?}", res.content);
        match &res.content[0] {
            super::super::protocol::ToolContent::Text { text } => {
                serde_json::from_str(text).unwrap()
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn schemas_cover_five_terrain_tools() {
        let list = schemas();
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "terrain_inspect",
                "terrain_fill_shape",
                "terrain_sculpt_stroke",
                "terrain_generate",
                "terrain_clear",
            ]
        );
    }

    #[test]
    fn terrain_fill_inspect_sculpt_and_clear_roundtrip() {
        let mut ed = test_ed();

        // Fill a Grass block centered at (0, 2, 0), 16x4x16 studs.
        let r = parse_json(call_tool(
            &mut ed,
            "terrain_fill_shape",
            json!({
                "shape": "block",
                "material": "Grass",
                "position": [0.0, 2.0, 0.0],
                "size": [16.0, 4.0, 16.0]
            }),
        ));
        assert!(r["voxel_count"].as_u64().unwrap() > 0);
        assert!(ed.scene.terrain_dirty);

        // Inspect with region sampling.
        let insp = parse_json(call_tool(
            &mut ed,
            "terrain_inspect",
            json!({
                "region": {
                    "min": [-8.0, 0.0, -8.0],
                    "max": [8.0, 4.0, 8.0],
                    "unit": "studs"
                }
            }),
        ));
        assert_eq!(insp["exists"], Value::Bool(true));
        assert!(insp["samples"]["count"].as_u64().unwrap() > 0);

        // Sculpt Grow stroke across two points.
        let sc = parse_json(call_tool(
            &mut ed,
            "terrain_sculpt_stroke",
            json!({
                "op": "Add",
                "points": [[0.0, 8.0, 0.0], [12.0, 8.0, 0.0]],
                "brush_size": 4.0,
                "material": "Rock"
            }),
        ));
        assert_eq!(sc["material"], "Rock");
        assert!(sc["points_processed"].as_u64().unwrap() >= 2);

        // Clear all & undo restores voxels cleanly.
        let cl = parse_json(call_tool(&mut ed, "terrain_clear", json!({})));
        assert_eq!(cl["voxel_count"], 0);
        assert_eq!(cl["chunk_count"], 0);

        ed.doc.undo();
        let after_undo = parse_json(call_tool(&mut ed, "terrain_inspect", json!({})));
        assert!(after_undo["voxel_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn terrain_generate_creates_expected_biomes() {
        let mut ed = test_ed();
        let res = parse_json(call_tool(
            &mut ed,
            "terrain_generate",
            json!({
                "position": [0.0, 0.0, 0.0],
                "size": [64.0, 64.0, 64.0],
                "seed": 618033988,
                "biomes": ["Mountains", "Hills", "Plains"],
                "blending": 0.25,
                "biome_size": 100.0
            }),
        ));
        assert!(res["voxel_count"].as_u64().unwrap() > 100);
        assert!(res["chunk_count"].as_u64().unwrap() >= 1);
    }
}
