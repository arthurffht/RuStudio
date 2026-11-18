//! World raycasting, spatial queries, procedural placement/building, and live physics tools.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;

use glam::{Mat3, Quat, Vec3};
use rbx_dom_weak::types::{Color3uint8, Enum, Ref, Variant, VariantType, Vector3};
use rbx_dom_weak::{ustr, InstanceBuilder};
use serde_json::{json, Value};

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::CallOutcome;
use super::variants::{
    self, enum_name_to_u32, enum_u32_to_name, instance_path, ref_to_str, resolve_datamodel,
    resolve_datamodel_mut, resolve_target,
};
use crate::doc::{self, Document};
use crate::math::{Ray, Xform};
use crate::physics;
use crate::play::PlaySession;
use crate::query_shapes;
use crate::runtime::domains::world::part as world_part;
use crate::runtime::Side;
use crate::scene;
use crate::terrain::{self, Voxels, WATER, VOXEL};

const DEFAULT_MAX_OVERLAP_PARTS: usize = 256;
const MAX_SCATTER_COUNT: usize = 500;
const MAX_STAIR_STEPS: usize = 200;
const MAX_ARCH_SEGMENTS: usize = 64;
const MAX_RING_SEGMENTS: usize = 256;

pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "world_raycast".into(),
            description: "Cast a single ray (origin + direction or target_pos) or a batch of rays against Workspace BaseParts (Box, Ball, Cylinder, Wedge, CornerWedge, MeshPart BVH, PhysicalConfigData) and Terrain (solid + water). Supports FilterDescendantsInstances semantics.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "origin": { "description": "Ray origin [x, y, z] in studs (single ray mode)." },
                    "direction": { "description": "Ray direction/displacement vector [dx, dy, dz]. Uses its magnitude as distance unless max_distance is provided." },
                    "target_pos": { "description": "Alternative to direction: target point [x, y, z] to cast toward." },
                    "max_distance": { "type": "number", "description": "Optional explicit ray length override in studs." },
                    "rays": {
                        "type": "array",
                        "description": "Batch ray mode: array of {origin, direction?, target_pos?, max_distance?}.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "origin": {},
                                "direction": {},
                                "target_pos": {},
                                "max_distance": { "type": "number" }
                            },
                            "required": ["origin"]
                        }
                    },
                    "filter_mode": { "type": "string", "enum": ["Exclude", "Include"], "description": "How filter_targets is applied (default: Exclude)." },
                    "filter_targets": {
                        "type": "array",
                        "description": "Refs or paths whose subtrees are excluded or included."
                    },
                    "ignore_water": { "type": "boolean", "description": "Ignore Terrain Water triangles (default: false)." },
                    "respect_can_collide": { "type": "boolean", "description": "Skip BaseParts with CanCollide = false (default: false)." }
                }
            }),
        },
        ToolDef {
            name: "world_spatial_query".into(),
            description: "Run spatial queries against Workspace geometry: bounding_box of an instance/Model/Folder subtree, box_overlap (OBB vs parts), or sphere_overlap (radius vs parts).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "operation": {
                        "type": "string",
                        "enum": ["bounding_box", "box_overlap", "sphere_overlap"],
                        "description": "Spatial query operation."
                    },
                    "target": { "description": "Target ref or path for bounding_box." },
                    "targets": { "type": "array", "description": "Multiple targets for combined bounding_box." },
                    "cframe": { "description": "Oriented query box CFrame for box_overlap." },
                    "position": { "description": "Query center [x, y, z] for box_overlap or sphere_overlap." },
                    "center": { "description": "Alias for position [x, y, z]." },
                    "orientation": { "description": "Optional Euler degrees [rx, ry, rz] for box_overlap." },
                    "size": { "description": "Box dimensions [sx, sy, sz] for box_overlap." },
                    "radius": { "type": "number", "description": "Sphere radius in studs for sphere_overlap." },
                    "filter_mode": { "type": "string", "enum": ["Exclude", "Include"] },
                    "filter_targets": { "type": "array" },
                    "respect_can_collide": { "type": "boolean" },
                    "max_parts": { "type": "integer", "minimum": 1, "maximum": 4096, "description": "Max parts returned for overlap queries (default: 256)." }
                },
                "required": ["operation"]
            }),
        },
        ToolDef {
            name: "world_place_on_surface".into(),
            description: "Place/snap target BasePart(s) or Model(s) flush onto the surface or Terrain below, computing exact oriented AABB bottom offset (min_y) so nothing floats or clips. Optionally aligns UpVector to surface normal. Undoable.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "target": { "description": "Single target BasePart/Model/Folder ref or path." },
                    "targets": { "type": "array", "description": "Multiple independent targets to place onto surface." },
                    "position": { "description": "Optional [x, z] or [x, y, z] world placement coordinate (defaults to target's XZ center)." },
                    "start_y": { "type": "number", "description": "Optional Y coordinate to cast downward from (useful inside caves/interiors)." },
                    "max_distance": { "type": "number", "description": "Max downward cast distance (default: 10000)." },
                    "y_offset": { "type": "number", "description": "Additional vertical offset after flush contact (default: 0)." },
                    "align_to_normal": { "type": "boolean", "description": "Rotate target Up axis to match surface normal before flush Y snap (default: false)." },
                    "ignore_water": { "type": "boolean", "description": "Ignore Terrain Water when finding surface (default: true)." },
                    "respect_can_collide": { "type": "boolean", "description": "Only snap onto CanCollide=true parts (default: false)." }
                }
            }),
        },
        ToolDef {
            name: "world_build_structure".into(),
            description: "Procedurally construct a clean single-undo Model of Parts/WedgeParts in Workspace (box_room, wall_run, staircase, ramp, arch, cylinder_ring, scatter with optional raycast-to-ground).".into(),
            input_schema: build_structure_schema(),
        },
        ToolDef {
            name: "physics_inspect".into(),
            description: "Inspect live Play/Run physics world state and per-part AssemblyLinearVelocity, AssemblyAngularVelocity, mass, grounded state, sleeping state, and joints/constraints.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Server", "Client"] },
                    "targets": { "type": "array", "description": "Optional specific BaseParts (or Models containing BaseParts) to inspect." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "When targets is omitted, max top moving/dynamic parts to return (default: 30)." },
                    "include_anchored": { "type": "boolean", "description": "Include anchored parts when scanning without targets (default: false)." },
                    "include_joints": { "type": "boolean", "description": "Include touching joints/constraints for each inspected part (default: true)." }
                }
            }),
        },
        ToolDef {
            name: "physics_manipulate".into(),
            description: "Manipulate live Play/Run physics bodies: set linear/angular velocity, apply impulse / angular impulse / impulse at position, wake sleeping bodies, toggle Anchored/CanCollide, or teleport CFrame.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Server", "Client"] },
                    "action": {
                        "type": "string",
                        "enum": [
                            "set_linear_velocity",
                            "set_angular_velocity",
                            "apply_impulse",
                            "apply_angular_impulse",
                            "apply_impulse_at_position",
                            "wake",
                            "set_anchored",
                            "set_can_collide",
                            "teleport"
                        ],
                        "description": "Live physics manipulation action."
                    },
                    "target": { "description": "Target BasePart (or Model with BaseParts) ref or path." },
                    "targets": { "type": "array", "description": "Multiple targets to manipulate together." },
                    "velocity": { "description": "[vx, vy, vz] for set_linear_velocity." },
                    "linear_velocity": { "description": "Alias for velocity." },
                    "angular_velocity": { "description": "[wx, wy, wz] for set_angular_velocity." },
                    "impulse": { "description": "[jx, jy, jz] for apply_impulse / apply_impulse_at_position / apply_angular_impulse." },
                    "angular_impulse": { "description": "Alias for impulse when action == apply_angular_impulse." },
                    "position": { "description": "World point [x, y, z] for apply_impulse_at_position or teleport." },
                    "cframe": { "description": "Target CFrame for teleport." },
                    "anchored": { "type": "boolean", "description": "For set_anchored." },
                    "can_collide": { "type": "boolean", "description": "For set_can_collide." }
                },
                "required": ["action"]
            }),
        },
    ]
}

fn build_structure_schema() -> Value {
    let mut props = serde_json::Map::new();
    let chunks = [
        json!({
            "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
            "kind": {
                "type": "string",
                "enum": ["box_room", "wall_run", "staircase", "ramp", "arch", "cylinder_ring", "scatter"],
                "description": "Structure generator preset."
            },
            "name": { "type": "string", "description": "Name of generated Model." },
            "parent": { "description": "Parent ref/path (defaults to Workspace)." },
            "material": { "type": "string", "description": "Part Material enum name (e.g. SmoothPlastic, Wood, Brick, Concrete)." },
            "color": { "description": "Color ([r,g,b] 0..1 or 0..255, or hex string '#rrggbb')." },
            "anchored": { "type": "boolean", "description": "Anchored property for created parts (default: true)." },
            "can_collide": { "type": "boolean", "description": "CanCollide property for created parts (default: true)." },
            "origin": { "description": "Base/center position [x, y, z] (alias: position, center)." },
            "position": {},
            "center": {},
            "size": { "description": "Dimensions [w, h, d] for box_room, or [sx, sy, sz] bounds for scatter." }
        }),
        json!({
            "wall_thickness": { "type": "number" },
            "roof": { "type": "boolean", "description": "For box_room: include ceiling slab (default: true)." },
            "doorway": { "description": "For box_room: false to disable, or {side: 'front'|'back'|'left'|'right', width?, height?}." },
            "points": { "type": "array", "description": "For wall_run: polyline points [[x, y, z], ...]." },
            "height": { "type": "number" },
            "thickness": { "type": "number" },
            "closed": { "type": "boolean", "description": "For wall_run: connect last point back to first (default: false)." },
            "pillars": { "type": "boolean", "description": "Include corner/end pillars on wall_run or arch (default: true)." },
            "direction": { "description": "Horizontal direction [dx, dz] or [dx, dy, dz] for staircase/ramp." },
            "yaw_deg": { "type": "number", "description": "Yaw angle in degrees for staircase, ramp, or arch." },
            "step_count": { "type": "integer", "minimum": 1, "maximum": 200 },
            "width": { "type": "number" }
        }),
        json!({
            "step_height": { "type": "number" },
            "step_depth": { "type": "number" },
            "length": { "type": "number" },
            "end": { "description": "End point [x, y, z] for ramp." },
            "use_wedge": { "type": "boolean", "description": "Use native WedgePart for ramp (default: true)." },
            "span": { "type": "number", "description": "Inner/total width span for arch." },
            "depth": { "type": "number" },
            "segments": { "type": "integer", "minimum": 3, "maximum": 64, "description": "Number of segments/voussoirs for arch (default: 9)." },
            "radius": { "type": "number" },
            "count": { "type": "integer", "minimum": 1, "maximum": 500 },
            "mode": { "type": "string", "enum": ["pillars", "ring_walls"], "description": "For cylinder_ring (default: pillars)." },
            "start_angle_deg": { "type": "number" },
            "arc_deg": { "type": "number" },
            "seed": { "description": "Deterministic integer seed for scatter (default: 1)." },
            "part_size_min": { "description": "Minimum [sx, sy, sz] for scattered parts." },
            "part_size_max": { "description": "Maximum [sx, sy, sz] for scattered parts." },
            "raycast_to_ground": { "type": "boolean", "description": "For scatter: raycast downward and seat flush on terrain/surface (default: true)." },
            "align_to_normal": { "type": "boolean", "description": "For scatter: align scattered items with surface normal (default: false)." }
        }),
    ];
    for chunk in chunks {
        if let Value::Object(map) = chunk {
            props.extend(map);
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": ["kind"]
    })
}

pub fn call(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    name: &str,
    args: &Value,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    let result = match name {
        "world_raycast" => handle_raycast(ed, args),
        "world_spatial_query" => handle_spatial_query(ed, args),
        "world_place_on_surface" => handle_place_on_surface(ed, ctx, args),
        "world_build_structure" => handle_build_structure(ed, ctx, args),
        "physics_inspect" => handle_physics_inspect(ed, args),
        "physics_manipulate" => handle_physics_manipulate(ed, ctx, args),
        _ => return None,
    };
    Some(CallOutcome::Immediate(result))
}

// ============================================================================
// Raycasting
// ============================================================================

#[derive(Clone, Debug)]
struct FilterSpec {
    include: bool,
    roots: Vec<Ref>,
    ignore_water: bool,
    respect_can_collide: bool,
}

#[derive(Clone, Debug)]
struct RayHit {
    instance: Ref,
    position: Vec3,
    normal: Vec3,
    distance: f32,
    material: String,
}

struct QPart {
    r: Ref,
    xf: Xform,
    size: Vec3,
    collide: bool,
    query: bool,
    mesh: bool,
    shape: u8,
    cvx: u32,
    initial: Vec3,
}

fn handle_raycast(ed: &crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let doc = match resolve_datamodel(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    let filter = match parse_filter(doc, args) {
        Ok(f) => f,
        Err(e) => return ToolCallResult::error(e),
    };

    if let Some(rays_val) = args.get("rays").filter(|v| !v.is_null()) {
        let Some(arr) = rays_val.as_array() else {
            return ToolCallResult::error("'rays' must be an array");
        };
        let parts = collect_query_parts(doc);
        let terrain = decode_terrain(doc);
        let mut results = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let (origin, dir_vec) = match parse_single_ray(item) {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(format!("rays[{i}]: {e}")),
            };
            let hit = raycast_with_cache(doc, &parts, terrain.as_ref(), origin, dir_vec, &filter);
            results.push(ray_hit_json(doc, hit.as_ref()));
        }
        return ToolCallResult::json(&json!({
            "count": results.len(),
            "hits": results,
        }));
    }

    let (origin, dir_vec) = match parse_single_ray(args) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error(e),
    };
    let hit = raycast_doc(doc, origin, dir_vec, &filter);
    ToolCallResult::json(&ray_hit_json(doc, hit.as_ref()))
}

