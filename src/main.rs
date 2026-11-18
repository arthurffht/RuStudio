//! RuStudio: a Windows-native Roblox Studio editor with OmniMCP AI control.

// Launch as a GUI application on Windows without opening a console window.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod grass_stream;
mod mcp;
mod plugin_store;
mod toolbox;
mod ui_pointer;
mod offscreen;
mod d3d11;
mod quality;
mod sim_clock;
mod bytecode_cache;
mod heap;
mod load_trace;
mod update;
mod env_probe;
mod account_ui;
mod char_builder;
mod game_avatar;
mod emote_wheel;
mod ribbon_quick;
mod cmeta;
mod studio_content;
mod paths;
mod platform;
mod dtmeta;
mod cloud_ui;
mod join_ui;
mod asset_report;
mod app;
mod frame_watchdog;
mod class_icons;
mod assets;
mod audio;
mod audio_dsp;
mod audio_scene;
mod audio_capture;
mod avatar;
mod bench;
mod gpu_timing;
mod bin_dump;
mod camera;
mod collision;
mod collision_groups;
mod control;
mod coverage;
mod crash;
mod default_scripts;
mod doc;
mod effects;
mod attachment;
mod explorer;
mod file_dialog;
mod game_settings;
mod studio_settings;
mod rustudio_settings;
mod fbx;
mod fbx_anim;
mod api;
mod api_stubs;
mod client_joints;
mod query_shapes;
mod fidelity;
mod telemetry;
mod gui;
mod gui_modal;
mod icons;
mod live_drafts;
mod layout_store;
mod panels;
mod lighting;
mod lightgrid;
mod keys;
mod mouse_lock;
mod lightocc;
mod ltypes;
mod math;
mod matpack;
mod mech;
mod part_drag;
mod output_panel;
mod props_layout;
mod prop_slider;
mod color_editors;
mod mesh;
mod truss;
mod mesh_bvh;
mod physics;
mod solver;
mod solver_ticks;
mod humanoid;
mod physics_mesh;
mod play;
mod plugin_host;
mod project;
mod project_config;
mod project_export;
mod project_ui;
mod project_sync;
mod project_values;
mod properties;
mod render;
mod clouds;
mod sun_shadow;
mod replicate;
mod anim_replicate;
mod rig;
mod runtime;
mod globals_parity;
mod sharedtable;
mod scene;
mod skin;
mod cage_rbf;
mod hsr;
mod skin_gpu;
mod highlight;
mod script_editor;
mod selftest;
mod terrain;
mod terrain_editor;
mod terrain_region;
mod terrain_ui;
mod terrain_brush;
mod terrain_gen;
mod terrain_mat;
mod world_gui;
mod texture;
mod theme;
mod ti;
mod studio_ui;
mod studio_actions;
mod studio_ribbon;
mod studio_panels;
mod start_page;
mod ui_shots;
mod grass_temporal;
mod viewport;
mod viewport_menu;
mod water;
mod wrap;

use std::path::PathBuf;

const USAGE: &str = "\
rustudio — edit Roblox places (.rbxl/.rbxlx) and models (.rbxm/.rbxmx)

