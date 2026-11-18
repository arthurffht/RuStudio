//! Live MCP Resources (`rustudio://*`) and expert workflow Prompts for OmniMCP.

use std::collections::BTreeMap;

use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::ustr;
use serde_json::{json, Map, Value};

use crate::app::{Editor, Level};
use crate::doc::{self, Document};
use crate::runtime::RunMode;

use super::protocol::{
    PromptArgument, PromptDef, PromptMessage, ResourceContent, ResourceDef, ToolContent,
};
use super::variants::{instance_path, ref_to_str, variant_to_json_with_doc};

const TREE_NODE_CAP: usize = 4000;
const OUTPUT_TAIL_CAP: usize = 500;
const BLOB_SUMMARY_THRESHOLD: usize = 2048;

/// Returns all engine-provided MCP resource definitions.
pub fn resources() -> Vec<ResourceDef> {
    vec![
        ResourceDef {
            uri: "rustudio://state".into(),
            name: "RuStudio Live State".into(),
            description: "Current editor mode (Edit/Play/Run), available DataModels, place metadata, camera, selection, undo stack, FPS, and memory.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://tree/workspace".into(),
            name: "Workspace Hierarchy".into(),
            description: "Hierarchical JSON tree of Workspace instances in the active DataModel.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://tree/game".into(),
            name: "DataModel Hierarchy".into(),
            description: "Hierarchical JSON tree of the full game DataModel (services and descendants).".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://scripts".into(),
            name: "All Scripts Manifest".into(),
            description: "Inventory of every Script, LocalScript, and ModuleScript in the active DataModel with refs, paths, line counts, and run settings.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://output".into(),
            name: "Output Console Log".into(),
            description: "Recent RuStudio Output console lines (Info/Warn/Error) with timestamps and counts.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://selection".into(),
            name: "Current Selection".into(),
            description: "Currently selected instances with refs, paths, tags, attributes, and stored properties.".into(),
            mime_type: Some("application/json".into()),
        },
    ]
}

/// Reads one live MCP resource from the running `Editor`.
pub fn read_resource(ed: &mut Editor, uri: &str) -> Result<Vec<ResourceContent>, String> {
    let payload = match uri {
        "rustudio://state" => build_state_json(ed),
        "rustudio://tree/workspace" => build_workspace_tree_json(&ed.doc),
        "rustudio://tree/game" => build_game_tree_json(&ed.doc),
        "rustudio://scripts" => collect_scripts_json(&ed.doc),
        "rustudio://output" => build_output_json(ed),
        "rustudio://selection" => serialize_selection_json(&ed.doc),
        _ => return Err(format!("unknown resource: {uri}")),
    };

    let text = serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("failed to serialize resource '{uri}': {e}"))?;

    Ok(vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".into()),
        text: Some(text),
        blob: None,
    }])
}

/// Returns all expert OmniMCP workflow prompts.
pub fn prompts() -> Vec<PromptDef> {
    vec![
        PromptDef {
            name: "omni-game-architect".into(),
            description: "Expert Roblox/Luau systems architect workflow using OmniMCP's DOM, script batching, Edit-during-Play, and verification tools.".into(),
            arguments: vec![
                PromptArgument {
                    name: "goal".into(),
                    description: "The game system, feature, or refactor to architect and build.".into(),
                    required: false,
                },
                PromptArgument {
                    name: "architecture".into(),
                    description: "Optional architectural constraints or patterns (e.g. Knit-style, ECS, module layout).".into(),
                    required: false,
                },
            ],
        },
        PromptDef {
            name: "omni-playtest-debugger".into(),
            description: "Expert playtest, physics, and Luau runtime debugging workflow with deterministic stepping, live VM execution, and log inspection.".into(),
            arguments: vec![
                PromptArgument {
                    name: "symptom".into(),
                    description: "Bug, unexpected physics behavior, script error, or test scenario to diagnose.".into(),
                    required: false,
                },
            ],
        },
        PromptDef {
            name: "omni-world-builder".into(),
            description: "Expert 3D environment, procedural structure, SmoothGrid terrain, and Lighting/Atmosphere builder with GPU visual verification.".into(),
            arguments: vec![
                PromptArgument {
                    name: "theme".into(),
                    description: "Visual style, biome, or environment area to construct and light.".into(),
                    required: false,
                },
            ],
        },
        PromptDef {
            name: "omni-ui-designer".into(),
            description: "Expert Roblox ScreenGui/BillboardGui/SurfaceGui designer with responsive layout inspection and visual verification.".into(),
            arguments: vec![
                PromptArgument {
                    name: "screen".into(),
                    description: "Target UI screen, HUD, or menu to create or refine.".into(),
                    required: false,
                },
            ],
        },
    ]
}

