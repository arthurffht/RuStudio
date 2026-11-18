# OmniMCP — Unrestricted AI Engine Control for RuStudio

**OmniMCP** is RuStudio's native Model Context Protocol (MCP) subsystem on Windows. It gives AI assistants and automation tools full read, write, Luau execution, physics, terrain, GUI, and GPU visual-inspection control over RuStudio—without Roblox Studio's plugin capability sandboxes, 50-item query caps, or Edit-lock restrictions during Play.

---

## Architecture

OmniMCP splits across two cooperating Windows binaries so AI clients keep a rock-solid stdio connection even when RuStudio windows are launched, restarted, crashed, or run in parallel:

```
  AI Client (Antigravity / Claude / Cursor / VS Code / Windsurf)
       │  JSON-RPC 2.0 over stdio
       │  (auto-detects newline-delimited JSON or Content-Length framing)
       ▼
  rustudio-mcp.exe  (Windows console binary — logs to stderr only)
       ├─ Handles bridge meta-tools & rustudio://instances offline/directly
       ├─ Offline fallback catalog (all 57 tools, 7 resources, 4 prompts)
       ├─ Discovers live editors in %LOCALAPPDATA%\rustudio\mcp-instances\<pid>.json
       └─ Routes per call via `studio_id` -> session selection -> most recent
            │  JSON-lines over 127.0.0.1:<port> (44777..44790 or --mcp-port)
            ▼
  rustudio.exe  (GUI or --headless / --no-render)
       ├─ McpServer TCP listener thread (src/mcp/server.rs)
       ├─ mpsc channel + egui::Context::request_repaint()
       └─ Editor::mcp_tick() on main frame loop
            ├─ Immediate synchronous tools & omni_batch
            └─ Multi-frame deferred ops (PendingSteps, PendingCapture)
```

### Why Two Binaries?

1. **Zero-instance startup**: `rustudio-mcp.exe` exposes the full tool, resource, and prompt catalog immediately at MCP handshake—even with zero `rustudio.exe` windows open. An agent can call `studio_launch` to start a GUI or headless `rustudio.exe` on demand.
2. **Stdio isolation**: `rustudio.exe` is a Windows GUI application (`#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]`). `rustudio-mcp.exe` is a Windows console binary that spawns `rustudio.exe` with `Stdio::null()` and `CREATE_NO_WINDOW` / `DETACHED_PROCESS`, guaranteeing child console output never corrupts JSON-RPC stdout.
3. **Multi-instance orchestration**: Open multiple places or headless test workers side-by-side. Each `rustudio.exe` publishes `%LOCALAPPDATA%\rustudio\mcp-instances\<pid>.json` (auto-pruned via `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` when a process exits) and binds `127.0.0.1:44777..44790` (falling back to an ephemeral port).

---

## Quick Start & Client Registration

### 1. Build or Install

From source (inside an MSVC environment / `vcvars64.bat`):

```powershell
cargo build --release --bins
```

This produces:
- `target\release\rustudio.exe`
- `target\release\rustudio-mcp.exe`

From a release ZIP (`RuStudio-windows-x64.zip`), run `install.cmd` or `tools\windows\install.ps1` (installs both executables to `%LOCALAPPDATA%\Programs\rustudio` by default).

### 2. Automatic Registration (`register-mcp.ps1`)

Run the registration script to locate `rustudio-mcp.exe` and safely merge `"RuStudio_OmniMCP"` into your AI client configs without clobbering existing servers:

```powershell
# Preview detected clients without modifying files
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1 -DryRun

# Register in all detected clients (Antigravity, Claude Desktop, Claude Code, Cursor, VS Code, Windsurf)
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1

# Register for specific clients (creates config file if absent)
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1 -Targets Antigravity,ClaudeCode,Cursor

# Remove the entry cleanly
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1 -Uninstall
```

### 3. Manual Configuration

| Client | Windows Config Path | Root Key |
|---|---|---|
| Antigravity | `%USERPROFILE%\.gemini\antigravity\mcp_config.json` | `mcpServers` |
| Claude Desktop | `%APPDATA%\Claude\claude_desktop_config.json` | `mcpServers` |
| Claude Code | `%USERPROFILE%\.claude.json` | `mcpServers` |
| Cursor | `%USERPROFILE%\.cursor\mcp.json` | `mcpServers` |
| Windsurf | `%USERPROFILE%\.codeium\windsurf\mcp_config.json` | `mcpServers` |
| VS Code | `%APPDATA%\Code\User\mcp.json` | `servers` |

