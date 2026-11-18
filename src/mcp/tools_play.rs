//! OmniMCP Play/Run Control, Deterministic Stepping, Character Control,
//! Input Injection & Output Console tools:
//! `play_control`, `play_step_frames`, `play_character_control`,
//! `play_input_inject`, `output_console`.

use std::sync::mpsc::Sender;
use std::time::Instant;

use glam::{Mat3, Vec3};
use mlua::Value as LuaValue;
use rbx_dom_weak::types::{Ref, Variant};
use serde_json::{json, Value};

use super::protocol::{BridgeResponse, ToolCallResult, ToolDef};
use super::server::{CallOutcome, PendingSteps};
use super::variants::{
    enum_u32_to_name, instance_path, json_to_variant, ref_to_str, resolve_datamodel,
    resolve_target, variant_to_json,
};
use crate::app::{HarnessAction, Level};
use crate::doc::{self, Document};
use crate::ltypes::LCFrame;
use crate::math::Xform;
use crate::rig::RigType;
use crate::runtime::{describe_error, inst_value, world, RunMode, Side};
use crate::scene::f32_of;

pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "play_control".into(),
            description: "Start, stop, or restart a Play/Run session, switch between Client and Server views, and configure Play RigType (R15/R6/Auto) and physics solver (studio/studio-nextgen/rapier).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start_play", "start_run", "stop", "restart", "view_client", "view_server"],
                        "description": "Play control action to execute."
                    },
                    "rig_type": {
                        "type": "string",
                        "enum": ["R15", "R6", "Auto"],
                        "description": "Optional character rig override applied before starting/restarting Play."
                    },
                    "solver": {
                        "type": "string",
                        "enum": ["studio", "studio-nextgen", "rapier"],
                        "description": "Optional physics solver override applied before starting/restarting Play/Run."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "play_step_frames".into(),
            description: "Advance the running Play/Run simulation by an exact number of frames (1..=600) or elapsed seconds, returning delta_play_time and new Output logs produced during the step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "frames": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600,
                        "description": "Exact number of simulation frames to step (default: 1 when 'seconds' is omitted)."
                    },
                    "seconds": {
                        "type": "number",
                        "minimum": 0.001,
                        "maximum": 30.0,
                        "description": "Optional wall-clock duration in seconds to step (alternative to 'frames')."
                    },
                    "dt": {
                        "type": "number",
                        "minimum": 0.0001,
                        "maximum": 1.0,
                        "description": "Optional fixed frame delta time in seconds (e.g. 0.0166667) applied for the duration of this step."
                    },
                    "auto_start_play": {
                        "type": "boolean",
                        "description": "If true and no Play/Run session is active, start Play mode automatically before stepping (default: false)."
                    }
                }
            }),
        },
        ToolDef {
            name: "play_character_control".into(),
            description: "Inspect and control the live Play player character: get_state, teleport (to [x,y,z] or target instance), move_to, jump, set_stats (Health/MaxHealth/WalkSpeed/JumpPower/JumpHeight), equip_tool, unequip_tools, activate_tool, and respawn.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "get_state",
                            "teleport",
                            "move_to",
                            "jump",
                            "set_stats",
                            "equip_tool",
                            "unequip_tools",
                            "activate_tool",
                            "respawn"
                        ],
                        "description": "Character control action."
                    },
                    "position": {
                        "type": "array",
                        "items": { "type": "number" },
                        "minItems": 3,
                        "maxItems": 3,
                        "description": "Target [x, y, z] world position for teleport or move_to."
                    },
                    "cframe": {
                        "description": "Optional CFrame for teleport ([x,y,z,...] or {pos, look_at|angles_deg})."
                    },
                    "target": {
                        "description": "Optional target instance path or ref:<id> for teleport, move_to, equip_tool, or activate_tool.",
                        "oneOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "offset": {
                        "type": "array",
                        "items": { "type": "number" },
                        "minItems": 3,
                        "maxItems": 3,
                        "description": "Optional [dx, dy, dz] offset when teleporting to a target instance (default: [0, 3, 0])."
                    },
                    "cancel": {
                        "type": "boolean",
                        "description": "When action='move_to', set true to cancel active MoveTo."
                    },
                    "health": { "type": "number" },
                    "max_health": { "type": "number" },
                    "walk_speed": { "type": "number" },
                    "jump_power": { "type": "number" },
                    "jump_height": { "type": "number" },
                    "use_jump_power": { "type": "boolean" },
                    "tool": {
                        "description": "Tool path, ref:<id>, or Tool name in Backpack/Character for equip_tool / activate_tool.",
                        "oneOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "slot": {
                        "type": "integer",
                        "description": "0-based hotbar slot index for equip_tool."
                    },
                    "deactivate": {
                        "type": "boolean",
                        "description": "Also fire Deactivated after Activated when action='activate_tool' (default: false)."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolDef {
            name: "play_input_inject".into(),
            description: "Inject real keyboard (press, down, up, hold), mouse (move, click, right_click, drag, wheel), typed text, or chat into the live game/editor input pipeline.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["press", "down", "up", "hold", "move", "click", "right_click", "drag", "wheel", "type_text", "chat"],
                        "description": "Input action kind (omit when providing an 'events' array)."
                    },
                    "key": { "type": "string", "description": "Key name (e.g. 'W', 'A', 'S', 'D', 'Space', 'LeftShift', 'E', 'Num1', 'Return', 'Escape')." },
                    "duration_secs": { "type": "number", "description": "Hold or drag duration in seconds (default: 0.2 for hold, 0.5 for drag)." },
                    "delay_secs": { "type": "number", "description": "Optional delay in seconds from now before injecting (default: 0)." },
                    "x": { "type": "number", "description": "Viewport-relative X pixel coordinate (defaults to viewport center)." },
                    "y": { "type": "number", "description": "Viewport-relative Y pixel coordinate (defaults to viewport center)." },
                    "x1": { "type": "number" },
                    "y1": { "type": "number" },
                    "x2": { "type": "number" },
                    "y2": { "type": "number" },
                    "dx": { "type": "number", "description": "Relative mouse X delta in pixels (for move/right_drag)." },
                    "dy": { "type": "number", "description": "Relative mouse Y delta in pixels (for move/right_drag)." },
                    "button": { "type": "string", "enum": ["left", "right"], "description": "Mouse button for click/drag." },
                    "steps": { "type": "integer", "minimum": 1, "maximum": 120, "description": "Interpolation steps for drag (default: 10)." },
                    "delta": { "type": "number", "description": "Wheel notches (+ = forward/zoom in, - = backward/zoom out)." },
                    "text": { "type": "string", "description": "Text for type_text or chat." },
                    "normalized": { "type": "boolean", "description": "Interpret x/y/x1/y1/x2/y2 as normalized 0..1 viewport fractions (default: false)." },
                    "events": {
                        "type": "array",
                        "description": "Optional sequence of input action objects (each using the same properties as top-level).",
                        "items": { "type": "object" }
                    }
                }
            }),
        },
        ToolDef {
            name: "output_console".into(),
            description: "Read, filter, clear, or append entries to the RuStudio Output console log (ed.log), returning lines, next_index, and per-level counts.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "level": {
                        "type": "string",
                        "enum": ["all", "info", "warn", "error"],
                        "description": "Filter log lines by level (default: 'all')."
                    },
                    "contains": {
                        "type": "string",
                        "description": "Optional substring filter."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether 'contains' is case-sensitive (default: false)."
                    },
                    "since_index": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Return lines from this 0-based index onward."
                    },
                    "tail": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Return at most the last N matching lines (default: 200 when since_index is omitted, unlimited when since_index is provided)."
                    },
                    "clear": {
                        "type": "boolean",
                        "description": "Clear the Output console log (default: false)."
                    },
                    "append": {
                        "type": "object",
                        "properties": {
                            "level": { "type": "string", "enum": ["info", "warn", "error"] },
                            "text": { "type": "string" }
                        },
                        "required": ["text"],
                        "description": "Append a log line to the Output console."
                    },
                    "strip_stack_mark": {
                        "type": "boolean",
                        "description": "Strip internal stack-indentation marker characters (default: true)."
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
    reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    match name {
        "play_control" => Some(CallOutcome::Immediate(play_control(ed, ctx, args))),
        "play_step_frames" => Some(play_step_frames(ed, ctx, args, reply)),
        "play_character_control" => Some(CallOutcome::Immediate(play_character_control(ed, ctx, args))),
        "play_input_inject" => Some(CallOutcome::Immediate(play_input_inject(ed, ctx, args))),
        "output_console" => Some(CallOutcome::Immediate(output_console(ed, args))),
        _ => None,
    }
}