/// Generates an expert prompt enriched with live state from the open RuStudio place.
pub fn get_prompt(
    ed: &mut Editor,
    name: &str,
    args: &Map<String, Value>,
) -> Result<(Option<String>, Vec<PromptMessage>), String> {
    let mode = mode_label(ed);
    let title = ed.doc.title.as_str();
    let path_str = ed
        .doc
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unsaved>".into());

    match name {
        "omni-game-architect" => {
            let goal = arg_str(args, "goal").unwrap_or("Design and implement robust Roblox game systems");
            let arch = arg_str(args, "architecture").unwrap_or("Clean server-authoritative modular Luau");
            let script_count = ed.doc.count_class("LuaSourceContainer");
            let services = top_level_services_summary(&ed.doc);
            let stats = ed.doc.stats();

            let text = format!(
                "You are an expert Roblox Studio & Luau Systems Architect operating RuStudio through OmniMCP.\n\n\
                 ## Live Session Context\n\
                 - Place: {title} ({path_str})\n\
                 - Mode: {mode}\n\
                 - Scripts in DataModel: {script_count}\n\
                 - Top-level Services: {services}\n\
                 - Document Stats:\n```\n{stats}```\n\n\
                 ## Objective\n\
                 - Goal: {goal}\n\
                 - Architecture Preference: {arch}\n\n\
                 ## Recommended OmniMCP Workflow\n\
                 1. **Discover first**: Call `dom_get_tree` (`max_depth: 2..4`), `script_list_all`, and `script_read_batch` / `script_grep` to understand existing services, remotes, and conventions before editing.\n\
                 2. **Design modular boundaries**: Put shared modules & Remotes under `ReplicatedStorage`, authoritative game logic in `ServerScriptService`, client controllers in `StarterPlayer.StarterPlayerScripts`, and UI in `StarterGui`.\n\
                 3. **Build in fast atomic batches**:\n\
                    - Use `dom_create_instances` for nested service/folder/instance hierarchies.\n\
                    - Use `script_write_batch` (with `create_parents: true`) for new scripts and `script_patch_batch` for surgical multi-file edits.\n\
                    - Combine synchronous DOM/property mutations into `omni_batch` when setting up many objects.\n\
                 4. **Edit while Play-testing**: Every DOM and script tool accepts `datamodel: \"Edit\"` even while a `Play` or `Run` session is active, so you can fix the source place without losing playtest context, then `play_control(action: \"restart\")` to verify.\n\
                 5. **Verify end-to-end**:\n\
                    - Run `script_check_syntax` on modified scripts.\n\
                    - Start Play with `play_control(action: \"start_play\")`, advance simulation with `play_step_frames`, verify console output via `output_console`, test server/client state via `luau_execute`, and inspect visuals via `viewport_capture`."
            );

            Ok((
                Some("Expert Roblox/Luau systems architecture prompt with live place context".into()),
                vec![PromptMessage {
                    role: "user".into(),
                    content: ToolContent::Text { text },
                }],
            ))
        }
        "omni-playtest-debugger" => {
            let symptom = arg_str(args, "symptom").unwrap_or("Diagnose runtime, replication, physics, or Luau issues");
            let play_time = ed.play.as_ref().map(|p| p.time).unwrap_or(0.0);
            let has_char = ed.play.as_ref().is_some_and(|p| p.character.is_some());
            let recent_issues = recent_errors_and_warnings(ed, 20);

            let text = format!(
                "You are an expert Roblox Runtime, Physics & Luau Playtest Debugger operating RuStudio through OmniMCP.\n\n\
                 ## Live Runtime Context\n\
                 - Place: {title} ({path_str})\n\
                 - Mode: {mode} (play_time: {play_time:.2}s, character_spawned: {has_char})\n\
                 - Recent Warnings/Errors:\n```\n{recent_issues}```\n\n\
                 ## Investigation Target\n\
                 - Symptom / Focus: {symptom}\n\n\
                 ## Recommended OmniMCP Debugging Workflow\n\
                 1. **Control the simulation**:\n\
                    - Use `play_control` (`start_play`, `start_run`, `view_server`, `view_client`, `restart`, `stop`).\n\
                    - Use `play_step_frames` to advance deterministically frame-by-frame and collect logs emitted during those exact frames.\n\
                 2. **Inspect & probe live VMs**:\n\
                    - Use `luau_execute` targeting `datamodel: \"Server\"`, `\"Client\"`, or `\"Edit\"`.\n\
                    - Set `identity: 8` for unrestricted inspection (e.g. CoreGui/internal state), or `identity: 2` to reproduce exact game-script capability limits.\n\
                    - Use `luau_fire_signal` to trigger RemoteEvents/BindableEvents/signals directly and `luau_vm_stats` to inspect VM memory & scheduler load.\n\
                 3. **Inspect physics & character**:\n\
                    - Use `physics_inspect` for dynamic body velocities, mass, groundedness, sleeping state, and constraints; use `physics_manipulate` to test impulses/velocities.\n\
                    - Drive the player via `play_character_control` (teleport, move_to, jump, set_stats, equip/activate tool) and `play_input_inject` (keys, mouse clicks, drags, text input).\n\
                 4. **Fix cleanly & re-test**: Patch the underlying `Edit` scripts (`datamodel: \"Edit\"`), verify via `output_console` and `viewport_capture`, and confirm zero errors."
            );

            Ok((
                Some("Expert playtest & runtime debugging prompt with live error context".into()),
                vec![PromptMessage {
                    role: "user".into(),
                    content: ToolContent::Text { text },
                }],
            ))
        }
        "omni-world-builder" => {
            let theme = arg_str(args, "theme").unwrap_or("Cohesive, well-grounded 3D environment with realistic lighting");
            let cam_pos = [ed.cam.pos.x, ed.cam.pos.y, ed.cam.pos.z];
            let cam_fwd = ed.cam.forward();
            let lighting_summary = lighting_summary(&ed.doc);
            let has_terrain = ed
                .doc
                .workspace()
                .and_then(|ws| ed.doc.find_child(ws, "Terrain"))
                .is_some();

            let text = format!(
                "You are an expert 3D World, Terrain & Lighting Artist operating RuStudio through OmniMCP.\n\n\
                 ## Live World Context\n\
                 - Place: {title} ({path_str})\n\
                 - Mode: {mode}\n\
                 - Camera Pos: [{:.2}, {:.2}, {:.2}], Forward: [{:.2}, {:.2}, {:.2}], FOV: {:.1}\n\
                 - Workspace Terrain Present: {has_terrain}\n\
                 - Lighting: {lighting_summary}\n\n\
                 ## Build Brief\n\
                 - Theme / Area: {theme}\n\n\
                 ## Recommended OmniMCP World-Building Workflow\n\
                 1. **Terrain first (when outdoors)**:\n\
                    - Use `terrain_inspect` to read water settings and existing voxel bounds.\n\
                    - Use `terrain_generate` (biomes, caves, seed), `terrain_fill_shape` (`block`, `ball`, `cylinder`, `wedge`, `water_level`), and `terrain_sculpt_stroke` (`Add`, `Subtract`, `Grow`, `Erode`, `Smooth`, `Flatten`, `Paint`, `SeaLevel`).\n\
                 2. **Grounded architecture & props**:\n\
                    - Use `world_build_structure` (`box_room`, `wall_run`, `staircase`, `ramp`, `arch`, `cylinder_ring`, `scatter`) and `dom_create_instances` / `dom_clone_and_transform`.\n\
                    - Always ground props with `world_place_on_surface` or verify heights via `world_raycast` and `world_spatial_query` (`bounding_box`, `box_overlap`, `sphere_overlap`) so nothing floats or clips.\n\
                    - Use `dom_set_properties` (and `MaterialColors`, `Color3`, `Material`, `CastShadow`, `Anchored`) and `dom_group_ungroup` to organize Models cleanly.\n\
                 3. **Atmosphere & visual verification**:\n\
                    - Configure `Lighting`, `Sky`, `Atmosphere`, `Clouds`, and post-processing effects (`BloomEffect`, `ColorCorrectionEffect`, `DepthOfFieldEffect`, `SunRaysEffect`) via `lighting_and_atmosphere`.\n\
                    - Aim `viewport_camera` (`look_at` or `focus_target`) from multiple angles and call `viewport_capture` (`clean_view: true`) to visually inspect your geometry, proportions, and lighting."
            , cam_pos[0], cam_pos[1], cam_pos[2], cam_fwd.x, cam_fwd.y, cam_fwd.z, ed.cam.fov_deg);

            Ok((
                Some("Expert 3D environment, terrain, and lighting prompt with live camera/lighting context".into()),
                vec![PromptMessage {
                    role: "user".into(),
                    content: ToolContent::Text { text },
                }],
            ))
        }
        "omni-ui-designer" => {
            let screen = arg_str(args, "screen").unwrap_or("Clean, responsive, accessible Roblox UI");
            let gui_summary = starter_gui_summary(&ed.doc);

            let text = format!(
                "You are an expert Roblox UI/UX Designer operating RuStudio through OmniMCP.\n\n\
                 ## Live UI Context\n\
                 - Place: {title} ({path_str})\n\
                 - Mode: {mode}\n\
                 - StarterGui Inventory: {gui_summary}\n\n\
                 ## Design Target\n\
                 - Screen / Component: {screen}\n\n\
                 ## Recommended OmniMCP UI Workflow\n\
                 1. **Responsive hierarchy**:\n\
                    - Build `ScreenGui`s (with explicit `ScreenInsets` / `IgnoreGuiInset`, `ResetOnSpawn`, `ZIndexBehavior`) using `dom_create_instances` and `dom_set_properties`.\n\
                    - Favor scale-based `UDim2` sizing/positioning combined with `UIAspectRatioConstraint`, `UIPadding`, `UIListLayout` / `UIGridLayout`, `UICorner`, `UIStroke`, and `UIGradient`.\n\
                    - Remember `dom_set_properties` supports rich Roblox types directly (`UDim2: [[sx, ox], [sy, oy]]`, `Color3: [r, g, b]` or `#RRGGBB`, `Font`, `Enum` names).\n\
                 2. **Inspect layout geometry**:\n\
                    - Call `gui_inspect` to verify computed `AbsolutePosition`, `AbsoluteSize`, visibility, text, ZIndex, and perform hit-testing (`at: [x, y]`) on `StarterGui`, `PlayerGui`, `SurfaceGui`, or `BillboardGui`.\n\
                 3. **Interactive Play verification**:\n\
                    - Run `play_control(action: \"start_play\")`, send clicks/typing with `play_input_inject`, and capture the full window (`viewport_capture(target: \"window\")`) to visually verify alignment, contrast, and interactivity."
            );

            Ok((
                Some("Expert Roblox UI/UX design prompt with live StarterGui inventory".into()),
                vec![PromptMessage {
                    role: "user".into(),
                    content: ToolContent::Text { text },
                }],
            ))
        }
        _ => Err(format!("unknown prompt: {name}")),
    }
}

fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn mode_label(ed: &Editor) -> &'static str {
    match ed.play.as_ref().map(|p| p.mode) {
        Some(RunMode::Play) => "Play",
        Some(RunMode::Run) => "Run",
        _ => "Edit",
    }
}

fn build_state_json(ed: &Editor) -> Value {
    let mode = mode_label(ed);
    let available_datamodels = match ed.play.as_ref().map(|p| p.mode) {
        Some(RunMode::Play) => vec!["Edit", "Server", "Client"],
        Some(RunMode::Run) => vec!["Edit", "Server"],
        _ => vec!["Edit"],
    };
    let selection: Vec<Value> = ed
        .doc
        .selection
        .iter()
        .filter_map(|&r| {
            let inst = ed.doc.dom.get_by_ref(r)?;
            Some(json!({
                "ref": ref_to_str(r),
                "path": instance_path(&ed.doc, r),
                "name": inst.name,
                "class": inst.class.as_str(),
            }))
        })
        .collect();

    let mut error_count = 0usize;
    let mut warn_count = 0usize;
    for l in &ed.log {
        match l.level {
            Level::Error => error_count += 1,
            Level::Warn => warn_count += 1,
            Level::Info => {}
        }
    }

    json!({
        "pid": std::process::id(),
        "mcp_port": ed.mcp.as_ref().map(|m| m.port),
        "mode": mode,
        "showing_side": ed.play.as_ref().map(|p| p.showing.label()),
        "available_datamodels": available_datamodels,
        "title": ed.doc.title,
        "path": ed.doc.path.as_ref().map(|p| p.display().to_string()),
        "dirty": ed.doc.dirty(),
        "revision": ed.doc.revision,
        "tree_revision": ed.doc.tree_revision,
        "undo_len": ed.doc.undo_len(),
        "can_undo": ed.doc.can_undo(),
        "can_redo": ed.doc.can_redo(),
        "selection_count": selection.len(),
        "selection": selection,
        "camera": {
            "pos": [ed.cam.pos.x, ed.cam.pos.y, ed.cam.pos.z],
            "focus": [ed.cam.focus.x, ed.cam.focus.y, ed.cam.focus.z],
            "forward": [ed.cam.forward().x, ed.cam.forward().y, ed.cam.forward().z],
            "yaw_deg": ed.cam.yaw.to_degrees(),
            "pitch_deg": ed.cam.pitch.to_degrees(),
            "fov_deg": ed.cam.fov_deg,
        },
        "editor": {
            "tool": ed.tool.label(),
            "snap_move": ed.snap_move,
            "move_inc": ed.move_inc,
            "snap_rotate": ed.snap_rotate,
            "rotate_inc": ed.rotate_inc,
            "local_space": ed.local_space,
            "show_grid": ed.show_grid,
            "collisions": ed.collisions,
            "join_surfaces": ed.join_surfaces,
            "clean_view": ed.clean_view,
        },
        "perf": {
            "fps": ed.fps,
            "last_work_ms": ed.last_work_ms,
            "resident_bytes": crate::platform::resident_bytes(),
        },
        "play": {
            "time": ed.play.as_ref().map(|p| p.time),
            "has_character": ed.play.as_ref().is_some_and(|p| p.character.is_some()),
        },
        "output_counts": {
            "total": ed.log.len(),
            "warnings": warn_count,
            "errors": error_count,
        },
    })
}