fn parse_single_ray(args: &Value) -> Result<(Vec3, Vec3), String> {
    let origin = parse_vec3(
        args.get("origin").ok_or("missing 'origin'")?,
        "origin",
    )?;
    let max_dist = args
        .get("max_distance")
        .and_then(|v| v.as_f64())
        .map(|d| d as f32);
    if let Some(d) = max_dist {
        if !d.is_finite() || d < 0.0 {
            return Err("max_distance must be a non-negative finite number".into());
        }
    }
    let dir = if let Some(d) = args.get("direction").filter(|v| !v.is_null()) {
        let v = parse_vec3(d, "direction")?;
        if let Some(dist) = max_dist {
            v.normalize_or_zero() * dist
        } else {
            v
        }
    } else if let Some(t) = args.get("target_pos").filter(|v| !v.is_null()) {
        let delta = parse_vec3(t, "target_pos")? - origin;
        if let Some(dist) = max_dist {
            delta.normalize_or_zero() * dist
        } else {
            delta
        }
    } else {
        return Err("missing 'direction' or 'target_pos'".into());
    };
    Ok((origin, dir))
}

fn parse_filter(doc: &Document, args: &Value) -> Result<FilterSpec, String> {
    let mode = args
        .get("filter_mode")
        .or_else(|| args.get("filter_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("Exclude");
    let include = match mode.to_ascii_lowercase().as_str() {
        "exclude" | "blacklist" => false,
        "include" | "whitelist" => true,
        other => return Err(format!("unknown filter_mode '{other}' (expected Exclude or Include)")),
    };
    let mut roots = Vec::new();
    if let Some(arr) = args
        .get("filter_targets")
        .or_else(|| args.get("filter_descendants_instances"))
        .filter(|v| !v.is_null())
    {
        let items = arr
            .as_array()
            .ok_or_else(|| "filter_targets must be an array".to_string())?;
        for (i, item) in items.iter().enumerate() {
            let r = resolve_target(doc, item)
                .map_err(|e| format!("filter_targets[{i}]: {e}"))?;
            roots.push(r);
        }
    }
    Ok(FilterSpec {
        include,
        roots,
        ignore_water: args
            .get("ignore_water")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        respect_can_collide: args
            .get("respect_can_collide")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

impl FilterSpec {
    fn passes(&self, doc: &Document, r: Ref) -> bool {
        let listed = self
            .roots
            .iter()
            .any(|root| *root == r || doc.is_ancestor(*root, r));
        if self.include {
            listed
        } else {
            !listed
        }
    }
}

fn ray_hit_json(doc: &Document, hit: Option<&RayHit>) -> Value {
    match hit {
        None => json!({ "hit": false }),
        Some(h) => {
            let class = doc
                .class_of(h.instance)
                .map(|c| c.as_str().to_string())
                .unwrap_or_default();
            let name = doc
                .dom
                .get_by_ref(h.instance)
                .map(|i| i.name.clone())
                .unwrap_or_default();
            json!({
                "hit": true,
                "instance": {
                    "ref": ref_to_str(h.instance),
                    "path": instance_path(doc, h.instance),
                    "class": class,
                    "name": name,
                },
                "position": [h.position.x, h.position.y, h.position.z],
                "normal": [h.normal.x, h.normal.y, h.normal.z],
                "distance": h.distance,
                "material": h.material,
            })
        }
    }
}

fn qpart(d: &Document, r: Ref) -> Option<QPart> {
    let class = d.class_of(r)?;
    if class == "Terrain" || !doc::is_a(&class, "BasePart") {
        return None;
    }
    let xf = scene::xform_of(d, r)?;
    let size = scene::vec3_of(d.prop(r, "Size")).unwrap_or(Vec3::ONE);
    let collide = !matches!(d.prop(r, "CanCollide"), Some(Variant::Bool(false)));
    let query = !matches!(d.prop(r, "CanQuery"), Some(Variant::Bool(false)));
    let mesh_class = class == "MeshPart" || doc::is_a(&class, "PartOperation");
    let geo = if mesh_class {
        crate::physics_mesh::of_part_id(d, r)
    } else {
        None
    };
    let fidelity = if mesh_class {
        crate::physics_mesh::fidelity(d, r)
    } else {
        None
    };
    let cvx = match &geo {
        Some((crate::physics_mesh::Geometry::Pieces(_), id))
            if fidelity != Some(crate::physics_mesh::FIDELITY_BOX) =>
        {
            *id
        }
        _ => 0,
    };
    let initial = scene::vec3_of(d.prop(r, "InitialSize"))
        .or_else(|| scene::vec3_of(d.prop(r, "MeshSize")))
        .filter(|v| v.max_element() > 0.0)
        .unwrap_or(size);
    let xf = if cvx != 0 {
        xf
    } else {
        geometry_box(d, r, &class, xf, size)
    };
    let mesh = mesh_class && cvx == 0 && fidelity != Some(crate::physics_mesh::FIDELITY_BOX);
    let shape = query_shapes::kind_of(d, r, &class);
    Some(QPart {
        r,
        xf,
        size,
        collide,
        query,
        mesh,
        shape,
        cvx,
        initial,
    })
}

fn geometry_box(d: &Document, r: Ref, class: &str, xf: Xform, size: Vec3) -> Xform {
    let id = if doc::is_a(class, "PartOperation") {
        scene::union_mesh(d, r).map(|m| m.id)
    } else if class == "MeshPart" {
        scene::content_string(d.prop(r, "MeshId"))
    } else {
        None
    };
    match id {
        Some(id) => Xform {
            pos: xf.pos + xf.rot * (crate::mesh_bvh::union_offset(&id) * size),
            rot: xf.rot,
        },
        None => xf,
    }
}

fn collect_query_parts(doc: &Document) -> Vec<QPart> {
    let Some(ws) = doc.workspace() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for inst in doc.dom.descendants_of(ws) {
        if let Some(qp) = qpart(doc, inst.referent()) {
            out.push(qp);
        }
    }
    out
}

fn decode_terrain(doc: &Document) -> Option<(Ref, Voxels)> {
    let ws = doc.workspace()?;
    let tref = doc.find_child(ws, "Terrain")?;
    let v = match Voxels::from_dom(&doc.dom) {
        Some(Ok(v)) => v,
        _ => return None,
    };
    Some((tref, v))
}

fn raycast_doc(doc: &Document, origin: Vec3, dir_vec: Vec3, filter: &FilterSpec) -> Option<RayHit> {
    let parts = collect_query_parts(doc);
    let terrain = decode_terrain(doc);
    raycast_with_cache(doc, &parts, terrain.as_ref(), origin, dir_vec, filter)
}

fn raycast_with_cache(
    doc: &Document,
    parts: &[QPart],
    terrain: Option<&(Ref, Voxels)>,
    origin: Vec3,
    dir_vec: Vec3,
    filter: &FilterSpec,
) -> Option<RayHit> {
    let len = dir_vec.length();
    if !len.is_finite() || len < 1e-6 {
        return None;
    }
    let ray = Ray {
        origin,
        dir: dir_vec / len,
    };
    let mut best: Option<(f32, &QPart, Option<Vec3>)> = None;

    for p in parts {
        if (!p.query && !p.collide) || (filter.respect_can_collide && !p.collide) {
            continue;
        }
        if !filter.passes(doc, p.r) {
            continue;
        }
        let mut shape_normal: Option<Vec3> = None;
        let t = if p.cvx != 0 {
            if ray
                .hit_obb(&p.xf, p.size)
                .is_none_or(|t| best.is_some_and(|(bt, _, _)| t >= bt))
            {
                continue;
            }
            let scale = crate::physics_mesh::scale(p.initial, p.size);
            let Some(pieces) = crate::physics_mesh::pieces(p.cvx) else {
                continue;
            };
            let Some((t, n)) =
                crate::physics_mesh::ray(&pieces, &p.xf, scale, ray.origin, ray.dir, len)
            else {
                continue;
            };
            shape_normal = Some(n);
            t
        } else if p.shape != query_shapes::BOX && !p.mesh {
            let Some((t, n)) = query_shapes::ray_part(&p.xf, p.size, p.shape, ray.origin, ray.dir)
            else {
                continue;
            };
            shape_normal = Some(n);
            t
        } else if p.mesh {
            if ray.hit_obb(&p.xf, p.size).is_none() {
                continue;
            }
            let id = match doc.prop(p.r, "MeshId") {
                Some(Variant::ContentId(c)) => c.as_str().to_string(),
                _ => scene::union_mesh(doc, p.r).map(|m| m.id).unwrap_or_default(),
            };
            match (!id.is_empty())
                .then(|| crate::mesh_bvh::for_mesh(&id))
                .flatten()
            {
                Some(bvh) => {
                    let inv = p.xf.rot.transpose();
                    let size = p.size.max(Vec3::splat(1e-4));
                    let o = (inv * (ray.origin - p.xf.pos)) / size;
                    let dl = (inv * ray.dir) / size;
                    let Some((t, n)) = bvh.ray_normal(o, dl) else {
                        continue;
                    };
                    shape_normal = Some((p.xf.rot * (n / size)).normalize_or_zero());
                    t
                }
                None => {
                    let Some(t) = ray.enter_obb(&p.xf, p.size) else {
                        continue;
                    };
                    if ray_box_chord(&ray, &p.xf, p.size) <= 1e-4 {
                        continue;
                    }
                    t
                }
            }
        } else {
            let Some(t) = ray.enter_obb(&p.xf, p.size) else {
                continue;
            };
            if ray_box_chord(&ray, &p.xf, p.size) <= 1e-4 {
                continue;
            }
            t
        };

        if t >= 0.0 && t <= len && best.is_none_or(|(bt, _, _)| t < bt) {
            best = Some((t, p, shape_normal));
        }
    }

    let mut part_hit = best.map(|(t, p, shape_normal)| {
        let normal = match shape_normal {
            Some(n) => n,
            None => box_entry_normal(&ray, &p.xf, p.size),
        };
        let mat_u32 = scene::enum_u32(doc.prop(p.r, "Material")).unwrap_or(256);
        let material = enum_u32_to_name("Material", mat_u32).unwrap_or_else(|| "Plastic".into());
        RayHit {
            instance: p.r,
            position: ray.at(t),
            normal,
            distance: t,
            material,
        }
    });

    if let Some((tref, voxels)) = terrain {
        if filter.passes(doc, *tref) {
            if let Some(th) = ray_terrain(*tref, voxels, ray.origin, ray.dir, len, filter.ignore_water) {
                if part_hit.as_ref().is_none_or(|ph| th.distance < ph.distance) {
                    part_hit = Some(th);
                }
            }
        }
    }

    part_hit
}

fn box_entry_normal(ray: &Ray, xf: &Xform, size: Vec3) -> Vec3 {
    let inv = xf.rot.transpose();
    let (lo, ld) = (inv * (ray.origin - xf.pos), inv * ray.dir);
    let half = (size * 0.5).max(Vec3::splat(1e-4));
    let mut slabs = [f32::NEG_INFINITY; 3];
    for k in 0..3 {
        if ld[k].abs() >= 1e-8 {
            slabs[k] = ((-half[k] - lo[k]) / ld[k]).min((half[k] - lo[k]) / ld[k]);
        }
    }
    let enter = slabs[0].max(slabs[1]).max(slabs[2]);
    let mut n = Vec3::ZERO;
    for k in 0..3 {
        if slabs[k].is_finite() && (enter - slabs[k]) * ld[k].abs() <= 1e-4 {
            n[k] = -ld[k].signum();
        }
    }
    let local_n = if n == Vec3::ZERO { -ld } else { n };
    (xf.rot * local_n).normalize_or_zero()
}

fn ray_box_chord(ray: &Ray, xf: &Xform, size: Vec3) -> f32 {
    let inv = xf.rot.transpose();
    let (o, d) = (inv * (ray.origin - xf.pos), inv * ray.dir);
    let h = size * 0.5;
    let (mut a, mut b) = (f32::NEG_INFINITY, f32::INFINITY);
    for k in 0..3 {
        if d[k].abs() < 1e-8 {
            if o[k] < -h[k] || o[k] > h[k] {
                return 0.0;
            }
        } else {
            let (t1, t2) = ((-h[k] - o[k]) / d[k], (h[k] - o[k]) / d[k]);
            a = a.max(t1.min(t2));
            b = b.min(t1.max(t2));
        }
    }
    (b - a).max(0.0)
}

fn ray_terrain(
    tref: Ref,
    v: &Voxels,
    origin: Vec3,
    dir_n: Vec3,
    len: f32,
    ignore_water: bool,
) -> Option<RayHit> {
    let (wb_lo, wb_hi) = v.world_bounds()?;
    let lo = Vec3::from(wb_lo) - Vec3::splat(VOXEL) - Vec3::Y * terrain::WATER_RAY_DROP;
    let hi = Vec3::from(wb_hi) + Vec3::splat(VOXEL);
    let (mut t0, mut t1) = (0.0f32, len);
    for k in 0..3 {
        if dir_n[k].abs() < 1e-9 {
            if origin[k] < lo[k] || origin[k] > hi[k] {
                return None;
            }
        } else {
            let (a, b) = ((lo[k] - origin[k]) / dir_n[k], (hi[k] - origin[k]) / dir_n[k]);
            t0 = t0.max(a.min(b));
            t1 = t1.min(a.max(b));
        }
    }
    if t0 > t1 {
        return None;
    }

    let chunk_studs = VOXEL * 32.0;
    let step = chunk_studs * 0.5;
    let mut seen = HashSet::new();
    let mut best: Option<(f32, Vec3, u8)> = None;
    let mut t = t0;

    loop {
        let seg_end = (t + step).min(t1);
        let (a, b) = (origin + dir_n * t, origin + dir_n * seg_end);
        let cmin = ((a.min(b) - Vec3::splat(VOXEL)) / chunk_studs)
            .floor()
            .as_ivec3();
        let cmax = ((a.max(b) + Vec3::splat(VOXEL)) / chunk_studs)
            .floor()
            .as_ivec3();
        for cx in cmin.x..=cmax.x {
            for cy in cmin.y..=cmax.y {
                for cz in cmin.z..=cmax.z {
                    let key = [cx, cy, cz];
                    if !seen.insert(key) || !v.chunk_near(key) {
                        continue;
                    }
                    let verts = terrain::chunk_triangles(v, key, &|_| false);
                    for tri in verts.chunks_exact(3) {
                        let pts = [
                            Vec3::from(tri[0].pos),
                            Vec3::from(tri[1].pos),
                            Vec3::from(tri[2].pos),
                        ];
                        if let Some((d, w)) = ray_triangle(origin, dir_n, &pts) {
                            if d >= t0 && d <= t1 && best.is_none_or(|(bt, _, _)| d < bt) {
                                let n = (pts[1] - pts[0]).cross(pts[2] - pts[0]).normalize_or_zero();
                                let k = if w.x >= w.y && w.x >= w.z {
                                    0
                                } else if w.y >= w.z {
                                    1
                                } else {
                                    2
                                };
                                let mat = tri[0].palette[k] / 3;
                                best = Some((d, n, mat));
                            }
                        }
                    }
                    if !ignore_water {
                        for tri_raw in terrain::chunk_water_triangles(v, key) {
                            let pts = tri_raw.map(Vec3::from);
                            if let Some((d, _)) = ray_triangle(origin, dir_n, &pts) {
                                if d >= t0 && d <= t1 && best.is_none_or(|(bt, _, _)| d < bt) {
                                    let n =
                                        (pts[1] - pts[0]).cross(pts[2] - pts[0]).normalize_or_zero();
                                    best = Some((d, n, WATER));
                                }
                            }
                        }
                    }
                }
            }
        }
        if best.is_some_and(|(bt, _, _)| bt <= seg_end) || seg_end >= t1 {
            break;
        }
        t = seg_end;
    }

    best.map(|(d, n, m)| RayHit {
        instance: tref,
        position: origin + dir_n * d,
        normal: n,
        distance: d,
        material: terrain::material_name(m).to_string(),
    })
}

fn ray_triangle(origin: Vec3, dir: Vec3, tri: &[Vec3; 3]) -> Option<(f32, Vec3)> {
    let e1 = tri[1] - tri[0];
    let e2 = tri[2] - tri[0];
    let h = dir.cross(e2);
    let a = e1.dot(h);
    if a <= 1e-8 {
        return None;
    }
    let f = 1.0 / a;
    let s = origin - tri[0];
    let u = f * s.dot(h);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = f * dir.dot(q);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = f * e2.dot(q);
    (t >= 0.0).then_some((t, Vec3::new(1.0 - u - v, u, v)))
}

// ============================================================================
// Spatial Queries
// ============================================================================

fn handle_spatial_query(ed: &crate::app::Editor, args: &Value) -> ToolCallResult {
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let doc = match resolve_datamodel(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    let op = match args.get("operation").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("missing required parameter 'operation'"),
    };

    match op.to_ascii_lowercase().as_str() {
        "bounding_box" => spatial_bounding_box(doc, args),
        "box_overlap" => spatial_box_overlap(doc, args),
        "sphere_overlap" => spatial_sphere_overlap(doc, args),
        other => ToolCallResult::error(format!("unknown spatial query operation '{other}'")),
    }
}

fn spatial_bounding_box(doc: &Document, args: &Value) -> ToolCallResult {
    let targets = match resolve_targets_arg(doc, args) {
        Ok(t) => t,
        Err(e) => return ToolCallResult::error(e),
    };
    let mut part_count = 0usize;
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    let mut seen = HashSet::new();

    for root in &targets {
        for inst in doc.dom.descendants_of(*root) {
            let r = inst.referent();
            if !seen.insert(r) {
                continue;
            }
            let Some(class) = doc.class_of(r) else {
                continue;
            };
            if class == "Terrain" || !doc::is_a(&class, "BasePart") {
                continue;
            }
            let Some(xf) = scene::xform_of(doc, r) else {
                continue;
            };
            let size = scene::vec3_of(doc.prop(r, "Size")).unwrap_or(Vec3::ONE);
            let (plo, phi) = obb_world_aabb(&xf, size);
            min = min.min(plo);
            max = max.max(phi);
            part_count += 1;
        }
    }

    if part_count == 0 {
        // Check WorldPivotData fallback for single empty Model.
        if targets.len() == 1 {
            if let Some(pivot) = model_pivot_xform(doc, targets[0]) {
                return ToolCallResult::json(&json!({
                    "part_count": 0,
                    "min": [pivot.pos.x, pivot.pos.y, pivot.pos.z],
                    "max": [pivot.pos.x, pivot.pos.y, pivot.pos.z],
                    "center": [pivot.pos.x, pivot.pos.y, pivot.pos.z],
                    "size": [0.0, 0.0, 0.0],
                }));
            }
        }
        return ToolCallResult::json(&json!({
            "part_count": 0,
            "min": Value::Null,
            "max": Value::Null,
            "center": Value::Null,
            "size": Value::Null,
        }));
    }

    let center = (min + max) * 0.5;
    let size = max - min;
    let mut out = json!({
        "part_count": part_count,
        "min": [min.x, min.y, min.z],
        "max": [max.x, max.y, max.z],
        "center": [center.x, center.y, center.z],
        "size": [size.x, size.y, size.z],
    });
    if targets.len() == 1 {
        if let Some((orient_xf, orient_size)) = oriented_subtree_bounds(doc, targets[0]) {
            out["oriented_bounds"] = json!({
                "position": [orient_xf.pos.x, orient_xf.pos.y, orient_xf.pos.z],
                "orientation": orient_xf.orientation_deg().to_array(),
                "size": [orient_size.x, orient_size.y, orient_size.z],
            });
        }
    }
    ToolCallResult::json(&out)
}

fn spatial_box_overlap(doc: &Document, args: &Value) -> ToolCallResult {
    let filter = match parse_filter(doc, args) {
        Ok(f) => f,
        Err(e) => return ToolCallResult::error(e),
    };
    let xf = match parse_xform_args(args) {
        Ok(x) => x,
        Err(e) => return ToolCallResult::error(e),
    };
    let size = match args
        .get("size")
        .ok_or_else(|| "box_overlap requires 'size'".to_string())
        .and_then(|v| parse_vec3(v, "size"))
    {
        Ok(s) => s.abs(),
        Err(e) => return ToolCallResult::error(e),
    };
    let max_parts = parse_max_parts(args);
    let (qmin, qmax) = obb_world_aabb(&xf, size);

    let mut matched = Vec::new();
    let mut truncated = false;
    for p in collect_query_parts(doc) {
        if filter.respect_can_collide && !p.collide {
            continue;
        }
        if !filter.passes(doc, p.r) {
            continue;
        }
        let (plo, phi) = obb_world_aabb(&p.xf, p.size);
        if phi.cmplt(qmin).any() || plo.cmpgt(qmax).any() {
            continue;
        }
        let hit = if p.mesh {
            let id = match doc.prop(p.r, "MeshId") {
                Some(Variant::ContentId(c)) => c.as_str().to_string(),
                _ => scene::union_mesh(doc, p.r).map(|m| m.id).unwrap_or_default(),
            };
            match (!id.is_empty())
                .then(|| crate::mesh_bvh::for_mesh(&id))
                .flatten()
            {
                Some(bvh) => query_shapes::obb_mesh_bvh(&xf, size, &p.xf, p.size, &bvh),
                None => query_shapes::part_part(&xf, size, query_shapes::BOX, &p.xf, p.size, p.shape),
            }
        } else {
            query_shapes::part_part(&xf, size, query_shapes::BOX, &p.xf, p.size, p.shape)
        };
        if hit {
            if matched.len() >= max_parts {
                truncated = true;
                break;
            }
            matched.push(part_summary_json(doc, &p));
        }
    }
    ToolCallResult::json(&json!({
        "count": matched.len(),
        "truncated": truncated,
        "parts": matched,
    }))
}

fn spatial_sphere_overlap(doc: &Document, args: &Value) -> ToolCallResult {
    let filter = match parse_filter(doc, args) {
        Ok(f) => f,
        Err(e) => return ToolCallResult::error(e),
    };
    let center = match args
        .get("center")
        .or_else(|| args.get("position"))
        .ok_or_else(|| "sphere_overlap requires 'center' or 'position'".to_string())
        .and_then(|v| parse_vec3(v, "center"))
    {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(e),
    };
    let radius = match args
        .get("radius")
        .and_then(|v| v.as_f64())
        .map(|r| r as f32)
    {
        Some(r) if r.is_finite() && r >= 0.0 => r,
        _ => return ToolCallResult::error("sphere_overlap requires non-negative 'radius'"),
    };
    let max_parts = parse_max_parts(args);
    let qmin = center - Vec3::splat(radius);
    let qmax = center + Vec3::splat(radius);
    let sphere_xf = Xform {
        pos: center,
        rot: Mat3::IDENTITY,
    };
    let sphere_size = Vec3::splat(radius * 2.0);

    let mut matched = Vec::new();
    let mut truncated = false;
    for p in collect_query_parts(doc) {
        if filter.respect_can_collide && !p.collide {
            continue;
        }
        if !filter.passes(doc, p.r) {
            continue;
        }
        let (plo, phi) = obb_world_aabb(&p.xf, p.size);
        if phi.cmplt(qmin).any() || plo.cmpgt(qmax).any() {
            continue;
        }
        let hit = if p.mesh {
            let id = match doc.prop(p.r, "MeshId") {
                Some(Variant::ContentId(c)) => c.as_str().to_string(),
                _ => scene::union_mesh(doc, p.r).map(|m| m.id).unwrap_or_default(),
            };
            match (!id.is_empty())
                .then(|| crate::mesh_bvh::for_mesh(&id))
                .flatten()
            {
                Some(bvh) => obb_vs_sphere_quick(&p.xf, p.size, center, radius)
                    && query_shapes::obb_mesh_bvh(&sphere_xf, sphere_size, &p.xf, p.size, &bvh),
                None => query_shapes::part_part(
                    &sphere_xf,
                    sphere_size,
                    query_shapes::BALL,
                    &p.xf,
                    p.size,
                    p.shape,
                ),
            }
        } else {
            query_shapes::part_part(
                &sphere_xf,
                sphere_size,
                query_shapes::BALL,
                &p.xf,
                p.size,
                p.shape,
            )
        };
        if hit {
            if matched.len() >= max_parts {
                truncated = true;
                break;
            }
            matched.push(part_summary_json(doc, &p));
        }
    }
    ToolCallResult::json(&json!({
        "count": matched.len(),
        "truncated": truncated,
        "parts": matched,
    }))
}

fn obb_vs_sphere_quick(xf: &Xform, size: Vec3, center: Vec3, radius: f32) -> bool {
    let local = xf.rot.transpose() * (center - xf.pos);
    let h = size * 0.5;
    let clamped = local.clamp(-h, h);
    (local - clamped).length_squared() <= radius * radius
}

fn part_summary_json(doc: &Document, p: &QPart) -> Value {
    let class = doc
        .class_of(p.r)
        .map(|c| c.as_str().to_string())
        .unwrap_or_default();
    let name = doc
        .dom
        .get_by_ref(p.r)
        .map(|i| i.name.clone())
        .unwrap_or_default();
    json!({
        "ref": ref_to_str(p.r),
        "path": instance_path(doc, p.r),
        "class": class,
        "name": name,
        "position": [p.xf.pos.x, p.xf.pos.y, p.xf.pos.z],
        "size": [p.size.x, p.size.y, p.size.z],
    })
}

fn parse_max_parts(args: &Value) -> usize {
    (args
        .get("max_parts")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_OVERLAP_PARTS as u64) as usize)
        .clamp(1, 4096)
}

fn obb_world_aabb(xf: &Xform, size: Vec3) -> (Vec3, Vec3) {
    let h = size * 0.5;
    let e = xf.rot.x_axis.abs() * h.x + xf.rot.y_axis.abs() * h.y + xf.rot.z_axis.abs() * h.z;
    (xf.pos - e, xf.pos + e)
}

fn oriented_subtree_bounds(doc: &Document, root: Ref) -> Option<(Xform, Vec3)> {
    let basis = if doc.class_of(root).is_some_and(|c| doc::is_a(&c, "BasePart")) {
        scene::xform_of(doc, root)?
    } else if let Some(pp) = match doc.prop(root, "PrimaryPart") {
        Some(Variant::Ref(p)) if p.is_some() => scene::xform_of(doc, p),
        _ => None,
    } {
        pp
    } else {
        Xform {
            pos: Vec3::ZERO,
            rot: Mat3::IDENTITY,
        }
    };
    let inv = basis.rot.transpose();
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    let mut found = false;
    for inst in doc.dom.descendants_of(root) {
        let r = inst.referent();
        let Some(class) = doc.class_of(r) else {
            continue;
        };
        if class == "Terrain" || !doc::is_a(&class, "BasePart") {
            continue;
        }
        let Some(xf) = scene::xform_of(doc, r) else {
            continue;
        };
        let h = scene::vec3_of(doc.prop(r, "Size")).unwrap_or(Vec3::ONE) * 0.5;
        let rel_rot = inv * xf.rot;
        let rel_pos = inv * (xf.pos - basis.pos);
        let e = rel_rot.x_axis.abs() * h.x
            + rel_rot.y_axis.abs() * h.y
            + rel_rot.z_axis.abs() * h.z;
        lo = lo.min(rel_pos - e);
        hi = hi.max(rel_pos + e);
        found = true;
    }
    if !found {
        return None;
    }
    let local_center = (lo + hi) * 0.5;
    Some((
        Xform {
            pos: basis.pos + basis.rot * local_center,
            rot: basis.rot,
        },
        hi - lo,
    ))
}

fn model_pivot_xform(doc: &Document, r: Ref) -> Option<Xform> {
    match doc.prop(r, "WorldPivotData") {
        Some(Variant::OptionalCFrame(Some(cf))) => Some(Xform::from_cframe(&cf)),
        _ => match doc.prop(r, "PrimaryPart") {
            Some(Variant::Ref(pp)) if pp.is_some() => scene::xform_of(doc, pp),
            _ => None,
        },
    }
}

// ============================================================================
// Place On Surface
// ============================================================================

fn handle_place_on_surface(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let y_offset = args
        .get("y_offset")
        .and_then(|v| v.as_f64())
        .map(|y| y as f32)
        .unwrap_or(0.0);
    let align_to_normal = args
        .get("align_to_normal")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let ignore_water = args
        .get("ignore_water")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let respect_can_collide = args
        .get("respect_can_collide")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let start_y_override = args
        .get("start_y")
        .and_then(|v| v.as_f64())
        .map(|y| y as f32);
    let max_distance = args
        .get("max_distance")
        .and_then(|v| v.as_f64())
        .map(|d| (d as f32).max(1.0))
        .unwrap_or(10_000.0);
    let pos_override = match args.get("position").filter(|v| !v.is_null()) {
        Some(v) => match parse_xz_or_xyz(v) {
            Ok(p) => Some(p),
            Err(e) => return ToolCallResult::error(e),
        },
        None => None,
    };

    let doc = match resolve_datamodel(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    let targets = match resolve_targets_arg(doc, args) {
        Ok(t) => t,
        Err(e) => return ToolCallResult::error(e),
    };

    let parts_cache = collect_query_parts(doc);
    let terrain_cache = decode_terrain(doc);

    let mut all_edits: Vec<(Ref, rbx_dom_weak::Ustr, Variant)> = Vec::new();
    let mut results = Vec::new();

    for target in &targets {
        let mut target_parts: Vec<(Ref, Xform, Vec3)> = Vec::new();
        let mut target_pivots: Vec<(Ref, Xform)> = Vec::new();
        let mut seen = HashSet::new();

        for inst in doc.dom.descendants_of(*target) {
            let r = inst.referent();
            if !seen.insert(r) {
                continue;
            }
            let Some(class) = doc.class_of(r) else {
                continue;
            };
            if class != "Terrain" && doc::is_a(&class, "BasePart") {
                if let Some(xf) = scene::xform_of(doc, r) {
                    let size = scene::vec3_of(doc.prop(r, "Size")).unwrap_or(Vec3::ONE);
                    target_parts.push((r, xf, size));
                }
            } else if doc::is_a(&class, "Model") {
                if let Some(Variant::OptionalCFrame(Some(cf))) = doc.prop(r, "WorldPivotData") {
                    target_pivots.push((r, Xform::from_cframe(&cf)));
                }
            }
        }

        if target_parts.is_empty() {
            return ToolCallResult::error(format!(
                "target '{}' contains no BaseParts to place on surface",
                instance_path(doc, *target)
            ));
        }

        let (cur_min, cur_max) = parts_world_aabb(&target_parts);
        let cur_center = (cur_min + cur_max) * 0.5;

        let (tx, tz, default_oy) = match pos_override {
            Some((px, Some(py), pz)) => (px, pz, py),
            Some((px, None, pz)) => (px, pz, (cur_max.y + 50.0).max(500.0)),
            None => (cur_center.x, cur_center.z, (cur_max.y + 50.0).max(500.0)),
        };
        let oy = start_y_override.unwrap_or(default_oy);
        let filter = FilterSpec {
            include: false,
            roots: targets.clone(),
            ignore_water,
            respect_can_collide,
        };

        let hit = match raycast_with_cache(
            doc,
            &parts_cache,
            terrain_cache.as_ref(),
            Vec3::new(tx, oy, tz),
            Vec3::new(0.0, -max_distance, 0.0),
            &filter,
        ) {
            Some(h) => h,
            None => {
                return ToolCallResult::error(format!(
                    "no surface or terrain found within {max_distance} studs below ({tx:.2}, {oy:.2}, {tz:.2})"
                ))
            }
        };

        let pivot_before = Vec3::new(cur_center.x, cur_min.y, cur_center.z);
        let rot_delta = if align_to_normal {
            let up = if let Some(Variant::Ref(pp)) = doc.prop(*target, "PrimaryPart") {
                scene::xform_of(doc, pp)
                    .map(|x| x.rot.y_axis)
                    .unwrap_or(Vec3::Y)
            } else if target_parts.len() == 1 {
                target_parts[0].1.rot.y_axis
            } else {
                Vec3::Y
            };
            rotation_arc_mat3(up, hit.normal)
        } else {
            Mat3::IDENTITY
        };

        // First rotate parts around pivot_before, then compute exact oriented min_y and center XZ.
        let mut rotated_parts: Vec<(Ref, Xform, Vec3)> = target_parts
            .iter()
            .map(|&(r, xf, size)| {
                let new_xf = Xform {
                    pos: pivot_before + rot_delta * (xf.pos - pivot_before),
                    rot: (rot_delta * xf.rot),
                }
                .orthonormalized();
                (r, new_xf, size)
            })
            .collect();

        let (rot_min, rot_max) = parts_world_aabb(&rotated_parts);
        let rot_center = (rot_min + rot_max) * 0.5;
        let translation = Vec3::new(
            tx - rot_center.x,
            (hit.position.y + y_offset) - rot_min.y,
            tz - rot_center.z,
        );
        let delta_y = (hit.position.y + y_offset) - cur_min.y;

        let cf_ustr = ustr("CFrame");
        for (r, xf, _) in &mut rotated_parts {
            xf.pos += translation;
            all_edits.push((*r, cf_ustr, Variant::CFrame(xf.to_cframe())));
        }
        let pivot_ustr = ustr("WorldPivotData");
        for (mr, px) in &target_pivots {
            let new_px = Xform {
                pos: pivot_before + rot_delta * (px.pos - pivot_before) + translation,
                rot: rot_delta * px.rot,
            }
            .orthonormalized();
            all_edits.push((*mr, pivot_ustr, Variant::OptionalCFrame(Some(new_px.to_cframe()))));
        }

        let (new_min, new_max) = parts_world_aabb(&rotated_parts);
        let new_center = (new_min + new_max) * 0.5;
        let new_size = new_max - new_min;

        results.push(json!({
            "ref": ref_to_str(*target),
            "path": instance_path(doc, *target),
            "hit_instance": {
                "ref": ref_to_str(hit.instance),
                "path": instance_path(doc, hit.instance),
            },
            "hit_position": hit.position.to_array(),
            "hit_normal": hit.normal.to_array(),
            "hit_material": hit.material,
            "delta_y": delta_y,
            "new_bounds": {
                "min": new_min.to_array(),
                "max": new_max.to_array(),
                "center": new_center.to_array(),
                "size": new_size.to_array(),
            },
        }));
    }

    let doc_mut = match resolve_datamodel_mut(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    doc_mut.set_props("Place On Surface", all_edits, None);
    ctx.request_repaint();

    ToolCallResult::json(&json!({
        "placed_count": results.len(),
        "results": results,
    }))
}

fn parts_world_aabb(parts: &[(Ref, Xform, Vec3)]) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for (_, xf, size) in parts {
        let (plo, phi) = obb_world_aabb(xf, *size);
        min = min.min(plo);
        max = max.max(phi);
    }
    (min, max)
}

fn rotation_arc_mat3(from: Vec3, to: Vec3) -> Mat3 {
    let a = from.normalize_or_zero();
    let b = to.normalize_or_zero();
    if a.length_squared() < 1e-8 || b.length_squared() < 1e-8 {
        return Mat3::IDENTITY;
    }
    let dot = a.dot(b).clamp(-1.0, 1.0);
    if dot > 0.99999 {
        return Mat3::IDENTITY;
    }
    if dot < -0.99999 {
        let ortho = if a.x.abs() < 0.9 { Vec3::X } else { Vec3::Z };
        let axis = a.cross(ortho).normalize();
        return Mat3::from_quat(Quat::from_axis_angle(axis, std::f32::consts::PI));
    }
    Mat3::from_quat(Quat::from_rotation_arc(a, b))
}

// ============================================================================
// Procedural 3D Builder
// ============================================================================

#[derive(Clone, Copy)]
struct PartStyle {
    color: [u8; 3],
    material: Option<u32>,
    anchored: bool,
    can_collide: bool,
}

struct PartSpec {
    name: String,
    class: &'static str,
    shape: u32,
    xf: Xform,
    size: Vec3,
    style_override: Option<PartStyle>,
}

fn handle_build_structure(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let kind = match args.get("kind").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => return ToolCallResult::error("missing required parameter 'kind'"),
    };
    let kind_l = kind.to_ascii_lowercase();
    let style = match parse_part_style(args) {
        Ok(s) => s,
        Err(e) => return ToolCallResult::error(e),
    };

    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let (specs, parts_cache, terrain_cache) = {
        let doc = match resolve_datamodel(ed, dm) {
            Ok(d) => d,
            Err(e) => return ToolCallResult::error(e),
        };
        let need_ray = kind_l == "scatter"
            && args
                .get("raycast_to_ground")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
        let parts = if need_ray {
            collect_query_parts(doc)
        } else {
            Vec::new()
        };
        let terrain = if need_ray { decode_terrain(doc) } else { None };
        let specs = match generate_structure_specs(doc, &parts, terrain.as_ref(), &kind_l, args, style)
        {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(e),
        };
        (specs, parts.len(), terrain.is_some())
    };
    let _ = (parts_cache, terrain_cache);

    if specs.is_empty() {
        return ToolCallResult::error("structure parameters produced zero parts");
    }

    let model_name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Structure_{kind_l}"));

    let doc_mut = match resolve_datamodel_mut(ed, dm) {
        Ok(d) => d,
        Err(e) => return ToolCallResult::error(e),
    };
    let parent = match args.get("parent").filter(|v| !v.is_null()) {
        Some(v) => match resolve_target(doc_mut, v) {
            Ok(p) => p,
            Err(e) => return ToolCallResult::error(format!("parent: {e}")),
        },
        None => match doc_mut.workspace() {
            Some(ws) => ws,
            None => return ToolCallResult::error("Workspace not found in DataModel"),
        },
    };

    let model_ref = doc_mut
        .dom
        .insert(parent, InstanceBuilder::new("Model").with_name(&model_name));
    let mut created_parts = Vec::with_capacity(specs.len());
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);

    for sp in &specs {
        let st = sp.style_override.unwrap_or(style);
        let builder = build_part_instance(sp, st);
        let pr = doc_mut.dom.insert(model_ref, builder);
        created_parts.push(pr);
        let (plo, phi) = obb_world_aabb(&sp.xf, sp.size);
        min = min.min(plo);
        max = max.max(phi);
    }

    if let Some(&first_part) = created_parts.first() {
        if let Some(m) = doc_mut.dom.get_by_ref_mut(model_ref) {
            m.properties
                .insert(ustr("PrimaryPart"), Variant::Ref(first_part));
        }
    }
    // Record as a single atomic undoable creation step of the Model subtree.
    doc_mut.record_drag(
        &format!("Build {kind_l}"),
        Vec::new(),
        vec![(model_ref, parent)],
    );
    ctx.request_repaint();

    let center = (min + max) * 0.5;
    let size = max - min;
    ToolCallResult::json(&json!({
        "kind": kind_l,
        "model": {
            "ref": ref_to_str(model_ref),
            "path": instance_path(doc_mut, model_ref),
            "name": model_name,
        },
        "part_count": created_parts.len(),
        "bounds": {
            "min": min.to_array(),
            "max": max.to_array(),
            "center": center.to_array(),
            "size": size.to_array(),
        }
    }))
}

fn build_part_instance(sp: &PartSpec, st: PartStyle) -> InstanceBuilder {
    let mut b = InstanceBuilder::new(sp.class)
        .with_name(&sp.name)
        .with_property("Anchored", st.anchored)
        .with_property("CanCollide", st.can_collide)
        .with_property("Size", Vector3::new(sp.size.x, sp.size.y, sp.size.z))
        .with_property("CFrame", sp.xf.to_cframe())
        .with_property(
            "Color",
            Color3uint8::new(st.color[0], st.color[1], st.color[2]),
        );
    if sp.class == "Part" && sp.shape != 1 {
        b.add_property("Shape", Enum::from_u32(sp.shape));
    }
    if let Some(m) = st.material {
        b.add_property("Material", Enum::from_u32(m));
    }
    b
}

fn generate_structure_specs(
    doc: &Document,
    parts_cache: &[QPart],
    terrain_cache: Option<&(Ref, Voxels)>,
    kind: &str,
    args: &Value,
    style: PartStyle,
) -> Result<Vec<PartSpec>, String> {
    let origin = args
        .get("origin")
        .or_else(|| args.get("position"))
        .or_else(|| args.get("center"))
        .map(|v| parse_vec3(v, "origin"))
        .transpose()?
        .unwrap_or(Vec3::ZERO);

    match kind {
        "box_room" => build_box_room(origin, args),
        "wall_run" => build_wall_run(origin, args),
        "staircase" => build_staircase(origin, args),
        "ramp" => build_ramp(origin, args),
        "arch" => build_arch(origin, args),
        "cylinder_ring" => build_cylinder_ring(origin, args),
        "scatter" => build_scatter(doc, parts_cache, terrain_cache, origin, args, style),
        other => Err(format!("unknown structure kind '{other}'")),
    }
}

fn box_spec(name: impl Into<String>, pos: Vec3, size: Vec3) -> PartSpec {
    PartSpec {
        name: name.into(),
        class: "Part",
        shape: 1,
        xf: Xform {
            pos,
            rot: Mat3::IDENTITY,
        },
        size: size.max(Vec3::splat(0.1)),
        style_override: None,
    }
}

fn build_box_room(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let sz = args
        .get("size")
        .map(|v| parse_vec3(v, "size"))
        .transpose()?
        .unwrap_or(Vec3::new(32.0, 14.0, 32.0))
        .abs()
        .max(Vec3::splat(2.0));
    let wt = opt_pos_f32(args, "wall_thickness", 1.5);
    let roof = args.get("roof").and_then(|v| v.as_bool()).unwrap_or(true);

    // origin is the center of the floor top or room bottom center: use origin as bottom-center.
    let cx = origin.x;
    let cy = origin.y;
    let cz = origin.z;
    let hw = sz.x * 0.5;
    let hd = sz.z * 0.5;
    let wall_h = (sz.y - wt * if roof { 2.0 } else { 1.0 }).max(1.0);
    let wall_y = cy + wt + wall_h * 0.5;

    let mut out = Vec::new();
    out.push(box_spec("Floor", Vec3::new(cx, cy + wt * 0.5, cz), Vec3::new(sz.x, wt, sz.z)));
    if roof {
        out.push(box_spec(
            "Roof",
            Vec3::new(cx, cy + wt + wall_h + wt * 0.5, cz),
            Vec3::new(sz.x, wt, sz.z),
        ));
    }

    let (door_side, door_w, door_h) = match args.get("doorway") {
        Some(Value::Bool(false)) => (None, 0.0, 0.0),
        Some(Value::Object(m)) => {
            let side = m
                .get("side")
                .and_then(|s| s.as_str())
                .unwrap_or("front")
                .to_ascii_lowercase();
            let dw = m
                .get("width")
                .and_then(|v| v.as_f64())
                .map(|x| x as f32)
                .unwrap_or((sz.x * 0.25).clamp(3.0, 8.0));
            let dh = m
                .get("height")
                .and_then(|v| v.as_f64())
                .map(|x| x as f32)
                .unwrap_or((wall_h * 0.7).min(wall_h - 0.5).max(1.0));
            (Some(side), dw, dh)
        }
        _ => (
            Some("front".to_string()),
            (sz.x * 0.25).clamp(3.0, 8.0),
            (wall_h * 0.7).min(wall_h - 0.5).max(1.0),
        ),
    };

    let walls = [
        ("front", "Wall_Front", Vec3::new(cx, wall_y, cz + hd - wt * 0.5), sz.x, true),
        ("back", "Wall_Back", Vec3::new(cx, wall_y, cz - hd + wt * 0.5), sz.x, true),
        ("left", "Wall_Left", Vec3::new(cx - hw + wt * 0.5, wall_y, cz), (sz.z - wt * 2.0).max(0.5), false),
        ("right", "Wall_Right", Vec3::new(cx + hw - wt * 0.5, wall_y, cz), (sz.z - wt * 2.0).max(0.5), false),
    ];

    for (key, wname, center, span, along_x) in walls {
        if door_side.as_deref() == Some(key) && door_w < span - 1.0 && door_h < wall_h {
            let side_len = (span - door_w) * 0.5;
            let offset = (door_w + side_len) * 0.5;
            let lintel_h = wall_h - door_h;
            if along_x {
                out.push(box_spec(
                    format!("{wname}_A"),
                    center - Vec3::X * offset,
                    Vec3::new(side_len, wall_h, wt),
                ));
                out.push(box_spec(
                    format!("{wname}_B"),
                    center + Vec3::X * offset,
                    Vec3::new(side_len, wall_h, wt),
                ));
                if lintel_h > 0.1 {
                    out.push(box_spec(
                        format!("{wname}_Header"),
                        Vec3::new(center.x, cy + wt + door_h + lintel_h * 0.5, center.z),
                        Vec3::new(door_w, lintel_h, wt),
                    ));
                }
            } else {
                out.push(box_spec(
                    format!("{wname}_A"),
                    center - Vec3::Z * offset,
                    Vec3::new(wt, wall_h, side_len),
                ));
                out.push(box_spec(
                    format!("{wname}_B"),
                    center + Vec3::Z * offset,
                    Vec3::new(wt, wall_h, side_len),
                ));
                if lintel_h > 0.1 {
                    out.push(box_spec(
                        format!("{wname}_Header"),
                        Vec3::new(center.x, cy + wt + door_h + lintel_h * 0.5, center.z),
                        Vec3::new(wt, lintel_h, door_w),
                    ));
                }
            }
        } else {
            let size = if along_x {
                Vec3::new(span, wall_h, wt)
            } else {
                Vec3::new(wt, wall_h, span)
            };
            out.push(box_spec(wname, center, size));
        }
    }

    Ok(out)
}

fn build_wall_run(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let pts_val = args
        .get("points")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "wall_run requires 'points' array of [x, y, z]".to_string())?;
    let mut pts = Vec::with_capacity(pts_val.len());
    for (i, v) in pts_val.iter().enumerate() {
        pts.push(parse_vec3(v, &format!("points[{i}]"))?);
    }
    if pts.len() < 2 {
        return Err("wall_run requires at least 2 points".into());
    }
    let height = opt_pos_f32(args, "height", 10.0);
    let thickness = opt_pos_f32(args, "thickness", 1.5);
    let closed = args.get("closed").and_then(|v| v.as_bool()).unwrap_or(false);
    let pillars = args.get("pillars").and_then(|v| v.as_bool()).unwrap_or(true);

    let mut out = Vec::new();
    let seg_count = if closed { pts.len() } else { pts.len() - 1 };
    for i in 0..seg_count {
        let a = pts[i] + origin;
        let b = pts[(i + 1) % pts.len()] + origin;
        let delta = b - a;
        let len = delta.length();
        if len < 0.05 {
            continue;
        }
        let mid = (a + b) * 0.5 + Vec3::Y * (height * 0.5);
        let dir = delta / len;
        let right = dir.cross(Vec3::Y).normalize_or_zero();
        let right = if right.length_squared() < 1e-6 {
            Vec3::X
        } else {
            right
        };
        let up = right.cross(dir).normalize_or_zero();
        let rot = Mat3::from_cols(right, up, dir);
        out.push(PartSpec {
            name: format!("Wall_{i}"),
            class: "Part",
            shape: 1,
            xf: Xform { pos: mid, rot }.orthonormalized(),
            size: Vec3::new(thickness, height, len),
            style_override: None,
        });
    }
    if pillars {
        let pw = thickness * 1.25;
        for (i, p) in pts.iter().enumerate() {
            let wp = *p + origin + Vec3::Y * (height * 0.5);
            out.push(box_spec(format!("Pillar_{i}"), wp, Vec3::new(pw, height + 0.25, pw)));
        }
    }
    Ok(out)
}

fn build_staircase(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let steps = (args
        .get("step_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize)
        .clamp(1, MAX_STAIR_STEPS);
    let width = opt_pos_f32(args, "width", 6.0);
    let step_h = opt_pos_f32(args, "step_height", 1.0);
    let step_d = opt_pos_f32(args, "step_depth", 1.5);
    let forward = parse_horiz_dir(args);
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    let rot = Mat3::from_cols(right, Vec3::Y, forward);

    let mut out = Vec::with_capacity(steps);
    for i in 0..steps {
        let k = i as f32;
        let pos = origin + forward * ((k + 0.5) * step_d) + Vec3::Y * ((k + 0.5) * step_h);
        out.push(PartSpec {
            name: format!("Step_{i}"),
            class: "Part",
            shape: 1,
            xf: Xform { pos, rot }.orthonormalized(),
            size: Vec3::new(width, step_h, step_d),
            style_override: None,
        });
    }
    Ok(out)
}

fn build_ramp(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let use_wedge = args
        .get("use_wedge")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let width = opt_pos_f32(args, "width", 8.0);

    let (pos, rot, length, height) = if let Some(end_v) = args.get("end").filter(|v| !v.is_null()) {
        let end = parse_vec3(end_v, "end")?;
        let horiz = Vec3::new(end.x - origin.x, 0.0, end.z - origin.z);
        let len = horiz.length().max(0.5);
        let fwd = horiz / len;
        let h = (end.y - origin.y).abs().max(0.2);
        let mid_y = origin.y.min(end.y) + h * 0.5;
        let mid = Vec3::new((origin.x + end.x) * 0.5, mid_y, (origin.z + end.z) * 0.5);
        // For WedgePart, full height is at +Z, thin tip is at -Z.
        let wedge_fwd = if end.y >= origin.y { fwd } else { -fwd };
        let r_col = wedge_fwd.cross(Vec3::Y).normalize_or_zero();
        (mid, Mat3::from_cols(r_col, Vec3::Y, wedge_fwd), len, h)
    } else {
        let length = opt_pos_f32(args, "length", 20.0);
        let height = opt_pos_f32(args, "height", 8.0);
        let fwd = parse_horiz_dir(args);
        let right = fwd.cross(Vec3::Y).normalize_or_zero();
        let pos = origin + fwd * (length * 0.5) + Vec3::Y * (height * 0.5);
        (pos, Mat3::from_cols(right, Vec3::Y, fwd), length, height)
    };

    if use_wedge {
        Ok(vec![PartSpec {
            name: "Ramp".into(),
            class: "WedgePart",
            shape: 3,
            xf: Xform { pos, rot }.orthonormalized(),
            size: Vec3::new(width, height, length),
            style_override: None,
        }])
    } else {
        let diag = (length * length + height * height).sqrt();
        let pitch = (height / length).atan();
        let pitched = rot * Mat3::from_rotation_x(-pitch);
        let thickness = opt_pos_f32(args, "thickness", 1.0);
        Ok(vec![PartSpec {
            name: "RampSlab".into(),
            class: "Part",
            shape: 1,
            xf: Xform { pos, rot: pitched }.orthonormalized(),
            size: Vec3::new(width, thickness, diag),
            style_override: None,
        }])
    }
}

fn build_arch(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let span = opt_pos_f32(args, "span", 16.0);
    let height = opt_pos_f32(args, "height", 14.0);
    let thickness = opt_pos_f32(args, "thickness", 2.0);
    let depth = opt_pos_f32(args, "depth", 4.0);
    let segments = (args
        .get("segments")
        .and_then(|v| v.as_u64())
        .unwrap_or(9) as usize)
        .clamp(3, MAX_ARCH_SEGMENTS);
    let pillars = args.get("pillars").and_then(|v| v.as_bool()).unwrap_or(true);

    let yaw = args
        .get("yaw_deg")
        .and_then(|v| v.as_f64())
        .map(|y| (y as f32).to_radians())
        .unwrap_or(0.0);
    let base_rot = Mat3::from_rotation_y(yaw);
    let radius = span * 0.5;
    let spring_h = (height - radius).max(0.0);

    let mut out = Vec::new();
    if pillars && spring_h > 0.1 {
        for (name, sign) in [("Pillar_Left", -1.0f32), ("Pillar_Right", 1.0)] {
            let local = Vec3::new(sign * radius, spring_h * 0.5, 0.0);
            out.push(PartSpec {
                name: name.into(),
                class: "Part",
                shape: 1,
                xf: Xform {
                    pos: origin + base_rot * local,
                    rot: base_rot,
                },
                size: Vec3::new(thickness, spring_h, depth),
                style_override: None,
            });
        }
    }

    let d_theta = std::f32::consts::PI / segments as f32;
    let seg_len = 2.0 * radius * (d_theta * 0.5).sin() * 1.08;
    for i in 0..segments {
        let theta = (i as f32 + 0.5) * d_theta;
        let local = Vec3::new(-theta.cos() * radius, spring_h + theta.sin() * radius, 0.0);
        // Tangent angle in XY plane.
        let tilt = Mat3::from_rotation_z(-(std::f32::consts::FRAC_PI_2 - theta));
        out.push(PartSpec {
            name: format!("ArchSeg_{i}"),
            class: "Part",
            shape: 1,
            xf: Xform {
                pos: origin + base_rot * local,
                rot: base_rot * tilt,
            }
            .orthonormalized(),
            size: Vec3::new(seg_len, thickness, depth),
            style_override: None,
        });
    }
    Ok(out)
}

fn build_cylinder_ring(origin: Vec3, args: &Value) -> Result<Vec<PartSpec>, String> {
    let radius = opt_pos_f32(args, "radius", 20.0);
    let count = (args.get("count").and_then(|v| v.as_u64()).unwrap_or(12) as usize)
        .clamp(3, MAX_RING_SEGMENTS);
    let height = opt_pos_f32(args, "height", 8.0);
    let thickness = opt_pos_f32(args, "thickness", 2.0);
    let width = opt_pos_f32(args, "width", thickness);
    let start_deg = args
        .get("start_angle_deg")
        .and_then(|v| v.as_f64())
        .map(|x| x as f32)
        .unwrap_or(0.0);
    let arc_deg = args
        .get("arc_deg")
        .and_then(|v| v.as_f64())
        .map(|x| x as f32)
        .unwrap_or(360.0);
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("pillars")
        .to_ascii_lowercase();

    let arc = arc_deg.to_radians();
    let start = start_deg.to_radians();
    let full = (arc_deg.abs() - 360.0).abs() < 1e-2;
    let denom = if full { count as f32 } else { (count - 1).max(1) as f32 };
    let seg_w = if mode == "ring_walls" {
        (2.0 * radius * (arc / (2.0 * count as f32)).abs().sin()).max(width) * 1.05
    } else {
        width
    };

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let ang = start + arc * (i as f32 / denom);
        let radial = Vec3::new(ang.cos(), 0.0, ang.sin());
        let tangent = Vec3::new(-ang.sin(), 0.0, ang.cos());
        let rot = Mat3::from_cols(tangent, Vec3::Y, radial);
        let pos = origin + radial * radius + Vec3::Y * (height * 0.5);
        out.push(PartSpec {
            name: format!("RingSeg_{i}"),
            class: "Part",
            shape: 1,
            xf: Xform { pos, rot }.orthonormalized(),
            size: Vec3::new(seg_w, height, thickness),
            style_override: None,
        });
    }
    Ok(out)
}

fn build_scatter(
    doc: &Document,
    parts_cache: &[QPart],
    terrain_cache: Option<&(Ref, Voxels)>,
    origin: Vec3,
    args: &Value,
    _style: PartStyle,
) -> Result<Vec<PartSpec>, String> {
    let count = (args.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize)
        .clamp(1, MAX_SCATTER_COUNT);
    let bounds = args
        .get("size")
        .map(|v| parse_vec3(v, "size"))
        .transpose()?
        .unwrap_or_else(|| {
            let r = args
                .get("radius")
                .and_then(|v| v.as_f64())
                .map(|x| x as f32)
                .unwrap_or(40.0);
            Vec3::new(r * 2.0, 0.0, r * 2.0)
        })
        .abs();
    let s_min = args
        .get("part_size_min")
        .map(|v| parse_vec3(v, "part_size_min"))
        .transpose()?
        .unwrap_or(Vec3::new(2.0, 2.0, 2.0))
        .max(Vec3::splat(0.2));
    let s_max = args
        .get("part_size_max")
        .map(|v| parse_vec3(v, "part_size_max"))
        .transpose()?
        .unwrap_or(Vec3::new(5.0, 6.0, 5.0))
        .max(s_min);
    let ray_ground = args
        .get("raycast_to_ground")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let align_normal = args
        .get("align_to_normal")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let shape = match args
        .get("shape")
        .and_then(|v| v.as_str())
        .unwrap_or("Block")
        .to_ascii_lowercase()
        .as_str()
    {
        "ball" | "sphere" => 0,
        "cylinder" => 2,
        _ => 1,
    };

    let mut rng: u64 = args
        .get("seed")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .wrapping_add(0x9e3779b97f4a7c15);
    let mut next_f32 = || -> f32 {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        let u = rng.wrapping_mul(0x2545F4914F6CDD1D);
        ((u >> 40) as f32) / ((1u64 << 24) as f32)
    };

    let filter = FilterSpec {
        include: false,
        roots: Vec::new(),
        ignore_water: true,
        respect_can_collide: false,
    };

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let rx = (next_f32() - 0.5) * bounds.x;
        let rz = (next_f32() - 0.5) * bounds.z;
        let sx = s_min.x + (s_max.x - s_min.x) * next_f32();
        let sy = s_min.y + (s_max.y - s_min.y) * next_f32();
        let sz = s_min.z + (s_max.z - s_min.z) * next_f32();
        let item_size = Vec3::new(sx, sy, sz);
        let yaw = next_f32() * std::f32::consts::TAU;
        let mut rot = Mat3::from_rotation_y(yaw);

        let mut pos = Vec3::new(origin.x + rx, origin.y + sy * 0.5, origin.z + rz);
        if ray_ground {
            let cast_y = origin.y + bounds.y.max(500.0);
            if let Some(hit) = raycast_with_cache(
                doc,
                parts_cache,
                terrain_cache,
                Vec3::new(pos.x, cast_y, pos.z),
                Vec3::new(0.0, -10_000.0, 0.0),
                &filter,
            ) {
                if align_normal {
                    rot = Xform {
                        pos: Vec3::ZERO,
                        rot: rotation_arc_mat3(Vec3::Y, hit.normal) * rot,
                    }
                    .orthonormalized()
                    .rot;
                }
                let h = item_size * 0.5;
                let half_ext_y = (rot.x_axis.y * h.x).abs()
                    + (rot.y_axis.y * h.y).abs()
                    + (rot.z_axis.y * h.z).abs();
                pos.y = hit.position.y + half_ext_y;
            }
        }
        out.push(PartSpec {
            name: format!("Item_{i}"),
            class: "Part",
            shape,
            xf: Xform { pos, rot }.orthonormalized(),
            size: item_size,
            style_override: None,
        });
    }
    Ok(out)
}

fn opt_pos_f32(args: &Value, key: &str, default: f32) -> f32 {
    args.get(key)
        .and_then(|v| v.as_f64())
        .map(|x| (x as f32).abs().max(0.05))
        .unwrap_or(default)
}

fn parse_horiz_dir(args: &Value) -> Vec3 {
    if let Some(dv) = args.get("direction").filter(|v| !v.is_null()) {
        if let Ok(v) = parse_vec3(dv, "direction") {
            let h = Vec3::new(v.x, 0.0, v.z);
            if h.length_squared() > 1e-6 {
                return h.normalize();
            }
        }
        if let Some(arr) = dv.as_array() {
            if arr.len() == 2 {
                if let (Some(x), Some(z)) = (arr[0].as_f64(), arr[1].as_f64()) {
                    let h = Vec3::new(x as f32, 0.0, z as f32);
                    if h.length_squared() > 1e-6 {
                        return h.normalize();
                    }
                }
            }
        }
    }
    if let Some(yaw) = args.get("yaw_deg").and_then(|v| v.as_f64()) {
        let rad = (yaw as f32).to_radians();
        return Vec3::new(-rad.sin(), 0.0, -rad.cos()).normalize();
    }
    Vec3::new(0.0, 0.0, -1.0)
}

fn parse_part_style(args: &Value) -> Result<PartStyle, String> {
    let color = match args.get("color").filter(|v| !v.is_null()) {
        Some(v) => match variants::json_to_variant(None, Some("Part"), Some("Color"), v, Some(VariantType::Color3uint8)) {
            Ok(Variant::Color3uint8(c)) => [c.r, c.g, c.b],
            Ok(Variant::Color3(c)) => [
                (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            ],
            Ok(other) => return Err(format!("unexpected Color variant {other:?}")),
            Err(e) => return Err(format!("color: {e}")),
        },
        None => [163, 162, 165],
    };
    let material = match args.get("material").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            let m = enum_name_to_u32(Some("Material"), s.trim())
                .ok_or_else(|| format!("unknown Enum.Material '{s}'"))?;
            Some(m)
        }
        Some(Value::Number(n)) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
        Some(other) => return Err(format!("invalid material '{other}'")),
        None => None,
    };
    Ok(PartStyle {
        color,
        material,
        anchored: args
            .get("anchored")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        can_collide: args
            .get("can_collide")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    })
}

// ============================================================================
// Live Physics Inspect & Manipulate
// ============================================================================

fn handle_physics_inspect(ed: &crate::app::Editor, args: &Value) -> ToolCallResult {
    let Some(play) = ed.play.as_ref() else {
        return ToolCallResult::error(
            "Not in Play/Run mode; physics simulation is only active while playing",
        );
    };
    let dm = args.get("datamodel").and_then(|v| v.as_str());
    let (side, doc) = match resolve_play_doc(ed, play, dm) {
        Ok(x) => x,
        Err(e) => return ToolCallResult::error(e),
    };

    let limit = (args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(30) as usize)
        .clamp(1, 200);
    let include_anchored = args
        .get("include_anchored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let include_joints = args
        .get("include_joints")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let (dyn_asms, fixed_colliders) = play.physics.counts();
    let awake_samples = play.physics.awake_sample(usize::MAX);
    let awake_first_parts: HashSet<Ref> = awake_samples.iter().map(|(r, _, _, _)| *r).collect();

    let server_doc: &Document = if play.showing == Side::Server {
        &ed.doc
    } else {
        ed.play_other_doc().unwrap_or(doc)
    };

    let joints_by_part = if include_joints {
        collect_joints_map(doc)
    } else {
        HashMap::new()
    };

    let part_refs: Vec<Ref> = if args
        .get("targets")
        .or_else(|| args.get("target"))
        .is_some_and(|v| !v.is_null())
    {
        let targets = match resolve_targets_arg(doc, args) {
            Ok(t) => t,
            Err(e) => return ToolCallResult::error(e),
        };
        let mut list = Vec::new();
        let mut seen = HashSet::new();
        for t in targets {
            for inst in doc.dom.descendants_of(t) {
                let r = inst.referent();
                if seen.insert(r)
                    && doc
                        .class_of(r)
                        .is_some_and(|c| c != "Terrain" && doc::is_a(&c, "BasePart"))
                {
                    list.push(r);
                }
            }
        }
        list
    } else if let Some(ws) = doc.workspace() {
        let mut candidates = Vec::new();
        for inst in doc.dom.descendants_of(ws) {
            let r = inst.referent();
            let Some(class) = doc.class_of(r) else {
                continue;
            };
            if class == "Terrain" || !doc::is_a(&class, "BasePart") {
                continue;
            }
            let anchored = matches!(doc.prop(r, "Anchored"), Some(Variant::Bool(true)));
            if anchored && !include_anchored {
                continue;
            }
            let srv_r = map_to_server(play, side, r);
            let speed = srv_r
                .and_then(|sr| play.physics.part_motion(sr))
                .map(|(v, _)| v.length_squared())
                .unwrap_or(0.0);
            candidates.push((r, !anchored, speed));
        }
        candidates.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        candidates.into_iter().take(limit).map(|(r, _, _)| r).collect()
    } else {
        Vec::new()
    };

    let char_model = play.character.as_ref().map(|c| c.model);
    let mut part_entries = Vec::with_capacity(part_refs.len());
    for r in part_refs {
        part_entries.push(inspect_part_json(
            doc,
            server_doc,
            play,
            side,
            r,
            &awake_first_parts,
            char_model,
            joints_by_part.get(&r),
        ));
    }

    ToolCallResult::json(&json!({
        "side": side.label(),
        "showing": play.showing.label(),
        "mode": format!("{:?}", play.mode),
        "time": play.time,
        "world": {
            "gravity": play.physics.gravity,
            "steps": play.physics.steps,
            "rebuilt": play.physics.rebuilt,
            "dynamic_assemblies": dyn_asms,
            "fixed_colliders": fixed_colliders,
            "awake_assemblies": play.physics.awake(),
            "movers": play.physics.mover_count(),
            "terrain_triangles": play.physics.terrain_triangles(),
            "stage_times": play.physics.stage_times(),
        },
        "part_count": part_entries.len(),
        "parts": part_entries,
    }))
}

#[allow(clippy::too_many_arguments)]
fn inspect_part_json(
    doc: &Document,
    server_doc: &Document,
    play: &PlaySession,
    side: Side,
    r: Ref,
    awake_first_parts: &HashSet<Ref>,
    char_model: Option<Ref>,
    joints: Option<&Vec<Value>>,
) -> Value {
    let class = doc
        .class_of(r)
        .map(|c| c.as_str().to_string())
        .unwrap_or_default();
    let name = doc
        .dom
        .get_by_ref(r)
        .map(|i| i.name.clone())
        .unwrap_or_default();
    let xf = scene::xform_of(doc, r).unwrap_or(Xform {
        pos: Vec3::ZERO,
        rot: Mat3::IDENTITY,
    });
    let anchored = matches!(doc.prop(r, "Anchored"), Some(Variant::Bool(true)));
    let can_collide = !matches!(doc.prop(r, "CanCollide"), Some(Variant::Bool(false)));
    let massless = matches!(doc.prop(r, "Massless"), Some(Variant::Bool(true)));
    let mass = world_part::part_mass(doc, r).mass;
    let (asm_mass, com, _) = world_part::assembly_mass(doc, r);
    let (grounded, root_part) = world_part::with_graph(doc, |g| (g.grounded(doc, r), g.root(doc, r)));

    let srv_r = map_to_server(play, side, r);
    let motion = srv_r
        .and_then(|sr| play.physics.part_motion(sr))
        .or_else(|| {
            if side == Side::Client {
                let lin = scene::vec3_of(doc.prop(r, "AssemblyLinearVelocity")).unwrap_or(Vec3::ZERO);
                let ang = physics::live::with_stats(doc.id, |st| st.spin.get(&r).map(|(w, _)| *w))
                    .flatten()
                    .or_else(|| scene::vec3_of(doc.prop(r, "AssemblyAngularVelocity")))
                    .unwrap_or(Vec3::ZERO);
                (!anchored).then_some((lin, ang))
            } else {
                None
            }
        });

    let (linvel, angvel) = motion.unwrap_or((Vec3::ZERO, Vec3::ZERO));
    let is_char_part = srv_r
        .zip(char_model)
        .is_some_and(|(sr, cm)| sr == cm || server_doc.is_ancestor(cm, sr));

    let sleeping = if grounded || anchored {
        false
    } else if let Some(sr) = srv_r {
        if is_char_part || play.physics.part_motion(sr).is_none() {
            false
        } else {
            let asm_awake = world_part::with_graph(server_doc, |g| {
                g.assembly(sr)
                    .iter()
                    .any(|p| awake_first_parts.contains(p))
            });
            !asm_awake
        }
    } else {
        false
    };

    let mut obj = json!({
        "ref": ref_to_str(r),
        "path": instance_path(doc, r),
        "class": class,
        "name": name,
        "Position": xf.pos.to_array(),
        "Orientation": xf.orientation_deg().to_array(),
        "AssemblyLinearVelocity": linvel.to_array(),
        "speed": linvel.length(),
        "AssemblyAngularVelocity": angvel.to_array(),
        "angular_speed": angvel.length(),
        "Mass": mass,
        "AssemblyMass": asm_mass,
        "CenterOfMass": com.to_array(),
        "Anchored": anchored,
        "CanCollide": can_collide,
        "Massless": massless,
        "sleeping": sleeping,
        "grounded": grounded,
        "root_part": {
            "ref": ref_to_str(root_part),
            "path": instance_path(doc, root_part),
        },
    });
    if let Some(j) = joints {
        obj["joints"] = Value::Array(j.clone());
    }
    obj
}

fn handle_physics_manipulate(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    if ed.play.is_none() {
        return ToolCallResult::error(
            "Not in Play/Run mode; physics manipulation requires an active Play or Run session",
        );
    }
    let action = match args.get("action").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return ToolCallResult::error("missing required parameter 'action'"),
    };
    let action_l = action.to_ascii_lowercase();
    let dm = args.get("datamodel").and_then(|v| v.as_str());

    let (side, targets, target_infos) = {
        let play = ed.play.as_ref().unwrap();
        let (side, doc) = match resolve_play_doc(ed, play, dm) {
            Ok(x) => x,
            Err(e) => return ToolCallResult::error(e),
        };
        let roots = match resolve_targets_arg(doc, args) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(e),
        };
        let mut parts = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            for inst in doc.dom.descendants_of(root) {
                let r = inst.referent();
                if seen.insert(r)
                    && doc
                        .class_of(r)
                        .is_some_and(|c| c != "Terrain" && doc::is_a(&c, "BasePart"))
                {
                    let (m, com, inertia) = world_part::assembly_mass(doc, r);
                    let (grounded, asm_root) =
                        world_part::with_graph(doc, |g| (g.grounded(doc, r), g.root(doc, r)));
                    let doc_id = doc.id;
                    let path = instance_path(doc, r);
                    let srv_r = map_to_server(play, side, r);
                    parts.push((r, path, srv_r, doc_id, m, com, inertia, grounded, asm_root));
                }
            }
        }
        if parts.is_empty() {
            return ToolCallResult::error("no BaseParts resolved from target/targets");
        }
        let target_infos = parts;
        let targets: Vec<Ref> = target_infos.iter().map(|x| x.0).collect();
        (side, targets, target_infos)
    };
    let _ = targets;

    let mut modified = Vec::new();
    match action_l.as_str() {
        "set_linear_velocity" => {
            let v = match args
                .get("velocity")
                .or_else(|| args.get("linear_velocity"))
                .ok_or_else(|| "set_linear_velocity requires 'velocity'".to_string())
                .and_then(|x| parse_vec3(x, "velocity"))
            {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(e),
            };
            let play = ed.play.as_mut().unwrap();
            for (r, path, srv_r, _, _, _, _, _, _) in &target_infos {
                if let Some(sr) = srv_r.filter(|_| side == Side::Server) {
                    play.physics.set_velocity(sr, v);
                }
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "velocity": v.to_array() }));
            }
            if side == Side::Client {
                for (r, _, _, _, _, _, _, _, _) in &target_infos {
                    set_prop_on_side(ed, side, *r, "AssemblyLinearVelocity", Variant::Vector3(Vector3::new(v.x, v.y, v.z)));
                }
            }
        }
        "set_angular_velocity" => {
            let w = match args
                .get("angular_velocity")
                .or_else(|| args.get("velocity"))
                .ok_or_else(|| "set_angular_velocity requires 'angular_velocity'".to_string())
                .and_then(|x| parse_vec3(x, "angular_velocity"))
            {
                Ok(w) => w,
                Err(e) => return ToolCallResult::error(e),
            };
            let play = ed.play.as_mut().unwrap();
            for (r, path, srv_r, doc_id, _, _, _, _, _) in &target_infos {
                if let Some(sr) = srv_r.filter(|_| side == Side::Server) {
                    play.physics.set_angular_velocity(sr, w);
                } else if side == Side::Client {
                    physics::live::queue_spin_set(*doc_id, *r, w);
                }
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "angular_velocity": w.to_array() }));
            }
        }
        "apply_impulse" | "apply_angular_impulse" | "apply_impulse_at_position" => {
            let j = match args
                .get("impulse")
                .or_else(|| args.get("angular_impulse"))
                .ok_or_else(|| format!("{action_l} requires 'impulse'"))
                .and_then(|x| parse_vec3(x, "impulse"))
            {
                Ok(v) => v,
                Err(e) => return ToolCallResult::error(e),
            };
            let at = if action_l == "apply_impulse_at_position" {
                match args
                    .get("position")
                    .ok_or_else(|| "apply_impulse_at_position requires 'position'".to_string())
                    .and_then(|x| parse_vec3(x, "position"))
                {
                    Ok(p) => Some(p),
                    Err(e) => return ToolCallResult::error(e),
                }
            } else {
                None
            };
            let play = ed.play.as_mut().unwrap();
            let mut client_lin_updates: Vec<(Ref, Vec3)> = Vec::new();
            for (r, path, srv_r, doc_id, m, com, inertia, grounded, _) in &target_infos {
                if *grounded || *m <= 0.0 {
                    modified.push(json!({ "ref": ref_to_str(*r), "path": path, "skipped": "grounded_or_zero_mass" }));
                    continue;
                }
                let (lin, ang) = match (action_l.as_str(), at) {
                    ("apply_angular_impulse", _) => (Vec3::ZERO, j),
                    (_, Some(p)) => (j, (p - *com).cross(j)),
                    _ => (j, Vec3::ZERO),
                };
                if let Some(sr) = srv_r.filter(|_| side == Side::Server) {
                    if lin != Vec3::ZERO {
                        let cur_v = play
                            .physics
                            .part_motion(sr)
                            .map(|(v, _)| v)
                            .unwrap_or(Vec3::ZERO);
                        play.physics.set_velocity(sr, cur_v + lin / *m);
                    }
                    if ang != Vec3::ZERO && inertia.determinant().abs() > 1e-12 {
                        play.physics.set_spin(sr, inertia.inverse() * ang, true);
                    }
                } else if side == Side::Client {
                    if lin != Vec3::ZERO {
                        client_lin_updates.push((*r, lin / *m));
                    }
                    if ang != Vec3::ZERO && inertia.determinant().abs() > 1e-12 {
                        physics::live::queue_spin(*doc_id, *r, inertia.inverse() * ang);
                    }
                }
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "applied": true }));
            }
            for (r, dv) in client_lin_updates {
                if let Some(cur) = read_vec3_on_side(ed, side, r, "AssemblyLinearVelocity") {
                    let nv = cur + dv;
                    set_prop_on_side(ed, side, r, "AssemblyLinearVelocity", Variant::Vector3(Vector3::new(nv.x, nv.y, nv.z)));
                }
            }
        }
        "wake" => {
            let play = ed.play.as_mut().unwrap();
            for (r, path, srv_r, _, _, _, _, _, _) in &target_infos {
                if let Some(sr) = srv_r.filter(|_| side == Side::Server) {
                    play.physics.set_spin(sr, Vec3::ZERO, true);
                }
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "woken": true }));
            }
        }
        "set_anchored" => {
            let anchored = match args.get("anchored").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => return ToolCallResult::error("set_anchored requires boolean 'anchored'"),
            };
            for (r, path, _, _, _, _, _, _, _) in &target_infos {
                set_prop_on_side(ed, side, *r, "Anchored", Variant::Bool(anchored));
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "Anchored": anchored }));
            }
            sync_server_if_showing(ed, side);
        }
        "set_can_collide" => {
            let can_collide = match args.get("can_collide").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => {
                    return ToolCallResult::error("set_can_collide requires boolean 'can_collide'")
                }
            };
            for (r, path, _, _, _, _, _, _, _) in &target_infos {
                set_prop_on_side(ed, side, *r, "CanCollide", Variant::Bool(can_collide));
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "CanCollide": can_collide }));
            }
            sync_server_if_showing(ed, side);
        }
        "teleport" | "set_cframe" | "cframe" => {
            let xf = match parse_xform_args(args) {
                Ok(x) => x,
                Err(e) => return ToolCallResult::error(e),
            };
            let cf = xf.to_cframe();
            for (r, path, _, _, _, _, _, _, _) in &target_infos {
                set_prop_on_side(ed, side, *r, "CFrame", Variant::CFrame(cf));
                modified.push(json!({ "ref": ref_to_str(*r), "path": path, "position": xf.pos.to_array() }));
            }
            sync_server_if_showing(ed, side);
        }
        other => return ToolCallResult::error(format!("unknown physics_manipulate action '{other}'")),
    }

    ctx.request_repaint();
    ToolCallResult::json(&json!({
        "action": action_l,
        "side": side.label(),
        "count": modified.len(),
        "results": modified,
    }))
}