fn mode_name(ed: &crate::app::Editor) -> &'static str {
    match ed.play.as_ref().map(|p| p.mode) {
        Some(RunMode::Play) => "Play",
        Some(RunMode::Run) => "Run",
        _ => "Edit",
    }
}

fn apply_rig_and_solver(ed: &mut crate::app::Editor, args: &Value) -> Result<(), String> {
    if let Some(rig_str) = args.get("rig_type").and_then(|v| v.as_str()) {
        match rig_str.trim().to_ascii_lowercase().as_str() {
            "r15" => ed.play_rig = Some(RigType::R15),
            "r6" => ed.play_rig = Some(RigType::R6),
            "auto" => ed.play_rig = None,
            other => {
                return Err(format!(
                    "unknown rig_type '{other}'; expected R15, R6, or Auto"
                ));
            }
        }
    }
    if let Some(solver_str) = args.get("solver").and_then(|v| v.as_str()) {
        let norm = solver_str.trim().to_ascii_lowercase();
        match norm.as_str() {
            "studio" | "studio-nextgen" | "rapier" => {
                // `RUSTUDIO_SOLVER` overrides saved settings and handles `studio-nextgen`
                // inside `rustudio_settings::latch_for_play()`.
                std::env::set_var("RUSTUDIO_SOLVER", &norm);
                crate::rustudio_settings::set_choice("solver", &norm, "studio");
            }
            other => {
                return Err(format!(
                    "unknown solver '{other}'; expected studio, studio-nextgen, or rapier"
                ));
            }
        }
    }
    Ok(())
}

fn play_control(ed: &mut crate::app::Editor, ctx: &egui::Context, args: &Value) -> ToolCallResult {
    let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
        return ToolCallResult::error("play_control requires 'action'");
    };

    if let Err(e) = apply_rig_and_solver(ed, args) {
        return ToolCallResult::error(e);
    }

    match action {
        "start_play" => {
            if ed.play.is_some() {
                return ToolCallResult::error(
                    "a Play/Run session is already active; stop or restart it first",
                );
            }
            ed.start.visible = false;
            ed.switch_to_viewport = true;
            ed.play_here = None;
            ed.start_play(RunMode::Play);
            if ed.harness.from_play && ed.harness.play_started.is_none() {
                ed.harness.play_started = Some(crate::sim_clock::now());
            }
            if ed.play.is_none() {
                return ToolCallResult::error(
                    "could not start Play session (place may still be loading)",
                );
            }
        }
        "start_run" => {
            if ed.play.is_some() {
                return ToolCallResult::error(
                    "a Play/Run session is already active; stop or restart it first",
                );
            }
            ed.start.visible = false;
            ed.switch_to_viewport = true;
            ed.play_here = None;
            ed.start_play(RunMode::Run);
            if ed.harness.from_play && ed.harness.play_started.is_none() {
                ed.harness.play_started = Some(crate::sim_clock::now());
            }
            if ed.play.is_none() {
                return ToolCallResult::error(
                    "could not start Run session (place may still be loading)",
                );
            }
        }
        "stop" => {
            let was_running = ed.play.is_some();
            if was_running {
                ed.stop_play();
            }
            ctx.request_repaint();
            return ToolCallResult::json(&json!({
                "action": "stop",
                "was_running": was_running,
                "mode": mode_name(ed),
                "showing_side": Value::Null,
                "play_time": 0.0,
                "has_character": false,
            }));
        }
        "restart" => {
            let mode = ed
                .play
                .as_ref()
                .map(|p| p.mode)
                .unwrap_or(RunMode::Play);
            if ed.play.is_some() {
                ed.stop_play();
            }
            ed.start.visible = false;
            ed.switch_to_viewport = true;
            ed.play_here = None;
            ed.start_play(mode);
            if ed.harness.from_play {
                ed.harness.play_started = Some(crate::sim_clock::now());
                ed.harness.play_game_time = Some(0.0);
            }
            if ed.play.is_none() {
                return ToolCallResult::error("could not restart Play/Run session");
            }
        }
        "view_client" => {
            let Some(p) = ed.play.as_ref() else {
                return ToolCallResult::error("view_client requires an active Play session");
            };
            if p.mode != RunMode::Play || ed.play_side_lua(Side::Client).is_none() {
                return ToolCallResult::error("Run mode has no Client view (use Play mode)");
            }
            ed.show_play_side(Side::Client);
        }
        "view_server" => {
            if ed.play.is_none() {
                return ToolCallResult::error("view_server requires an active Play or Run session");
            }
            ed.show_play_side(Side::Server);
        }
        other => {
            return ToolCallResult::error(format!(
                "unknown play_control action '{other}'; expected start_play, start_run, stop, restart, view_client, or view_server"
            ));
        }
    }

    ctx.request_repaint();
    let rig_label = match ed.play_rig {
        Some(RigType::R15) => "R15",
        Some(RigType::R6) => "R6",
        None => "Auto",
    };
    let solver = if ed.play.is_some() {
        crate::solver::kind().name()
    } else {
        crate::rustudio_settings::solver_now().name()
    };

    ToolCallResult::json(&json!({
        "action": action,
        "mode": mode_name(ed),
        "showing_side": ed.play.as_ref().map(|p| p.showing.label()),
        "rig_type": rig_label,
        "solver": solver,
        "play_time": ed.play.as_ref().map(|p| p.time).unwrap_or(0.0),
        "has_character": ed.play_has_character(),
    }))
}