Example entry (`mcpServers` clients):

```json
{
  "mcpServers": {
    "RuStudio_OmniMCP": {
      "command": "C:\\Users\\YOU\\AppData\\Local\\Programs\\rustudio\\rustudio-mcp.exe"
    }
  }
}
```

Example entry (VS Code `mcp.json`):

```json
{
  "servers": {
    "RuStudio_OmniMCP": {
      "type": "stdio",
      "command": "C:\\Users\\YOU\\AppData\\Local\\Programs\\rustudio\\rustudio-mcp.exe"
    }
  }
}
```

### 4. CLI Flags & Environment Variables

On `rustudio.exe`:
- `--mcp-port PORT` — bind the OmniMCP TCP bridge to `127.0.0.1:PORT`.
- `--no-mcp` — disable the OmniMCP bridge on startup.
- `RUSTUDIO_MCP_PORT=PORT` — environment override for bridge port binding.

On `rustudio-mcp.exe`:
- `--port PORT` — default TCP port to connect to when no `studio_id` or `studio_select_instance` is active.
- `--pid PID` — default RuStudio PID to target.
- `--list-instances` — print JSON list of live RuStudio instances and exit.

---

## Key Capabilities & Design

### 1. `NotScriptable` & Security Capability Bypass

In standard Roblox Studio plugins, Luau scripts are blocked by `NotScriptable` tags and thread capability gates (`Plugin`, `RobloxScript`, `RobloxEngine`, `PluginOrOpenCloud`, `NotAccessible`).

OmniMCP bypasses these restrictions cleanly in two complementary ways:
- **Direct DOM & Reflection Access (`src/mcp/variants.rs`, `tools_dom.rs`, `tools_scripts.rs`)**: DOM inspection and mutation read/write directly against `rbx_dom_weak::WeakDom` and `rbx_reflection_database`, exposing `NotScriptable` and internal properties (`UniqueId`, `Tags`, `Attributes`, `MaterialColors`, `SmoothGrid`, `Script.Source`, etc.) with undo/redo history and full type conversion (`Vector3`, `CFrame`, `Color3`, `BrickColor`, `UDim2`, `Rect`, `Region3`, `NumberSequence`, `ColorSequence`, `PhysicalProperties`, `Enum`, `Font`, `BinaryString`, `SharedString`, `Content`, `Ref`).
- **Selectable Thread Security Identity in `luau_execute` (`src/security.rs`)**: Execute arbitrary Luau in `Edit`, `Server`, or `Client` VMs at identity `8` (default — unrestricted access) or test real game-script boundaries at identity `2` (game scripts), `4` (CoreScripts), or `6` (engine).

### 2. `Edit` During `Play` (Multi-DataModel Access)

When Play (`F5`) or Run (`F8`) starts, RuStudio clones the place into separate `Server` and `Client` DataModels while preserving the original `Edit` DataModel inside `PlaySession`.

Every DOM, script, spatial, and terrain tool accepts an optional `datamodel` selector:
- `"Active"` (default) — whichever DataModel the viewport is currently showing.
- `"Edit"` — reads or mutates the source `Edit` DataModel **even while Play or Run is active**. Fix bugs in your source place while watching a live playtest, then call `play_control(action: "restart")` to test the updated place.
- `"Server"` — inspects or mutates the live server simulation DataModel during Play/Run.
- `"Client"` — inspects or mutates the live client DataModel during Play.

### 3. `omni_batch` (Single-Frame Atomic Execution)

`omni_batch` runs an ordered array of synchronous OmniMCP tools (`calls: [{ name, arguments }]`) within a single engine frame:
- Eliminates TCP/stdio round-trips when assembling complex scenes or modifying dozens of objects.
- Supports `stop_on_error: true` for fail-fast workflows.
- Asynchronous deferred tools (`viewport_capture`, `play_step_frames`) return an explicit diagnostic if placed inside `omni_batch`.

### 4. SmoothGrid Terrain, Real Physics & GPU Vision

- **Terrain (`tools_terrain.rs`)**: Directly decodes, edits, sculpts, generates, and re-encodes `Terrain.SmoothGrid` (`src/terrain.rs`, `terrain_brush.rs`, `terrain_gen.rs`), marking modified chunks dirty so the renderer and physics meshes update immediately with undo support.
- **Physics & Deterministic Stepping (`tools_play.rs`, `tools_physics_world.rs`)**: Query raycasts and spatial overlaps, snap models flush onto surfaces (`world_place_on_surface`), inspect dynamic assembly velocities/mass/sleep states (`physics_inspect`), apply impulses (`physics_manipulate`), control the player (`play_character_control`), inject inputs (`play_input_inject`), and step exact frame counts (`play_step_frames`) with synchronized Output log capture.
- **GPU Vision (`tools_viewport.rs`)**: `viewport_capture` captures the live `wgpu` 3D viewport buffer (via direct GPU readback or egui screenshot) or the entire RuStudio window, optionally overriding `camera` and temporarily enabling `clean_view` (hiding grid, selection outlines, and gizmos). Works both in windowed mode and in `--headless` offscreen GPU mode.

