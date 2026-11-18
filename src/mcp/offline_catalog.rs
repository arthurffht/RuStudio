//! Offline OmniMCP catalog shared with `rustudio-mcp.exe` so `tools/list`,
//! `resources/list`, and `prompts/list` match `rustudio.exe` 100% even when
//! zero editor instances are running.

#![allow(dead_code)]

use serde_json::{json, Map, Value};

use super::protocol::{PromptArgument, PromptDef, ResourceDef, ToolDef};

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

/// Complete offline catalog of all 53 OmniMCP engine tools (`omni_batch` + 52
/// domain tools) matching `crate::mcp::all_schemas()` identically.
pub fn engine_tools() -> Vec<ToolDef> {
    let dm_dom_prop = json!({
        "type": "string",
        "enum": ["Active", "Edit", "Server", "Client"],
        "description": "Target DataModel (default: Active; 'Edit' works even during Play)."
    });

    vec![
        // Batch (1)
        tool(
            "omni_batch",
            "Execute multiple synchronous OmniMCP tool calls sequentially within a single engine frame.",
            json!({
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "description": "Ordered list of tool invocations to execute sequentially.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Tool name" },
                                "arguments": { "type": "object", "description": "Tool arguments" }
                            },
                            "required": ["name"]
                        }
                    },
                    "stop_on_error": {
                        "type": "boolean",
                        "description": "Stop immediately if any call returns an error (default: false)"
                    }
                },
                "required": ["calls"]
            }),
        ),

        // Session & Place (6)
        tool(
            "studio_get_state",
            "Return a complete live snapshot of RuStudio state: mode (Edit/Play/Run), available DataModels, active place metadata, dirty/undo/redo status, selection, editor camera, FPS, frame work time, memory usage, physics solver, play time, character state, and active view side.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        tool(
            "place_new",
            "Reset the editor to a fresh Baseplate place, stopping any active Play/Run session first and resetting editor caches, tabs, and Start Page cleanly.",
            json!({
                "type": "object",
                "properties": {
                    "template": {
                        "type": "string",
                        "enum": ["baseplate", "studio_baseplate", "empty"],
                        "description": "Optional place template (default: 'baseplate' via Document::new_place())."
                    }
                }
            }),
        ),
        tool(
            "place_open",
            "Open a local Roblox place/model (.rbxl, .rbxlx, .rbxm, .rbxmx) or a Rojo/Argon *.project.json file or project directory into the live editor, stopping Play/Run if active.",
            json!({
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
        ),
        tool(
            "place_save",
            "Save the open place (Edit or Active DataModel, even Edit while Play is running) to disk, sync Rojo project files, or export selected/targeted instances as a .rbxm/.rbxmx model.",
            json!({
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
        ),
        tool(
            "place_diagnostics",
            "Return detailed place and engine diagnostics: instance stats summary, service list, script counts and total lines, recent crash reports, resident memory, and frame performance metrics.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": {
                        "type": "string",
                        "enum": ["Active", "Edit", "Server", "Client"],
                        "description": "Which DataModel to inspect (default: 'Active')."
                    }
                }
            }),
        ),
        tool(
            "studio_quit",
            "Cleanly close the RuStudio process, flushing settings and optionally saving the place beforehand.",
            json!({
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
        ),

        // DOM (9)
        tool(
            "dom_get_tree",
            "Read the DataModel instance hierarchy (hierarchical or flat) from any root with optional depth, class (IsA), name, tag filters, and properties.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "dom_inspect_instance",
            "Inspect full details of one or more instances: ALL stored DOM properties + reflection defaults (including NotScriptable & RobloxScriptSecurity), property security metadata, Attributes, Tags, Archivable, children summary, and world CFrame/Position/Size.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "target": { "description": "Instance path, ref:<id>, or {ref|path}." },
                    "targets": { "type": "array", "description": "Multiple instance targets to inspect in one call." },
                    "include_defaults": { "type": "boolean", "description": "Include reflection defaults from the database (default: true)." },
                    "include_security": { "type": "boolean", "description": "Include per-property security & scriptability metadata (default: true)." }
                }
            }),
        ),
        tool(
            "dom_query",
            "Unlimited DOM query across the DataModel (no 50-result cap). Supports class, inheritance (is_a), name substring, path prefix, tags, attributes, property equality, and 3D sphere/box spatial filters.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "dom_create_instances",
            "Create arbitrary recursive instance hierarchies with properties, attributes, tags, and cross-references (temp_id) in one atomic undoable batch.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "items": {
                        "type": "array",
                        "description": "Top-level creation specs { parent?, class, name?, temp_id?, properties?, attributes?, tags?, children?: [...] }.",
                        "items": { "type": "object" }
                    }
                },
                "required": ["items"]
            }),
        ),
        tool(
            "dom_set_properties",
            "Batch-mutate properties, attributes, tags, and names on any instances, bypassing ALL NotScriptable and security capability restrictions.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "dom_reparent_rename_delete",
            "Batch reparent, rename, delete, or clear children across any number of instances in one call.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "dom_clone_and_transform",
            "Clone target instance(s) N times with optional parent, name_prefix, offset_step, rotation_step_deg, grid layout, or explicit transforms.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "dom_group_ungroup",
            "Group targets into a Model or Folder, or ungroup Model/Folder containers.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "action": { "type": "string", "enum": ["group", "ungroup"] },
                    "targets": { "type": "array" },
                    "target": {},
                    "class": { "type": "string", "enum": ["Model", "Folder"], "description": "Container class when grouping (default: Model)." },
                    "name": { "type": "string", "description": "Optional name for the created group." },
                    "parent": { "description": "Optional parent override when grouping." }
                },
                "required": ["action"]
            }),
        ),
        tool(
            "dom_serialize_subtree",
            "Export target subtree(s) to .rbxmx XML or base64 .rbxm binary, or import .rbxmx XML / base64 .rbxm / file_path under a parent.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),

        // Scripts (7)
        tool(
            "script_list_all",
            "List every Script, LocalScript, and ModuleScript (LuaSourceContainer) with ref_id, path, class, lines, bytes, disabled, run_context, and syntax check status.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "path_prefix": { "type": "string", "description": "Optional dot/slash path prefix filter." },
                    "class_filter": { "type": "string", "description": "Optional class filter (e.g. Script, LocalScript, ModuleScript)." },
                    "include_syntax_check": { "type": "boolean", "description": "Run Luau syntax check per script (default: true)." }
                }
            }),
        ),
        tool(
            "script_read_batch",
            "Read one or many scripts in one call (full source or line ranges), returning line-numbered source, raw source, byte length, and MD5 digest.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "script_write_batch",
            "Create or replace Source across multiple scripts atomically in one call, optionally auto-creating missing Folder parents.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "script_patch_batch",
            "Atomically apply surgical search/replace or line-range edits across arbitrarily many scripts. Validates ALL patches before committing ANY edit.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
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
        ),
        tool(
            "script_grep",
            "Unlimited search across all scripts (no 50-match cap), returning path, ref_id, line, column, text, and optional context lines.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "query": { "type": "string" },
                    "case_sensitive": { "type": "boolean", "description": "Case-sensitive search (default: false)." },
                    "context_lines": { "type": "integer", "minimum": 0, "description": "Number of context lines before/after each match (default: 0)." },
                    "path_prefix": { "type": "string" },
                    "class_filter": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Optional cap (unlimited if omitted)." }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "script_check_syntax",
            "Compile-check Luau syntax on one script, multiple scripts, or all scripts in the DataModel.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "target": {},
                    "targets": { "type": "array" },
                    "path_prefix": { "type": "string" },
                    "errors_only": { "type": "boolean", "description": "Only list scripts with syntax errors (default: false)." }
                }
            }),
        ),
        tool(
            "script_open_tab",
            "Open or focus script tab(s) inside RuStudio's code editor.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_dom_prop,
                    "target": {},
                    "targets": { "type": "array" }
                }
            }),
        ),

        // Luau (3)
        tool(
            "luau_execute",
            "Execute arbitrary Luau code inside the live Edit, Server, or Client DataModel VM with selectable thread security identity (default: 8 RobloxScript / maximum unrestricted access). Captures print/warn output, errors with stack tracebacks, execution duration in microseconds, and converts all Luau/Roblox return values to JSON.",
            json!({
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
        ),
        tool(
            "luau_fire_signal",
            "Fire any signal/event on a target Instance inside Edit, Server, or Client (Runtime::fire + dispatch), converting a JSON args array into Luau/Roblox values.",
            json!({
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
        ),
        tool(
            "luau_vm_stats",
            "Return live Luau VM statistics (lua_slots, mem_stats, gc_counts, heap memory, and SimClock state) for Edit, Server, and Client VMs.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": {
                        "type": "string",
                        "enum": ["All", "Edit", "Server", "Client"],
                        "description": "Which VM(s) to inspect (default: 'All')."
                    }
                }
            }),
        ),

        // Play & Input (5)
        tool(
            "play_control",
            "Start, stop, or restart a Play/Run session, switch between Client and Server views, and configure Play RigType (R15/R6/Auto) and physics solver (studio/studio-nextgen/rapier).",
            json!({
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
        ),
        tool(
            "play_step_frames",
            "Advance the running Play/Run simulation by an exact number of frames (1..=600) or elapsed seconds, returning delta_play_time and new Output logs produced during the step.",
            json!({
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
        ),
        tool(
            "play_character_control",
            "Inspect and control the live Play player character: get_state, teleport (to [x,y,z] or target instance), move_to, jump, set_stats (Health/MaxHealth/WalkSpeed/JumpPower/JumpHeight), equip_tool, unequip_tools, activate_tool, and respawn.",
            json!({
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
        ),
        tool(
            "play_input_inject",
            "Inject real keyboard (press, down, up, hold), mouse (move, click, right_click, drag, wheel), typed text, or chat into the live game/editor input pipeline.",
            json!({
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
        ),
        tool(
            "output_console",
            "Read, filter, clear, or append entries to the RuStudio Output console log (ed.log), returning lines, next_index, and per-level counts.",
            json!({
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
        ),

        // World & Physics (6)
        tool(
            "world_raycast",
            "Cast a single ray (origin + direction or target_pos) or a batch of rays against Workspace BaseParts (Box, Ball, Cylinder, Wedge, CornerWedge, MeshPart BVH, PhysicalConfigData) and Terrain (solid + water). Supports FilterDescendantsInstances semantics.",
            json!({
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
        ),
        tool(
            "world_spatial_query",
            "Run spatial queries against Workspace geometry: bounding_box of an instance/Model/Folder subtree, box_overlap (OBB vs parts), or sphere_overlap (radius vs parts).",
            json!({
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
        ),
        tool(
            "world_place_on_surface",
            "Place/snap target BasePart(s) or Model(s) flush onto the surface or Terrain below, computing exact oriented AABB bottom offset (min_y) so nothing floats or clips. Optionally aligns UpVector to surface normal. Undoable.",
            json!({
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
        ),
        tool(
            "world_build_structure",
            "Procedurally construct a clean single-undo Model of Parts/WedgeParts in Workspace (box_room, wall_run, staircase, ramp, arch, cylinder_ring, scatter with optional raycast-to-ground).",
            build_structure_schema(),
        ),
        tool(
            "physics_inspect",
            "Inspect live Play/Run physics world state and per-part AssemblyLinearVelocity, AssemblyAngularVelocity, mass, grounded state, sleeping state, and joints/constraints.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Server", "Client"] },
                    "targets": { "type": "array", "description": "Optional specific BaseParts (or Models containing BaseParts) to inspect." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "When targets is omitted, max top moving/dynamic parts to return (default: 30)." },
                    "include_anchored": { "type": "boolean", "description": "Include anchored parts when scanning without targets (default: false)." },
                    "include_joints": { "type": "boolean", "description": "Include touching joints/constraints for each inspected part (default: true)." }
                }
            }),
        ),
        tool(
            "physics_manipulate",
            "Manipulate live Play/Run physics bodies: set linear/angular velocity, apply impulse / angular impulse / impulse at position, wake sleeping bodies, toggle Anchored/CanCollide, or teleport CFrame.",
            json!({
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
        ),

        // Terrain (5)
        tool(
            "terrain_inspect",
            "Inspect Workspace.Terrain properties (water look, Decoration, GrassLength, MaterialColors), chunk/voxel counts, world/voxel bounds, present materials, and optional 3D voxel sampling in a region.",
            json!({
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
        ),
        tool(
            "terrain_fill_shape",
            "Fill or carve (material: 'Air') SmoothGrid terrain voxels using block, ball, cylinder, wedge, or water_level shapes. Undoable via History.",
            json!({
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
        ),
        tool(
            "terrain_sculpt_stroke",
            "Apply Terrain Brush strokes (Add, Subtract, Grow, Erode, Smooth, Flatten, Paint, SeaLevel) at a point or interpolated along a sequence of world points. Undoable.",
            json!({
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
        ),
        tool(
            "terrain_generate",
            "Procedurally generate multi-biome SmoothGrid terrain into Workspace.Terrain using RuStudio's Studio-matched noise generator. Undoable.",
            json!({
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
        ),
        tool(
            "terrain_clear",
            "Clear all SmoothGrid terrain voxels or clear a specific world/voxel box region. Undoable.",
            json!({
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
        ),

        // Viewport & Lighting (3)
        tool(
            "viewport_camera",
            "Get or set the 3D editor viewport camera (position, look_at, yaw/pitch degrees, FOV, Orbit/Fly mode) or frame/focus any target instance, Model, selection, or the entire workspace.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"], "description": "DataModel used when resolving focus_target (defaults to Active)." },
                    "pos": { "type": ["array", "object"], "description": "Camera world position [x, y, z] or {x, y, z}." },
                    "look_at": { "type": ["array", "object", "string"], "description": "Target world point [x, y, z], {x, y, z}, or instance path/ref to orient the camera toward." },
                    "focus": { "type": ["array", "object"], "description": "Explicit orbit focus point [x, y, z] without changing orientation." },
                    "yaw_deg": { "type": "number", "description": "Camera yaw angle in degrees (0 looks along -Z)." },
                    "pitch_deg": { "type": "number", "description": "Camera pitch angle in degrees (clamped to [-89.5, 89.5])." },
                    "fov_deg": { "type": "number", "description": "Vertical field of view in degrees (clamped to [1, 120])." },
                    "mode": { "type": "string", "enum": ["Fly", "Orbit"], "description": "Camera navigation mode." },
                    "focus_target": {
                        "description": "Frame an instance/Model (ref or path), array of targets, 'selection', 'all', or true (frames current selection).",
                        "type": ["string", "array", "object", "boolean"]
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "viewport_capture",
            "Capture a live screenshot of the 3D viewport (GPU framebuffer fast path) or full Studio window. Returns base64 image/png and optionally saves to disk.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"], "description": "DataModel used when resolving camera.focus_target (defaults to Active)." },
                    "target": { "type": "string", "enum": ["viewport", "window"], "default": "viewport", "description": "Capture only the 3D viewport render or the full application window." },
                    "camera": {
                        "type": "object",
                        "description": "Optional camera configuration applied right before capturing (same fields as viewport_camera)."
                    },
                    "clean_view": { "type": "boolean", "description": "Hide editor gizmos/UI overlays cleanly during capture (restored automatically afterwards)." },
                    "settle_frames": { "type": "integer", "minimum": 0, "maximum": 60, "description": "Frames to wait before reading pixels (automatically >= 1 when camera or clean_view changes)." },
                    "save_path": { "type": "string", "description": "Optional PNG file path to save the captured image." },
                    "max_width": { "type": "integer", "minimum": 16, "maximum": 8192, "description": "Downscale image proportionally so its width does not exceed max_width." },
                    "return_image": { "type": "boolean", "default": true, "description": "Whether to return an inline MCP image/png content block." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "lighting_and_atmosphere",
            "Inspect or configure Lighting properties, Sky, Atmosphere, Workspace.Terrain.Clouds, post-processing effects (Bloom, Blur, ColorCorrection, DepthOfField, SunRays), or apply complete lighting presets.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"], "description": "Target DataModel (defaults to Active)." },
                    "preset": {
                        "type": "string",
                        "enum": ["daylight", "golden_hour", "cyberpunk_night", "foggy_horror", "studio_neutral"],
                        "description": "Apply a curated environment preset before any per-object overrides."
                    },
                    "lighting": { "type": "object", "description": "Map of Lighting properties to set (e.g. Brightness, ClockTime, TimeOfDay, GeographicLatitude, Ambient, OutdoorAmbient, ColorShift_Top, ColorShift_Bottom, GlobalShadows, FogStart, FogEnd, FogColor, ExposureCompensation, EnvironmentDiffuseScale, EnvironmentSpecularScale)." },
                    "sky": { "type": ["object", "null"], "description": "Properties for Lighting.Sky (creates one if missing; pass null to remove Sky)." },
                    "atmosphere": { "type": ["object", "null"], "description": "Properties for Lighting.Atmosphere (creates one if missing; pass null to remove Atmosphere)." },
                    "clouds": { "type": ["object", "null"], "description": "Properties for Workspace.Terrain.Clouds (Enabled, Cover, Density, Color; pass null to remove Clouds)." },
                    "effects": {
                        "type": "object",
                        "description": "Map of effect class names (BloomEffect, BlurEffect, ColorCorrectionEffect, DepthOfFieldEffect, SunRaysEffect, ColorGradingEffect) to property objects (creates/updates child under Lighting, or null to delete)."
                    }
                },
                "additionalProperties": false
            }),
        ),

        // Editor & GUI (4)
        tool(
            "editor_selection",
            "Read or manipulate Studio's selected instances: get, set, add, remove, clear, or select_all_of_class.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get", "set", "add", "remove", "clear", "select_all_of_class"],
                        "default": "get"
                    },
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "targets": {
                        "type": ["array", "string", "object"],
                        "description": "Target instance ref(s) or path(s) for set, add, or remove."
                    },
                    "class_name": { "type": "string", "description": "Class name for select_all_of_class." },
                    "include_subclasses": { "type": "boolean", "default": false, "description": "Match subclasses via IsA during select_all_of_class." },
                    "root": { "type": ["string", "object"], "description": "Optional subtree root for select_all_of_class (defaults to DataModel root)." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "editor_undo_redo",
            "Inspect, execute, or clear the undo/redo history stack.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["status", "undo", "redo", "clear"], "default": "status" },
                    "steps": { "type": "integer", "minimum": 1, "maximum": 200, "default": 1, "description": "Number of undo or redo steps to execute." },
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "editor_ui_and_settings",
            "Read or control editor tools (Select/Move/Scale/Rotate/Transform), snapping/grid/collision toggles, panels & Studio actions, RuStudio engine options, and Roblox Studio Settings.",
            json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "enum": ["none", "select", "move", "scale", "rotate", "transform"] },
                    "snap_move": { "type": "boolean" },
                    "move_inc": { "type": "number", "minimum": 0.001 },
                    "snap_rotate": { "type": "boolean" },
                    "rotate_inc": { "type": "number", "minimum": 0.01 },
                    "collisions": { "type": "boolean" },
                    "join_surfaces": { "type": "boolean" },
                    "local_space": { "type": "boolean" },
                    "show_grid": { "type": "boolean" },
                    "physical_drag": { "type": "boolean" },
                    "part_snap": { "type": "boolean" },
                    "use_bounding_boxes": { "type": "boolean" },
                    "align_dragged": { "type": "boolean" },
                    "clean_view": { "type": "boolean" },
                    "show_gui": { "type": "boolean" },
                    "grid_size": { "type": "number", "minimum": 0.05 },
                    "action": { "type": "string", "description": "Invoke a Studio UI action by name (e.g. 'panel_explorer', 'panel_properties', 'panel_output', 'toolbox', 'terrain_editor', 'avatar_settings', 'studio_settings', 'manage_plugins', 'show_grid', 'show_ui', 'reset_layout', 'dock_all', 'focus', 'frame_all')." },
                    "list_actions": { "type": "boolean", "description": "Include known Studio action names and their enabled/checked state." },
                    "panel": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "enum": ["viewport", "explorer", "properties", "output", "avatar_settings", "terrain_editor", "toolbox"] },
                            "open": { "type": "boolean", "description": "Omit to toggle the panel." }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "rustudio_settings_set": { "type": "object", "description": "Map of rustudio_settings keys to new values (null resets to default)." },
                    "rustudio_settings_query": { "type": ["string", "boolean"], "description": "Filter string or true to list rustudio_settings options." },
                    "studio_settings_set": {
                        "type": ["array", "object"],
                        "description": "Either array of {class, prop, value} or object mapping 'Class.Prop' to value."
                    },
                    "studio_settings_query": {
                        "type": ["object", "string", "boolean"],
                        "description": "Filter Studio Settings by string, true, or {page?, class?, query?}."
                    },
                    "reset_studio_settings": { "type": "boolean", "description": "Reset all Studio Settings to defaults." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "gui_inspect",
            "Walk StarterGui (or PlayerGui / Workspace SurfaceGui & BillboardGui) computing 2D layout rects (AbsolutePosition, AbsoluteSize, AbsoluteRotation), effective visibility, Text, ZIndex, BackgroundColor3, and optional 2D hit-testing at [x, y].",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] },
                    "root": { "type": ["string", "object"], "description": "Optional GUI root container ref/path (defaults to StarterGui or active PlayerGui)." },
                    "include_world_guis": { "type": "boolean", "default": true, "description": "Include Workspace SurfaceGui and BillboardGui objects when no explicit root is specified." },
                    "include_hidden": { "type": "boolean", "default": true, "description": "Include objects with Visible=false or inside disabled ScreenGuis." },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "default": 32 },
                    "at": {
                        "type": "array",
                        "items": { "type": "number" },
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Optional point [x, y] to hit-test visible GUI elements from front to back."
                    },
                    "coord_space": { "type": "string", "enum": ["viewport", "window"], "default": "viewport", "description": "Coordinate space for `at` hit-test point." }
                },
                "additionalProperties": false
            }),
        ),

        // Project, Plugins, Assets, Reflection (4)
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
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] }
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
                    "datamodel": { "type": "string", "enum": ["Active", "Edit", "Server", "Client"] }
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

fn build_structure_schema() -> Value {
    let mut props = Map::new();
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

/// Complete offline catalog of the 6 engine-provided MCP resources.
pub fn engine_resources() -> Vec<ResourceDef> {
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

/// Complete offline catalog of the 4 expert OmniMCP workflow prompts.
pub fn engine_prompts() -> Vec<PromptDef> {
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
            arguments: vec![PromptArgument {
                name: "symptom".into(),
                description: "Bug, unexpected physics behavior, script error, or test scenario to diagnose.".into(),
                required: false,
            }],
        },
        PromptDef {
            name: "omni-world-builder".into(),
            description: "Expert 3D environment, procedural structure, SmoothGrid terrain, and Lighting/Atmosphere builder with GPU visual verification.".into(),
            arguments: vec![PromptArgument {
                name: "theme".into(),
                description: "Visual style, biome, or environment area to construct and light.".into(),
                required: false,
            }],
        },
        PromptDef {
            name: "omni-ui-designer".into(),
            description: "Expert Roblox ScreenGui/BillboardGui/SurfaceGui designer with responsive layout inspection and visual verification.".into(),
            arguments: vec![PromptArgument {
                name: "screen".into(),
                description: "Target UI screen, HUD, or menu to create or refine.".into(),
                required: false,
            }],
        },
    ]
}