fn build_workspace_tree_json(doc: &Document) -> Value {
    match doc.workspace() {
        Some(ws) => serialize_subtree(doc, ws, 10, TREE_NODE_CAP),
        None => json!({ "error": "Workspace service not found" }),
    }
}

fn build_game_tree_json(doc: &Document) -> Value {
    serialize_subtree(doc, doc.dom.root_ref(), 6, TREE_NODE_CAP)
}

pub(crate) fn serialize_subtree(
    doc: &Document,
    root: Ref,
    max_depth: usize,
    node_cap: usize,
) -> Value {
    let mut visited = 0usize;
    let mut truncated = false;
    let tree = serialize_node(doc, root, 0, max_depth, node_cap, &mut visited, &mut truncated);
    json!({
        "root": instance_path(doc, root),
        "nodes_emitted": visited,
        "truncated": truncated,
        "tree": tree,
    })
}

fn serialize_node(
    doc: &Document,
    r: Ref,
    depth: usize,
    max_depth: usize,
    node_cap: usize,
    visited: &mut usize,
    truncated: &mut bool,
) -> Value {
    let Some(inst) = doc.dom.get_by_ref(r) else {
        return Value::Null;
    };
    *visited += 1;
    let raw_children = inst.children();
    let mut children = Vec::new();
    if depth < max_depth {
        for &ch in raw_children {
            if *visited >= node_cap {
                *truncated = true;
                break;
            }
            children.push(serialize_node(
                doc,
                ch,
                depth + 1,
                max_depth,
                node_cap,
                visited,
                truncated,
            ));
        }
    } else if !raw_children.is_empty() {
        *truncated = true;
    }

    let mut obj = Map::new();
    obj.insert("ref".into(), Value::String(ref_to_str(r)));
    obj.insert("name".into(), Value::String(inst.name.clone()));
    obj.insert("class".into(), Value::String(inst.class.as_str().to_string()));
    obj.insert("path".into(), Value::String(instance_path(doc, r)));
    obj.insert("child_count".into(), json!(raw_children.len()));
    if !children.is_empty() {
        obj.insert("children".into(), Value::Array(children));
    }
    Value::Object(obj)
}