---

## Multi-Instance Targeting

`rustudio-mcp.exe` resolves the target `rustudio.exe` on every engine tool call in this order:
1. Per-call `studio_id` argument (stripped before forwarding to `rustudio.exe`):
   - PID integer or string (e.g. `12345` or `"pid:12345"`)
   - TCP port (e.g. `44777`, `"port:44777"`, or `"127.0.0.1:44777"`)
   - Unique place title match
2. Session pin set by `studio_select_instance` (or `studio_launch` with `select: true`, the default)
3. CLI `--port` / `--pid` passed to `rustudio-mcp.exe`
4. `RUSTUDIO_MCP_PORT` environment variable
5. Most recently launched live instance in `%LOCALAPPDATA%\rustudio\mcp-instances\`

---

## Complete Tool Reference (57 Tools)

### Bridge & Process Management (4 — handled directly in `rustudio-mcp.exe`)

| Tool | Description |
|---|---|
| `studio_list_instances` | Scan `%LOCALAPPDATA%\rustudio\mcp-instances`, TCP-ping every live editor, and report the active routing target. |
| `studio_launch` | Launch `rustudio.exe` (`place_path`, `demo`, `play`, `headless`, `no_render`, `overlays`, `window_size`, `port`, `fixed_dt`, `no_plugins`, `select`, `timeout_secs`) and wait until TCP `Ping` succeeds. |
| `studio_kill_instance` | Terminate a `rustudio.exe` process by `pid` (or `studio_id`), trying graceful `studio_quit` first unless `force: true`, then `TerminateProcess`, and removing `<pid>.json`. |
| `studio_select_instance` | Pin a running `rustudio.exe` (`pid`, `port`, `studio_id`, or `clear: true`) as the default target for this MCP session. |

### Batching (1)

| Tool | Description |
|---|---|
| `omni_batch` | Execute an array of synchronous OmniMCP tool calls sequentially within one engine frame (`calls`, `stop_on_error`). |

### Session & Place Lifecycle (6)

| Tool | Description |
|---|---|
| `studio_get_state` | Live snapshot: `mode`, `available_datamodels`, `title`, `path`, `dirty`, `revision`, `tree_revision`, `undo_len`, `selection`, `camera`, `fps`, `last_work_ms`, `resident_bytes`, `solver`, `play_time`. |
| `place_new` | Reset to a fresh Baseplate place (stopping Play if active). |
| `place_open` | Open a `.rbxl`/`.rbxlx`/`.rbxm`/`.rbxmx` file or Rojo `*.project.json` / folder. |
| `place_save` | Save `Edit` or `Active` place, or export selected targets to `.rbxm`/`.rbxmx`. |
| `place_diagnostics` | Return `Document::stats()`, services, script counts/lines, crash reports, memory, and frame timing. |
| `studio_quit` | Cleanly close RuStudio (optionally saving first). |

### DOM Inspection & Mutation (9)

| Tool | Description |
|---|---|
| `dom_get_tree` | Hierarchical or flat DataModel tree dump with `max_depth`, `class`, `name_contains`, `tag`, and `include_properties`. |
| `dom_inspect_instance` | Full stored DOM properties + reflection defaults (including `NotScriptable`), `Attributes`, `Tags`, `Archivable`, children, and world bounds. |
| `dom_query` | Unlimited DOM search by `is_a`, `exact_class`, `name_pattern`, `path_prefix`, `has_tag`, `has_attribute`, `attribute_equals`, `property_equals`, `within_sphere`, `within_box`. |
| `dom_create_instances` | Batch recursive tree builder (`{ parent, class, name, properties, attributes, tags, children }`) in one undoable batch. |
| `dom_set_properties` | Batch property/attribute/tag/name mutator bypassing `NotScriptable` & capability restrictions. |
| `dom_reparent_rename_delete` | Batch `reparent`, `rename`, `delete`, and `clear_children`. |
| `dom_clone_and_transform` | Clone target(s) `count` times with `offset_step`, `rotation_step_deg`, 2D `grid`, or explicit `transforms`. |
| `dom_group_ungroup` | Group targets into a `Model` or `Folder`, or ungroup containers. |
| `dom_serialize_subtree` | Export targets to `.rbxmx` XML or base64 `.rbxm`, or import `.rbxmx`/`.rbxm` under a parent. |

### Scripts & Code Editor (7)

| Tool | Description |
|---|---|
| `script_list_all` | List all `LuaSourceContainer` instances with `ref`, `path`, `class`, `lines`, `bytes`, `disabled`, `run_context`, and syntax status. |
| `script_read_batch` | Read one or many scripts (full or line-range) with line numbers and hash digest. |
| `script_write_batch` | Atomically create or overwrite multiple scripts (optionally creating missing `Folder` parents). |
| `script_patch_batch` | Atomic multi-script surgical patcher validating all edits across all scripts before committing. |
| `script_grep` | Unlimited full-text search across all scripts with context lines and optional path/class filters. |
| `script_check_syntax` | Compile-check Luau syntax on target script(s) or all scripts in the DataModel. |
| `script_open_tab` | Open and focus script(s) in RuStudio's script editor tabs. |

### Luau Execution & VMs (3)

| Tool | Description |
|---|---|
| `luau_execute` | Run arbitrary Luau in `Edit`, `Server`, or `Client` at selectable security `identity` (`2`, `4`, `6`, `8`; default `8`), capturing output, return values, and `duration_us`. |
| `luau_fire_signal` | Fire any signal/event on a target instance in `Edit`, `Server`, or `Client` with JSON arguments. |
| `luau_vm_stats` | Return Luau VM memory, GC counts, active thread/slot counts, and `SimClock` state. |

### Playtest, Character, Input & Output (5)

| Tool | Description |
|---|---|
| `play_control` | `start_play`, `start_run`, `stop`, `restart`, `view_client`, `view_server` with optional `rig_type` (`R15`/`R6`/`Auto`) and `solver` (`studio`/`studio-nextgen`/`rapier`). |
| `play_step_frames` | Step Play/Run deterministically by `frames` (`1..=600`) or `seconds` and return emitted Output logs. |
| `play_character_control` | `get_state`, `teleport`, `move_to`, `jump`, `set_stats`, `equip_tool`, `unequip_tools`, `activate_tool`, `respawn`. |
| `play_input_inject` | Inject keyboard (`press`/`down`/`up`/`hold`), mouse (`move`/`click`/`right_click`/`drag`/`wheel`), or `type_text` events. |
| `output_console` | Read, filter (`level`, `contains`, `since_index`, `tail`), append, or clear `Editor.log`. |

### World Queries, Building & Physics (6)

| Tool | Description |
|---|---|
| `world_raycast` | Single or batch raycasts against `Edit`, `Server`, or `Client` BaseParts, Terrain, and Meshes. |
| `world_spatial_query` | `bounding_box` of any instance/Model subtree, `box_overlap`, or `sphere_overlap`. |
| `world_place_on_surface` | Snap instance(s)/Models flush onto the ground or surface below `[x, z]` with optional normal alignment. |
| `world_build_structure` | Procedural 3D builder (`box_room`, `wall_run`, `staircase`, `ramp`, `arch`, `cylinder_ring`, `scatter`). |
| `physics_inspect` | Inspect live dynamic body positions, velocities, mass, sleep/grounded state, and constraints during Play/Run. |
| `physics_manipulate` | Set linear/angular velocity, apply impulses, wake bodies, or toggle `Anchored`/`CanCollide` during Play/Run. |

### SmoothGrid Voxel Terrain (5)

| Tool | Description |
|---|---|
| `terrain_inspect` | Read `Terrain` water/decoration/material properties, chunk bounds, and optional 3D voxel samples. |
| `terrain_fill_shape` | Fill or carve (`Air`) `block`, `ball`, `cylinder`, `wedge`, or `water_level` regions. |
| `terrain_sculpt_stroke` | Apply terrain brush strokes (`Add`, `Subtract`, `Grow`, `Erode`, `Smooth`, `Flatten`, `Paint`, `SeaLevel`). |
| `terrain_generate` | Run procedural biome SmoothGrid generation (`seed`, `position`, `size`, `biomes`, `biome_size`, `caves`). |
| `terrain_clear` | Clear all SmoothGrid voxels or a `{ min, max }` region. |

### Viewport, Camera & Lighting (3)

| Tool | Description |
|---|---|
| `viewport_camera` | Read/set editor camera (`pos`, `look_at`, `yaw_deg`, `pitch_deg`, `fov_deg`) or `focus_target`. |
| `viewport_capture` | Deferred GPU capture of 3D `viewport` or full `window` with optional `camera` override, `clean_view`, `settle_frames`, `save_path`, `max_width`, and `return_image` (PNG). |
| `lighting_and_atmosphere` | Configure `Lighting`, `Sky`, `Atmosphere`, `Clouds`, post-processing effects (`Bloom`, `Blur`, `ColorCorrection`, `DepthOfField`, `SunRays`), or presets (`daylight`, `golden_hour`, `cyberpunk_night`, `foggy_horror`, `studio_neutral`). |

### Editor State & GUI Inspection (4)

| Tool | Description |
|---|---|
| `editor_selection` | `get`, `set`, `add`, `remove`, `clear`, or `select_all_of_class`. |
| `editor_undo_redo` | `status`, `undo` (`steps`), `redo` (`steps`), or `clear`. |
| `editor_ui_and_settings` | Set active transform `tool`, snap increments, collisions, join surfaces, local space, grid, trigger Studio actions, or read/write settings. |
| `gui_inspect` | Walk `StarterGui` / `PlayerGui` / `SurfaceGui` / `BillboardGui` and compute 2D `AbsolutePosition`/`AbsoluteSize` rects, visibility, text, ZIndex, and point hit-testing. |

### Project Sync, Plugins, Assets & Reflection (4)

| Tool | Description |
|---|---|
| `project_rojo` | Export to Rojo project folder, compute `sourcemap.json`, or build `.project.json` into `.rbxl`. |
| `plugin_control` | `list`, `load`, `reload`, `unload`, `list_ui`, or `trigger` Studio plugin toolbar buttons/actions. |
| `asset_import` | Import local `.rbxm`/`.rbxmx`/`.fbx`/`.obj` assets or insert numeric Roblox asset IDs. |
| `reflection_api_search` | Query RuStudio's embedded `rbx_reflection_database` and `api-dump.json` for classes, properties, methods, events, security tags, defaults, and Enum items. |

---

## MCP Resources (`rustudio://*`)