fn play_step_frames(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
    reply: &Sender<BridgeResponse>,
) -> CallOutcome {
    let auto_start = args
        .get("auto_start_play")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let seconds = args.get("seconds").and_then(|v| v.as_f64());
    let frames_opt = args.get("frames").and_then(|v| v.as_u64());
    let dt_opt = args
        .get("dt")
        .or_else(|| args.get("fixed_dt"))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32);

    if seconds.is_some() && frames_opt.is_some() {
        return CallOutcome::Immediate(ToolCallResult::error(
            "specify either 'frames' or 'seconds', not both",
        ));
    }

    let frames = if seconds.is_none() {
        let f = frames_opt.unwrap_or(1);
        if !(1..=600).contains(&f) {
            return CallOutcome::Immediate(ToolCallResult::error(
                "frames must be between 1 and 600",
            ));
        }
        Some(f as u32)
    } else {
        None
    };

    if let Some(secs) = seconds {
        if !secs.is_finite() || !(0.001..=30.0).contains(&secs) {
            return CallOutcome::Immediate(ToolCallResult::error(
                "seconds must be between 0.001 and 30.0",
            ));
        }
    }

    if let Some(dt) = dt_opt {
        if !dt.is_finite() || !(0.0001..=1.0).contains(&dt) {
            return CallOutcome::Immediate(ToolCallResult::error(
                "dt must be between 0.0001 and 1.0",
            ));
        }
    }

    if ed
        .mcp
        .as_ref()
        .is_some_and(|m| m.state.pending_steps.is_some())
    {
        return CallOutcome::Immediate(ToolCallResult::error(
            "another play_step_frames operation is already in progress",
        ));
    }
    if ed.mcp.is_none() {
        return CallOutcome::Immediate(ToolCallResult::error("OmniMCP server state unavailable"));
    }

    if ed.play.is_none() {
        if auto_start {
            ed.start.visible = false;
            ed.switch_to_viewport = true;
            ed.play_here = None;
            ed.start_play(RunMode::Play);
            if ed.harness.from_play && ed.harness.play_started.is_none() {
                ed.harness.play_started = Some(crate::sim_clock::now());
            }
        }
        if ed.play.is_none() {
            return CallOutcome::Immediate(ToolCallResult::error(
                "Play/Run session is not active (start Play/Run or pass auto_start_play: true)",
            ));
        }
    }

    let start_play_time = ed.play.as_ref().map(|p| p.time).unwrap_or(0.0);
    let log_start_index = ed.log.len();
    let saved_fixed_dt = dt_opt.map(|dt| {
        let prev = ed.fixed_dt;
        ed.fixed_dt = Some(dt);
        prev
    });

    // `tick_mcp_steps` runs later on the SAME frame (after `play_tick` already ran),
    // so `remaining_frames` needs `frames + 1` to step `frames` new simulation ticks.
    let pending = PendingSteps {
        remaining_frames: frames.map(|f| f.saturating_add(1)),
        target_elapsed_secs: seconds,
        frames_stepped: 0,
        started_at: Instant::now(),
        start_play_time,
        saved_fixed_dt,
        log_start_index,
        reply: reply.clone(),
    };

    if let Some(mcp) = ed.mcp.as_mut() {
        mcp.state.pending_steps = Some(pending);
    }
    ctx.request_repaint();
    CallOutcome::Deferred
}

fn parse_vec3_json(v: &Value) -> Result<Vec3, String> {
    match v {
        Value::Array(a) if a.len() == 3 => {
            let x = a[0].as_f64().ok_or("vector x must be number")? as f32;
            let y = a[1].as_f64().ok_or("vector y must be number")? as f32;
            let z = a[2].as_f64().ok_or("vector z must be number")? as f32;
            Ok(Vec3::new(x, y, z))
        }
        Value::Object(m) => {
            let x = m
                .get("x")
                .or_else(|| m.get("X"))
                .and_then(|n| n.as_f64())
                .ok_or("vector missing x")? as f32;
            let y = m
                .get("y")
                .or_else(|| m.get("Y"))
                .and_then(|n| n.as_f64())
                .ok_or("vector missing y")? as f32;
            let z = m
                .get("z")
                .or_else(|| m.get("Z"))
                .and_then(|n| n.as_f64())
                .ok_or("vector missing z")? as f32;
            Ok(Vec3::new(x, y, z))
        }
        _ => Err(format!("expected [x, y, z] or {{x, y, z}}, got {v}")),
    }
}

fn resolve_instance_position(doc: &Document, r: Ref) -> Option<Vec3> {
    if let Some(xf) = crate::scene::xform_of(doc, r) {
        return Some(xf.pos);
    }
    if let Some(Variant::Ref(pp)) = doc.prop(r, "PrimaryPart") {
        if pp.is_some() && doc.exists(pp) {
            if let Some(xf) = crate::scene::xform_of(doc, pp) {
                return Some(xf.pos);
            }
        }
    }
    if let Some(Variant::CFrame(wp)) = doc.prop(r, "WorldPivot") {
        return Some(Vec3::new(wp.position.x, wp.position.y, wp.position.z));
    }
    for d in doc.dom.descendants_of(r) {
        if doc::is_a(&d.class, "BasePart") {
            if let Some(xf) = crate::scene::xform_of(doc, d.referent()) {
                return Some(xf.pos);
            }
        }
    }
    None
}

fn find_tool_for_character(
    ed: &crate::app::Editor,
    query: &Value,
) -> Result<(Side, Ref), String> {
    let s = ed
        .play
        .as_ref()
        .ok_or_else(|| "Play session is not active".to_string())?;
    let player_s = s.player;
    let char_s = s.character.as_ref().map(|c| c.model);

    let server_doc = resolve_datamodel(ed, Some("Server"))?;
    if let Ok(r) = resolve_target(server_doc, query) {
        if server_doc
            .class_of(r)
            .is_some_and(|c| doc::is_a(&c, "BackpackItem"))
        {
            return Ok((Side::Server, r));
        }
    }

    if let Some(name) = query.as_str() {
        let mut roots = Vec::new();
        if let Some(cm) = char_s {
            roots.push(cm);
        }
        if let Some(bp) = server_doc.find_child(player_s, "Backpack") {
            roots.push(bp);
        }
        if let Some(ws) = server_doc.workspace() {
            roots.push(ws);
        }
        for root in roots {
            if let Some(r) = server_doc.find_named(root, name) {
                if server_doc
                    .class_of(r)
                    .is_some_and(|c| doc::is_a(&c, "BackpackItem"))
                {
                    return Ok((Side::Server, r));
                }
            }
        }
    }

    if let Ok(client_doc) = resolve_datamodel(ed, Some("Client")) {
        if let Ok(r) = resolve_target(client_doc, query) {
            if client_doc
                .class_of(r)
                .is_some_and(|c| doc::is_a(&c, "BackpackItem"))
            {
                return Ok((Side::Client, r));
            }
        }
        if let Some(name) = query.as_str() {
            if let Some(cp) = s.map.s2c.get(&player_s).copied() {
                let mut roots = Vec::new();
                if let Some(cm) = char_s.and_then(|m| s.map.s2c.get(&m).copied()) {
                    roots.push(cm);
                }
                if let Some(bp) = client_doc.find_child(cp, "Backpack") {
                    roots.push(bp);
                }
                for root in roots {
                    if let Some(r) = client_doc.find_named(root, name) {
                        if client_doc
                            .class_of(r)
                            .is_some_and(|c| doc::is_a(&c, "BackpackItem"))
                        {
                            return Ok((Side::Client, r));
                        }
                    }
                }
            }
        }
    }

    Err(format!("tool not found: {query}"))
}

fn find_equipped_tool(ed: &crate::app::Editor) -> Option<(Side, Ref)> {
    let s = ed.play.as_ref()?;
    let ch = s.character.as_ref()?;
    let server_doc = resolve_datamodel(ed, Some("Server")).ok()?;
    if let Some(m) = server_doc.dom.get_by_ref(ch.model) {
        if let Some(&t) = m
            .children()
            .iter()
            .find(|&&c| server_doc.class_of(c).is_some_and(|k| doc::is_a(&k, "Tool")))
        {
            return Some((Side::Server, t));
        }
    }
    if let Ok(client_doc) = resolve_datamodel(ed, Some("Client")) {
        if let Some(cm) = s.map.s2c.get(&ch.model).copied() {
            if let Some(m) = client_doc.dom.get_by_ref(cm) {
                if let Some(&t) = m
                    .children()
                    .iter()
                    .find(|&&c| client_doc.class_of(c).is_some_and(|k| doc::is_a(&k, "Tool")))
                {
                    return Some((Side::Client, t));
                }
            }
        }
    }
    None
}