fn resolve_play_doc<'a>(
    ed: &'a crate::app::Editor,
    play: &'a PlaySession,
    dm: Option<&str>,
) -> Result<(Side, &'a Document), String> {
    let side = match dm.unwrap_or("Active").to_ascii_lowercase().as_str() {
        "active" | "" => play.showing,
        "server" => Side::Server,
        "client" => Side::Client,
        "edit" => {
            return Err(
                "physics_inspect/manipulate requires live Server/Client/Active simulation, not Edit"
                    .into(),
            )
        }
        other => return Err(format!("unknown datamodel '{other}'")),
    };
    let doc = if side == play.showing {
        &ed.doc
    } else {
        ed.play_other_doc()
            .ok_or_else(|| format!("{} DataModel is not present in {:?} mode", side.label(), play.mode))?
    };
    Ok((side, doc))
}

fn map_to_server(play: &PlaySession, side: Side, r: Ref) -> Option<Ref> {
    match side {
        Side::Server => Some(r),
        Side::Client => play.map.c2s.get(&r).copied(),
    }
}

fn set_prop_on_side(ed: &mut crate::app::Editor, side: Side, r: Ref, name: &str, val: Variant) {
    let showing = ed.play.as_ref().map(|p| p.showing);
    if showing == Some(side) {
        ed.doc.set_prop_live(r, ustr(name), val);
    } else if let Some(d) = ed.play_other_doc_mut() {
        d.set_prop_live(r, ustr(name), val);
    }
}