pub(crate) fn collect_scripts_json(doc: &Document) -> Value {
    let mut scripts = Vec::new();
    for inst in doc.dom.descendants() {
        if !doc::is_a(inst.class.as_str(), "LuaSourceContainer") {
            continue;
        }
        let r = inst.referent();
        let source = match doc.prop(r, "Source") {
            Some(Variant::String(s)) => s,
            _ => String::new(),
        };
        let disabled = match doc.prop(r, "Disabled") {
            Some(Variant::Bool(b)) => Some(b),
            _ => match doc.prop(r, "Enabled") {
                Some(Variant::Bool(e)) => Some(!e),
                _ => None,
            },
        };
        let run_context = doc
            .prop(r, "RunContext")
            .map(|v| variant_to_json_with_doc(doc, Some(inst.class.as_str()), Some("RunContext"), &v));

        scripts.push(json!({
            "ref": ref_to_str(r),
            "path": instance_path(doc, r),
            "name": inst.name,
            "class": inst.class.as_str(),
            "lines": source.lines().count(),
            "bytes": source.len(),
            "disabled": disabled,
            "run_context": run_context,
        }));
    }
    scripts.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!({
        "count": scripts.len(),
        "scripts": scripts,
    })
}

fn build_output_json(ed: &Editor) -> Value {
    let total = ed.log.len();
    let start = total.saturating_sub(OUTPUT_TAIL_CAP);
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for l in &ed.log {
        match l.level {
            Level::Error => errors += 1,
            Level::Warn => warnings += 1,
            Level::Info => {}
        }
    }
    let lines: Vec<Value> = ed
        .log
        .iter()
        .enumerate()
        .skip(start)
        .map(|(idx, l)| {
            json!({
                "index": idx,
                "at": l.at,
                "level": match l.level {
                    Level::Info => "Info",
                    Level::Warn => "Warn",
                    Level::Error => "Error",
                },
                "text": l.text,
            })
        })
        .collect();
    json!({
        "total": total,
        "start_index": start,
        "warnings": warnings,
        "errors": errors,
        "lines": lines,
    })
}