fn play_character_control(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
        return ToolCallResult::error("play_character_control requires 'action'");
    };
    let Some(mode) = ed.play.as_ref().map(|p| p.mode) else {
        return ToolCallResult::error("play_character_control requires an active Play session");
    };
    if mode != RunMode::Play {
        return ToolCallResult::error(
            "Run mode has no Player or Character (start Play mode instead)",
        );
    }

    match action {
        "get_state" => {
            let server_doc = match resolve_datamodel(ed, Some("Server")) {
                Ok(d) => d,
                Err(e) => return ToolCallResult::error(e),
            };
            let s = ed.play.as_ref().expect("checked");
            let player_ref = s.player;
            let player_json = if player_ref.is_some() && server_doc.exists(player_ref) {
                json!({
                    "ref": ref_to_str(player_ref),
                    "path": instance_path(server_doc, player_ref),
                    "name": server_doc.dom.get_by_ref(player_ref).map(|i| i.name.as_str()),
                })
            } else {
                Value::Null
            };

            let backpack_tools: Vec<Value> = server_doc
                .find_child(player_ref, "Backpack")
                .and_then(|bp| server_doc.dom.get_by_ref(bp))
                .map(|bp_inst| {
                    bp_inst
                        .children()
                        .iter()
                        .filter(|&&c| {
                            server_doc
                                .class_of(c)
                                .is_some_and(|k| doc::is_a(&k, "BackpackItem"))
                        })
                        .map(|&c| {
                            json!({
                                "ref": ref_to_str(c),
                                "path": instance_path(server_doc, c),
                                "name": server_doc.dom.get_by_ref(c).map(|i| i.name.as_str()),
                                "class": server_doc.class_of(c).map(|k| k.as_str().to_string()),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let equipped_json = find_equipped_tool(ed).map(|(side, tr)| {
                let d = match side {
                    Side::Server => Some(server_doc),
                    Side::Client => resolve_datamodel(ed, Some("Client")).ok(),
                };
                json!({
                    "side": side.label(),
                    "ref": ref_to_str(tr),
                    "path": d.map(|doc| instance_path(doc, tr)),
                    "name": d.and_then(|doc| doc.dom.get_by_ref(tr).map(|i| i.name.clone())),
                })
            });

            let slots_json: Vec<Value> = s
                .game
                .slots
                .iter()
                .enumerate()
                .map(|(idx, &r)| {
                    json!({
                        "slot": idx,
                        "ref": ref_to_str(r),
                        "name": server_doc.dom.get_by_ref(r).map(|i| i.name.as_str()),
                    })
                })
                .collect();

            let char_json = s.character.as_ref().map(|c| {
                let xf = crate::scene::xform_of(server_doc, c.rig.root);
                let pos = xf.map(|x| x.pos).unwrap_or(c.body.pos);
                let client_hum = resolve_datamodel(ed, Some("Client"))
                    .ok()
                    .zip(s.map.s2c.get(&c.humanoid).copied());
                let read_hum_f32 = |prop: &str, default: f32| {
                    client_hum
                        .and_then(|(cd, ch)| f32_of(cd.prop(ch, prop)))
                        .or_else(|| f32_of(server_doc.prop(c.humanoid, prop)))
                        .unwrap_or(default)
                };
                let use_jump_power = client_hum
                    .and_then(|(cd, ch)| cd.prop(ch, "UseJumpPower"))
                    .or_else(|| server_doc.prop(c.humanoid, "UseJumpPower"))
                    .is_some_and(|v| matches!(v, Variant::Bool(true)));

                json!({
                    "dead": c.dead,
                    "rig_type": if c.rig_type == RigType::R15 { "R15" } else { "R6" },
                    "state": c.state.enum_name(),
                    "model": {
                        "ref": ref_to_str(c.model),
                        "path": instance_path(server_doc, c.model),
                    },
                    "humanoid": {
                        "ref": ref_to_str(c.humanoid),
                        "path": instance_path(server_doc, c.humanoid),
                    },
                    "root_ref": ref_to_str(c.rig.root),
                    "position": [pos.x, pos.y, pos.z],
                    "cframe": xf.map(|x| variant_to_json(&Variant::CFrame(x.to_cframe()))),
                    "velocity": [c.velocity.x, c.velocity.y, c.velocity.z],
                    "yaw_deg": c.yaw.to_degrees(),
                    "floor_material": {
                        "value": c.floor_material,
                        "name": enum_u32_to_name("Material", c.floor_material),
                    },
                    "seated": c.seat.is_some(),
                    "seat_ref": c.seat.map(ref_to_str),
                    "health": read_hum_f32("Health", 100.0),
                    "max_health": read_hum_f32("MaxHealth", 100.0),
                    "walk_speed": read_hum_f32("WalkSpeed", crate::mech::WALK_SPEED),
                    "jump_power": read_hum_f32("JumpPower", crate::mech::JUMP_POWER),
                    "jump_height": read_hum_f32("JumpHeight", crate::mech::JUMP_HEIGHT),
                    "use_jump_power": use_jump_power,
                })
            });

            ToolCallResult::json(&json!({
                "has_character": ed.play_has_character(),
                "play_time": s.time,
                "player": player_json,
                "character": char_json,
                "equipped_tool": equipped_json,
                "backpack_tools": backpack_tools,
                "hotbar_slots": slots_json,
                "move_target": s.game.move_target.map(|t| [t.x, t.y, t.z]),
            }))
        }

        "teleport" => {
            let (model_s, root_s, cur_xf) = {
                let server_doc = match resolve_datamodel(ed, Some("Server")) {
                    Ok(d) => d,
                    Err(e) => return ToolCallResult::error(e),
                };
                let s = ed.play.as_ref().expect("checked");
                let Some(ch) = s.character.as_ref().filter(|c| !c.dead) else {
                    return ToolCallResult::error("player has no live character");
                };
                let cur_xf = crate::scene::xform_of(server_doc, ch.rig.root).unwrap_or(Xform {
                    pos: ch.body.pos,
                    rot: Mat3::from_rotation_y(ch.yaw),
                });
                (ch.model, ch.rig.root, cur_xf)
            };

            let new_xf = if let Some(cf_val) = args.get("cframe") {
                match json_to_variant(
                    None,
                    None,
                    None,
                    cf_val,
                    Some(rbx_dom_weak::types::VariantType::CFrame),
                ) {
                    Ok(Variant::CFrame(cf)) => Xform::from_cframe(&cf).orthonormalized(),
                    Ok(_) => return ToolCallResult::error("invalid CFrame"),
                    Err(e) => return ToolCallResult::error(e),
                }
            } else if let Some(pos_val) = args.get("position") {
                let pos = match parse_vec3_json(pos_val) {
                    Ok(p) => p,
                    Err(e) => return ToolCallResult::error(e),
                };
                Xform {
                    pos,
                    rot: cur_xf.rot,
                }
            } else if let Some(target_val) = args.get("target") {
                let server_doc = match resolve_datamodel(ed, Some("Server")) {
                    Ok(d) => d,
                    Err(e) => return ToolCallResult::error(e),
                };
                let r = match resolve_target(server_doc, target_val) {
                    Ok(r) => r,
                    Err(e) => return ToolCallResult::error(e),
                };
                let Some(base_pos) = resolve_instance_position(server_doc, r) else {
                    return ToolCallResult::error("target instance has no world position");
                };
                let offset = match args.get("offset") {
                    Some(ov) => match parse_vec3_json(ov) {
                        Ok(o) => o,
                        Err(e) => return ToolCallResult::error(e),
                    },
                    None => Vec3::new(0.0, 3.0, 0.0),
                };
                Xform {
                    pos: base_pos + offset,
                    rot: cur_xf.rot,
                }
            } else {
                return ToolCallResult::error(
                    "teleport requires 'position', 'cframe', or 'target'",
                );
            };

            let pivot_res = ed.with_play_side(Side::Server, |rt, _, _| -> Result<(), String> {
                let lua = &rt.lua;
                let m = inst_value(lua, model_s).map_err(|e| describe_error(&e))?;
                let cf = lua
                    .create_userdata(LCFrame(new_xf))
                    .map_err(|e| describe_error(&e))?;
                let f = lua
                    .load("local m, cf = ...; m:PivotTo(cf)")
                    .into_function()
                    .map_err(|e| describe_error(&e))?;
                f.call::<()>((m, LuaValue::UserData(cf)))
                    .map_err(|e| describe_error(&e))?;
                rt.dispatch();
                Ok(())
            });
            match pivot_res {
                Some(Ok(())) => {}
                Some(Err(e)) => return ToolCallResult::error(e),
                None => return ToolCallResult::error("Server VM unavailable"),
            }

            // Mirror to Client copy and sync physics immediately so reads before the next tick match.
            let client_model = ed
                .play
                .as_ref()
                .and_then(|s| s.map.s2c.get(&model_s).copied());
            if let Some(cm) = client_model {
                let _ = ed.with_play_side(Side::Client, |rt, _, _| {
                    let lua = &rt.lua;
                    if let (Ok(m), Ok(cf)) = (inst_value(lua, cm), lua.create_userdata(LCFrame(new_xf))) {
                        if let Ok(f) = lua.load("local m, cf = ...; m:PivotTo(cf)").into_function() {
                            let _ = f.call::<()>((m, LuaValue::UserData(cf)));
                        }
                    }
                    rt.dispatch();
                });
            }

            if let Some(s) = ed.play.as_mut() {
                s.physics.teleport_humanoid(model_s, &new_xf);
                if let Some(ch) = s.character.as_mut() {
                    ch.body.pos = new_xf.pos;
                    ch.velocity = Vec3::ZERO;
                }
            }
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "teleport",
                "root_ref": ref_to_str(root_s),
                "position": [new_xf.pos.x, new_xf.pos.y, new_xf.pos.z],
            }))
        }

        "move_to" => {
            let cancel = args
                .get("cancel")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if cancel {
                if let Some(s) = ed.play.as_mut() {
                    s.game.move_target = None;
                }
                ctx.request_repaint();
                return ToolCallResult::json(&json!({
                    "action": "move_to",
                    "cancelled": true,
                }));
            }

            let target_pos = if let Some(pos_val) = args.get("position") {
                match parse_vec3_json(pos_val) {
                    Ok(p) => p,
                    Err(e) => return ToolCallResult::error(e),
                }
            } else if let Some(target_val) = args.get("target") {
                let server_doc = match resolve_datamodel(ed, Some("Server")) {
                    Ok(d) => d,
                    Err(e) => return ToolCallResult::error(e),
                };
                let r = match resolve_target(server_doc, target_val) {
                    Ok(r) => r,
                    Err(e) => return ToolCallResult::error(e),
                };
                let Some(pos) = resolve_instance_position(server_doc, r) else {
                    return ToolCallResult::error("target instance has no world position");
                };
                pos
            } else {
                return ToolCallResult::error(
                    "move_to requires 'position', 'target', or 'cancel: true'",
                );
            };

            let Some(s) = ed.play.as_mut() else {
                return ToolCallResult::error("Play session is not active");
            };
            if s.character.as_ref().is_none_or(|c| c.dead) {
                return ToolCallResult::error("player has no live character");
            }
            s.game.move_target = Some(target_pos);
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "move_to",
                "target_position": [target_pos.x, target_pos.y, target_pos.z],
            }))
        }

        "jump" => {
            let Some(s) = ed.play.as_mut() else {
                return ToolCallResult::error("Play session is not active");
            };
            if s.character.as_ref().is_none_or(|c| c.dead) {
                return ToolCallResult::error("player has no live character");
            }
            s.game.jump = true;
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "jump",
                "queued": true,
            }))
        }

        "set_stats" => {
            let Some((hum_s, hum_c)) = ed.play.as_ref().and_then(|s| {
                let c = s.character.as_ref().filter(|c| !c.dead)?;
                Some((c.humanoid, s.map.s2c.get(&c.humanoid).copied()))
            }) else {
                return ToolCallResult::error("player has no live character");
            };

            let get_num = |a: &str, b: &str| {
                args.get(a)
                    .or_else(|| args.get(b))
                    .and_then(|v| v.as_f64())
            };
            let max_health = get_num("max_health", "MaxHealth");
            let health = get_num("health", "Health");
            let walk_speed = get_num("walk_speed", "WalkSpeed");
            let jump_power = get_num("jump_power", "JumpPower");
            let jump_height = get_num("jump_height", "JumpHeight");
            let use_jump_power = args
                .get("use_jump_power")
                .or_else(|| args.get("UseJumpPower"))
                .and_then(|v| v.as_bool());

            let mut updates: Vec<(&'static str, LuaValue)> = Vec::new();
            // Apply MaxHealth before Health so raising both doesn't clamp Health.
            if let Some(v) = max_health {
                updates.push(("MaxHealth", LuaValue::Number(v)));
            }
            if let Some(v) = health {
                updates.push(("Health", LuaValue::Number(v)));
            }
            if let Some(v) = walk_speed {
                updates.push(("WalkSpeed", LuaValue::Number(v)));
            }
            if let Some(on) = use_jump_power {
                updates.push(("UseJumpPower", LuaValue::Boolean(on)));
            } else if jump_power.is_some() && jump_height.is_none() {
                updates.push(("UseJumpPower", LuaValue::Boolean(true)));
            } else if jump_height.is_some() && jump_power.is_none() {
                updates.push(("UseJumpPower", LuaValue::Boolean(false)));
            }
            if let Some(v) = jump_power {
                updates.push(("JumpPower", LuaValue::Number(v)));
            }
            if let Some(v) = jump_height {
                updates.push(("JumpHeight", LuaValue::Number(v)));
            }

            if updates.is_empty() {
                return ToolCallResult::error(
                    "set_stats requires at least one of health, max_health, walk_speed, jump_power, jump_height, use_jump_power",
                );
            }

            let server_updates = updates.clone();
            ed.with_play_side(Side::Server, |rt, _, _| {
                for (prop, val) in server_updates {
                    rt.set(hum_s, prop, val);
                }
                rt.dispatch();
            });
            if let Some(hc) = hum_c {
                let client_updates = updates.clone();
                ed.with_play_side(Side::Client, |rt, _, _| {
                    for (prop, val) in client_updates {
                        rt.set(hc, prop, val);
                    }
                    rt.dispatch();
                });
            }
            ctx.request_repaint();

            let mut applied = serde_json::Map::new();
            for (k, v) in &updates {
                match v {
                    LuaValue::Number(n) => {
                        applied.insert((*k).to_string(), json!(n));
                    }
                    LuaValue::Boolean(b) => {
                        applied.insert((*k).to_string(), json!(b));
                    }
                    _ => {}
                }
            }
            ToolCallResult::json(&json!({
                "action": "set_stats",
                "humanoid_ref": ref_to_str(hum_s),
                "applied": applied,
            }))
        }

        "equip_tool" => {
            let Some(hum_s) = ed
                .play
                .as_ref()
                .and_then(|s| s.character.as_ref().filter(|c| !c.dead).map(|c| c.humanoid))
            else {
                return ToolCallResult::error("player has no live character");
            };

            let (side, tool_ref) = if let Some(slot_idx) = args.get("slot").and_then(|v| v.as_u64()) {
                let Some(tool_r) = ed
                    .play
                    .as_ref()
                    .and_then(|s| s.game.slots.get(slot_idx as usize).copied())
                else {
                    return ToolCallResult::error(format!("no tool in hotbar slot {slot_idx}"));
                };
                (Side::Server, tool_r)
            } else if let Some(tool_q) = args.get("tool").or_else(|| args.get("target")) {
                match find_tool_for_character(ed, tool_q) {
                    Ok(found) => found,
                    Err(e) => return ToolCallResult::error(e),
                }
            } else {
                return ToolCallResult::error("equip_tool requires 'tool', 'target', or 'slot'");
            };

            let hum_target = match side {
                Side::Server => hum_s,
                Side::Client => {
                    let Some(hc) = ed
                        .play
                        .as_ref()
                        .and_then(|s| s.map.s2c.get(&hum_s).copied())
                    else {
                        return ToolCallResult::error("client Humanoid not found");
                    };
                    hc
                }
            };

            let equip_res = ed.with_play_side(side, |rt, _, _| -> Result<(), String> {
                let lua = &rt.lua;
                let h = inst_value(lua, hum_target).map_err(|e| describe_error(&e))?;
                let t = inst_value(lua, tool_ref).map_err(|e| describe_error(&e))?;
                let f = lua
                    .load("local h, t = ...; h:EquipTool(t)")
                    .into_function()
                    .map_err(|e| describe_error(&e))?;
                f.call::<()>((h, t)).map_err(|e| describe_error(&e))?;
                rt.dispatch();
                Ok(())
            });
            match equip_res {
                Some(Ok(())) => {}
                Some(Err(e)) => return ToolCallResult::error(e),
                None => return ToolCallResult::error("VM unavailable for equip_tool"),
            }
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "equip_tool",
                "side": side.label(),
                "tool_ref": ref_to_str(tool_ref),
            }))
        }

        "unequip_tools" => {
            let Some(hum_s) = ed
                .play
                .as_ref()
                .and_then(|s| s.character.as_ref().filter(|c| !c.dead).map(|c| c.humanoid))
            else {
                return ToolCallResult::error("player has no live character");
            };
            let hum_c = ed
                .play
                .as_ref()
                .and_then(|s| s.map.s2c.get(&hum_s).copied());
            let has_client = ed.play_side_lua(Side::Client).is_some();

            let _ = ed.with_play_side(Side::Server, |rt, _, _| {
                let lua = &rt.lua;
                if let Ok(h) = inst_value(lua, hum_s) {
                    if let Ok(f) = lua.load("local h = ...; h:UnequipTools()").into_function() {
                        let _ = f.call::<()>(h);
                    }
                }
                rt.dispatch();
            });
            if has_client {
                if let Some(hc) = hum_c {
                    let _ = ed.with_play_side(Side::Client, |rt, _, _| {
                        let lua = &rt.lua;
                        if let Ok(h) = inst_value(lua, hc) {
                            if let Ok(f) =
                                lua.load("local h = ...; h:UnequipTools()").into_function()
                            {
                                let _ = f.call::<()>(h);
                            }
                        }
                        rt.dispatch();
                    });
                }
            }
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "unequip_tools",
                "unequipped": true,
            }))
        }

        "activate_tool" => {
            let (side, tool_ref) = if let Some(tool_q) = args.get("tool").or_else(|| args.get("target")) {
                match find_tool_for_character(ed, tool_q) {
                    Ok(found) => found,
                    Err(e) => return ToolCallResult::error(e),
                }
            } else if let Some(found) = find_equipped_tool(ed) {
                found
            } else {
                return ToolCallResult::error("no equipped tool found (equip a tool first or pass 'tool')");
            };

            let deactivate = args
                .get("deactivate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            match side {
                Side::Server => {
                    let client_tool_ref = ed
                        .play
                        .as_ref()
                        .and_then(|s| s.map.s2c.get(&tool_ref).copied());
                    ed.with_play_side(Side::Server, |rt, _, _| {
                        rt.fire(tool_ref, "Activated", vec![]);
                        if deactivate {
                            rt.fire(tool_ref, "Deactivated", vec![]);
                        }
                        rt.dispatch();
                    });
                    if let Some(ct) = client_tool_ref {
                        ed.with_play_side(Side::Client, |rt, _, _| {
                            rt.fire(ct, "Activated", vec![]);
                            if deactivate {
                                rt.fire(ct, "Deactivated", vec![]);
                            }
                            rt.dispatch();
                        });
                    }
                }
                Side::Client => {
                    ed.with_play_side(Side::Client, |rt, _, _| {
                        rt.fire(tool_ref, "Activated", vec![]);
                        if deactivate {
                            rt.fire(tool_ref, "Deactivated", vec![]);
                        }
                        rt.dispatch();
                    });
                }
            }
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "activate_tool",
                "side": side.label(),
                "tool_ref": ref_to_str(tool_ref),
                "deactivated": deactivate,
            }))
        }

        "respawn" => {
            let player_ref = ed.play.as_ref().map(|s| s.player).unwrap_or_else(Ref::none);
            if player_ref.is_none() {
                return ToolCallResult::error("no Player instance in Play session");
            }
            ed.with_play_side(Side::Server, |rt, _, _| {
                if let Ok(mut w) = world(&rt.lua) {
                    w.load_character.push(player_ref);
                }
            });
            ctx.request_repaint();

            ToolCallResult::json(&json!({
                "action": "respawn",
                "player_ref": ref_to_str(player_ref),
                "queued": true,
            }))
        }

        other => ToolCallResult::error(format!("unknown play_character_control action '{other}'")),
    }
}