fn read_vec3_on_side(ed: &crate::app::Editor, side: Side, r: Ref, name: &str) -> Option<Vec3> {
    let showing = ed.play.as_ref().map(|p| p.showing);
    let d = if showing == Some(side) {
        &ed.doc
    } else {
        ed.play_other_doc()?
    };
    Some(scene::vec3_of(d.prop(r, name)).unwrap_or(Vec3::ZERO))
}

fn sync_server_if_showing(ed: &mut crate::app::Editor, side: Side) {
    if side != Side::Server {
        return;
    }
    if let Some(play) = ed.play.as_mut() {
        if play.showing == Side::Server {
            play.physics.sync(&mut ed.doc);
        }
    }
}

fn collect_joints_map(doc: &Document) -> HashMap<Ref, Vec<Value>> {
    let mut map: HashMap<Ref, Vec<Value>> = HashMap::new();
    let Some(ws) = doc.workspace() else {
        return map;
    };
    for inst in doc.dom.descendants_of(ws) {
        let j = inst.referent();
        let Some(class) = doc.class_of(j) else {
            continue;
        };
        let parts = if let Some((a, b)) = physics::joint_parts(doc, j) {
            Some((a, b))
        } else {
            let a = match doc.prop(j, "Attachment0") {
                Some(Variant::Ref(att)) if att.is_some() => physics::attachment_part(doc, att),
                _ => None,
            };
            let b = match doc.prop(j, "Attachment1") {
                Some(Variant::Ref(att)) if att.is_some() => physics::attachment_part(doc, att),
                _ => None,
            };
            a.zip(b)
        };
        let Some((a, b)) = parts else {
            continue;
        };
        if a.is_none() && b.is_none() {
            continue;
        }
        let info = json!({
            "ref": ref_to_str(j),
            "class": class.as_str(),
            "name": inst.name,
            "enabled": physics::joint_enabled(doc, j),
            "part0": ref_to_str(a),
            "part1": ref_to_str(b),
        });
        if a.is_some() {
            map.entry(a).or_default().push(info.clone());
        }
        if b.is_some() && b != a {
            map.entry(b).or_default().push(info);
        }
    }
    map
}