pub(crate) fn serialize_selection_json(doc: &Document) -> Value {
    let items: Vec<Value> = doc
        .selection
        .iter()
        .filter_map(|&r| {
            let inst = doc.dom.get_by_ref(r)?;
            let parent = if inst.parent().is_some() {
                json!({
                    "ref": ref_to_str(inst.parent()),
                    "path": instance_path(doc, inst.parent()),
                })
            } else {
                Value::Null
            };
            let tags = doc
                .prop(r, "Tags")
                .map(|v| variant_to_json_with_doc(doc, Some(inst.class.as_str()), Some("Tags"), &v));
            let attributes = doc
                .prop(r, "Attributes")
                .or_else(|| inst.properties.get(&ustr("AttributesSerialize")).cloned())
                .map(|v| variant_to_json_with_doc(doc, Some(inst.class.as_str()), Some("Attributes"), &v));

            let mut props = BTreeMap::new();
            for (k, v) in &inst.properties {
                let key = k.as_str();
                if matches!(key, "Tags" | "Attributes" | "AttributesSerialize") {
                    continue;
                }
                let val = match v {
                    Variant::BinaryString(bs)
                        if (bs.as_ref() as &[u8]).len() > BLOB_SUMMARY_THRESHOLD =>
                    {
                        json!({
                            "type": "BinaryString",
                            "bytes": (bs.as_ref() as &[u8]).len(),
                            "truncated": true,
                        })
                    }
                    Variant::SharedString(ss) if ss.data().len() > BLOB_SUMMARY_THRESHOLD => {
                        json!({
                            "type": "SharedString",
                            "bytes": ss.data().len(),
                            "hash": ss.hash().to_string(),
                            "truncated": true,
                        })
                    }
                    _ => variant_to_json_with_doc(doc, Some(inst.class.as_str()), Some(key), v),
                };
                props.insert(key.to_string(), val);
            }

            Some(json!({
                "ref": ref_to_str(r),
                "path": instance_path(doc, r),
                "name": inst.name,
                "class": inst.class.as_str(),
                "parent": parent,
                "child_count": inst.children().len(),
                "tags": tags,
                "attributes": attributes,
                "properties": props,
            }))
        })
        .collect();

    json!({
        "count": items.len(),
        "selection": items,
    })
}

fn top_level_services_summary(doc: &Document) -> String {
    let Some(root) = doc.dom.get_by_ref(doc.dom.root_ref()) else {
        return String::new();
    };
    let names: Vec<String> = root
        .children()
        .iter()
        .filter_map(|&c| {
            let inst = doc.dom.get_by_ref(c)?;
            Some(format!("{}({})", inst.name, inst.children().len()))
        })
        .collect();
    names.join(", ")
}