enum ParsedKey {
    Key(egui::Key),
    Shift,
}

fn parse_egui_key(raw: &str) -> Result<ParsedKey, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("shift")
        || trimmed.eq_ignore_ascii_case("leftshift")
        || trimmed.eq_ignore_ascii_case("rightshift")
    {
        return Ok(ParsedKey::Shift);
    }
    if let Some(k) = egui::Key::from_name(trimmed) {
        return Ok(ParsedKey::Key(k));
    }
    let key = match trimmed.to_ascii_lowercase().as_str() {
        "return" | "enter" => egui::Key::Enter,
        "up" | "arrowup" => egui::Key::ArrowUp,
        "down" | "arrowdown" => egui::Key::ArrowDown,
        "left" | "arrowleft" => egui::Key::ArrowLeft,
        "right" | "arrowright" => egui::Key::ArrowRight,
        "space" | "spacebar" | " " => egui::Key::Space,
        "esc" | "escape" => egui::Key::Escape,
        "back" | "backspace" => egui::Key::Backspace,
        "del" | "delete" => egui::Key::Delete,
        "tab" => egui::Key::Tab,
        "ins" | "insert" => egui::Key::Insert,
        "home" => egui::Key::Home,
        "end" => egui::Key::End,
        "pageup" => egui::Key::PageUp,
        "pagedown" => egui::Key::PageDown,
        "0" | "zero" | "num0" => egui::Key::Num0,
        "1" | "one" | "num1" => egui::Key::Num1,
        "2" | "two" | "num2" => egui::Key::Num2,
        "3" | "three" | "num3" => egui::Key::Num3,
        "4" | "four" | "num4" => egui::Key::Num4,
        "5" | "five" | "num5" => egui::Key::Num5,
        "6" | "six" | "num6" => egui::Key::Num6,
        "7" | "seven" | "num7" => egui::Key::Num7,
        "8" | "eight" | "num8" => egui::Key::Num8,
        "9" | "nine" | "num9" => egui::Key::Num9,
        "/" | "slash" => egui::Key::Slash,
        "," | "comma" => egui::Key::Comma,
        "." | "period" => egui::Key::Period,
        "-" | "minus" => egui::Key::Minus,
        "=" | "equals" => egui::Key::Equals,
        ";" | "semicolon" => egui::Key::Semicolon,
        "'" | "quote" => egui::Key::Quote,
        "[" | "leftbracket" | "openbracket" => egui::Key::OpenBracket,
        "]" | "rightbracket" | "closebracket" => egui::Key::CloseBracket,
        "\\" | "backslash" => egui::Key::Backslash,
        "`" | "backquote" | "backtick" => egui::Key::Backtick,
        _ => return Err(format!("unknown key '{raw}'")),
    };
    Ok(ParsedKey::Key(key))
}