// ============================================================================
// Shared Argument Helpers
// ============================================================================

fn resolve_targets_arg(doc: &Document, args: &Value) -> Result<Vec<Ref>, String> {
    if let Some(arr) = args.get("targets").filter(|v| !v.is_null()) {
        let items = arr
            .as_array()
            .ok_or_else(|| "'targets' must be an array".to_string())?;
        if items.is_empty() {
            return Err("'targets' array must not be empty".into());
        }
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            out.push(resolve_target(doc, item).map_err(|e| format!("targets[{i}]: {e}"))?);
        }
        return Ok(out);
    }
    let single = args
        .get("target")
        .filter(|v| !v.is_null())
        .ok_or_else(|| "missing 'target' or 'targets'".to_string())?;
    Ok(vec![resolve_target(doc, single)?])
}

fn parse_vec3(v: &Value, field: &str) -> Result<Vec3, String> {
    match v {
        Value::Array(a) if a.len() == 3 => Ok(Vec3::new(
            as_f32(&a[0], field)?,
            as_f32(&a[1], field)?,
            as_f32(&a[2], field)?,
        )),
        Value::Object(m) => Ok(Vec3::new(
            as_f32(
                m.get("x")
                    .or_else(|| m.get("X"))
                    .ok_or_else(|| format!("{field}.x missing"))?,
                field,
            )?,
            as_f32(
                m.get("y")
                    .or_else(|| m.get("Y"))
                    .ok_or_else(|| format!("{field}.y missing"))?,
                field,
            )?,
            as_f32(
                m.get("z")
                    .or_else(|| m.get("Z"))
                    .ok_or_else(|| format!("{field}.z missing"))?,
                field,
            )?,
        )),
        _ => Err(format!("expected [x, y, z] or {{x,y,z}} for '{field}', got {v}")),
    }
}