| URI | Provider | Contents |
|---|---|---|
| `rustudio://instances` | `rustudio-mcp.exe` | Live RuStudio instances, TCP ping status, and active routing target (works with 0 windows open). |
| `rustudio://state` | `rustudio.exe` | Editor mode, available DataModels, document metadata, camera, selection, tool state, FPS, memory, and output counts. |
| `rustudio://tree/workspace` | `rustudio.exe` | Hierarchical JSON dump of `Workspace`. |
| `rustudio://tree/game` | `rustudio.exe` | Hierarchical JSON dump of the entire `DataModel`. |
| `rustudio://scripts` | `rustudio.exe` | Inventory of all scripts with refs, paths, line counts, byte sizes, disabled flag, and `RunContext`. |
| `rustudio://output` | `rustudio.exe` | Recent `Editor.log` entries (`Info`/`Warn`/`Error`) with indices and counts. |
| `rustudio://selection` | `rustudio.exe` | Currently selected instances with tags, attributes, and stored properties. |

---

## MCP Expert Prompts

| Prompt | Arguments | Purpose |
|---|---|---|
| `omni-game-architect` | `goal?`, `architecture?` | Guides end-to-end Luau system design, atomic batch writes, Edit-during-Play iteration, and verification using live place stats. |
| `omni-playtest-debugger` | `symptom?` | Guides deterministic stepping, `luau_execute` VM probing, physics/character testing, and log triage using live warning/error context. |
| `omni-world-builder` | `theme?` | Guides SmoothGrid terrain sculpting, grounded procedural structures, lighting/atmosphere setup, and visual inspection using live camera/lighting context. |
| `omni-ui-designer` | `screen?` | Guides responsive `ScreenGui` construction, `gui_inspect` geometry checks, and interactive Play verification using live `StarterGui` inventory. |

---

## Headless & Automated AI Loops

Launch an offscreen GPU instance from MCP (no desktop window, full `wgpu` screenshots enabled):

```json
{
  "name": "studio_launch",
  "arguments": {
    "place_path": "C:\\Games\\MyPlace.rbxl",
    "headless": true,
    "window_size": "1280x720"
  }
}
```

Or launch a `--no-render` instance when you only need fast Luau/DOM/script/physics checks without GPU overhead:

```json
{
  "name": "studio_launch",
  "arguments": {
    "demo": true,
    "no_render": true,
    "fixed_dt": "1/60"
  }
}
```