fn viewport_size(ed: &crate::app::Editor, ctx: &egui::Context) -> (f32, f32) {
    if ed.viewport_rect.is_finite() && ed.viewport_rect.is_positive() {
        (ed.viewport_rect.width(), ed.viewport_rect.height())
    } else {
        let r = ctx.content_rect();
        if r.is_finite() && r.is_positive() {
            (r.width().max(1.0), r.height().max(1.0))
        } else {
            (1280.0, 720.0)
        }
    }
}

fn resolve_xy(
    item: &Value,
    kx: &str,
    ky: &str,
    vw: f32,
    vh: f32,
    normalized: bool,
) -> (f32, f32) {
    let x = item
        .get(kx)
        .and_then(|v| v.as_f64())
        .map(|n| if normalized { n as f32 * vw } else { n as f32 })
        .unwrap_or(vw * 0.5);
    let y = item
        .get(ky)
        .and_then(|v| v.as_f64())
        .map(|n| if normalized { n as f32 * vh } else { n as f32 })
        .unwrap_or(vh * 0.5);
    (x, y)
}

fn queue_input_event(
    ed: &mut crate::app::Editor,
    vw: f32,
    vh: f32,
    base_t: f32,
    item: &Value,
) -> Result<usize, String> {
    let action = item
        .get("action")
        .or_else(|| item.get("kind"))
        .and_then(|v| v.as_str())
        .ok_or("input event requires 'action'")?;
    let delay = item
        .get("delay_secs")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .max(0.0) as f32;
    let at = base_t + delay;
    let normalized = item
        .get("normalized")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut queued = 0usize;
    let mut push = |t: f32, a: HarnessAction| {
        ed.harness.actions.push((t, a));
        queued += 1;
    };

    match action {
        "press" => {
            let key_s = item
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("keyboard action requires 'key'")?;
            match parse_egui_key(key_s)? {
                ParsedKey::Key(k) => push(at, HarnessAction::Key(k)),
                ParsedKey::Shift => {
                    push(at, HarnessAction::Shift(true));
                    push(at + 0.05, HarnessAction::Shift(false));
                }
            }
        }
        "down" => {
            let key_s = item
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("keyboard action requires 'key'")?;
            match parse_egui_key(key_s)? {
                ParsedKey::Key(k) => push(at, HarnessAction::KeyState(k, true)),
                ParsedKey::Shift => push(at, HarnessAction::Shift(true)),
            }
        }
        "up" => {
            let key_s = item
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("keyboard action requires 'key'")?;
            match parse_egui_key(key_s)? {
                ParsedKey::Key(k) => push(at, HarnessAction::KeyState(k, false)),
                ParsedKey::Shift => push(at, HarnessAction::Shift(false)),
            }
        }
        "hold" => {
            let key_s = item
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("keyboard action requires 'key'")?;
            let dur = item
                .get("duration_secs")
                .or_else(|| item.get("hold_secs"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.2)
                .clamp(0.01, 30.0) as f32;
            match parse_egui_key(key_s)? {
                ParsedKey::Key(k) => {
                    push(at, HarnessAction::KeyState(k, true));
                    push(at + dur, HarnessAction::KeyState(k, false));
                }
                ParsedKey::Shift => {
                    push(at, HarnessAction::Shift(true));
                    push(at + dur, HarnessAction::Shift(false));
                }
            }
        }
        "move" => {
            if item.get("dx").is_some() || item.get("dy").is_some() {
                let dx = item.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let dy = item.get("dy").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                push(at, HarnessAction::MouseMotion(dx, dy));
            } else {
                let (x, y) = resolve_xy(item, "x", "y", vw, vh, normalized);
                push(at, HarnessAction::PointerAt(x, y));
            }
        }
        "click" => {
            let (x, y) = resolve_xy(item, "x", "y", vw, vh, normalized);
            let right = item
                .get("button")
                .and_then(|v| v.as_str())
                .is_some_and(|b| b.eq_ignore_ascii_case("right"));
            if right {
                push(at, HarnessAction::PointerAt(x, y));
                push(at, HarnessAction::RightButton(true));
                push(at + 0.03, HarnessAction::RightButton(false));
            } else {
                push(at, HarnessAction::Click(x, y));
            }
        }
        "right_click" => {
            let (x, y) = resolve_xy(item, "x", "y", vw, vh, normalized);
            push(at, HarnessAction::PointerAt(x, y));
            push(at, HarnessAction::RightButton(true));
            push(at + 0.03, HarnessAction::RightButton(false));
        }
        "drag" => {
            let dur = item
                .get("duration_secs")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5)
                .clamp(0.05, 10.0) as f32;
            let steps = item
                .get("steps")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .clamp(1, 120) as usize;
            let right = item
                .get("button")
                .and_then(|v| v.as_str())
                .is_some_and(|b| b.eq_ignore_ascii_case("right"));
            if right {
                let dx = item
                    .get("dx")
                    .and_then(|v| v.as_f64())
                    .or_else(|| {
                        let x1 = item.get("x1").and_then(|v| v.as_f64())?;
                        let x2 = item.get("x2").and_then(|v| v.as_f64())?;
                        Some(x2 - x1)
                    })
                    .unwrap_or(0.0) as f32;
                let dy = item
                    .get("dy")
                    .and_then(|v| v.as_f64())
                    .or_else(|| {
                        let y1 = item.get("y1").and_then(|v| v.as_f64())?;
                        let y2 = item.get("y2").and_then(|v| v.as_f64())?;
                        Some(y2 - y1)
                    })
                    .unwrap_or(0.0) as f32;
                push(at, HarnessAction::RightButton(true));
                let step_dt = dur / steps as f32;
                for k in 1..=steps {
                    push(
                        at + step_dt * k as f32,
                        HarnessAction::MouseMotion(dx / steps as f32, dy / steps as f32),
                    );
                }
                push(at + dur + 0.03, HarnessAction::RightButton(false));
            } else {
                let (x1, y1) = resolve_xy(item, "x1", "y1", vw, vh, normalized);
                let (x2, y2) = if item.get("x2").is_some() || item.get("y2").is_some() {
                    resolve_xy(item, "x2", "y2", vw, vh, normalized)
                } else {
                    let dx = item.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let dy = item.get("dy").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    (x1 + dx, y1 + dy)
                };
                push(at, HarnessAction::PointerAt(x1, y1));
                push(at, HarnessAction::LeftButton(x1, y1, true));
                let step_dt = dur / steps as f32;
                for k in 1..=steps {
                    let f = k as f32 / steps as f32;
                    push(
                        at + step_dt * k as f32,
                        HarnessAction::PointerAt(x1 + (x2 - x1) * f, y1 + (y2 - y1) * f),
                    );
                }
                push(at + dur + 0.03, HarnessAction::LeftButton(x2, y2, false));
            }
        }
        "wheel" => {
            let delta = item
                .get("delta")
                .or_else(|| item.get("notches"))
                .and_then(|v| v.as_f64())
                .ok_or("wheel action requires 'delta' (notches)")? as f32;
            let pos = if item.get("x").is_some() || item.get("y").is_some() {
                Some(resolve_xy(item, "x", "y", vw, vh, normalized))
            } else {
                None
            };
            push(at, HarnessAction::Wheel(delta, pos));
        }
        "type_text" => {
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("type_text requires 'text'")?;
            push(at, HarnessAction::Text(text.to_string()));
        }
        "chat" => {
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("chat requires 'text'")?;
            let Some(s) = ed.play.as_mut() else {
                return Err("chat requires an active Play session".into());
            };
            s.game.input.chat.push(text.to_string());
            queued += 1;
        }
        other => {
            return Err(format!("unknown input action '{other}'"));
        }
    }

    Ok(queued)
}