fn parse_xz_or_xyz(v: &Value) -> Result<(f32, Option<f32>, f32), String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok((
            as_f32(&a[0], "position[0]")?,
            None,
            as_f32(&a[1], "position[1]")?,
        )),
        Value::Array(a) if a.len() == 3 => Ok((
            as_f32(&a[0], "position[0]")?,
            Some(as_f32(&a[1], "position[1]")?),
            as_f32(&a[2], "position[2]")?,
        )),
        Value::Object(m) => {
            let x = as_f32(m.get("x").or_else(|| m.get("X")).ok_or("position.x missing")?, "position.x")?;
            let y = m
                .get("y")
                .or_else(|| m.get("Y"))
                .map(|yv| as_f32(yv, "position.y"))
                .transpose()?;
            let z = as_f32(m.get("z").or_else(|| m.get("Z")).ok_or("position.z missing")?, "position.z")?;
            Ok((x, y, z))
        }
        _ => Err(format!("expected [x, z] or [x, y, z] for position, got {v}")),
    }
}

fn parse_xform_args(args: &Value) -> Result<Xform, String> {
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
        .ok_or_else(|| "missing 'cframe' or 'position'".to_string())
        .and_then(|v| parse_vec3(v, "position"))?;
    let rot = if let Some(ov) = args.get("orientation").filter(|v| !v.is_null()) {
        let deg = parse_vec3(ov, "orientation")?;
        Xform::from_orientation_deg(Vec3::ZERO, deg).rot
    } else {
        Mat3::IDENTITY
    };
    Ok(Xform { pos, rot })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Harness, RustudioApp};
    use crate::runtime::RunMode;
    use rbx_dom_weak::types::{BinaryString, CFrame, Matrix3};

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
    fn schemas_cover_six_physics_world_tools() {
        let list = schemas();
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "world_raycast",
                "world_spatial_query",
                "world_place_on_surface",
                "world_build_structure",
                "physics_inspect",
                "physics_manipulate",
            ]
        );
    }

    #[test]
    fn raycast_and_spatial_queries_work_on_parts_and_terrain() {
        let mut ed = test_ed();
        let ws = ed.doc.workspace().unwrap();
        let p = ed.doc.insert(
            ws,
            doc::new_part(
                "TargetBlock",
                1,
                Vec3::new(0.0, 10.0, 0.0),
                Vec3::new(6.0, 4.0, 6.0),
                [200, 50, 50],
            ),
        );

        // Raycast downward hits top of TargetBlock at Y=12.
        let hit = parse_json(call_tool(
            &mut ed,
            "world_raycast",
            json!({
                "origin": [0.0, 30.0, 0.0],
                "direction": [0.0, -50.0, 0.0]
            }),
        ));
        assert_eq!(hit["hit"], Value::Bool(true));
        assert_eq!(hit["instance"]["name"], "TargetBlock");
        assert!((hit["position"][1].as_f64().unwrap() - 12.0).abs() < 1e-3);
        assert!((hit["normal"][1].as_f64().unwrap() - 1.0).abs() < 1e-3);

        // Exclude TargetBlock via filter_targets -> misses or hits Baseplate.
        let excl = parse_json(call_tool(
            &mut ed,
            "world_raycast",
            json!({
                "origin": [0.0, 30.0, 0.0],
                "direction": [0.0, -50.0, 0.0],
                "filter_mode": "Exclude",
                "filter_targets": [ref_to_str(p)]
            }),
        ));
        assert_ne!(excl["instance"]["name"], "TargetBlock");

        // Spatial bounding_box and sphere_overlap.
        let bb = parse_json(call_tool(
            &mut ed,
            "world_spatial_query",
            json!({
                "operation": "bounding_box",
                "target": ref_to_str(p)
            }),
        ));
        assert_eq!(bb["part_count"], 1);
        assert!((bb["min"][1].as_f64().unwrap() - 8.0).abs() < 1e-3);
        assert!((bb["max"][1].as_f64().unwrap() - 12.0).abs() < 1e-3);

        let ov = parse_json(call_tool(
            &mut ed,
            "world_spatial_query",
            json!({
                "operation": "sphere_overlap",
                "center": [2.5, 10.0, 0.0],
                "radius": 2.0
            }),
        ));
        let names: Vec<&str> = ov["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["name"].as_str())
            .collect();
        assert!(names.contains(&"TargetBlock"), "got {names:?}");

        // Terrain raycast hit test.
        let tref = ed.doc.find_child(ws, "Terrain").unwrap();
        let mut v = Voxels::empty();
        v.fill([-16.0, 20.0, -16.0], [16.0, 24.0, 16.0], 2, |c| {
            let p = Vec3::from(c) - Vec3::new(0.0, 22.0, 0.0);
            let q = p.abs() - Vec3::new(16.0, 2.0, 16.0);
            q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
        });
        ed.doc.set_props(
            "Terrain Test",
            vec![(
                tref,
                ustr("SmoothGrid"),
                Variant::BinaryString(BinaryString::from(v.encode())),
            )],
            None,
        );
        let thit = parse_json(call_tool(
            &mut ed,
            "world_raycast",
            json!({
                "origin": [0.0, 40.0, 0.0],
                "direction": [0.0, -30.0, 0.0],
                "ignore_water": true
            }),
        ));
        assert_eq!(thit["hit"], Value::Bool(true));
        assert_eq!(thit["instance"]["class"], "Terrain");
        assert_eq!(thit["material"], "Grass");
    }

    #[test]
    fn place_on_surface_and_build_structure_work_and_undo() {
        let mut ed = test_ed();
        let ws = ed.doc.workspace().unwrap();
        // Floor slab at Y=4..8.
        let floor = ed.doc.insert(
            ws,
            doc::new_part(
                "Platform",
                1,
                Vec3::new(50.0, 6.0, 0.0),
                Vec3::new(20.0, 4.0, 20.0),
                [120, 120, 120],
            ),
        );
        // Rotated box floating at Y=40.
        let crate_part = ed.doc.insert(
            ws,
            doc::new_part(
                "Crate",
                1,
                Vec3::new(50.0, 40.0, 0.0),
                Vec3::new(4.0, 6.0, 4.0),
                [180, 140, 80],
            ),
        );
        ed.doc.set_props(
            "Rotate Crate",
            vec![(
                crate_part,
                ustr("CFrame"),
                Variant::CFrame(
                    Xform {
                        pos: Vec3::new(50.0, 40.0, 0.0),
                        rot: Xform::from_orientation_deg(Vec3::ZERO, Vec3::new(35.0, 20.0, 15.0))
                            .rot,
                    }
                    .to_cframe(),
                ),
            )],
            None,
        );

        let placed = parse_json(call_tool(
            &mut ed,
            "world_place_on_surface",
            json!({
                "target": ref_to_str(crate_part),
                "align_to_normal": false
            }),
        ));
        assert_eq!(placed["placed_count"], 1);
        assert_eq!(placed["results"][0]["hit_instance"]["ref"], ref_to_str(floor));
        let new_min_y = placed["results"][0]["new_bounds"]["min"][1]
            .as_f64()
            .unwrap();
        assert!((new_min_y - 8.0).abs() < 1e-3, "min_y was {new_min_y}");

        // Build structure presets and verify single undo removes whole Model.
        for kind in ["box_room", "staircase", "ramp", "arch", "cylinder_ring", "scatter"] {
            let built = parse_json(call_tool(
                &mut ed,
                "world_build_structure",
                json!({
                    "kind": kind,
                    "origin": [120.0, 0.0, 0.0],
                    "count": 5
                }),
            ));
            assert!(built["part_count"].as_u64().unwrap() >= 1);
            let mref = variants::str_to_ref(built["model"]["ref"].as_str().unwrap()).unwrap();
            assert!(ed.doc.exists(mref));
            ed.doc.undo();
            assert!(!ed.doc.exists(mref));
        }
    }

    #[test]
    fn physics_inspect_and_manipulate_live_simulation() {
        let mut ed = test_ed();
        let ws = ed.doc.workspace().unwrap();
        let _edit_ball = ed.doc.insert(
            ws,
            InstanceBuilder::new("Part")
                .with_name("FreeBall")
                .with_property("Anchored", false)
                .with_property("Size", Vector3::new(2.0, 2.0, 2.0))
                .with_property("CFrame", CFrame::new(Vector3::new(0.0, 20.0, 0.0), Matrix3::identity())),
        );

        ed.start_play(RunMode::Run);
        let insp = parse_json(call_tool(
            &mut ed,
            "physics_inspect",
            json!({ "targets": ["Workspace.FreeBall"] }),
        ));
        assert_eq!(insp["part_count"], 1);
        assert_eq!(insp["parts"][0]["Anchored"], Value::Bool(false));

        let manip = parse_json(call_tool(
            &mut ed,
            "physics_manipulate",
            json!({
                "action": "set_linear_velocity",
                "target": "Workspace.FreeBall",
                "velocity": [12.0, 3.0, -4.0]
            }),
        ));
        assert_eq!(manip["count"], 1);

        let insp2 = parse_json(call_tool(
            &mut ed,
            "physics_inspect",
            json!({ "targets": ["Workspace.FreeBall"] }),
        ));
        let v = &insp2["parts"][0]["AssemblyLinearVelocity"];
        assert!((v[0].as_f64().unwrap() - 12.0).abs() < 1e-3);
        assert!((v[1].as_f64().unwrap() - 3.0).abs() < 1e-3);
        assert!((v[2].as_f64().unwrap() + 4.0).abs() < 1e-3);

        // Apply impulse and verify velocity changes proportionally to assembly mass.
        let mass = insp2["parts"][0]["AssemblyMass"].as_f64().unwrap() as f32;
        parse_json(call_tool(
            &mut ed,
            "physics_manipulate",
            json!({
                "action": "apply_impulse",
                "target": "Workspace.FreeBall",
                "impulse": [0.0, 10.0 * mass, 0.0]
            }),
        ));
        let insp3 = parse_json(call_tool(
            &mut ed,
            "physics_inspect",
            json!({ "targets": ["Workspace.FreeBall"] }),
        ));
        let vy3 = insp3["parts"][0]["AssemblyLinearVelocity"][1]
            .as_f64()
            .unwrap();
        assert!((vy3 - 13.0).abs() < 1e-2, "expected ~13, got {vy3}");

        // Toggle Anchored & CanCollide live.
        parse_json(call_tool(
            &mut ed,
            "physics_manipulate",
            json!({
                "action": "set_anchored",
                "target": "Workspace.FreeBall",
                "anchored": true
            }),
        ));
        let insp4 = parse_json(call_tool(
            &mut ed,
            "physics_inspect",
            json!({ "targets": ["Workspace.FreeBall"] }),
        ));
        assert_eq!(insp4["parts"][0]["Anchored"], Value::Bool(true));

        ed.stop_play();
    }
}
