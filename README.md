<p align="center">
  <img src="assets/icons/rustudio-256.png" width="128" alt="RuStudio icon">
</p>

<h1 align="center">RuStudio (Windows + OmniMCP)</h1>

<p align="center">
  <strong>Windows-native Roblox Studio in Rust with OmniMCP unrestricted AI game engine control.</strong>
</p>

Open, edit, and save Roblox places, run real Luau scripts, physics, characters, terrain, and GUIs natively on Windows, and give AI agents full programmatic + visual engine access through **OmniMCP** (`rustudio-mcp.exe`).

> Forked from [rippelz/rustudio](https://github.com/rippelz/rustudio) and tailored exclusively for Windows (`x86_64-pc-windows-msvc`) at [arthurffht/RuStudio](https://github.com/arthurffht/RuStudio).

![Playing a test place in RuStudio](docs/images/play.png)

![The RuStudio home screen](docs/images/home.png)

## Highlights

- **OmniMCP AI Engine Bridge (57 Tools, 7 Resources, 4 Prompts):**
  - Dual-process Windows architecture: `rustudio.exe` exposes a localhost TCP bridge (`127.0.0.1:44777..44790`) while `rustudio-mcp.exe` provides a stdio JSON-RPC 2.0 server (both newline-delimited and `Content-Length` framed) that works even when zero editor windows are open.
  - **Multi-instance control**: `studio_list_instances`, `studio_launch` (windowed, `--headless` offscreen GPU, or `--no-render`), `studio_select_instance`, `studio_kill_instance`, and per-call `studio_id` routing.
  - **Unrestricted DOM & Script tooling**: Batch recursive tree creation (`dom_create_instances`), property/attribute/tag edits bypassing `NotScriptable` and capability gates (`dom_set_properties`), unlimited queries (`dom_query`, `script_grep`), atomic multi-script patches (`script_write_batch`, `script_patch_batch`), and single-frame `omni_batch`.
  - **Edit during Play**: Read and edit the source `Edit` DataModel even while Play (`F5`) or Run (`F8`) is running, plus inspect/mutate `Server` and `Client` DataModels.
  - **Live Luau VM & Physics**: Execute Luau at security identity `2`/`4`/`6`/`8` (`luau_execute`), fire signals (`luau_fire_signal`), step deterministic frames (`play_step_frames`), drive the character (`play_character_control`), inject inputs (`play_input_inject`), and inspect/manipulate dynamic physics (`physics_inspect`, `physics_manipulate`).
  - **SmoothGrid Terrain & GPU Vision**: Programmatic voxel fill/sculpt/generate (`terrain_*`), raycasts & surface placement (`world_*`), lighting/atmosphere presets (`lighting_and_atmosphere`), GUI rect inspection (`gui_inspect`), and live `wgpu` PNG viewport captures (`viewport_capture`).
  - Full details in [docs/MCP.md](docs/MCP.md).
- **Editor:** Explorer, Properties, Output, script tabs with Luau syntax checking, move/scale/rotate gizmos with drag duplication and selection rotation, pop-out panels, terrain brushes and threaded generation, undo/redo, and Studio menus/shortcuts.
- **Files & Rojo/Argon:** `.rbxl` / `.rbxlx` places and `.rbxm` / `.rbxmx` models, plus project folders (`*.project.json` with plain files as source of truth, `sourcemap.json`, and File > Save as Project Folder…).
- **Roblox Destinations, Plugins & Toolbox:** Save/Publish to Roblox, run local and Creator Store Studio plugins, and search/insert Creator Store assets (when signed in).
- **Play / Run:** Separate server and client DataModels, remotes, Studio-tuned physics solver (`RUSTUDIO_SOLVER=rapier` available as fallback), R15/R6 characters with climbing and held jump, shift lock, sounds, tweens, 2D and 3D GUIs.
- **Rendering:** Native `wgpu` (DX12 / Vulkan) with lighting, shadows, sky, atmosphere, materials tuned against Studio, local lights, meshes, FBX import, and voxel terrain.

## Install on Windows

### Prebuilt Release (`RuStudio-windows-x64.zip`)

1. Download `RuStudio-windows-x64.zip` from [Releases](https://github.com/arthurffht/RuStudio/releases).
2. Extract the archive and double-click `install.cmd` (or run `tools\windows\install.ps1`).
   - Installs `rustudio.exe`, `rustudio-mcp.exe`, `api-dump.json`, and helper scripts to `%LOCALAPPDATA%\Programs\rustudio` (no administrator rights or Rust toolchain required).
   - Portable mode is also supported: run `rustudio.exe` directly from the extracted folder.
3. Close RuStudio before upgrading, then run `install.cmd` from the new release. Run `uninstall.cmd` to remove the installation while preserving places, settings, sign-in state, and downloaded content.

### Register OmniMCP in Your AI Client

After installing or building, register `rustudio-mcp.exe` in Antigravity, Claude Desktop, Claude Code, Cursor, VS Code, or Windsurf:

```powershell
# Preview changes without touching files
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1 -DryRun

# Register for detected clients (or specify -Targets Antigravity,ClaudeCode,Cursor,VSCode,Windsurf)
powershell -ExecutionPolicy Bypass -File .\tools\windows\register-mcp.ps1
```

See [docs/MCP.md](docs/MCP.md) for manual JSON examples and headless automation instructions.

## Build from Source (Windows MSVC)

Requirements:
- Windows 10/11 (x64)
- Rust toolchain (`1.85+`, `x86_64-pc-windows-msvc`)
- Visual Studio C++ Build Tools & Windows SDK (`vcvars64.bat` / Developer PowerShell)
- GPU supporting DirectX 12 or Vulkan (WARP software adapter works for `--headless`)

```powershell
git clone https://github.com/arthurffht/RuStudio.git
cd RuStudio
cargo build --release --bins
.\target\release\rustudio.exe --demo
.\target\release\rustudio.exe path\to\place.rbxl
```

Run tests:

```powershell
cargo check --workspace --all-targets
cargo test --workspace --features skip-physics-tests
```

## Windows Paths & Studio Content

- **Local data, caches & MCP instances:** `%LOCALAPPDATA%\rustudio` (`%LOCALAPPDATA%\rustudio\mcp-instances\<pid>.json`, `%LOCALAPPDATA%\rustudio\crash\`)
- **Settings:** `%APPDATA%\rustudio`
- **Built-in Studio content:** On first launch, RuStudio auto-detects an existing Windows Roblox Studio installation under `%LOCALAPPDATA%\Roblox\Versions\version-*` or downloads Studio's built-in content from Roblox's CDN into `%LOCALAPPDATA%\rustudio\studio` (or run `rustudio.exe --install-studio-content`; override via `RUSTUDIO_STUDIO_DIR`).
- **Auto-update:** Checks [arthurffht/RuStudio releases](https://github.com/arthurffht/RuStudio/releases) against `SHA256SUMS` (set `RUSTUDIO_NO_UPDATE_CHECK=1` to disable).

## Signing In

Offline editing, local places, and OmniMCP work signed out. To open cloud places, load private assets, or publish experiences, click the account button at the top right and **Sign in with your browser** (or paste `.ROBLOSECURITY`, stored via the Windows Credential Manager). See [docs/AUTH.md](docs/AUTH.md).

## Docs

- [docs/MCP.md](docs/MCP.md): OmniMCP architecture, all 57 tools, resources, prompts, `NotScriptable` bypass, `Edit`-during-`Play`, and client setup
- [docs/DEVELOPING.md](docs/DEVELOPING.md): CLI options, tests, and code layout
- [docs/MECHANICS.md](docs/MECHANICS.md): Every tuning number and where it comes from
- [docs/ENGINE-GAPS.md](docs/ENGINE-GAPS.md): Engine API gaps measured against real places
- [docs/TEAMCREATE.md](docs/TEAMCREATE.md): Team Create protocol and workflow

## Not Affiliated with Roblox

RuStudio is an independent project. It is not made, endorsed, or supported by Roblox Corporation. "Roblox" and "Roblox Studio" are trademarks of Roblox Corporation.

This repository does not redistribute Roblox Studio or its content. Built-in Studio content (`ReflectionMetadata.xml`, fonts, textures, avatar parts) is read from your local Studio installation or downloaded from Roblox's CDN at runtime.

## License

MIT OR Apache-2.0, at your option. Vendored crates in `vendor/` keep their own licenses.