fn play_input_inject(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &Value,
) -> ToolCallResult {
    if ed.harness.from_play && ed.harness.play_started.is_none() && ed.play.is_some() {
        ed.harness.play_started = Some(crate::sim_clock::now());
    }
    let base_t = ed.harness.now().unwrap_or(0.0);
    let (vw, vh) = viewport_size(ed, ctx);

    let mut total_queued = 0usize;
    if let Some(events) = args.get("events").and_then(|v| v.as_array()) {
        for ev in events {
            match queue_input_event(ed, vw, vh, base_t, ev) {
                Ok(n) => total_queued += n,
                Err(e) => return ToolCallResult::error(e),
            }
        }
    } else {
        match queue_input_event(ed, vw, vh, base_t, args) {
            Ok(n) => total_queued += n,
            Err(e) => return ToolCallResult::error(e),
        }
    }

    ctx.request_repaint();
    ToolCallResult::json(&json!({
        "injected": true,
        "queued_actions": total_queued,
        "pending_harness_actions": ed.harness.actions.len(),
        "base_time": base_t,
    }))
}

fn parse_level(s: &str) -> Result<Level, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "info" => Ok(Level::Info),
        "warn" | "warning" => Ok(Level::Warn),
        "error" | "err" => Ok(Level::Error),
        other => Err(format!("unknown log level '{other}'; expected info, warn, or error")),
    }
}