fn recent_errors_and_warnings(ed: &Editor, limit: usize) -> String {
    let items: Vec<String> = ed
        .log
        .iter()
        .rev()
        .filter(|l| matches!(l.level, Level::Warn | Level::Error))
        .take(limit)
        .map(|l| {
            let tag = match l.level {
                Level::Error => "ERROR",
                Level::Warn => "WARN",
                Level::Info => "INFO",
            };
            format!("[{} {tag}] {}", l.at, l.text)
        })
        .collect();
    if items.is_empty() {
        "(none)\n".into()
    } else {
        let mut rev = items;
        rev.reverse();
        rev.join("\n") + "\n"
    }
}

fn lighting_summary(doc: &Document) -> String {
    let Some(lighting) = doc.find_child(doc.dom.root_ref(), "Lighting") else {
        return "Lighting service absent".into();
    };
    let clock = doc
        .prop(lighting, "ClockTime")
        .map(|v| variant_to_json_with_doc(doc, Some("Lighting"), Some("ClockTime"), &v))
        .unwrap_or(Value::Null);
    let bright = doc
        .prop(lighting, "Brightness")
        .map(|v| variant_to_json_with_doc(doc, Some("Lighting"), Some("Brightness"), &v))
        .unwrap_or(Value::Null);
    let shadows = doc
        .prop(lighting, "GlobalShadows")
        .map(|v| variant_to_json_with_doc(doc, Some("Lighting"), Some("GlobalShadows"), &v))
        .unwrap_or(Value::Null);
    let children: Vec<String> = doc
        .dom
        .get_by_ref(lighting)
        .map(|i| {
            i.children()
                .iter()
                .filter_map(|&c| doc.dom.get_by_ref(c).map(|ci| format!("{}:{}", ci.name, ci.class)))
                .collect()
        })
        .unwrap_or_default();
    format!(
        "ClockTime={clock}, Brightness={bright}, GlobalShadows={shadows}, children=[{}]",
        children.join(", ")
    )
}

fn starter_gui_summary(doc: &Document) -> String {
    let Some(sg) = doc.find_child(doc.dom.root_ref(), "StarterGui") else {
        return "StarterGui absent".into();
    };
    let children: Vec<String> = doc
        .dom
        .get_by_ref(sg)
        .map(|i| {
            i.children()
                .iter()
                .filter_map(|&c| {
                    doc.dom
                        .get_by_ref(c)
                        .map(|ci| format!("{}:{}({} children)", ci.name, ci.class, ci.children().len()))
                })
                .collect()
        })
        .unwrap_or_default();
    if children.is_empty() {
        "(empty)".into()
    } else {
        children.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn resources_and_prompts_have_expected_entries() {
        let res = resources();
        let uris: Vec<&str> = res.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            vec![
                "rustudio://state",
                "rustudio://tree/workspace",
                "rustudio://tree/game",
                "rustudio://scripts",
                "rustudio://output",
                "rustudio://selection",
            ]
        );

        let pr = prompts();
        let names: Vec<&str> = pr.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "omni-game-architect",
                "omni-playtest-debugger",
                "omni-world-builder",
                "omni-ui-designer",
            ]
        );
    }

    #[test]
    fn document_resource_helpers_serialize_tree_scripts_and_selection() {
        let mut doc = Document::new_place();
        let sss = doc.find_child(doc.dom.root_ref(), "ServerScriptService").unwrap();
        let script_ref = doc.insert(
            sss,
            InstanceBuilder::new("Script")
                .with_name("InitGame")
                .with_property("Source", "print('hello')\nreturn true\n".to_string()),
        );
        doc.selection = vec![script_ref];

        let tree = build_workspace_tree_json(&doc);
        assert_eq!(tree["root"], "Workspace");
        assert!(tree["nodes_emitted"].as_u64().unwrap() >= 1);

        let scripts = collect_scripts_json(&doc);
        assert_eq!(scripts["count"], 1);
        assert_eq!(scripts["scripts"][0]["path"], "ServerScriptService.InitGame");
        assert_eq!(scripts["scripts"][0]["lines"], 2);

        let sel = serialize_selection_json(&doc);
        assert_eq!(sel["count"], 1);
        assert_eq!(sel["selection"][0]["name"], "InitGame");
        assert_eq!(sel["selection"][0]["class"], "Script");
    }
}