usage: rustudio [FILE] [--demo] [--play] [--screenshot OUT.png] [--self-test OUT.json]
                [--bench OUT.json [--seconds N]]

  FILE                 open a place or model on start
  --demo               start with the sample scene
  --open-roblox ID|URL open a place from Roblox (place id, roblox.com/games/… or
                       create.roblox.com/…/places/… link) once signed in; with
                       --local-copy, a Team Create place opens as a local copy
  --roblox-picker TAB  show File > Open from Roblox (recent, mine, groups, shared),
                       or File > Experience Settings (settings)
  --play               start Play (F5) right after loading
  --team-create U P    join the live Team Create server for universe U, place P
  --no-ui              no rustudio UI: the window (exactly --window WxH) shows the
                       3D view alone, no menus, ribbon, Explorer, Properties,
                       Output or status bar (none of them is built). Without
                       --play: the place in Edit state through the edit camera
                       (--camera), scene only: grid, selection, gizmos, hover,
                       view cube and readouts are off (--overlays keeps them).
                       With --play: the game as a player sees it (ScreenGuis,
                       cursor, the Esc menu); every key goes to the game, F1–F12
                       and Escape included (Escape opens the menu and reaches
                       UserInputService as gameProcessedEvent true); no editor
                       shortcut (F5, Shift+F5, F8, …) and no --action.
                       --click/--move/--ldrag/--wheel X,Y are window pixels.
                       --shots writes one picture per time, FILE-<t>s.png, the
                       view at the window's size (no FILE-<t>s.view.png; the
                       .frames.json / .cursor.json beside it as in the editor)
  --play-only          --no-ui --play
  --overlays           with --no-ui in Edit state: keep the editor's overlays
  --quality LEVEL      override RenderSettings.EditQualityLevel (automatic or 1..=21)
  --frm MODE           override RenderSettings.FrameRateManager (auto, on, or off)
  --headless           no window, no compositor: frames are drawn offscreen at --window
                       WxH and read back for shots; 60 frames a second (unpaced
                       with --bench). Silent unless --audio. With --no-ui or
                       --play-only only
  --mcp-port PORT      bind the OmniMCP TCP bridge on 127.0.0.1:PORT (default:
                       auto-pick 44777..44790)
  --no-mcp             disable the OmniMCP TCP server on startup
  --no-render          --headless with no GPU at all: scripts, physics, sound
                       state and the harness run, nothing is drawn; --shots
                       write the log and frame times but no picture. The Output
                       log is a rendered run's minus the renderer's own lines
                       (Sky: …)
  --audio              with --headless / --no-render: play sound (default silent)
  --fixed-dt DT        deterministic stepping: every frame advances the game by
                       exactly DT (1/60, 0.0166 or 60hz) and frames run as fast
                       as the machine allows. Harness times (--key, --luau,
                       --shots, --seconds), os.clock, tick, os.time, time,
                       DateTime.now, task.wait, RunService deltas and
                       FrameTime, tweens, animations, sound positions and
                       physics all follow the simulated clock; unseeded
                       math.random / Random.new / BrickColor.Random take seeds
                       in a fixed order. Not deterministic: when downloaded
                       assets arrive, table/function addresses in tostring,
                       GUIDs. tick / os.time / os.date / DateTime.now start at
                       2026-01-01 00:00 UTC (RUSTUDIO_EPOCH=SECONDS to change)
  --screenshot FILE    draw ~120 frames, save the window as PNG, exit
  --self-test FILE     drive the editor with scripted input, write results, exit
  --shot-after S       with --screenshot: take it S seconds after start
  --shots T1,T2,…      with --screenshot: a shot (window and viewport) at each time,
                       the Output log and frame times with each (FILE-<t>s.png …)
  --from-play          harness times (shots, --luau, --key) count from Play start
  --write-corpus DIR   write the fidelity corpus places (demo, control) and exit
  --plugin FILE        load this Studio plugin (.lua/.luau/.rbxm/.rbxmx) once the place
                       is open, as a local plugin (user_<file name>); repeatable
  --plugins-dir DIR    load every plugin file in DIR, in Studio's order (.lua, .luau,
                       .rbxm, .rbxmx, each by name)
  --no-plugins         no plugin loads at all (also RUSTUDIO_NO_PLUGINS=1)
  --installed-plugin ID:FILE  load FILE as the installed plugin ID (cloud_<ID>)
  --action plugin-button:TOOLBAR/BUTTON@S, --action plugin-action:ACTIONID@S,
  --action plugin-menu:CHOICE@S (answers an open PluginMenu:ShowAsync: N, N.M,
                       an action id or text, or escape)
                       fire a plugin toolbar button's Click / a PluginAction's
                       Triggered at S seconds (works with --no-ui)
  --api-coverage OUT [GAPS…]  measure API coverage against app/api-dump.json
                       (RUSTUDIO_GAPS files from real runs add call gaps)
  --dump-scripts PLACE DIR  write every script's source under DIR
  --log FILE           with --screenshot: write the Output lines to FILE
  --key NAME@S[:HOLD]  press a key at S seconds (held HOLD seconds)
  --shift S:HOLD       hold Shift from S seconds for HOLD seconds
  --luau server|client FILE@S  run the Luau in FILE on that side of Play at S
                       seconds (with --play; see --from-play)
  --action NAME@S      run a Studio menu action by id at S seconds (e.g. panel_output)
  --move X,Y@S         move the pointer to view position X,Y (hover)
  --wheel N@S[:X,Y]    turn the mouse wheel N notches over the 3D view's centre
                       (or at view position X,Y)
  --type TEXT@S        type TEXT at S seconds (into a focused TextBox)
  --rdrag DX,DY@S      right-drag over the 3D view by DX,DY pixels
  --ldrag X1,Y1>X2,Y2@S  left-drag in the 3D view between view positions
  --lhold X,Y@S:HOLD   press the left button at X,Y at S seconds, let go HOLD seconds later
  --click X,Y@S        click the 3D view at X,Y (pixels) S seconds after start
  --key NAME@S         press a key (egui name, e.g. Enter, B) S seconds after start
  --window WxH         window size for test runs
  --camera X,Y,Z,YAW,PITCH  start the editor camera there (degrees)
  --bench FILE         measure frame times (after a 3 s warm-up) for N seconds
                       (default 10) at 1265x900 without vsync, write JSON, exit
  --seconds N          with --bench: how long to measure; with --no-ui (no
                       --bench): end the run at N seconds (from Play's start
                       with --from-play)
  --result FILE        at the end of a test run, one JSON: why it ended (shots,
                       seconds, bench, EndTest), exit code, Edit or Play, the
                       EndTest value, error and warning counts, frame times
                       (count, mean, p50, p99, max, work), the shots written,
                       CPU time and peak RSS, and the whole Output log
  --fail-on-error      exit 1 if the Output has any error line (script errors,
                       --luau errors, engine errors shown in red)
                       A server script ends a --no-ui (or --result) run with
                       game:GetService(\"StudioTestService\"):EndTest(value): Play
                       stops, --log / --result are written, the process exits
  --ui-shots DIR       pictures of the ribbon tabs, menus and panels in DIR, exit

tools (no window; each exits when done):
  --stats PLACE        counts of what is in a place
  --script PLACE PATTERN  print the scripts whose full name contains PATTERN
  --fetch-assets DIR ID…  download public assets by id into DIR
  --install-studio-content  download Studio's built-in content from Roblox's CDN
  --list-plugins [--json]   the local and installed plugins rustudio would load
  --install-plugin ID       Get (free) + download a Creator Store plugin; --update-plugin ID
  --uninstall-plugin ID     remove an installed plugin from rustudio
  --enable-plugin ID / --disable-plugin ID   (cloud_<n>, user_<file>, asset id or file name)
  --sync-plugins            read your installed plugins from Roblox
  --import-studio-plugins [N]  list Studio installs / copy install N's plugins in
  --search-plugins WORDS    Creator Store plugin search
  --publish-plugin FILE NAME [--overwrite ID] [--dry-run]  upload a model as a plugin
  --export-project OUT_DIR PLACE [--lua] [--no-sourcemap] [--containers A,B/C]
                       write a place out as a Rojo / Argon project folder
  --sourcemap FOLDER|FILE.project.json [OUT.json] [--include-non-scripts]
                       write Rojo's sourcemap.json (stdout without OUT)
  --build OUT.rbxl FOLDER|FILE.project.json  save a project folder as one place
  --solver-ticks SCENE.json [--out FILE]  step one physics lab scene tick by tick
                       and print every body's state

physics (environment):
  RUSTUDIO_SOLVER=studio   Studio's own (legacy) solver: the default when unset
  RUSTUDIO_SOLVER=rapier   the old physics (rapier), as before the switch; use it
                       for a place that uses what the Studio solver does not
                       cover yet (rustudio names the class once when it sees one)
";

/// A harness run that ends by itself (pictures, a self-test, a benchmark,
/// a CI result, or a --no-ui run with --seconds): no window on screen.
fn is_test_run(argv: &[String]) -> bool {
    let has = |f: &str| argv.iter().any(|a| a == f);
    ["--screenshot", "--self-test", "--bench", "--ui-shots", "--result", "--fail-on-error"].iter().any(|f| has(f))
        || ((has("--no-ui") || has("--play-only")) && has("--seconds"))
}

fn window_arg(argv: &[String]) -> Option<(u32, u32)> {
    let i = argv.iter().position(|a| a == "--window")?;
    let (w, h) = argv.get(i + 1)?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn parse_quality_arg(s: &str) -> Option<u32> {
    let lower = s.trim().to_ascii_lowercase();
    if lower == "automatic" || lower == "auto" {
        return Some(0);
    }
    let digits = lower.strip_prefix("level").unwrap_or(&lower);
    let n: u32 = digits.parse().ok()?;
    (n <= 21).then_some(n)
}

fn parse_frm_arg(s: &str) -> Option<u32> {
    match s.trim().to_ascii_lowercase().as_str() {
        "automatic" | "auto" | "0" => Some(0),
        "on" | "1" => Some(1),
        "off" | "2" => Some(2),
        _ => None,
    }
}

fn main() -> Result<(), String> {
    // When invoked from cmd/PowerShell with CLI arguments, attach to the
    // launching console so stdout/stderr output appears despite windows_subsystem.
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // SAFETY: attaching to parent console if one exists; harmless if none or redirected.
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    // Headless runs are silent unless --audio (before any thread starts).
    {
        let argv: Vec<String> = std::env::args().collect();
        let headless = argv.iter().any(|a| a == "--headless" || a == "--no-render");
        if headless && !argv.iter().any(|a| a == "--audio") && std::env::var_os("RUSTUDIO_NO_AUDIO").is_none() {
            std::env::set_var("RUSTUDIO_NO_AUDIO", "1");
        }
        // No update check (and so no "Update available" notice in the
        // corner) in headless and harness runs: their captures are compared
        // picture by picture.
        if (headless || is_test_run(&argv)) && std::env::var_os("RUSTUDIO_NO_UPDATE_CHECK").is_none() {
            std::env::set_var("RUSTUDIO_NO_UPDATE_CHECK", "1");
        }
    }
    load_trace::init();
    // First: a panic or unhandled exception from here on leaves a report.
    crash::install();
    // RUSTUDIO_CRASH_TEST=panic|thread: check the reports themselves.
    match std::env::var("RUSTUDIO_CRASH_TEST").as_deref() {
        Ok("panic") => panic!("RUSTUDIO_CRASH_TEST panic"),
        Ok("thread") => {
            let _ = std::thread::Builder::new()
                .name("crash-test".into())
                .spawn(|| panic!("RUSTUDIO_CRASH_TEST thread panic"))
                .map(|h| h.join());
        }
        _ => {}
    }
    // os.clock counts from here, as Studio's does from its process start.
    let _ = runtime::process_start();
    // Before any thread makes a Luau VM: the flags are process-global.
    runtime::luau_flags();
    matpack::init_from_env();
    // RUSTUDIO_PLACE_CONTEXT=ID: ask asset delivery in that Roblox place's
    // context for a local file (a copy of a Roblox place).
    if let Some(p) = std::env::var("RUSTUDIO_PLACE_CONTEXT").ok().and_then(|v| v.parse().ok()) {
        rustudio_account::session::set_place_context(p);
    }
    // Plugin store commands (src/plugin_store/cli.rs), no window.
    {
        let argv: Vec<String> = std::env::args().collect();
        if let Some(code) = plugin_store::cli::run(&argv) {
            std::process::exit(code);
        }
        if let Some(code) = toolbox::cli::run(&argv) {
            std::process::exit(code);
        }
    }
    // `--install-studio-content`: download Studio's built-in content from
    // Roblox's CDN into ~/.local/share/rustudio/studio, then exit.
    if std::env::args().any(|a| a == "--install-studio-content") {
        match studio_content::install(&|s| eprintln!("studio content: {s}")) {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                eprintln!("studio content: {e}");
                std::process::exit(1);
            }
        }
    }
    // `--write-corpus DIR`: the fidelity corpus's place files, then exit.
    let argv: Vec<String> = std::env::args().collect();
    if let Some(i) = argv.iter().position(|a| a == "--write-corpus") {
        let dir = argv.get(i + 1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        match fidelity::write_corpus(&dir) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("write-corpus: {e}");
                std::process::exit(1);
            }
        }
    }
    // `--solver-ticks SCENE.json [--out FILE]`: one compiled physics scene
    // stepped tick by tick, every body's state printed (src/solver_ticks.rs).
    if let Some(i) = argv.iter().position(|a| a == "--solver-ticks") {
        let scene = argv.get(i + 1).cloned().unwrap_or_default();
        let out = argv.iter().position(|a| a == "--out").and_then(|j| argv.get(j + 1)).map(|s| s.as_str());
        match solver_ticks::run(&scene, out) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("solver-ticks: {e}");
                std::process::exit(2);
            }
        }
    }
    // `--export-project OUT_DIR PLACE.rbxl [--lua] [--containers A,B/C]`: a
    // place written out as a Rojo / Argon project folder (src/project_export.rs).
    if let Some(i) = argv.iter().position(|a| a == "--export-project") {
        let (Some(out), Some(src)) = (argv.get(i + 1), argv.get(i + 2)) else {
            eprintln!("usage: rustudio --export-project OUT_DIR PLACE.rbxl [--lua] [--no-sourcemap] [--containers ReplicatedStorage,StarterPlayer/StarterPlayerScripts,...]");
            std::process::exit(2);
        };
        let out = PathBuf::from(out);
        let mut o = project_export::ExportOptions {
            name: out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "game".into()),
            lua: argv.iter().any(|a| a == "--lua"),
            sourcemap: !argv.iter().any(|a| a == "--no-sourcemap"),
            ..Default::default()
        };
        if let Some(j) = argv.iter().position(|a| a == "--containers") {
            o.containers = argv.get(j + 1).map(|s| s.split(',').map(String::from).collect()).unwrap_or_default();
        }
        let res = doc::Document::open(std::path::Path::new(src)).and_then(|d| project_export::export(&d, &out, &o));
        match res {
            Ok(r) => {
                for w in &r.warnings {
                    eprintln!("warning: {w}");
                }
                println!(
                    "{}: {} scripts, {} folders, {} models",
                    r.project_file.display(), r.scripts, r.folders, r.models
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("export-project: {e}");
                std::process::exit(1);
            }
        }
    }
    // `--sourcemap FOLDER [OUT] [--include-non-scripts]`: Rojo's sourcemap.json.
    if let Some(i) = argv.iter().position(|a| a == "--sourcemap") {
        let Some(src) = argv.get(i + 1) else {
            eprintln!("usage: rustudio --sourcemap FOLDER|FILE.project.json [OUT.json] [--include-non-scripts]");
            std::process::exit(2);
        };
        let src = PathBuf::from(src);
        let file = if src.is_dir() { project::project_files(&src).into_iter().next() } else { Some(src.clone()) };
        let all = argv.iter().any(|a| a == "--include-non-scripts");
        let res = file
            .ok_or_else(|| format!("no *.project.json in {}", src.display()))
            .and_then(|f| project_sync::open(&f))
            .map(|(doc, st)| st.sourcemap_bytes(&doc, all));
        match res {
            Ok(bytes) => {
                match argv.get(i + 2).filter(|a| !a.starts_with("--")) {
                    Some(out) => {
                        if let Err(e) = std::fs::write(out, &bytes) {
                            eprintln!("sourcemap: {out}: {e}");
                            std::process::exit(1);
                        }
                    }
                    None => {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(&bytes);
                    }
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("sourcemap: {e}");
                std::process::exit(1);
            }
        }
    }
    // `--build OUT.rbxl FOLDER`: a project folder (its default project file,
    // over the place file it names) saved as one place file, then exit.
    if let Some(i) = argv.iter().position(|a| a == "--build") {
        let (Some(out), Some(src)) = (argv.get(i + 1), argv.get(i + 2)) else {
            eprintln!("usage: rustudio --build OUT.rbxl FOLDER|FILE.project.json");
            std::process::exit(2);
        };
        let src = PathBuf::from(src);
        let file = if src.is_dir() {
            project::project_files(&src).into_iter().next()
        } else {
            Some(src.clone())
        };
        let res = file
            .ok_or_else(|| format!("no *.project.json in {}", src.display()))
            .and_then(|f| project_sync::open(&f))
            .and_then(|(mut doc, _)| doc.save_to(std::path::Path::new(out)));
        match res {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("build: {e}");
                std::process::exit(1);
            }
        }
    }
    // First launch without a Studio install: fetch Studio's built-in content
    // in the background (never in test runs, which must be reproducible).
    let test_run = is_test_run(&std::env::args().collect::<Vec<_>>());
    if !test_run {
        studio_content::spawn_background_install_if_missing();
    }
    let mut open: Option<PathBuf> = None;
    let mut screenshot: Option<PathBuf> = None;
    let mut selftest: Option<PathBuf> = None;
    let mut ui_shots: Option<PathBuf> = None;
    let mut demo = false;
    let mut start_play = false;
    let mut bench: Option<PathBuf> = None;
    let mut camera: Option<[f32; 5]> = None;
    let mut seconds = mech::PERF_SECONDS;
    let mut shot_after: Option<f32> = None;
    let mut shots: Vec<f32> = Vec::new();
    let mut from_play = false;
    let mut log_out: Option<PathBuf> = None;
    let mut actions: Vec<(f32, app::HarnessAction)> = Vec::new();
    let mut open_roblox: Option<u64> = None;
    let mut team_create: Option<(u64, u64)> = None;
    let mut roblox_picker: Option<cloud_ui::PickerTab> = None;
    let mut roblox_settings = false;
    let mut local_copy = false;
    let mut no_ui = false;
    let mut overlays = false;
    let mut headless = false;
    let mut no_render = false;
    let mut seconds_given = false;
    let mut fixed_dt: Option<f64> = None;
    let mut fail_on_error = false;
    let mut result: Option<PathBuf> = None;
    let mut plugin_files: Vec<PathBuf> = Vec::new();
    let mut installed_plugins: Vec<(u64, PathBuf)> = Vec::new();
    let mut plugin_dirs: Vec<PathBuf> = Vec::new();
    let mut no_plugins = false;
    let mut mcp_port: Option<u16> = None;
    let mut no_mcp = false;
    let mut quality_override: Option<u32> = None;
    let mut frm_override: Option<u32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--screenshot" => screenshot = args.next().map(PathBuf::from),
            "--bench" => bench = args.next().map(PathBuf::from),
            "--camera" => {
                let v: Vec<f32> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|x| x.trim().parse().ok())
                    .collect();
                if v.len() == 5 {
                    camera = Some([v[0], v[1], v[2], v[3], v[4]]);
                }
            }
            "--shot-after" => shot_after = args.next().and_then(|s| s.parse().ok()),
            "--shots" => {
                shots = args
                    .next()
                    .map(|v| v.split(',').filter_map(|t| t.trim().parse().ok()).collect())
                    .unwrap_or_default()
            }
            "--from-play" => from_play = true,
            "--log" => log_out = args.next().map(PathBuf::from),
            "--click" | "--key" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse(&v, a == "--click") {
                    Some(x) => actions.extend(x),
                    None => eprintln!("{a}: can't read {v:?}"),
                }
            }
            "--move" => {
                // The pointer to view position X,Y at S (hover, no buttons).
                let v = args.next().unwrap_or_default();
                match v.split_once('@').and_then(|(xy, at)| {
                    let (x, y) = xy.split_once(',')?;
                    Some((
                        at.trim().parse::<f32>().ok()?,
                        app::HarnessAction::PointerAt(x.trim().parse().ok()?, y.trim().parse().ok()?),
                    ))
                }) {
                    Some(x) => actions.push(x),
                    None => eprintln!("--move: can't read {v:?} (X,Y@SECONDS)"),
                }
            }
            "--wheel" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse_wheel(&v) {
                    Some(x) => actions.push(x),
                    None => eprintln!("--wheel: can't read {v:?} (NOTCHES@SECONDS or NOTCHES@SECONDS:X,Y)"),
                }
            }
            "--type" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse_type(&v) {
                    Some(x) => actions.push(x),
                    None => eprintln!("--type: can't read {v:?} (TEXT@SECONDS)"),
                }
            }
            "--shift" => {
                let v = args.next().unwrap_or_default();
                match v.split_once(':').and_then(|(a, h)| {
                    Some((a.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?))
                }) {
                    Some((at, hold)) => {
                        actions.push((at, app::HarnessAction::Shift(true)));
                        actions.push((at + hold, app::HarnessAction::Shift(false)));
                    }
                    None => eprintln!("--shift: can't read {v:?} (SECONDS:HOLD)"),
                }
            }
            "--action" => {
                let v = args.next().unwrap_or_default();
                match v.rsplit_once('@').and_then(|(n, at)| Some((at.trim().parse::<f32>().ok()?, n.trim().to_string()))) {
                    Some((at, name)) => actions.push((at, app::HarnessAction::UiAction(name))),
                    None => eprintln!("--action: can't read {v:?} (NAME@SECONDS)"),
                }
            }
            "--ldrag" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse_ldrag(&v) {
                    Some(list) => actions.extend(list),
                    None => eprintln!("--ldrag: can't read {v:?} (X1,Y1>X2,Y2@SECONDS)"),
                }
            }
            "--lhold" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse_lhold(&v) {
                    Some(list) => actions.extend(list),
                    None => eprintln!("--lhold: can't read {v:?} (X,Y@SECONDS:HOLD)"),
                }
            }
            "--rdrag" => {
                let v = args.next().unwrap_or_default();
                match app::HarnessAction::parse_rdrag(&v) {
                    Some(x) => actions.extend(x),
                    None => eprintln!("--rdrag: can't read {v:?} (DX,DY@SECONDS)"),
                }
            }
            "--luau" => {
                // --luau server|client FILE@S
                let side = args.next().unwrap_or_default();
                let v = args.next().unwrap_or_default();
                match v.rsplit_once('@').and_then(|(f, at)| {
                    Some((std::fs::read_to_string(f).ok()?, at.parse::<f32>().ok()?))
                }) {
                    Some((code, at)) => {
                        actions.push((at, app::HarnessAction::Luau(side != "client", code)))
                    }
                    None => eprintln!("--luau: can't read {v:?} (FILE@SECONDS)"),
                }
            }
            "--window" => {
                args.next();
            }
            "--seconds" => {
                seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(seconds);
                seconds_given = true;
            }
            "--fail-on-error" => fail_on_error = true,
            "--fixed-dt" => {
                let v = args.next().unwrap_or_default();
                match sim_clock::parse_dt(&v) {
                    Some(dt) => fixed_dt = Some(dt),
                    None => {
                        eprintln!("--fixed-dt: can't read {v:?} (1/60, 0.0166 or 60hz)");
                        std::process::exit(2);
                    }
                }
            }
            "--result" => result = args.next().map(PathBuf::from),
            "--plugin" => plugin_files.extend(args.next().map(PathBuf::from)),
            // An installed (Creator Store) plugin: ASSETID:FILE, named cloud_<id>.
            "--installed-plugin" => {
                if let Some((id, f)) = args.next().as_deref().and_then(|v| v.split_once(':')) {
                    if let Ok(id) = id.parse() {
                        installed_plugins.push((id, PathBuf::from(f)));
                    }
                }
            }
            "--plugins-dir" => plugin_dirs.extend(args.next().map(PathBuf::from)),
            "--no-plugins" => no_plugins = true,
            "--self-test" => selftest = args.next().map(PathBuf::from),
            "--ui-shots" => ui_shots = args.next().map(PathBuf::from),
            "--stats" => {
                // Counts of what's in a place (no window).
                let path = args.next().map(PathBuf::from).unwrap_or_default();
                match doc::Document::open(&path) {
                    Ok(d) => print!("{}", d.stats()),
                    Err(e) => eprintln!("{e}"),
                }
                return Ok(());
            }
            "--api-coverage" => {
                // Measures API coverage against app/api-dump.json (no window):
                // --api-coverage OUT [GAPS.json …] (real-run gap files).
                let out = args.next().map(PathBuf::from).unwrap_or_else(|| "coverage.json".into());
                let gaps: Vec<PathBuf> = args.by_ref().map(PathBuf::from).collect();
                match coverage::run(&out, &gaps) {
                    Ok(r) => {
                        for k in ["functions", "properties", "events", "callbacks"] {
                            println!(
                                "{k}: exists {} / real {} / {} (game scripts: exists {} / real {} / {})",
                                r[k]["exists"], r[k]["implemented"], r[k]["total"],
                                r[k]["game_scripts_exists"], r[k]["game_scripts_implemented"],
                                r[k]["game_scripts_total"]
                            );
                        }
                    }
                    Err(e) => eprintln!("coverage: {e}"),
                }
                return Ok(());
            }
            "--dump-scripts" => {
                // Writes every script's full source under DIR as <Full.Name>.luau (no window).
                let path = args.next().map(PathBuf::from).unwrap_or_default();
                let dir = args.next().map(PathBuf::from).unwrap_or_default();
                match doc::Document::open(&path) {
                    Ok(d) => {
                        let _ = std::fs::create_dir_all(&dir);
                        let mut n = 0;
                        for (name, src) in d.all_script_sources() {
                            if std::fs::write(dir.join(format!("{name}.luau")), src).is_ok() {
                                n += 1;
                            }
                        }
                        println!("wrote {n} scripts to {}", dir.display());
                    }
                    Err(e) => eprintln!("{e}"),
                }
                return Ok(());
            }
            "--fetch-assets" => {
                // Downloads public assets by id (rustudio's cache path) into
                // DIR as <id>.<ext>; a texture pack's maps come along. Used to
                // give an offline Studio the terrain packs (round 21).
                let dir = args.next().map(PathBuf::from).unwrap_or_default();
                let _ = std::fs::create_dir_all(&dir);
                let mut queue: Vec<u64> = args.by_ref().filter_map(|a| a.parse().ok()).collect();
                let mut done = std::collections::HashSet::new();
                while let Some(id) = queue.pop() {
                    if !done.insert(id) {
                        continue;
                    }
                    match assets::public_cached(id) {
                        Ok(b) => {
                            let text = std::str::from_utf8(&b).ok();
                            let ext = if b.starts_with(b"\x89PNG") {
                                "png"
                            } else if b.starts_with(b"\xff\xd8\xff") {
                                "jpg"
                            } else if text.is_some_and(|t| t.contains("texturepack_version")) {
                                "texturepack.xml"
                            } else {
                                "bin"
                            };
                            if let Some(p) = text.and_then(|t| matpack::parse_pack(t).ok()) {
                                queue.extend([p.color, p.normal, p.roughness, p.metalness, p.height].into_iter().flatten());
                            }
                            let out = dir.join(format!("{id}.{ext}"));
                            match std::fs::write(&out, &b[..]) {
                                Ok(()) => println!("{id} {} bytes -> {}", b.len(), out.display()),
                                Err(e) => eprintln!("{id}: {e}"),
                            }
                        }
                        Err(e) => eprintln!("{id}: {e}"),
                    }
                }
                return Ok(());
            }
            "--script" => {
                // Prints the source of scripts whose full name contains PATTERN (no window).
                let path = args.next().map(PathBuf::from).unwrap_or_default();
                let pat = args.next().unwrap_or_default();
                match doc::Document::open(&path) {
                    Ok(d) => print!("{}", d.script_sources(&pat)),
                    Err(e) => eprintln!("{e}"),
                }
                return Ok(());
            }
            "--team-create" => {
                let universe = args.next().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v != 0);
                let place = args.next().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v != 0);
                match (universe, place) {
                    (Some(u), Some(p)) => team_create = Some((u, p)),
                    _ => { eprintln!("--team-create requires UNIVERSE_ID PLACE_ID"); std::process::exit(2); }
                }
            }
            "--open-roblox" => {
                let v = args.next().unwrap_or_default();
                match rustudio_account::parse_place_ref(&v) {
                    Ok(p) => open_roblox = Some(p.place_id),
                    Err(e) => {
                        eprintln!("--open-roblox: {e}");
                        std::process::exit(2);
                    }
                }
            }
            "--local-copy" => local_copy = true,
            "--roblox-picker" => {
                let v = args.next().unwrap_or_default();
                match cloud_ui::PickerTab::parse(&v) {
                    Some(t) => roblox_picker = Some(t),
                    // File > Experience Settings instead of the picker.
                    None if v == "settings" => roblox_settings = true,
                    None => {
                        eprintln!("--roblox-picker: {v:?} (recent, mine, groups, shared or settings)");
                        std::process::exit(2);
                    }
                }
            }
            "--demo" => demo = true,
            "--play" => start_play = true,
            "--no-ui" => no_ui = true,
            "--play-only" => {
                no_ui = true;
                start_play = true;
            }
            "--overlays" => overlays = true,
            "--headless" => headless = true,
            "--no-render" => {
                headless = true;
                no_render = true;
            }
            "--mcp-port" => {
                let v = args.next().unwrap_or_default();
                match v.parse::<u16>() {
                    Ok(p) if p > 0 => mcp_port = Some(p),
                    _ => {
                        eprintln!("--mcp-port: expected port number 1..65535, got {v:?}");
                        std::process::exit(2);
                    }
                }
            }
            "--no-mcp" => no_mcp = true,
            "--quality" => {
                let v = args.next().unwrap_or_default();
                match parse_quality_arg(&v) {
                    Some(q) => quality_override = Some(q),
                    None => {
                        eprintln!("--quality: expected automatic or 0..=21, got {v:?}");
                        std::process::exit(2);
                    }
                }
            }
            "--frm" => {
                let v = args.next().unwrap_or_default();
                match parse_frm_arg(&v) {
                    Some(f) => frm_override = Some(f),
                    None => {
                        eprintln!("--frm: expected auto, on, or off, got {v:?}");
                        std::process::exit(2);
                    }
                }
            }
            // (read at the start of main)
            "--audio" => {}
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ if a.starts_with("--") => {
                eprint!("unknown option {a}\n\n{USAGE}");
                std::process::exit(2);
            }
            _ => open = Some(PathBuf::from(a)),
        }
    }
    if headless && !no_ui {
        eprintln!("--headless / --no-render: only with --no-ui (or --play-only); the editor's own UI needs a window");
        std::process::exit(2);
    }
    if no_ui && (selftest.is_some() || ui_shots.is_some()) {
        eprintln!("--no-ui: --self-test and --ui-shots drive the editor's UI; run them without --no-ui");
        std::process::exit(2);
    }
    // The screenshot shows the editor with something in it to look at.
    if screenshot.is_some() && open.is_none() {
        demo = true;
    }

    // --fixed-dt: simulated clocks from here (src/sim_clock.rs).
    if let Some(dt) = fixed_dt {
        sim_clock::enable(dt);
    }
    let harness = app::Harness {
        shot_after,
        log_out,
        actions,
        pointer: None,
        started: sim_clock::now(),
        held: 0.0,
        screenshot,
        selftest: selftest.map(selftest::SelfTest::new),
        ui_shots: ui_shots.map(ui_shots::UiShots::new),
        screenshot_requested: false,
        start_play,
        camera,
        shift: false,
        shots,
        shot_next: 0,
        shot_lines: Vec::new(),
        from_play,
        play_started: None,
        play_game_time: None,
        frame_ms: Vec::new(),
        last_frame: None,
        open_roblox: open_roblox.map(|p| (p, local_copy)),
        team_create,
        roblox_picker,
        roblox_settings,
        ui_due: Vec::new(),
        no_ui,
        overlays,
        no_render,
        // (--seconds ends a --no-ui run; with --bench it is the measuring time)
        end_after: (no_ui && seconds_given && bench.is_none()).then_some(seconds),
        fail_on_error,
        result,
        plugin_files,
        installed_plugins,
        plugin_dirs,
        no_plugins,
        mcp_port,
        no_mcp,
        plugins_started: false,
        end_value: None,
        run_frames: Vec::new(),
        run_last: None,
        run_playing: false,
        finished: false,
        bench: bench.map(|out| {
            let label = open
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "new place".into());
            bench::Bench::new(Some(out), &label, start_play, seconds)
        }),
    };
    // No vsync when measuring, so frame times show the real cost.
    // (--fixed-dt: as fast as the machine allows)
    let bench_mode = harness.bench.is_some() || harness.selftest.is_some() || sim_clock::on();
    let argv: Vec<String> = std::env::args().collect();
    let size = match window_arg(&argv) {
        Some((w, h)) => [w as f32, h as f32],
        None if harness.bench.is_some() => [1265.0, 900.0],
        // The self-test's lighting references were read in a 952×519 view
        // (rounds 7–15); round 16's chrome is 25 px shorter, so 33 px less window.
        None if harness.selftest.is_some() => [1265.0, 867.0],
        None => [1600.0, 900.0],
    };
    // Studio Settings: the user's for plain runs (src/studio_settings.rs).
    studio_settings::init(layout_store::enabled(&harness));
    if let Some(q) = quality_override {
        studio_settings::set(
            "RenderSettings",
            "EditQualityLevel",
            studio_settings::Val::Enum(q),
        );
    }
    if let Some(f) = frm_override {
        studio_settings::set(
            "RenderSettings",
            "FrameRateManager",
            studio_settings::Val::Enum(f),
        );
    }
    // rustudio's own options: likewise only for runs a person drives, never
    // under RUSTUDIO_DETERMINISTIC (src/rustudio_settings.rs).
    rustudio_settings::init(layout_store::enabled(&harness));
    // The saved window (src/layout_store.rs), for plain interactive runs.
    let saved_layout = (!harness.no_ui && layout_store::enabled(&harness)).then(layout_store::load).flatten();
    let icon = egui::IconData {
        rgba: icons::logo_rgba(64),
        width: 64,
        height: 64,
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("rustudio")
        .with_app_id("rustudio")
        .with_inner_size(size)
        .with_icon(icon);
    // (--no-ui: any size, the view is the whole window)
    if !harness.no_ui {
        viewport = viewport.with_min_inner_size([900.0, 560.0]);
    }
    // A test run gets the size it asked for even on a smaller desktop
    // (window host clamps the first size to the monitor: an 800x600 session
    // desktop made `--window 1024x768` an 800x600 editor, its docked
    // widgets too short for the checks).
    if cfg!(windows) && is_test_run(&argv) {
        viewport = viewport.with_clamp_size_to_monitor_size(false);
    }
    if let Some(l) = &saved_layout {
        viewport = l.window_builder(viewport);
    }
    // Driven for comparison captures (src/control.rs): open without taking
    // focus and stay below other windows, so it never covers the person's
    // work (X11 keeps drawing covered windows, and shots read the frame back).
    if std::env::var_os("RUSTUDIO_CONTROL").is_some() {
        viewport = viewport.with_active(false).with_window_level(egui::WindowLevel::AlwaysOnBottom);
    }
    let vsync = !(bench_mode || std::env::var_os("RUSTUDIO_CONTROL").is_some());
    // --headless / --no-render: no window at all (src/offscreen.rs).
    if headless {
        let px = [size[0].round() as u32, size[1].round() as u32];
        let code = offscreen::run(
            offscreen::Options { size: px, render: !no_render, paced: !bench_mode },
            move |ctx, rs| app::RustudioApp::new_in(ctx, rs, None, open, harness, demo),
        );
        crate::telemetry::flush();
        std::process::exit(code.max(app::EXIT_CODE.load(std::sync::atomic::Ordering::Relaxed)));
    }
    let rs = d3d11::create_device()?;
    let r = d3d11::window::run(
        d3d11::WindowOptions { viewport, vsync },
        rs,
        Box::new(move |ctx, rs| {
            Box::new(app::RustudioApp::new_in(
                &ctx,
                Some(rs),
                Some(false),
                open,
                harness,
                demo,
            ))
        }),
    );
    // --fail-on-error (app::Editor::finish_run); a windowed test run's exit
    // status reaches the caller through the headless wrapper's status file.
    let code = app::EXIT_CODE.load(std::sync::atomic::Ordering::Relaxed);
    if code != 0 {
        std::process::exit(code);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quality_and_frm_cli_flags() {
        assert_eq!(parse_quality_arg("automatic"), Some(0));
        assert_eq!(parse_quality_arg("Auto"), Some(0));
        assert_eq!(parse_quality_arg("0"), Some(0));
        assert_eq!(parse_quality_arg("21"), Some(21));
        assert_eq!(parse_quality_arg("Level05"), Some(5));
        assert_eq!(parse_quality_arg("22"), None);
        assert_eq!(parse_quality_arg("invalid"), None);

        assert_eq!(parse_frm_arg("auto"), Some(0));
        assert_eq!(parse_frm_arg("Automatic"), Some(0));
        assert_eq!(parse_frm_arg("on"), Some(1));
        assert_eq!(parse_frm_arg("off"), Some(2));
        assert_eq!(parse_frm_arg("3"), None);
    }
}