fn output_console(ed: &mut crate::app::Editor, args: &Value) -> ToolCallResult {
    if args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
        let cleared = ed.log.len();
        ed.log.clear();
        ed.output_view.log_cleared();
        return ToolCallResult::json(&json!({
            "cleared": true,
            "cleared_count": cleared,
            "next_index": 0,
            "counts": { "info": 0, "warn": 0, "error": 0 },
            "lines": [],
        }));
    }

    if let Some(app) = args.get("append").and_then(|v| v.as_object()) {
        let Some(text) = app.get("text").and_then(|v| v.as_str()) else {
            return ToolCallResult::error("append object requires 'text' string");
        };
        let level = match app.get("level").and_then(|v| v.as_str()) {
            Some(l) => match parse_level(l) {
                Ok(lv) => lv,
                Err(e) => return ToolCallResult::error(e),
            },
            None => Level::Info,
        };
        ed.push_log(level, text.to_string());
    }

    let level_filter: Option<Level> = match args.get("level").and_then(|v| v.as_str()) {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("all") || s.trim().is_empty() => None,
        Some(s) => match parse_level(s) {
            Ok(lv) => Some(lv),
            Err(e) => return ToolCallResult::error(e),
        },
    };
    let contains = args.get("contains").and_then(|v| v.as_str());
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let since_index = args.get("since_index").and_then(|v| v.as_u64()).map(|n| n as usize);
    let tail = args
        .get("tail")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .or_else(|| if since_index.is_none() { Some(200) } else { None });
    let strip_stack_mark = args
        .get("strip_stack_mark")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let (mut info_count, mut warn_count, mut error_count) = (0usize, 0usize, 0usize);
    for l in &ed.log {
        match l.level {
            Level::Info => info_count += 1,
            Level::Warn => warn_count += 1,
            Level::Error => error_count += 1,
        }
    }

    let start = since_index.unwrap_or(0).min(ed.log.len());
    let contains_lower = contains.map(|c| c.to_ascii_lowercase());

    let mut matched: Vec<Value> = ed
        .log
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, l)| level_filter.is_none_or(|lv| l.level == lv))
        .filter(|(_, l)| match (contains, &contains_lower) {
            (Some(raw), Some(low)) => {
                if case_sensitive {
                    l.text.contains(raw)
                } else {
                    l.text.to_ascii_lowercase().contains(low)
                }
            }
            _ => true,
        })
        .map(|(idx, l)| {
            let text = if strip_stack_mark {
                l.text.replace(crate::output_panel::STACK_MARK, "")
            } else {
                l.text.clone()
            };
            json!({
                "index": idx,
                "at": l.at,
                "level": format!("{:?}", l.level),
                "text": text,
            })
        })
        .collect();

    if let Some(t) = tail.filter(|&n| n > 0 && matched.len() > n) {
        let drop_n = matched.len() - t;
        matched.drain(..drop_n);
    }

    let next_index = ed.log.len();
    let returned_count = matched.len();
    ToolCallResult::json(&json!({
        "lines": matched,
        "next_index": next_index,
        "total_count": next_index,
        "returned_count": returned_count,
        "counts": {
            "info": info_count,
            "warn": warn_count,
            "error": error_count,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_cover_all_play_tools() {
        let names: Vec<String> = schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "play_control",
                "play_step_frames",
                "play_character_control",
                "play_input_inject",
                "output_console",
            ]
        );
    }

    #[test]
    fn parses_common_keys_and_aliases() {
        assert!(matches!(parse_egui_key("W").unwrap(), ParsedKey::Key(egui::Key::W)));
        assert!(matches!(parse_egui_key("Space").unwrap(), ParsedKey::Key(egui::Key::Space)));
        assert!(matches!(parse_egui_key("LeftShift").unwrap(), ParsedKey::Shift));
        assert!(matches!(parse_egui_key("Return").unwrap(), ParsedKey::Key(egui::Key::Enter)));
        assert!(matches!(parse_egui_key("1").unwrap(), ParsedKey::Key(egui::Key::Num1)));
        assert!(matches!(parse_egui_key("Up").unwrap(), ParsedKey::Key(egui::Key::ArrowUp)));
        assert!(parse_egui_key("NonExistentKeyXYZ").is_err());
    }
}
