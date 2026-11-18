//! The editor: state shared by every panel, menus, toolbar, shortcuts and
//! the docked layout.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use egui::{Key, KeyboardShortcut, Modifiers, RichText};
use egui_dock::{DockArea, DockState, TabViewer};
use glam::Vec3;
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::InstanceBuilder;

use crate::account_ui::AccountUi;
use crate::camera::Camera;
use crate::doc::{self, DocKind, Document};
use crate::explorer::ExplorerState;
use crate::mech;
use crate::properties::PropertiesState;
use crate::render::{MeshKind, Renderer};
use crate::scene::Scene;
use crate::selftest::SelfTest;
use crate::theme;
use crate::viewport::GizmoDrag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    None,
    Transform,
    Select,
    Move,
    Scale,
    Rotate,
}

impl Tool {
    pub fn toggle(self, requested: Self) -> Self {
        if self == requested { Self::None } else { requested }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::None => "None",
            Tool::Transform => "Transform",
            Tool::Select => "Select",
            Tool::Move => "Move",
            Tool::Scale => "Scale",
            Tool::Rotate => "Rotate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

pub struct LogLine {
    pub at: String,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Viewport,
    Explorer,
    Properties,
    Output,
    /// Studio's Avatar Settings (File > Avatar Settings).
    AvatarSettings,
    /// Studio's Terrain Editor (Home / View > Terrain Editor).
    TerrainEditor,
    /// Studio's Toolbox (Home / View > Toolbox): src/toolbox.
    Toolbox,
    /// A script open in the code editor.
    Script(Ref),
    /// A plugin's DockWidgetPluginGui (src/plugin_host/widgets_ui.rs: the
    /// slot number of its `edit_<plugin>_<id>` key).
    Plugin(u32),
}

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Viewport => "Viewport",
            Tab::Explorer => "Explorer",
            Tab::Properties => "Properties",
            Tab::Output => "Output",
            Tab::AvatarSettings => "Avatar Settings",
            Tab::TerrainEditor => "Terrain Editor",
            Tab::Toolbox => "Toolbox",
            Tab::Script(_) => "Script",
            Tab::Plugin(_) => "Plugin",
        }
    }
}

/// Counters the self-test reads to know each panel was actually drawn.
#[derive(Default, Debug, Clone)]
pub struct HudStats {
    pub explorer_rows: usize,
    pub property_rows: usize,
    pub output_lines: usize,
    pub viewport_drawn: bool,
    pub toolbar_buttons: usize,
    pub status_drawn: bool,
}

pub struct Harness {
    /// `--shot-after S`: take the screenshot S seconds after start instead of
    /// after SCREENSHOT_FRAMES frames (Play sessions need time to load).
    pub shot_after: Option<f32>,
    /// `--log FILE`: write the Output lines there on exit.
    pub log_out: Option<PathBuf>,
    /// `--click X,Y@S` / `--key NAME@S`: input at S seconds (X,Y in the 3D
    /// view, pixels from its top-left).
    pub actions: Vec<(f32, HarnessAction)>,
    /// The pointer's last window position seen in the input (harness clicks
    /// while the game holds the mouse press there).
    pub pointer: Option<egui::Pos2>,
    pub started: Instant,
    pub screenshot: Option<PathBuf>,
    pub selftest: Option<SelfTest>,
    /// `--ui-shots DIR`: pictures of the chrome (src/ui_shots.rs).
    pub ui_shots: Option<crate::ui_shots::UiShots>,
    pub screenshot_requested: bool,
    /// `--play`: start a Play session right after loading.
    pub start_play: bool,
    /// `--bench`: frame-time measurement.
    pub bench: Option<crate::bench::Bench>,
    /// `--camera x,y,z,yaw,pitch`: starting camera (degrees).
    pub camera: Option<[f32; 5]>,
    /// `--shift S:HOLD`: Shift held (egui reports it as a modifier, not a key).
    pub shift: bool,
    /// `--shots T1,T2,…`: a window and a viewport screenshot at each time
    /// (FILE-<t>s.png / FILE-<t>s.view.png), the Output log with each, then exit.
    pub shots: Vec<f32>,
    pub shot_next: usize,
    /// `[shot] PATH` lines for the shots written so far (stdout and --log).
    pub shot_lines: Vec<String>,
    /// `--from-play`: harness times count from the moment Play starts (as a
    /// Studio playtest's do), not from program start.
    pub from_play: bool,
    pub play_started: Option<Instant>,
    /// Play's simulated seconds since it started, when frames step a fixed dt
    /// (RUSTUDIO_FIXED_DT): then the harness clock is this one, so a run's shots
    /// and exit come at the same game time however loaded the machine is.
    pub play_game_time: Option<f32>,
    /// Frame times (ms) since Play started, for the fidelity report.
    pub frame_ms: Vec<f32>,
    pub last_frame: Option<Instant>,
    /// `--open-roblox ID|URL`: open that place from Roblox once the session
    /// is checked.
    pub open_roblox: Option<(u64, bool)>,
    pub team_create: Option<(u64, u64)>,
    /// `--roblox-picker TAB`: show File > Open from Roblox on that tab.
    pub roblox_picker: Option<crate::cloud_ui::PickerTab>,
    pub roblox_settings: bool,
    /// `--action NAME@S` menu actions now due (run where the egui context is).
    pub ui_due: Vec<String>,
    /// `--no-ui`: the 3D view (or the game) alone over the whole window; no
    /// editor UI is built (RustudioApp::no_ui_frame).
    pub no_ui: bool,
    /// `--overlays`: with --no-ui, keep the editor's view overlays (grid,
    /// selection, gizmos, hover, view cube, readouts) in Edit state.
    pub overlays: bool,
    /// `--no-render`: no GPU device on purpose (src/offscreen.rs); shots
    /// write no picture.
    pub no_render: bool,
    /// `--seconds N` without --bench in a --no-ui run: end the run at N
    /// seconds on the harness clock (from Play's start with --from-play).
    pub end_after: Option<f32>,
    /// `--fail-on-error`: exit 1 when the Output has an error line.
    pub fail_on_error: bool,
    /// `--result FILE`: one JSON for CI at the end of the run (finish_run).
    pub result: Option<PathBuf>,
    /// `--plugin FILE` (repeatable) / `--plugins-dir DIR`: plugins loaded
    /// once the place is open; `--no-plugins`: none at all.
    pub plugin_files: Vec<PathBuf>,
    /// --installed-plugin ASSETID:FILE (loaded as cloud_<id>).
    pub installed_plugins: Vec<(u64, PathBuf)>,
    pub plugin_dirs: Vec<PathBuf>,
    pub no_plugins: bool,
    pub mcp_port: Option<u16>,
    pub no_mcp: bool,
    pub plugins_started: bool,
    /// StudioTestService:EndTest's value once a script ended the run.
    pub end_value: Option<String>,
    /// Every frame's interval and work (ms), from the first frame in Edit
    /// state, from Play's start in Play (the --result frame statistics).
    pub run_frames: Vec<(f32, f32)>,
    pub run_last: Option<Instant>,
    pub run_playing: bool,
    pub finished: bool,
    /// Seconds the (Edit state) schedule was held by a harness wait for an
    /// event (`toolbox:await-ask`): actions, shots and the end come that
    /// much later, so what follows the wait runs relative to the event.
    pub held: f32,
}

/// The process's exit code once a harness run finished (finish_run).
pub static EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

impl Harness {
    /// Whether StudioTestService:EndTest ends the whole run here (a --no-ui
    /// run, or any run with --result) rather than only stopping Play.
    pub fn ends_on_end_test(&self) -> bool {
        self.no_ui || self.result.is_some()
    }

    /// Seconds on the harness clock (None before Play when --from-play).
    pub fn now(&self) -> Option<f32> {
        if self.from_play {
            if self.play_started.is_some() && self.play_game_time.is_some() {
                return self.play_game_time;
            }
            self.play_started.map(|t| crate::sim_clock::since(t).as_secs_f32())
        } else {
            Some(self.unheld_now() - self.held)
        }
    }

    /// Seconds since start, holds included.
    pub fn unheld_now(&self) -> f32 {
        crate::sim_clock::since(self.started).as_secs_f32()
    }
}

pub struct Editor {
    pub doc: Document,
    /// The document's Highlights, kept from its change journal (viewport.rs).
    pub highlight_index: Option<crate::doc::ClassIndex>,
    pub cam: Camera,
    pub tool: Tool,
    pub snap_move: bool,
    /// Model tab Collisions: handle moves / scales stop where parts would
    /// overlap (TransformHandlesImplementation._safelyTransformParts).
    pub collisions: bool,
    /// Model tab Join Surfaces: drops weld touching faces (JointPairs).
    pub join_surfaces: bool,
    /// Dragger modes: Physical (DraggerMovementMode.Physical) drags in the
    /// camera plane like Studio's IK dragger; Geometric rests on surfaces.
    pub physical_drag: bool,
    /// Snap to Parts (DraggerService.PartSnapEnabled): free drags snap the
    /// selection's edges / centre to the target face's (soft snaps).
    pub part_snap: bool,
    /// Use Bounding Boxes (DraggerService.UseBoundingBoxes): the part under
    /// the mouse is treated as its box when building the drop surface.
    pub use_bounding_boxes: bool,
    /// Align Dragged Objects (StudioService.AlignDraggedObjects): dropped
    /// selections turn to the nearest axis alignment of the target surface.
    pub align_dragged: bool,
    pub move_inc: f32,
    pub snap_rotate: bool,
    pub rotate_inc: f32,
    pub local_space: bool,
    pub show_grid: bool,
    /// The scene alone in the 3D view, without the editor's own drawing (the
    /// ground grid, selection and hover boxes, handles, the drag box, the
    /// tool readout and axis compass): comparison captures (src/control.rs).
    pub clean_view: bool,
    pub log: Vec<LogLine>,
    pub clipboard: Option<Vec<u8>>,
    pub render_state: Option<crate::d3d11::RenderState>,
    pub renderer: Option<Renderer>,
    pub scene: Scene,
    /// The parts inside drawn ViewportFrames (vf_render.rs), drawn by the
    /// same `sync_draws` as the world's, with their own draw cache.
    pub vf_scene: Scene,
    pub vf_draw_cache: crate::viewport::DrawCache,
    pub vf_draws_asset_gen: u64,
    pub hover: Option<Ref>,
    pub drag: Option<GizmoDrag>,
    pub marquee: Option<egui::Pos2>,
    /// The selection when the box started (Shift / Ctrl add to it).
    pub marquee_base: Vec<Ref>,
    /// The Output panel's toolbar choices (src/output_panel.rs).
    pub output_view: crate::output_panel::OutputState,
    /// Dragging parts with the Select / Move tool (src/part_drag.rs).
    pub part_drag: Option<crate::part_drag::PartDrag>,
    pub explorer: ExplorerState,
    pub props: PropertiesState,
    pub account: AccountUi,
    /// The character builder window (Home / Avatar → Character).
    pub chars: crate::char_builder::CharBuilder,
    /// Places stored on Roblox: File > Open from Roblox, Save to Roblox.
    pub cloud: crate::cloud_ui::CloudUi,
    /// Scripts to open (or focus) as editor tabs at the end of the frame.
    pub open_script: Vec<Ref>,
    /// Script tabs a plugin closed (ScriptDocument:CloseAsync).
    pub close_script: Vec<Ref>,
    document_tabs_reset: bool,
    pub scripts: crate::script_editor::ScriptTabs,
    /// The running Play / Run session, if any.
    pub play: Option<crate::play::PlaySession>,
    /// Studio plugins in the edit DataModel (src/plugin_host).
    pub plugins: crate::plugin_host::PluginHost,
    /// Plugin toolbars / menus / shortcuts as the editor draws them
    /// (src/plugin_host/ui_ribbon.rs).
    pub plugin_bar: crate::plugin_host::ui_ribbon::PluginUi,
    /// Plugin dock widgets' panels, CoreGui over the edit viewport, and the
    /// edit-mode input plugins hear (src/plugin_host/widgets_ui.rs).
    pub plugin_ui: crate::plugin_host::widgets_ui::PluginUi,
    /// Fixed frame time for deterministic self-tests.
    pub fixed_dt: Option<f32>,
    /// ParticleEmitters, Beams and Trails (round 25).
    pub effects: crate::effects::Effects,
    /// Loaded MeshPart / SpecialMesh geometry.
    pub meshes: crate::assets::MeshCache,
    pub textures: crate::assets::TextureCache,
    /// Cloud asset downloads + disk cache.
    pub assets: crate::assets::AssetStore,
    /// ScreenGuis / SurfaceGuis / BillboardGuis: textures, fonts, hover, focus.
    pub gui: crate::gui::Gui,
    /// Clothing-template box meshes by body part name.
    pub clothing_meshes: std::collections::HashMap<String, u32>,
    /// The rig picker: Some(R15 / R6) is an explicit choice that wins over
    /// the place's setting and the avatar; None (Auto) lets them decide.
    pub play_rig: Option<crate::rig::RigType>,
    /// Play opens on the client's view, as Studio does (the self-test's
    /// older steps turn this off to watch the server).
    pub play_shows_client: bool,
    /// Last frame: (loaded meshes drawn, textured parts, decals, clothed body parts).
    pub draw_stats: [usize; 4],
    /// What the renderer holds per part.
    pub draw_cache: crate::viewport::DrawCache,
    /// RUSTUDIO_CONTROL: the comparison-capture TCP socket (src/control.rs).
    pub control: Option<crate::control::Control>,
    /// OmniMCP localhost TCP bridge server (src/mcp/server.rs).
    pub mcp: Option<crate::mcp::server::McpServer>,
    /// Bumped whenever a downloaded asset arrives (parts waiting on one are redone).
    pub asset_gen: u64,
    pub draws_asset_gen: u64,
    /// matpack::generation() and wrap::generation() last seen.
    pub material_gen: u64,
    pub wrap_gen: u64,
    /// Fitting failures already reported.
    pub wrap_warned: std::collections::HashSet<String>,
    /// Triangles in the drawn voxel terrain.
    pub terrain_tris: usize,
    /// The terrain as last drawn (document id, Terrain, voxels), so an edit
    /// re-meshes only the chunks it changed (round 21).
    pub terrain_drawn: Option<(u64, rbx_dom_weak::types::Ref, crate::terrain::Voxels)>,
    /// Terrain shader parameters to rebuild (colours, packs arriving), and
    /// the material-pack generation they were built at.
    pub terrain_params_dirty: bool,
    pub terrain_params_gen: u64,
    pub terrain_present: [bool; 23],
    /// Chunks with Grass tops (decoration candidates) and those whose blades
    /// are built.
    pub terrain_grass_chunks: std::collections::HashSet<[i32; 3]>,
    pub terrain_grass_built: std::collections::HashSet<[i32; 3]>,
    pub terrain_grass_length: f32,
    /// The water look last drawn (WaterColor, WaterTransparency, WaterReflectance).
    pub terrain_water_look: Option<([u8; 3], f32, f32)>,
    /// Terrain water's waves and clearness for the water pass (round 24).
    pub water_waves: crate::water::WaterLook,
    /// Studio's scene environment probe (env_probe.rs).
    pub env_probe: crate::env_probe::EnvProbe,
    /// The Terrain Editor's tools and settings.
    pub terrain_ed: crate::terrain_editor::TerrainEditor,
    /// The Toolbox panel's state (src/toolbox), kept while it is hidden.
    pub toolbox: crate::toolbox::Toolbox,
    /// This frame's Toolbox preview ViewportFrames (drawn with the GUI's).
    pub toolbox_vf: std::collections::HashMap<rbx_dom_weak::types::Ref, crate::render::vf::Request>,
    /// A brush stroke's voxels, the chunks it touched and whether water may
    /// have changed: drawn next frame without decoding the grid.
    pub terrain_brushed: Option<(crate::terrain::Voxels, Vec<[i32; 3]>, bool)>,
    /// Physical key positions held (movement keys; src/keys.rs).
    pub phys: crate::keys::PhysicalKeys,
    /// Seconds the fly keys have been held / released (speed ramp).
    pub fly_held: f32,
    pub fly_idle: f32,
    /// Last wheel zoom: (cursor ray direction, hit distance) for the self-test.
    pub last_zoom: Option<(Vec3, Option<f32>)>,
    pub hud: HudStats,
    pub ui_rects: HashMap<&'static str, egui::Rect>,
    /// Screen positions of gizmo handles last frame (axis, sign) -> point.
    pub gizmo_points: Vec<(crate::viewport::Handle, egui::Pos2)>,
    /// Selection / hover outlines kept while nothing they depend on changes.
    pub outline_cache: std::cell::RefCell<crate::viewport::OutlineCache>,
    /// The hovered instance's outline parts, for (hover, journal end).
    /// The selection's outline parts, for (journal end, selection).
    pub selection_outline_refs: std::cell::RefCell<(u64, Vec<rbx_dom_weak::types::Ref>, Vec<rbx_dom_weak::types::Ref>)>,
    /// The view selector's meshes for the current view.
    pub selector_cache: crate::viewport::SelectorCache,
    pub hover_outline_refs: (Option<(rbx_dom_weak::types::Ref, u64)>, Vec<rbx_dom_weak::types::Ref>),
    pub viewport_rect: egui::Rect,
    pub frame_no: u64,
    pub harness: Harness,
    pub last_frame: Instant,
    pub fps: f32,
    pub confirm_discard: Option<PendingAction>,
    /// Selection as of the last Explorer sync.
    pub last_selection: Vec<Ref>,
    /// Active right-drag camera look with the cursor grabbed.
    pub look_grab: Option<crate::viewport::LookGrab>,
    /// Locked grab failed once: use Confined from then on.
    pub grab_use_confined: bool,
    /// The game holds the cursor (UserInputService.MouseBehavior while playing).
    pub game_grab: Option<crate::viewport::LookGrab>,
    pub game_cursor_hidden: bool,
    /// Cursor commands sent, newest last (the self-test checks them).
    pub cursor_log: Vec<String>,
    /// The 3D view rendered at this many pixels instead of its panel's
    /// (camera aspect from it; drawn scaled into the panel): self-test
    /// steps that compare with a Studio capture at Studio's view size.
    pub view_render_size: Option<[u32; 2]>,
    /// CPU time of the last frame's update + draw submission (ms).
    pub last_work_ms: f32,
    /// Local lights, sky faces and the sky-visibility map.
    pub lighting: crate::viewport::LightingState,
    /// The open project folder (files are the truth; src/project_sync.rs).
    pub project: Option<crate::project_sync::ProjectState>,
    /// A folder with several project files: (folder, the files) to choose from.
    pub project_choice: Option<(PathBuf, Vec<PathBuf>)>,
    /// File > Save as Project Folder… / Project Settings… (src/project_ui.rs).
    pub project_export: Option<crate::project_ui::ExportDialog>,
    pub project_settings: Option<crate::project_ui::SettingsDialog>,
    /// Studio plugins: the local / installed store, Manage Plugins, and the
    /// runtime they're handed to (src/plugin_store).
    pub plugin_store: crate::plugin_store::manager::PluginManager,
    /// Open / save file dialogs, run off the UI thread (src/file_dialog.rs).
    pub dialogs: crate::file_dialog::Dialogs,
    /// Studio's chrome: ribbon tab, run type, panel toggles, popups (src/studio_ui.rs).
    pub ui: crate::studio_ui::UiState,
    /// Test Here: the character's first spawn point (the camera's position).
    pub play_here: Option<Vec3>,
    /// View > Screenshot waiting for the frame.
    pub pending_user_shot: Option<PathBuf>,
    /// The Start Page (src/start_page.rs): shown when nothing is open.
    pub start: crate::start_page::StartPage,
    /// Requests switching to the main viewport tab.
    pub switch_to_viewport: bool,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    New,
    Open(Option<PathBuf>),
    OpenFolder,
    /// A place from Roblox: place id, a version (else the latest), and
    /// whether to take a local copy of a Team Create place.
    OpenCloud(u64, Option<u64>, bool),
    /// File > Close Place: back to the Start Page.
    Close,
    Quit,
    /// File > Restart rustudio: a fresh process on the same place.
    Restart,
}

pub struct RustudioApp {
    watchdog: crate::frame_watchdog::Watchdog,
    updater: crate::update::Updater,
    pub dock: DockState<Tab>,
    pub ed: Editor,
    /// Keeps the dock, window and view settings (src/layout_store.rs).
    layout: Option<crate::layout_store::Saver>,
    /// Panels popped out into their own windows, and where each panel was
    /// last docked (src/panels.rs).
    pub panels: crate::panels::Panels,
}

/// Studio's starting layout, from the placement table (src/panels.rs):
/// Explorer right, Properties under it, Output under the 3D view.
pub fn default_dock() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Viewport]);
    let none = HashMap::new();
    for t in [Tab::Explorer, Tab::Properties, Tab::Output, Tab::Toolbox] {
        crate::panels::place(&mut dock, t, &none);
    }
    dock
}

impl RustudioApp {
    /// The app on an egui context and (optional) D3D11 RenderState of its own:
    /// the Win32 D3D11 window (`src/d3d11/window.rs`) or the offscreen runner
    /// (`src/offscreen.rs`, `--headless`, no window; `--no-render`, no GPU).
    pub fn new_in(
        egui_ctx: &egui::Context,
        render_state: Option<crate::d3d11::RenderState>,
        open: Option<PathBuf>,
        harness: Harness,
        demo: bool,
    ) -> Self {
        crate::ti::install(egui_ctx);
        theme::apply(egui_ctx);
        let renderer = render_state.as_ref().map(Renderer::new);
        let mut ed = Editor {
            dialogs: crate::file_dialog::Dialogs::new(egui_ctx.clone()),
            doc: Document::new_place(),
            highlight_index: None,
            cam: Camera::default(),
            tool: Tool::Select,
            snap_move: mech::SNAP_ON_BY_DEFAULT,
            collisions: mech::COLLISIONS_ON_BY_DEFAULT,
            join_surfaces: mech::JOIN_SURFACES_ON_BY_DEFAULT,
            physical_drag: mech::PHYSICAL_DRAG_BY_DEFAULT,
            part_snap: mech::PART_SNAP_ON_BY_DEFAULT,
            use_bounding_boxes: mech::USE_BOUNDING_BOXES_BY_DEFAULT,
            align_dragged: mech::ALIGN_DRAGGED_BY_DEFAULT,
            move_inc: mech::MOVE_SNAP,
            snap_rotate: mech::SNAP_ON_BY_DEFAULT,
            rotate_inc: mech::ROTATE_SNAP_DEG,
            local_space: false,
            show_grid: true,
            clean_view: false,
            log: Vec::new(),
            clipboard: None,
            render_state,
            renderer,
            scene: Scene::default(),
            vf_scene: Scene::viewport_frames(),
            vf_draw_cache: Default::default(),
            vf_draws_asset_gen: 0,
            hover: None,
            drag: None,
            marquee: None,
            marquee_base: Vec::new(),
            output_view: Default::default(),
            part_drag: None,
            explorer: ExplorerState::default(),
            props: PropertiesState::default(),
            account: AccountUi::new(),
            chars: Default::default(),
            cloud: crate::cloud_ui::CloudUi::new(),
            open_script: Vec::new(),
            close_script: Vec::new(),
            document_tabs_reset: false,
            scripts: Default::default(),
            play: None,
            plugins: Default::default(),
            plugin_ui: Default::default(),
            plugin_bar: Default::default(),
            fixed_dt: None,
            effects: Default::default(),
            meshes: Default::default(),
            textures: Default::default(),
            assets: Default::default(),
            gui: Default::default(),
            clothing_meshes: Default::default(),
            play_rig: None,
            play_shows_client: true,
            draw_stats: [0; 4],
            draw_cache: Default::default(),
            control: None,
            mcp: None,
            asset_gen: 0,
            draws_asset_gen: 0,
            material_gen: 0,
            wrap_gen: 0,
            wrap_warned: Default::default(),
            terrain_tris: 0,
            terrain_drawn: None,
            terrain_params_dirty: true,
            terrain_params_gen: u64::MAX,
            terrain_present: [false; 23],
            terrain_grass_chunks: Default::default(),
            terrain_grass_built: Default::default(),
            terrain_grass_length: -1.0,
            terrain_water_look: None,
            water_waves: Default::default(),
            env_probe: Default::default(),
            terrain_ed: Default::default(),
            toolbox: Default::default(),
            toolbox_vf: Default::default(),
            terrain_brushed: None,
            phys: Default::default(),
            fly_held: 0.0,
            fly_idle: 0.0,
            last_zoom: None,
            hud: HudStats::default(),
            ui_rects: HashMap::new(),
            gizmo_points: Vec::new(),
            outline_cache: Default::default(),
            hover_outline_refs: (None, Vec::new()),
            selector_cache: Default::default(),
            selection_outline_refs: std::cell::RefCell::new((u64::MAX, Vec::new(), Vec::new())),
            viewport_rect: egui::Rect::NOTHING,
            frame_no: 0,
            harness,
            last_frame: Instant::now(),
            fps: 0.0,
            confirm_discard: None,
            last_selection: Vec::new(),
            look_grab: None,
            grab_use_confined: !crate::mouse_lock::platform_locks(),
            game_grab: None,
            game_cursor_hidden: false,
            cursor_log: Vec::new(),
            view_render_size: None,
            last_work_ms: 0.0,
            lighting: Default::default(),
            project: None,
            project_choice: None,
            project_export: None,
            project_settings: None,
            plugin_store: crate::plugin_store::manager::PluginManager::new(
                crate::plugin_store::Store::open(crate::plugin_store::default_root(), crate::plugin_store::default_plugins_dir(), 0),
                Box::new(crate::plugin_store::runtime::HostBridge::default()),
            ),
            ui: crate::studio_ui::UiState::default(),
            play_here: None,
            pending_user_shot: None,
            start: Default::default(),
            switch_to_viewport: false,
        };
        ed.info(format!(
            "rustudio {} ready. Reflection database has {} classes.",
            env!("CARGO_PKG_VERSION"),
            doc::db().classes.len()
        ));
        for line in crate::crash::unseen_reports() {
            ed.error(line);
        }
        if ed.renderer.is_none() && !ed.harness.no_render {
            ed.error("No D3D11 renderer available; the 3D viewport is disabled.");
        }
        let open_none = open.is_none();
        if let Some(path) = open {
            ed.open_path(path);
        } else if demo {
            ed.build_demo();
        }
        if !ed.harness.no_ui {
            ed.explorer.expand_defaults(&ed.doc);
        }
        // No UI: the scene alone (the comparison captures' clean view)
        // unless --overlays keeps the editor's overlays.
        if ed.harness.no_ui && !ed.harness.overlays {
            ed.clean_view = true;
        }
        if ed.harness.selftest.is_some() {
            // Deterministic play-session timing for the scripted test.
            ed.fixed_dt = Some(1.0 / 60.0);
        }
        // RUSTUDIO_FIXED_DT=HZ (tests only): Play steps 1/HZ s a frame however
        // long frames take (with RUSTUDIO_GAME_CLOCK, probes timed by os.clock
        // see an unloaded machine).
        if let Some(hz) = std::env::var("RUSTUDIO_FIXED_DT").ok().and_then(|v| v.parse::<f32>().ok()).filter(|h| *h > 0.0) {
            ed.fixed_dt = Some(1.0 / hz);
        }
        // --fixed-dt (src/sim_clock.rs): the same fixed step, every clock on it.
        if let Some(dt) = crate::sim_clock::dt() {
            ed.fixed_dt = Some(dt);
        }
        if ((ed.harness.selftest.is_none() && ed.harness.bench.is_none())
            || std::env::var_os("RUSTUDIO_SESSION").is_some())
            && std::env::var_os("RUSTUDIO_NO_SESSION").is_none()
        {
            // The session (login keyring) is checked in the background, for
            // real playthroughs too (--screenshot): asset downloads and the
            // avatar. LoadAsset at Play start waits for the check. The
            // self-test and --bench opt in with RUSTUDIO_SESSION=1;
            // RUSTUDIO_NO_SESSION=1 keeps any run signed out.
            ed.account.restore();
        }
        if let Some((u, p)) = ed.harness.team_create { ed.open_team_create(u, p); }
        ed.cloud.open_when_ready = ed.harness.open_roblox.map(|(p, copy)| (p, None, copy));
        ed.cloud.picker_when_ready = ed.harness.roblox_picker;
        ed.cloud.settings_open = ed.harness.roblox_settings;
        if let Ok(path) = std::env::var("RUSTUDIO_CONTROL") {
            match crate::control::Control::listen(&path) {
                Ok(c) => {
                    ed.control = Some(c);
                    ed.clean_view = true;
                }
                Err(e) => eprintln!("[control] can't listen on {path}: {e}"),
            }
        }
        if !ed.harness.no_mcp && std::env::var_os("RUSTUDIO_NO_MCP").is_none() {
            match crate::mcp::server::McpServer::start(
                egui_ctx.clone(),
                ed.harness.mcp_port,
                &ed.doc.title,
                ed.doc.path.as_deref(),
            ) {
                Ok(s) => ed.mcp = Some(s),
                Err(e) => eprintln!("[mcp] can't start OmniMCP server: {e}"),
            }
        }
        if let Some([x, y, z, yaw, pitch]) = ed.harness.camera {
            ed.cam.pos = Vec3::new(x, y, z);
            ed.cam.yaw = yaw.to_radians();
            ed.cam.pitch = pitch.to_radians();
            ed.cam.focus = ed.cam.pos + ed.cam.forward() * 50.0;
        } else if ed.harness.bench.as_ref().is_some_and(|b| !b.play) {
            // Worst case for drawing: the whole place in view.
            ed.frame_all();
        }
        // Nothing to open: the Start Page, as Studio starts. Test and harness
        // runs (and RUSTUDIO_NO_START_PAGE=1) go straight to the editor.
        let plain = ed.harness.plain();
        // RUSTUDIO_START_PAGE=1 forces it (screenshots of the page itself).
        let forced = std::env::var_os("RUSTUDIO_START_PAGE").is_some();
        if !ed.harness.no_ui && (forced || (open_none && !demo && plain && std::env::var_os("RUSTUDIO_NO_START_PAGE").is_none())) {
            ed.show_start_page();
            let page = std::env::var("RUSTUDIO_START_PAGE").unwrap_or_default();
            ed.start.open_page(&page);
        }
        // The last session's layout and view settings (plain runs only).
        let mut dock = default_dock();
        let mut layout = None;
        let mut panels = crate::panels::Panels::default();
        if !ed.harness.no_ui && crate::layout_store::enabled(&ed.harness) {
            let saved = crate::layout_store::load();
            if let Some(l) = &saved {
                l.apply(&mut ed);
                if let Some(d) = l.dock() {
                    dock = d;
                }
                panels = l.panels(&dock);
            }
            layout = Some(crate::layout_store::Saver::new(saved.as_ref()));
            // (plugin widgets' restore state beside layout.json)
            crate::plugin_host::widgets_ui::set_persist_dir(Some(crate::paths::config().join("rustudio")));
        }
        // Comparison captures (src/control.rs) want the 3D view as big as the
        // other program's: the view alone, no Explorer / Properties / Output.
        if ed.control.is_some() && layout.is_none() {
            dock = DockState::new(vec![Tab::Viewport]);
        }
        // (--no-ui never shows the dock.)
        if ed.harness.no_ui {
            dock = DockState::new(vec![Tab::Viewport]);
        }
        // `--play` starts once the 3D view has its size (Camera.ViewportSize).
        RustudioApp {
            watchdog: crate::frame_watchdog::Watchdog::new(egui_ctx.clone()),
            updater: crate::update::Updater::start(),
            dock,
            ed,
            layout,
            panels,
        }
    }
}

// ------------------------------------------------------------------ logging

fn timestamp() -> String {
    // Studio's Output timestamp: HH:MM:SS.mmm in local time.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = (ms / 1000) as i64;
    let offset = chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).offset().local_minus_utc())
        .unwrap_or(0);
    let s = ((ms / 1000) as i64 + offset as i64).rem_euclid(86_400) as u64;
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, (s / 60) % 60, s % 60, ms % 1000)
}

/// `RUSTUDIO_LOG_TEE=FILE`: every Output line is also appended to FILE as it
/// is logged (the Output keeps only its newest lines; per-frame traces such
/// as checks/humanoid-physics/feel need them all).
pub(crate) fn tee_log(text: &str) {
    use std::io::Write;
    static TEE: std::sync::OnceLock<Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>> = std::sync::OnceLock::new();
    let tee = TEE.get_or_init(|| {
        let path = std::env::var_os("RUSTUDIO_LOG_TEE")?;
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(std::sync::Mutex::new(std::io::BufWriter::new(f)))
    });
    if let Some(f) = tee {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{text}");
            // Whole lines reach the file even if the run is killed.
            let _ = f.flush();
        }
    }
}

impl Editor {
    pub fn push_log(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        if level != Level::Info {
            eprintln!("[{level:?}] {text}");
        }
        tee_log(&text);
        self.log.push(LogLine {
            at: timestamp(),
            level,
            text,
        });
        // Studio Settings > Output > Maximum Output Lines (5000: past it the
        // oldest fifth goes).
        let max = crate::studio_settings::studio("Maximum Output Lines", 5000.0).clamp(10.0, 1e7) as usize;
        if self.log.len() > max {
            let n = self.log.len() - max - 1 + max / 5;
            self.log.drain(0..n);
            self.output_view.log_drained(n);
        }
    }
    pub fn info(&mut self, t: impl Into<String>) {
        self.push_log(Level::Info, t)
    }

    /// The plugin store's frame: startup loads, folder watching, Roblox jobs;
    /// its lines go to Output.
    pub fn plugins_tick(&mut self) {
        let client = self.account.shared_client();
        let host_on = self.plugins.enabled();
        if let Some(b) = self.plugin_store.runtime.bridge() {
            b.host_enabled = host_on;
        }
        self.plugin_store.tick(client, self.frame_no);
        // The store's loads / unloads / reloads, applied to the host.
        let cmds = self.plugin_store.runtime.bridge().map(|b| std::mem::take(&mut b.cmds)).unwrap_or_default();
        for cmd in cmds {
            use crate::plugin_store::runtime::{host_source, RtCmd};
            match cmd {
                RtCmd::Load(src) | RtCmd::Reload(src) => {
                    let known = self.plugin_store.runtime.bridge().and_then(|b| b.ids.get(&src.id).copied());
                    let host_path = |h: &crate::plugin_host::PluginHost, pid| {
                        h.loaded().into_iter().find(|i| i.id == pid).and_then(|i| i.source.path().map(|p| p.to_path_buf()))
                    };
                    let r = match known {
                        // Same file (an edit): the host reads it again. (A
                        // subfolder .lua is an in-memory tree: no path, so
                        // it is loaded anew below.)
                        Some(pid) if host_path(&self.plugins, pid).as_deref() == Some(src.path.as_path()) => {
                            self.plugins.reload(pid);
                            Ok(pid)
                        }
                        // Another file (an update) or an in-memory tree: unload, load anew.
                        Some(pid) => {
                            self.plugins.unload(pid);
                            host_source(&src).and_then(|h| self.plugins.load(h))
                        }
                        None => host_source(&src).and_then(|h| self.plugins.load(h)),
                    };
                    let b = self.plugin_store.runtime.bridge().unwrap();
                    match r {
                        Ok(pid) => {
                            b.ids.insert(src.id.clone(), pid);
                        }
                        Err(e) => {
                            b.ids.remove(&src.id);
                            b.loaded.retain(|l| *l != src.id);
                            self.push_log(Level::Error, format!("Plugin {} failed to load: {e}", src.id));
                        }
                    }
                }
                RtCmd::Unload(id) => {
                    if let Some(pid) = self.plugin_store.runtime.bridge().and_then(|b| b.ids.remove(&id)) {
                        self.plugins.unload(pid);
                    }
                }
            }
        }
        for l in std::mem::take(&mut self.plugin_store.lines) {
            let level = match l.level {
                crate::plugin_store::watcher::Level::Info => Level::Info,
                crate::plugin_store::watcher::Level::Warn => Level::Warn,
                crate::plugin_store::watcher::Level::Error => Level::Error,
            };
            self.push_log(level, l.text);
        }
    }

    /// Manage Plugins, permission prompts, Publish as Plugin.
    pub fn plugins_ui(&mut self, ctx: &egui::Context) {
        let client = self.account.shared_client();
        crate::plugin_store::ui::show(ctx, &mut self.plugin_store, client);
        // Plugin menus (ShowAsync), Customize Shortcuts, bound plugin keys.
        self.plugin_popups(ctx);
        self.customize_shortcuts_window(ctx);
        self.plugin_shortcut_keys(ctx);
        // Dialogs plugins opened (src/plugin_host/prompts.rs).
        crate::plugin_host::prompts::ui(ctx, &self.doc);
    }
    pub fn warn(&mut self, t: impl Into<String>) {
        self.push_log(Level::Warn, t)
    }
    pub fn error(&mut self, t: impl Into<String>) {
        self.push_log(Level::Error, t)
    }
}

// ------------------------------------------------------------------ actions

impl Editor {
    pub fn set_document(&mut self, doc: Document) {
        self.reset_cloud_save_dialogs();
        // Another document closes the open project (open_project sets it again).
        self.project = None;
        crate::project_config::set_active(None);
        self.document_tabs_reset = true;
        self.ui.insert_popup = None;
        self.last_selection.clear();
        self.marquee_base.clear();
        self.ui.dock_moves.clear();
        self.ui.panel_requests.retain(|(tab, _)| !matches!(tab, Tab::Script(_)));
        self.doc = doc;
        self.scene = Scene::default();
        self.drag = None;
        self.hover = None;
        self.open_script.clear();
        self.scripts = Default::default();
        self.explorer = ExplorerState::default();
        self.explorer.expand_defaults(&self.doc);
        if !self.use_saved_camera() {
            self.frame_all();
        }
        // Plugins run in the place that's open: the
        // old place's go, the new place gets them all.
        self.plugin_store.on_place_opened();
    }

    /// Starts at the place's saved camera (Workspace.CurrentCamera), as
    /// Studio does. False when the place has none.
    fn use_saved_camera(&mut self) -> bool {
        let Some(ws) = self.doc.workspace() else {
            return false;
        };
        let cam = match self.doc.prop(ws, "CurrentCamera") {
            Some(Variant::Ref(r)) if self.doc.exists(r) => Some(r),
            _ => self.doc.find_child(ws, "Camera"),
        };
        let Some(xf) = cam.and_then(|c| crate::scene::xform_of(&self.doc, c)) else {
            return false;
        };
        if !xf.pos.is_finite() || xf.pos.length() < 1e-3 {
            return false;
        }
        let look = -xf.rot.z_axis;
        let focus = cam.and_then(|c| match self.doc.prop(c, "Focus") {
            Some(Variant::CFrame(f)) => Some(crate::math::v3(f.position)),
            _ => None,
        });
        self.cam = Camera {
            mode: self.cam.mode,
            ..Camera::default()
        };
        self.cam.pos = xf.pos;
        self.cam.pitch = look.y.clamp(-1.0, 1.0).asin();
        self.cam.yaw = (-look.x).atan2(-look.z);
        self.cam.focus = focus
            .filter(|f| (*f - xf.pos).length() > 0.5)
            .unwrap_or(xf.pos + look * 20.0);
        true
    }

    pub fn open_path(&mut self, path: PathBuf) {
        // File > Recent remembers what a person opens (not test runs).
        let harness = self.harness.screenshot.is_some()
            || self.harness.selftest.is_some()
            || self.harness.bench.is_some()
            || self.harness.ui_shots.is_some();
        if !harness && path.exists() {
            crate::studio_ui::remember_recent(&mut self.ui, &path);
        }
        // A copy downloaded from Roblox (places/<id>/origin.json beside it):
        // assets are asked for in that place's context, as when opened from
        // Roblox. RUSTUDIO_PLACE_CONTEXT (harness) wins.
        if std::env::var_os("RUSTUDIO_PLACE_CONTEXT").is_none() {
            let origin = path.parent().map(|d| d.join("origin.json"));
            let place = origin
                .and_then(|o| std::fs::read_to_string(o).ok())
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("place_id")?.as_u64())
                .unwrap_or(0);
            rustudio_account::session::set_place_context(place);
        }
        // A project folder, or a project file itself.
        if path.is_dir() {
            return self.open_folder(path, false);
        }
        if crate::project::name_of(&path).ends_with(".project.json") {
            return self.open_project(&path);
        }
        match Document::open(&path) {
            Ok(d) => {
                let n = d.dom.descendants().count();
                crate::load_trace::mark("document built");
                self.set_document(d);
                crate::load_trace::mark(&format!("opened ({n} instances)"));
                self.info(format!("Opened {} ({n} instances)", path.display()));
            }
            Err(e) => self.error(format!("Could not open {}: {e}", path.display())),
        }
    }

    /// A project folder: its project file (asks which one when there are
    /// several and `ask`, else default.project.json or the first).
    pub fn open_folder(&mut self, dir: PathBuf, ask: bool) {
        let files = crate::project::project_files(&dir);
        match files.len() {
            0 => self.error(format!("{} has no *.project.json", dir.display())),
            1 => self.open_project(&files[0].clone()),
            _ if ask => self.project_choice = Some((dir, files)),
            _ => self.open_project(&files[0].clone()),
        }
    }

    pub fn open_project(&mut self, file: &std::path::Path) {
        match crate::project_sync::open(file) {
            Ok((doc, mut st)) => {
                let n = doc.dom.descendants().count();
                let warnings = std::mem::take(&mut st.warnings);
                let place = st
                    .place
                    .as_ref()
                    .map(|p| format!(", place {}", p.display()))
                    .unwrap_or_else(|| ", no place file (an empty DataModel under it)".into());
                let synced = st.map.len();
                self.set_document(doc);
                crate::project_config::set_active(Some(st.config.clone()));
                self.project = Some(st);
                self.info(format!(
                    "Opened project \"{}\" from {} ({n} instances, {synced} from files{place})",
                    self.project.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
                    file.display()
                ));
                for w in warnings.iter().take(20) {
                    self.warn(format!("Project: {w}"));
                }
                if warnings.len() > 20 {
                    self.warn(format!("Project: {} more warnings", warnings.len() - 20));
                }
            }
            Err(e) => self.error(format!("Could not open project {}: {e}", file.display())),
        }
    }

    /// Once a frame: edits to synced instances go to their files, and files
    /// changed on disk come into the place (in edit mode; during Play they
    /// wait until Play stops).
    pub fn project_tick(&mut self) {
        let Some(mut st) = self.project.take() else { return };
        let playing = self.play.is_some();
        if !playing && self.doc.id == st.doc_id {
            st.write_back(&mut self.doc);
        }
        if let Some(changed) = st.poll_disk() {
            if playing {
                st.reapply_after_play = true;
            } else if self.doc.id == st.doc_id {
                let n = st.reconcile(&mut self.doc);
                if n > 0 {
                    st.sourcemap_dirty = true;
                    let mut names: Vec<String> = changed
                        .iter()
                        .filter_map(|p| p.strip_prefix(&st.root).ok().map(|x| x.display().to_string()))
                        .collect();
                    names.sort();
                    names.truncate(4);
                    self.info(format!("Project: {n} instance(s) updated from disk ({})", names.join(", ")));
                }
            }
        }
        if !playing && st.reapply_after_play && self.doc.id == st.doc_id {
            st.reapply_after_play = false;
            let n = st.reconcile(&mut self.doc);
            self.info(format!("Project: {n} instance(s) updated from files changed during Play"));
        }
        if !st.log.is_empty() {
            st.sourcemap_dirty = true;
        }
        if st.sourcemap_dirty && !playing && self.doc.id == st.doc_id {
            st.update_sourcemap(&self.doc);
        }
        for l in std::mem::take(&mut st.log) {
            self.info(format!("Project: {l}"));
        }
        for w in std::mem::take(&mut st.warnings) {
            self.warn(format!("Project: {w}"));
        }
        self.project = Some(st);
    }

    /// Ctrl+S with a project: synced instances are already in their files;
    /// the place file is written only when something outside the mapped
    /// paths changed (so an untouched project saves nothing).
    pub(crate) fn save_project(&mut self) {
        let Some(mut st) = self.project.take() else { return };
        if self.play.is_none() {
            st.write_back(&mut self.doc);
        }
        let dirty = st.place_dirty;
        let place = st.place.clone();
        self.project = Some(st);
        match (dirty, place) {
            (false, _) => self.info("Saved: every synced file is up to date; nothing outside them changed"),
            (true, Some(p)) => {
                self.save_to(p);
                if let Some(st) = self.project.as_mut() {
                    st.place_dirty = false;
                }
            }
            (true, None) => self.save_as(),
        }
    }

    pub fn open_folder_dialog(&mut self) {
        use crate::file_dialog::{Kind, Purpose};
        self.dialogs.start(Purpose::OpenFolder, rfd::AsyncFileDialog::new(), Kind::PickFolder);
    }

    /// A file dialog closed: act on its answer.
    pub fn poll_file_dialogs(&mut self) {
        use crate::file_dialog::Purpose;
        let Some((purpose, path)) = self.dialogs.poll() else {
            return;
        };
        let Some(path) = path else {
            return;
        };
        match purpose {
            Purpose::OpenPlace => self.open_path(path),
            Purpose::OpenFolder => self.open_folder(path, true),
            Purpose::SaveAs => self.save_to(path),
            Purpose::InsertModel => self.insert_model_file(path),
            Purpose::SaveSelection(sel) => match self.doc.save_selection_to(&sel, &path) {
                Ok(()) => self.info(format!("Saved selection to {}", path.display())),
                Err(e) => self.error(format!("Save failed: {e}")),
            },
            Purpose::SaveLocalPlugin(sel) => {
                self.doc.selection = sel;
                self.save_local_plugin_to(path);
            }
            Purpose::TerrainImage { height } => self.load_import_image(path, height),
            Purpose::ProjectParent => {
                if let Some(d) = self.project_export.as_mut() {
                    d.parent = path.display().to_string();
                }
            }
        }
    }

    /// File > New / Ctrl+N: Studio's Baseplate template as Studio opens it
    /// (start_page/templates.rs), its editor camera at the template's saved
    /// Camera.
    pub fn new_place(&mut self) {
        self.set_document(crate::start_page::templates::baseplate());
        self.info("New place created");
    }

    pub fn open_dialog(&mut self) {
        use crate::file_dialog::{Kind, Purpose};
        let d = rfd::AsyncFileDialog::new().add_filter("Roblox place or model", &["rbxl", "rbxlx", "rbxm", "rbxmx"]);
        self.dialogs.start(Purpose::OpenPlace, d, Kind::PickFile);
    }

    pub fn save(&mut self) {
        if self.project.is_some() {
            return self.save_project();
        }
        // A local copy of a Team Create place saves to a file.
        if self.doc.cloud.as_ref().is_some_and(|o| o.local_copy) {
            return match self.doc.path.clone() {
                Some(p) => self.save_to(p),
                None => self.save_as(),
            };
        }
        // A place opened from Roblox: Ctrl+S is Save to Roblox, as in Studio.
        if self.doc.cloud.is_some() || self.live_team_create_ids().is_some() {
            return self.save_cloud(rustudio_account::VersionType::Saved, false);
        }
        match self.doc.path.clone() {
            Some(p) => self.save_to(p),
            None => self.save_as(),
        }
    }

    pub fn save_as(&mut self) {
        let (name, exts): (&str, &[&str]) = match self.doc.kind {
            DocKind::Place => ("Roblox place", &["rbxl", "rbxlx"]),
            DocKind::Model => ("Roblox model", &["rbxm", "rbxmx"]),
        };
        let default = match self.doc.kind {
            DocKind::Place => "place.rbxl",
            DocKind::Model => "model.rbxm",
        };
        use crate::file_dialog::{Kind, Purpose};
        let d = rfd::AsyncFileDialog::new().add_filter(name, exts).set_file_name(default);
        self.dialogs.start(Purpose::SaveAs, d, Kind::SaveFile);
    }

    pub fn save_to(&mut self, p: PathBuf) {
        match self.doc.save_to(&p) {
            Ok(()) => self.info(format!("Saved {}", p.display())),
            Err(e) => self.error(format!("Save failed: {e}")),
        }
    }

    pub fn insert_from_file(&mut self) {
        use crate::file_dialog::{Kind, Purpose};
        let d = rfd::AsyncFileDialog::new().add_filter("Roblox model", &["rbxm", "rbxmx"]);
        self.dialogs.start(Purpose::InsertModel, d, Kind::PickFile);
    }

    fn insert_model_file(&mut self, path: PathBuf) {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let parent = self.paste_target(false);
                let refs = self.doc.paste(&bytes, parent, "Insert from file");
                self.info(format!(
                    "Inserted {} object(s) from {}",
                    refs.len(),
                    path.display()
                ));
                self.select(refs);
            }
            Err(e) => self.error(format!("{}: {e}", path.display())),
        }
    }

    pub fn save_selection(&mut self) {
        if self.doc.selection.is_empty() {
            self.warn("Select something to save as a model");
            return;
        }
        use crate::file_dialog::{Kind, Purpose};
        let d = rfd::AsyncFileDialog::new().add_filter("Roblox model", &["rbxm", "rbxmx"]).set_file_name("model.rbxm");
        self.dialogs.start(Purpose::SaveSelection(self.doc.selection.clone()), d, Kind::SaveFile);
    }

    pub fn select(&mut self, refs: Vec<Ref>) {
        self.doc.selection = refs;
    }

    /// Whenever the selection changes (viewport click, undo/redo, paste,
    /// Explorer…), expand the ancestors of every selected object and scroll
    /// the newest one into view.
    fn sync_explorer_to_selection(&mut self) {
        if self.doc.selection == self.last_selection {
            return;
        }
        if std::env::var_os("RUSTUDIO_PICK_TRACE").is_some() {
            eprintln!("[pick selection] {:?}", self.doc.selection.iter().map(|r| self.doc.dom.get_by_ref(*r).map(|i| (&i.name, &i.class))).collect::<Vec<_>>());
        }
        let added: Vec<Ref> = self
            .doc
            .selection
            .iter()
            .copied()
            .filter(|r| !self.last_selection.contains(r))
            .collect();
        for &r in &self.doc.selection {
            self.explorer.expand_ancestors(&self.doc, r);
        }
        if let Some(&r) = added.last().or(self.doc.selection.last()) {
            self.explorer.scroll_to = Some(r);
        }
        self.last_selection = self.doc.selection.clone();
    }

    pub fn undo(&mut self) {
        match self.doc.undo() {
            Some(l) => self.info(format!("Undo: {l}")),
            None => self.warn("Nothing to undo"),
        }
    }

    pub fn redo(&mut self) {
        match self.doc.redo() {
            Some(l) => self.info(format!("Redo: {l}")),
            None => self.warn("Nothing to redo"),
        }
    }

    pub fn copy(&mut self) -> bool {
        match self.doc.copy(&self.doc.selection.clone()) {
            Some(bytes) => {
                self.clipboard = Some(bytes);
                true
            }
            None => false,
        }
    }

    pub fn cut(&mut self) {
        if self.copy() {
            let sel = self.doc.selection.clone();
            self.doc.remove(&sel);
        }
    }

    /// Paste goes next to the selection (or into it with `into`), else Workspace.
    fn paste_target(&self, into: bool) -> Ref {
        match self.doc.selection.first() {
            Some(&r) if into => r,
            Some(&r) => self
                .doc
                .dom
                .get_by_ref(r)
                .map(|i| i.parent())
                .filter(|p| p.is_some())
                .unwrap_or(self.doc.default_parent()),
            None => self.doc.default_parent(),
        }
    }

    pub fn paste(&mut self, into: bool) {
        let Some(bytes) = self.clipboard.clone() else {
            self.warn("Clipboard is empty");
            return;
        };
        let parent = self.paste_target(into);
        let refs = self.doc.paste(&bytes, parent, "Paste");
        self.select(refs);
    }

    pub fn duplicate(&mut self) {
        let sel = self.doc.selection.clone();
        // Commit the motion so far before Duplicate's own undo step. Keep the
        // pointer geometry, but transfer the active targets to the copies.
        let free = self.part_drag.clone();
        let handle = self.drag.clone();
        self.finish_part_drag();
        self.finish_drag();
        let roots: Vec<_> = self.doc.top_level(&sel).into_iter()
            .filter(|r| self.doc.can_edit_structure(*r)).collect();
        let refs = self.doc.duplicate(&sel);
        self.part_drag = free;
        self.drag = handle;
        if !refs.is_empty() {
            let mut map = std::collections::HashMap::new();
            let mut pending: Vec<_> = roots.into_iter().zip(refs.iter().copied()).collect();
            while let Some((old, new)) = pending.pop() {
                map.insert(old, new);
                if let (Some(a), Some(b)) = (self.doc.dom.get_by_ref(old), self.doc.dom.get_by_ref(new)) {
                    pending.extend(a.children().iter().copied().zip(b.children().iter().copied()));
                }
            }
            self.duplicate_part_drag(&map);
            self.duplicate_handle_drag(&map);
            self.select(refs);
        }
    }

    pub fn delete(&mut self) {
        let sel = self.doc.selection.clone();
        let n = self.doc.remove(&sel);
        if n == 0 && !sel.is_empty() {
            self.warn("Services and the root cannot be deleted");
        }
    }

    pub fn group(&mut self) {
        let sel = self.doc.selection.clone();
        if let Some(m) = self.doc.group(&sel) {
            self.select(vec![m]);
        }
    }

    pub fn ungroup(&mut self) {
        let sel = self.doc.selection.clone();
        let refs = self.doc.ungroup(&sel);
        if !refs.is_empty() {
            self.select(refs);
        }
    }

    /// Inserts a new part in front of the camera, on top of whatever is there.
    pub fn insert_part(&mut self, mesh: MeshKind) {
        let (class, shape) = match mesh {
            MeshKind::Block => ("Part", 1),
            MeshKind::Ball => ("Part", 0),
            MeshKind::Cylinder => ("Part", 2),
            MeshKind::Wedge => ("WedgePart", 1),
            MeshKind::CornerWedge => ("CornerWedgePart", 1),
        };
        let mut size = Vec3::from(mech::NEW_PART_SIZE);
        if mesh == MeshKind::Ball {
            size = Vec3::splat(2.0);
        } else if mesh == MeshKind::Cylinder {
            size = Vec3::new(2.0, 2.0, 2.0);
        }
        let pos = self.spawn_point(size);
        let mut b = doc::new_part(
            if class == "Part" { "Part" } else { class },
            shape,
            pos,
            size,
            mech::NEW_PART_COLOR,
        );
        if class != "Part" {
            b = b.with_class(class);
        }
        let parent = self.doc.default_parent();
        let r = self.doc.insert(parent, b);
        self.select(vec![r]);
    }

    /// Where a new part lands: the point under the screen centre, snapped.
    pub(crate) fn spawn_point(&mut self, size: Vec3) -> Vec3 {
        self.scene.sync(&mut self.doc);
        let rect = if self.viewport_rect.is_positive() {
            self.viewport_rect
        } else {
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))
        };
        let ray = self.cam.ray(rect, rect.center());
        let mut best: Option<f32> = None;
        for p in &self.scene.parts {
            if let Some(t) = ray.hit_obb(&p.xf, p.size) {
                if best.is_none_or(|b| t < b) {
                    best = Some(t);
                }
            }
        }
        let hit = match best {
            Some(t) if t < 500.0 => ray.at(t),
            _ => ray
                .hit_plane(Vec3::ZERO, Vec3::Y)
                .filter(|t| *t < 500.0)
                .map(|t| ray.at(t))
                .unwrap_or(self.cam.pos + self.cam.forward() * 20.0),
        };
        let snap = |v: f32| crate::math::snap(v, if self.snap_move { self.move_inc } else { 0.0 });
        Vec3::new(snap(hit.x), hit.y + size.y / 2.0, snap(hit.z))
    }

    pub fn insert_class(&mut self, class: &str, parent: Ref) {
        if class == "Part" {
            self.insert_part(MeshKind::Block);
            if parent != self.doc.default_parent() {
                let sel = self.doc.selection.clone();
                self.doc.reparent(&sel, parent);
            }
            return;
        }
        let mut b = InstanceBuilder::new(class).with_name(class);
        if doc::is_a(class, "BasePart") {
            let size = Vec3::from(mech::NEW_PART_SIZE);
            let pos = self.spawn_point(size);
            b = b
                .with_property("Anchored", true)
                .with_property(
                    "Size",
                    rbx_dom_weak::types::Vector3::new(size.x, size.y, size.z),
                )
                .with_property(
                    "CFrame",
                    rbx_dom_weak::types::CFrame::new(
                        crate::math::rv3(pos),
                        rbx_dom_weak::types::Matrix3::identity(),
                    ),
                );
        }
        if matches!(class, "Script" | "LocalScript" | "ModuleScript") {
            let src = if class == "ModuleScript" {
                "local module = {}\n\nreturn module\n"
            } else {
                "print(\"Hello world!\")\n"
            };
            b = b.with_property("Source", src.to_string());
        }
        let r = self.doc.insert(parent, b);
        self.select(vec![r]);
    }

    pub fn selected_parts(&self) -> Vec<Ref> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for &r in &self.doc.selection {
            if self.scene.get(r).is_some() && seen.insert(r) {
                out.push(r);
            }
            for d in self.doc.dom.descendants_of(r) {
                let dr = d.referent();
                if dr != r && self.scene.get(dr).is_some() && seen.insert(dr) {
                    out.push(dr);
                }
            }
        }
        out
    }

    /// Bounding box (min, max) of the selected parts.
    pub fn selection_bounds(&self) -> Option<(Vec3, Vec3)> {
        let parts = self.selected_parts();
        let mut it = parts.iter().filter_map(|r| self.scene.get(*r));
        let first = it.next()?;
        let mut b = crate::viewport::part_aabb(first);
        for p in it {
            let (mn, mx) = crate::viewport::part_aabb(p);
            b = (b.0.min(mn), b.1.max(mx));
        }
        Some(b)
    }

    pub fn focus_selection(&mut self) {
        self.scene.sync(&mut self.doc);
        match self.selection_bounds() {
            Some((mn, mx)) => self.cam.frame((mn + mx) * 0.5, (mx - mn).length() * 0.5),
            None => self.warn("Select a part to focus on"),
        }
    }

    /// Frames everything except huge baseplates.
    /// Frames where most of the parts are: the 10th–90th percentile box of
    /// part centres, so a few stray far-away parts don't throw it off.
    pub fn frame_all(&mut self) {
        self.scene.sync(&mut self.doc);
        let centers: Vec<Vec3> = self
            .scene
            .parts
            .iter()
            .filter(|p| p.size.max_element() <= 200.0 && p.xf.pos.is_finite())
            .map(|p| p.xf.pos)
            .collect();
        self.cam = Camera {
            mode: self.cam.mode,
            ..Camera::default()
        };
        if centers.is_empty() {
            return;
        }
        let quantile = |axis: usize, q: f32| {
            let mut v: Vec<f32> = centers.iter().map(|c| c[axis]).collect();
            v.sort_by(f32::total_cmp);
            v[((v.len() - 1) as f32 * q).round() as usize]
        };
        let lo = Vec3::new(quantile(0, 0.1), quantile(1, 0.1), quantile(2, 0.1));
        let hi = Vec3::new(quantile(0, 0.9), quantile(1, 0.9), quantile(2, 0.9));
        self.cam.frame(
            (lo + hi) * 0.5,
            ((hi - lo).length() * 0.5).clamp(8.0, 2000.0),
        );
    }

    /// The sample scene used for screenshots and `--demo`.
    pub fn build_demo(&mut self) {
        let last = build_demo_parts(&mut self.doc);
        self.doc.selection = vec![last];
        self.tool = Tool::Move;
        self.cam = Camera::default();
        self.cam.pos = Vec3::new(14.0, 22.0, 34.0);
        self.cam.yaw = 0.32;
        self.cam.pitch = -0.5;
        self.cam.focus = self.cam.pos + self.cam.forward() * 40.0;
        // Demo edits should not show up as undoable history.
        self.doc.clear_history();
        self.info("Loaded the sample scene");
    }
}

// ----------------------------------------------------------------- shortcuts

impl Editor {
    fn shortcuts(&mut self, ctx: &egui::Context) {
        // Test controls work even while typing in a script.
        let (stop, play, run) = ctx.input_mut(|i| {
            let stop = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::SHIFT, Key::F5));
            let play = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::F5));
            let run = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::F8));
            (stop, play, run)
        });
        if stop {
            self.stop_play();
        } else if play {
            self.start_run(crate::studio_ui::RunType::Test);
        } else if run {
            self.start_run(crate::studio_ui::RunType::Run);
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        // While playing, WASD/Space/etc. belong to the game.
        self.game_keys(ctx);
        // Studio's menu accelerators (Ctrl+Shift+V, Ctrl+Alt+G, Alt+A, F11…).
        for a in self.menu_shortcut_actions(ctx) {
            self.ui_action(ctx, a);
        }
        for a in std::mem::take(&mut self.harness.ui_due) {
            self.ui_action(ctx, &a);
        }
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let mut actions: Vec<&str> = Vec::new();
        ctx.input_mut(|i| {
            let mut check = |s: KeyboardShortcut, name: &'static str| {
                if i.consume_shortcut(&s) {
                    actions.push(name);
                }
            };
            check(sc(cs, Key::P), "focus_props_filter");
            check(sc(cs, Key::Z), "redo");
            check(sc(cs, Key::S), "save_as");
            check(sc(cs, Key::V), "paste_into");
            check(sc(Modifiers::COMMAND, Key::Z), "undo");
            check(sc(Modifiers::COMMAND, Key::Y), "redo");
            check(sc(Modifiers::COMMAND, Key::D), "duplicate");
            check(sc(Modifiers::COMMAND, Key::G), "group");
            check(sc(Modifiers::COMMAND, Key::U), "ungroup");
            check(sc(Modifiers::COMMAND, Key::S), "save");
            check(sc(Modifiers::COMMAND, Key::O), "open");
            check(sc(Modifiers::COMMAND, Key::N), "new");
            check(sc(Modifiers::COMMAND, Key::I), "insert_object");
            check(sc(Modifiers::COMMAND, Key::L), "toggle_space");
            check(sc(Modifiers::COMMAND, Key::R), "rotate_selection");
            check(sc(Modifiers::COMMAND, Key::T), "tilt_selection");
            check(sc(Modifiers::NONE, Key::Num1), "tool_select");
            check(sc(Modifiers::NONE, Key::Num2), "tool_move");
            check(sc(Modifiers::NONE, Key::Num3), "tool_scale");
            check(sc(Modifiers::NONE, Key::Num4), "tool_rotate");
            check(sc(Modifiers::COMMAND, Key::Num1), "tool_select");
            check(sc(Modifiers::COMMAND, Key::Num2), "tool_move");
            check(sc(Modifiers::COMMAND, Key::Num3), "tool_scale");
            check(sc(Modifiers::COMMAND, Key::Num4), "tool_rotate");
            check(sc(Modifiers::NONE, Key::Delete), "delete");
            check(sc(Modifiers::NONE, Key::F2), "rename");
            check(sc(Modifiers::NONE, Key::F), "focus");
            // egui turns Ctrl+C/X/V into clipboard events.
            for ev in &i.events {
                match ev {
                    egui::Event::Copy => actions.push("copy"),
                    egui::Event::Cut => actions.push("cut"),
                    // egui-winit sends Ctrl+V and Ctrl+Shift+V as Paste (only
                    // when the system clipboard holds text: copy puts a note
                    // there); Shift held means Paste Into.
                    egui::Event::Paste(_) if i.modifiers.shift => actions.push("paste_into"),
                    egui::Event::Paste(_) => actions.push("paste"),
                    _ => {}
                }
            }
        });
        if actions.contains(&"focus_props_filter") {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new(crate::properties::PROPS_FILTER_ID)));
        }
        let copied = actions.iter().any(|a| matches!(*a, "copy" | "cut"));
        for a in actions {
            self.run_action(a);
        }
        if copied {
            self.note_os_clipboard(ctx);
        }
    }

    /// While the client of a Play session is shown: this frame's key presses
    /// go to the game (UserInputService, the hotbar, the in-game menu) and
    /// are taken out of the input so no editor shortcut sees them. Shared by
    /// the editor and the no-UI mode (`--no-ui`), where Escape also reaches
    /// the game's scripts (gameProcessedEvent true) as well as opening the
    /// menu, and no editor shortcut (F5, Shift+F5, F8) comes first.
    pub(crate) fn game_keys(&mut self, ctx: &egui::Context) {
        let no_ui = self.harness.no_ui;
        if self.play_game_input() {
            let mut quality_delta: i8 = 0;
            let esc = ctx.input(|i| {
                let mut esc = false;
                for e in &i.events {
                    match e {
                        egui::Event::Key {
                            key: Key::Escape,
                            pressed: true,
                            repeat: false,
                            ..
                        } => esc = true,
                        egui::Event::Key {
                            key: Key::F10,
                            pressed: true,
                            repeat: false,
                            modifiers,
                            ..
                        } if !modifiers.ctrl && !modifiers.alt && !modifiers.command => {
                            quality_delta = if modifiers.shift { -1 } else { 1 };
                        }
                        _ => {}
                    }
                }
                esc
            });
            if quality_delta != 0 {
                crate::game_settings::step_graphics_quality(quality_delta);
            }
            if esc {
                self.toggle_game_menu();
            }
            // Presses this frame: a tap pressed and released within one frame
            // never shows in keys_down, yet scripts must see it. The key
            // positions are physical, like Roblox's KeyCodes (src/keys.rs).
            // (Escape belongs to the Roblox menu and F10 to graphics quality:
            // games never hear them.)
            let taps: Vec<Key> = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Key {
                            key,
                            physical_key,
                            pressed: true,
                            repeat: false,
                            modifiers,
                            ..
                        } if physical_key.unwrap_or(*key) != Key::Escape
                            && !(physical_key.unwrap_or(*key) == Key::F10
                                && !modifiers.ctrl
                                && !modifiers.alt
                                && !modifiers.command) =>
                        {
                            Some(physical_key.unwrap_or(*key))
                        }
                        _ => None,
                    })
                    .collect()
            });
            // The open menu takes the keys (R, L); the game hears none.
            if !self.game_menu_keys(&taps) {
                // (A focused TextBox has them: game-processed, Enter included.)
                let processed = self.gui.focus.is_some();
                if let Some(s) = self.play.as_mut() {
                    if !processed {
                        s.game.hotbar_taps.extend(taps.iter().copied());
                    }
                    s.game.key_taps.extend(taps.into_iter().map(|k| (k, processed)));
                }
            }
            // No UI: the menu has Escape first, and UserInputService hears it
            // too, as processed (play_tick lets it past the open menu).
            if esc && no_ui {
                if let Some(s) = self.play.as_mut() {
                    s.game.key_taps.push((Key::Escape, true));
                }
            }
            ctx.input_mut(|i| {
                i.events.retain(|e| {
                    !matches!(
                        e,
                        egui::Event::Key { key, modifiers, .. }
                            if modifiers.is_none()
                                || (*key == Key::F10 && !modifiers.ctrl && !modifiers.alt && !modifiers.command)
                    )
                });
            });
        }
    }

    /// Puts a line on the system clipboard after a copy: egui-winit only
    /// delivers Ctrl+V when the system clipboard holds text.
    pub fn note_os_clipboard(&self, ctx: &egui::Context) {
        if self.terrain_ed.tool.is_some_and(crate::terrain_editor::Tool::is_region) {
            if self.terrain_ed.clipboard.is_some() && self.harness.selftest.is_none() {
                ctx.copy_text("rustudio: terrain region copied".to_string());
            }
            return;
        }
        if self.clipboard.is_some() && self.harness.selftest.is_none() {
            ctx.copy_text(format!("rustudio: {} object(s) copied", self.doc.selection.len()));
        }
    }

    pub fn run_action(&mut self, a: &str) {
        // A Terrain Editor Region tool has Copy, Cut, Paste, Duplicate and Delete.
        if self.terrain_ed.tool.is_some_and(crate::terrain_editor::Tool::is_region)
            && matches!(a, "copy" | "cut" | "paste" | "duplicate" | "delete")
            && self.terrain_region_action(a)
        {
            return;
        }
        match a {
            "undo" => self.undo(),
            "redo" => self.redo(),
            "copy" => {
                self.copy();
            }
            "cut" => self.cut(),
            "paste" => self.paste(false),
            "paste_into" => self.paste(true),
            "duplicate" => self.duplicate(),
            "delete" => self.delete(),
            "group" => self.group(),
            "ungroup" => self.ungroup(),
            "save" => self.save(),
            "save_as" => self.save_as(),
            "open" => self.request(PendingAction::Open(None)),
            "new" => self.request(PendingAction::New),
            "insert_object" => {
                let parent = self.insert_parent();
                let at = self.viewport_rect.center() - egui::vec2(120.0, 200.0);
                self.open_insert_popup(parent, at);
            }
            "toggle_space" => self.local_space = !self.local_space,
            "rotate_selection" => self.rotate_selection(false),
            "tilt_selection" => self.rotate_selection(true),
            "tool_select" => self.tool = self.tool.toggle(Tool::Select),
            "tool_move" => self.tool = self.tool.toggle(Tool::Move),
            "tool_scale" => self.tool = self.tool.toggle(Tool::Scale),
            "tool_rotate" => self.tool = self.tool.toggle(Tool::Rotate),
            "rename" => {
                if let Some(&r) = self.doc.selection.first() {
                    let name = self
                        .doc
                        .dom
                        .get_by_ref(r)
                        .map(|i| i.name.clone())
                        .unwrap_or_default();
                    self.explorer.renaming = Some((r, name));
                }
            }
            "focus" => self.focus_selection(),
            "focus_props_filter" => self.ui.panel_requests.push((Tab::Properties, Some(true))),
            _ => {}
        }
    }

    /// Asks before throwing away unsaved changes.
    pub fn request(&mut self, action: PendingAction) {
        if self.doc.dirty() && self.harness.selftest.is_none() {
            self.confirm_discard = Some(action);
        } else {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: PendingAction) {
        match action {
            PendingAction::New => self.new_place(),
            PendingAction::Open(None) => self.open_dialog(),
            PendingAction::Open(Some(p)) => self.open_path(p),
            PendingAction::OpenFolder => self.open_folder_dialog(),
            PendingAction::OpenCloud(p, v, copy) => self.perform_open_cloud(p, v, copy),
            PendingAction::Close => {
                if self.play.is_some() {
                    self.stop_play();
                }
                self.new_place();
                self.show_start_page();
            }
            PendingAction::Quit => {
                crate::studio_settings::flush();
                crate::rustudio_settings::flush();
                std::process::exit(0)
            }
            PendingAction::Restart => self.restart(),
        }
    }

    /// Starts a new rustudio on what's open now (the live Team Create
    /// session, the file or project folder, or a Roblox place's local copy)
    /// and exits this one.
    fn restart(&mut self) {
        let mut args: Vec<String> = Vec::new();
        if let Some((universe, place)) = self.live_team_create_ids() {
            args = vec!["--team-create".into(), universe.to_string(), place.to_string()];
        } else if let Some(path) = &self.doc.path {
            args.push(path.display().to_string());
        } else if let Some(origin) = &self.doc.cloud {
            args = vec!["--open-roblox".into(), origin.place_id.to_string(), "--local-copy".into()];
        }
        // After an update the running file was renamed: start the new one.
        let exe = crate::update::installed_exe()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("rustudio"));
        match std::process::Command::new(&exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => self.error(format!("Restart failed: couldn't start {}: {e}", exe.display())),
        }
    }
}

// --------------------------------------------------------------- menus & bars

impl Editor {
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        self.hud.status_drawn = true;
        let row = egui::vec2(ui.available_size_before_wrap().x, ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.label(RichText::new(format!("{:.0} fps", self.fps)).color(theme::TEXT_DIM));
            if let Some(mcp) = &self.mcp {
                ui.separator();
                ui.label(RichText::new(format!("MCP :{}", mcp.port)).color(theme::TEXT_DIM))
                    .on_hover_text(format!("OmniMCP bridge listening on 127.0.0.1:{}", mcp.port));
            }
            let left_rect = ui.available_rect_before_wrap();
            if left_rect.width() > 0.0 {
                let mut left = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(left_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                left.set_clip_rect(left_rect.intersect(ui.clip_rect()));
                left.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                let sel = self.doc.selection.len();
                if let Some(s) = self.play_status() {
                    left.label(RichText::new(format!("● {s}")).color(theme::ERROR).strong());
                    left.separator();
                }
                left.label(RichText::new(format!("{} tool", self.tool.label())).color(theme::ACCENT));
                left.separator();
                left.label(format!("{sel} selected"));
                left.separator();
                left.label(format!("{} parts", self.scene.parts.len()));
                left.separator();
                let p = self.cam.pos;
                left.label(format!("Camera {:.1}, {:.1}, {:.1}", p.x, p.y, p.z));
                left.separator();
                left.label(if self.snap_move {
                    format!("Snap {} st / {}°", self.move_inc, self.rotate_inc)
                } else {
                    "Snap off".into()
                });
            }
        });
    }

    /// Opens requested scripts as tabs next to the viewport (or focuses
    /// them, in a panel window too).
    fn open_script_tabs(&mut self, dock: &mut DockState<Tab>, panels: &mut crate::panels::Panels) {
        // Tabs plugins closed, then the open script tabs as
        // ScriptEditorService documents (a tab the person closed closes its
        // document; the front one is StudioService.ActiveScript).
        for r in std::mem::take(&mut self.close_script) {
            let tab = Tab::Script(r);
            if let Some(at) = dock.find_tab(&tab) {
                dock.remove_tab(at);
            }
            for w in panels.popped.iter_mut() {
                if let Some(at) = w.dock.find_tab(&tab) {
                    w.dock.remove_tab(at);
                }
            }
        }
        let mut tabs: Vec<Ref> = dock.iter_all_tabs().filter_map(|(_, t)| match t { Tab::Script(r) => Some(*r), _ => None }).collect();
        for w in panels.popped.iter() {
            tabs.extend(w.dock.iter_all_tabs().filter_map(|(_, t)| match t { Tab::Script(r) => Some(*r), _ => None }));
        }
        tabs.extend(self.open_script.iter().copied());
        crate::plugin_host::script_docs::sync_tabs(&tabs);
        if let Some((_, Tab::Script(r))) = dock.find_active_focused() {
            crate::plugin_host::script_docs::set_active(Some(*r));
        }
        for r in std::mem::take(&mut self.open_script) {
            let tab = Tab::Script(r);
            if let Some(w) = panels.popped.iter_mut().find(|w| w.dock.find_tab(&tab).is_some()) {
                if let Some(at) = w.dock.find_tab(&tab) {
                    let _ = w.dock.set_active_tab(at);
                }
                w.focus = true;
                continue;
            }
            let tree = dock.main_surface_mut();
            if let Some((node, tab)) = tree.find_tab(&tab) {
                let _ = tree.set_active_tab(node, tab);
                tree.set_focused_node(node);
                continue;
            }
            crate::panels::place(dock, tab, &panels.homes);
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm_discard.clone() else {
            return;
        };
        let mut decided: Option<bool> = None;
        egui::Modal::new(egui::Id::new("confirm_discard")).show(ctx, |ui| {
            ui.label(RichText::new("Discard unsaved changes?").strong());
            ui.label(format!(
                "{} has changes that are not saved.",
                self.doc.title
            ));
            ui.horizontal(|ui| {
                if ui.button("Save first").clicked() {
                    self.save();
                    decided = Some(!self.doc.dirty());
                }
                if ui.button("Discard").clicked() {
                    decided = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decided = Some(false);
                }
            });
        });
        if let Some(go) = decided {
            self.confirm_discard = None;
            if go {
                self.perform(action);
            }
        }
    }
}

struct Viewer<'a> {
    ed: &'a mut Editor,
    /// The panel window this dock is in (None: the main window).
    window: Option<&'a str>,
    /// Every panel window: key and title (the tab menu's Move to window).
    windows: &'a [(String, String)],
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Script(r) => self.ed.script_title(*r).into(),
            // Studio's document tab: the place's name.
            Tab::Viewport => match self.ed.play_status() {
                Some(s) => RichText::new(format!("{} — {s}", self.ed.doc.title))
                    .color(theme::ACCENT)
                    .into(),
                None => self.ed.doc.title.clone().into(),
            },
            Tab::Properties => crate::properties::panel_title(self.ed).into(),
            Tab::Plugin(n) => self.ed.plugin_widget_title(*n).into(),
            other => other.title().into(),
        }
    }

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        match tab {
            Tab::Script(r) => egui::Id::new(("script_tab", *r)),
            Tab::Plugin(n) => egui::Id::new(("plugin_widget_tab", *n)),
            other => egui::Id::new(("tab", other.title())),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let _p = crate::bench::scope(match tab {
            Tab::Viewport => "viewport",
            Tab::Explorer => "explorer",
            Tab::Properties => "properties",
            Tab::Output => "output",
            _ => "other_tab",
        });
        match tab {
            Tab::Viewport => self.ed.viewport_ui(ui),
            Tab::Explorer => self.ed.explorer_ui(ui),
            Tab::Properties => self.ed.properties_ui(ui),
            Tab::Output => self.ed.output_ui(ui),
            Tab::AvatarSettings => self.ed.avatar_settings_ui(ui),
            Tab::TerrainEditor => self.ed.terrain_editor_ui(ui),
            Tab::Plugin(n) => self.ed.plugin_widget_ui(ui, *n),
            Tab::Toolbox => self.ed.toolbox_ui(ui),
            Tab::Script(r) => {
                if !self.ed.document_tabs_reset {
                    self.ed.script_tab_ui(ui, *r);
                }
            },
        }
    }

    fn show_pop_out_button(&self, tab: &Tab) -> bool {
        *tab != Tab::Viewport
    }

    fn pop_out_docks_back(&self, _tab: &Tab) -> bool {
        self.window.is_some()
    }

    fn pop_out_tooltip(&self, _tab: &Tab) -> String {
        if self.window.is_some() {
            "Dock back into the main window".into()
        } else {
            "Pop out into its own window".into()
        }
    }

    fn on_pop_out(&mut self, tab: &mut Tab) {
        use crate::panels::DockMove;
        self.ed.ui.dock_moves.push(if self.window.is_some() { DockMove::DockBack(*tab) } else { DockMove::PopOut(*tab) });
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        use crate::panels::DockMove;
        if *tab == Tab::Viewport {
            return;
        }
        let alone = self.window.is_some()
            && self.ed.ui.popped.iter().find(|(k, _)| Some(k.as_str()) == self.window).is_some_and(|(_, t)| t.len() == 1);
        if !alone && ui.button("Pop Out").clicked() {
            self.ed.ui.dock_moves.push(DockMove::PopOut(*tab));
        }
        if self.window.is_some() && ui.button("Dock Back").clicked() {
            self.ed.ui.dock_moves.push(DockMove::DockBack(*tab));
        }
        let others: Vec<&(String, String)> = self.windows.iter().filter(|(k, _)| Some(k.as_str()) != self.window).collect();
        if !others.is_empty() {
            ui.menu_button("Move to Window", |ui| {
                for (k, title) in others {
                    if ui.button(title).clicked() {
                        self.ed.ui.dock_moves.push(DockMove::MoveTo(*tab, k.clone()));
                    }
                }
            });
        }
    }

    fn no_tab_bar(&self, tabs: &[Tab]) -> bool {
        // The 3D view alone: no strip with the place's name over it.
        tabs == [Tab::Viewport]
    }

    fn allowed_in_windows(&self, tab: &mut Tab) -> bool {
        // Dragged out, a tab becomes a panel window; the 3D view stays.
        *tab != Tab::Viewport
    }

    fn is_closeable(&self, tab: &Tab) -> bool {
        // The place tab's × is File > Close Place (not while playing).
        *tab != Tab::Viewport || self.ed.play_status().is_none()
    }

    fn on_close(&mut self, tab: &mut Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        if *tab == Tab::Viewport {
            // Closing the place goes back to the Start Page (asks first
            // when there are unsaved changes); the tab itself stays.
            self.ed.request(PendingAction::Close);
            return egui_dock::tab_viewer::OnCloseResponse::Ignore;
        }
        if let Tab::Plugin(n) = *tab {
            // The plugin hears of it (BindToClose, or Enabled = false) and
            // the panel goes when the widget is disabled.
            self.ed.plugin_widget_close(n);
            return egui_dock::tab_viewer::OnCloseResponse::Ignore;
        }
        egui_dock::tab_viewer::OnCloseResponse::Close
    }

    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    fn clear_background(&self, tab: &Tab) -> bool {
        *tab != Tab::Viewport
    }
}

impl RustudioApp {
    fn reset_document_tabs(&mut self) {
        if std::mem::take(&mut self.ed.document_tabs_reset) {
            crate::panels::close_document_tabs(&mut self.dock, &mut self.panels);
            self.ed.ui.popped.clear();
            self.ed.ui.viewport_group.clear();
        }
    }

    /// Draws each panel window (its own OS window: an immediate viewport),
    /// then does the frame's pop-out / dock-back moves and turns tabs
    /// dragged out of a dock into windows. `dock_rect`: the main dock's
    /// area (a popped tab's window opens over where it was).
    fn panel_windows(&mut self, ctx: &egui::Context, windows: &[(String, String)], dock_rect: egui::Rect) {
        self.panels.remember_windows();
        let mut closed = Vec::new();
        for (i, w) in self.panels.popped.iter_mut().enumerate() {
            let id = w.viewport_id();
            if std::mem::take(&mut w.focus) {
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
            }
            let ed = &mut self.ed;
            // The viewport host skips the callback if it
            // can't make the window this frame (no event loop context, or
            // the platform refused the window: seen on a Windows host's hidden
            // desktop). That's not fatal: skip the window this frame, and
            // after a run of failures dock its tabs back.
            let builder = w.builder();
            let shown = crate::crash::recoverable(std::panic::AssertUnwindSafe(|| ctx.show_viewport_immediate(id, builder, |ui, class| {
                ed.ui.popped_class = Some(class);
                let vctx = ui.ctx().clone();
                w.track(&vctx);
                let key = w.key.clone();
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(ui, |ui| {
                        DockArea::new(&mut w.dock)
                            .id(egui::Id::new(("panel_window_dock", &key)))
                            .style(theme::dock_style(ui.style()))
                            .show_add_buttons(false)
                            .show_leaf_collapse_buttons(false)
                            .show_leaf_close_all_buttons(false)
                            .show_inside(ui, &mut Viewer { ed: &mut *ed, window: Some(&key), windows });
                    });
                vctx.input(|i| i.viewport().close_requested())
            })));
            let close = match shown {
                Ok(c) => {
                    w.fails = 0;
                    c
                }
                Err(_) => {
                    w.fails += 1;
                    eprintln!("rustudio: panel window {} could not be shown ({} frames)", w.key, w.fails);
                    ctx.request_repaint();
                    w.fails >= crate::mech::PANEL_WINDOW_MAX_FAILS
                }
            };
            if close {
                // Closing the window docks its panels back (not lost).
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            self.panels.dock_back_window(&mut self.dock, i);
        }
        // Tabs dragged out of a panel window's dock: windows of their own.
        let zoom = ctx.zoom_factor();
        let mut adopted = Vec::new();
        for w in self.panels.popped.iter_mut() {
            let mut sub = crate::panels::Panels::default();
            let origin = w.pos.map(|p| egui::pos2(p[0] / zoom, p[1] / zoom));
            sub.adopt_floating(&mut w.dock, origin, zoom);
            adopted.extend(sub.popped);
        }
        self.panels.popped.retain(|w| w.dock.iter_all_tabs().next().is_some());
        for mut w in adopted {
            w.key = self.panels.free_key(w.tabs()[0].key());
            self.panels.popped.push(w);
        }
        // Main window: its outer top-left on the screen.
        let origin = ctx.input(|i| {
            let v = i.viewport();
            match (v.outer_rect, v.inner_rect) {
                (Some(_), Some(inner)) => Some(inner.min),
                _ => None,
            }
        });
        self.panels.adopt_floating(&mut self.dock, origin, zoom);
        for mv in std::mem::take(&mut self.ed.ui.dock_moves) {
            // A popped tab opens about where it sat, at its size.
            let tab = match &mv {
                crate::panels::DockMove::PopOut(t) => Some(*t),
                _ => None,
            };
            let rect = tab
                .and_then(|t| self.dock.find_main_surface_tab(&t))
                .and_then(|(n, _)| self.dock.main_surface()[n].get_leaf().map(|l| l.rect))
                .filter(|r| r.is_finite() && r.width() > 50.0 && r.height() > 50.0)
                .unwrap_or_else(|| egui::Rect::from_center_size(dock_rect.center(), egui::vec2(420.0, 320.0)));
            let size = [(rect.width() * zoom).max(320.0), (rect.height() * zoom).max(220.0)];
            let pos = origin.filter(|_| rect.is_finite()).map(|o| [(o.x + rect.min.x) * zoom, (o.y + rect.min.y) * zoom]);
            self.panels.apply(&mut self.dock, mv, size, pos);
        }
    }
}

impl crate::d3d11::WindowApp for RustudioApp {
    fn on_exit(&mut self) {
        self.ed.mcp = None;
        crate::studio_settings::flush();
        crate::rustudio_settings::flush();
        crate::plugin_host::widgets_ui::on_exit();
        if let Some(s) = self.layout.as_mut() {
            s.save(&self.dock, &self.panels, &self.ed);
        }
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        // A scripted run's window sits on the real desktop: the person's (or an
        // idle session's) mouse pointer over it would hover and click like the
        // script (a ribbon tooltip in a screenshot), so the real pointer is
        // dropped before the script's own input goes in.
        if (self.ed.harness.selftest.is_some()
            || self.ed.harness.screenshot.is_some()
            || self.ed.harness.ui_shots.is_some())
            && std::env::var_os("RUSTUDIO_VISIBLE_TESTS").is_none()
        {
            raw.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::MouseMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::PointerGone
                        | egui::Event::MouseWheel { .. }
                        | egui::Event::Touch { .. }
                )
            });
        }
        if let Some(t) = self.ed.harness.selftest.as_mut() {
            t.inject(raw);
        }
        if !self.ed.harness.actions.is_empty() {
            self.ed.harness_inject(raw);
        }
        if let Some(u) = self.ed.harness.ui_shots.as_mut() {
            u.inject(raw);
        }
        self.ed.phys.ingest(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        self.frame(ui);
    }
}

impl RustudioApp {
    /// One frame, in the D3D11 window host or offscreen (src/offscreen.rs).
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        // (the whole frame, for --bench: the editor's and --no-ui's alike)
        let _frame_scope = crate::bench::scope("frame");
        crate::sim_clock::advance();
        let ctx = ui.ctx().clone();
        self.watchdog.begin(ctx.input(|i| !i.viewport().minimized.unwrap_or(false)) && (self.ed.play.is_some() || self.ed.harness.screenshot.is_some()));
        let ed = &mut self.ed;
        ed.poll_file_dialogs();
        if let crate::update::Action::Restart = self.updater.show(&ctx) {
            ed.request(PendingAction::Restart);
        }
        ed.frame_no += 1;
        let frame_start = Instant::now();
        let now = Instant::now();
        let dt = now.duration_since(ed.last_frame).as_secs_f32();
        ed.last_frame = now;
        crate::load_trace::frame(ed.frame_no.saturating_sub(1) as u64, dt as f64 * 1000.0);
        if dt > 0.0 {
            ed.fps = ed.fps * 0.9 + (1.0 / dt) * 0.1;
        }
        ed.hud = HudStats::default();
        for (err, m) in crate::studio_content::take_messages() {
            if err { ed.error(m) } else { ed.info(m) }
        }
        ed.assets.set_client(ed.account.shared_client());
        // --fixed-dt: last frame's downloads land now, not whenever (src/sim_clock.rs).
        if crate::sim_clock::on() {
            ed.assets.wait_pending(std::time::Duration::from_secs(60));
        }
        let done = ed.assets.poll();
        if ed.textures.poll_decoded() {
            ed.asset_gen += 1;
        }
        if crate::load_trace::on() {
            let (pending, loaded) = ed.textures.counts();
            crate::load_trace::assets(ed.meshes.count(), pending, loaded);
        }
        if !done.is_empty() {
            ed.meshes.retry(&done);
            ed.textures.retry(&done);
            ed.asset_gen += 1;
        }
        // Material texture packs load on their own threads.
        let mat_gen = crate::matpack::generation();
        if mat_gen != ed.material_gen {
            ed.material_gen = mat_gen;
            ed.asset_gen += 1;
        }
        let wrap_gen = crate::wrap::generation();
        if wrap_gen != ed.wrap_gen {
            ed.wrap_gen = wrap_gen;
            ed.asset_gen += 1;
        }
        for msg in std::mem::take(&mut ed.assets.messages) {
            ed.info(msg);
        }
        for (level, text) in ed.account.poll() {
            ed.push_log(level, text);
        }
        self.watchdog.stage("asset polling", "Play tick: scripts/physics");
        if ed.play.is_some() {
            let mut step = ed.fixed_dt.unwrap_or_else(|| frame_grid(dt));
            if let Some(f) = ed.fixed_dt.filter(|_| std::env::var_os("RUSTUDIO_GAME_CLOCK").is_some()) {
                // (the scripts' os.clock spinning of the last frame: whole fixed periods)
                step += crate::runtime::take_game_clock_spent(1.0 / f);
            }
            if ed.fixed_dt.is_some() && ed.harness.play_started.is_some() {
                *ed.harness.play_game_time.get_or_insert(0.0) += step;
            }
            let _p = crate::bench::scope("play_tick");
            ed.play_tick(&ctx, step);
        }
        self.watchdog.stage("Play tick", "plugins");
        {
            let _p = crate::bench::scope("plugin_tick");
            let step = ed.fixed_dt.map(|f| f as f64).unwrap_or_else(|| frame_grid(dt) as f64);
            ed.plugin_tick(step);
        }
        self.watchdog.stage("plugins", "project and scene synchronization");
        ed.project_tick();
        ed.plugins_tick();
        ed.toolbox_tick(&ctx);
        ed.toolbox_harness_tick();
        if self.ed.harness.no_ui {
            self.no_ui_frame(ui, &ctx, frame_start, dt);
            return;
        }
        let ed = &mut self.ed;
        // A folder with several project files: which one to open.
        if let Some((dir, files)) = ed.project_choice.clone() {
            let mut pick = None;
            let mut open = true;
            egui::Window::new("Open project")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(&ctx, |ui| {
                    ui.label(format!("{} has several project files:", dir.display()));
                    for f in &files {
                        let n = crate::project::name_of(f).to_string();
                        let label = if n == "default.project.json" { format!("{n} (default)") } else { n };
                        if ui.button(label).clicked() {
                            pick = Some(f.clone());
                        }
                    }
                });
            if let Some(f) = pick {
                ed.project_choice = None;
                ed.open_project(&f);
            } else if !open {
                ed.project_choice = None;
            }
        }
        ed.project_windows(&ctx);
        ed.sync_explorer_to_selection();
        {
            let _p = crate::bench::scope("scene_sync");
            ed.scene.sync(&mut ed.doc);
        }

        if ctx.input(|i| i.viewport().close_requested())
            && ed.doc.dirty()
            && ed.harness.screenshot.is_none()
            && ed.harness.selftest.is_none()
            && ed.harness.bench.is_none()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ed.confirm_discard = Some(PendingAction::Quit);
        }
        if ctx
            .data_mut(|d| d.remove_temp::<bool>(egui::Id::new("reset_dock")))
            .unwrap_or(false)
        {
            // Reset Layout: panel windows close, their tabs go back too.
            let popped: Vec<Tab> = self.panels.popped_tabs().collect();
            self.panels = crate::panels::Panels::default();
            self.dock = default_dock();
            for t in popped {
                if self.dock.find_tab(&t).is_none() && !matches!(t, Tab::AvatarSettings | Tab::TerrainEditor | Tab::Toolbox) {
                    crate::panels::place(&mut self.dock, t, &self.panels.homes);
                }
            }
        }

        self.reset_document_tabs();
        let ed = &mut self.ed;
        if (ctx.zoom_factor() - ed.ui.zoom).abs() > 1e-3 {
            ctx.set_zoom_factor(ed.ui.zoom);
        }
        // The Start Page replaces the editor's chrome until something opens.
        if ed.start_page_ui(ui) {
            ed.confirm_ui(&ctx);
            ed.account_window(&ctx);
            ed.cloud_windows(&ctx);
            if ed.harness.screenshot.is_some() {
                ed.harness_tick(&ctx);
                ctx.request_repaint();
            }
            if ed.account.busy() || ed.assets.busy() {
                if ed.harness.plain() {
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                } else {
                    ctx.request_repaint();
                }
            }
            ed.mcp_tick(&ctx);
            self.watchdog.complete();
            return;
        }
        self.watchdog.stage("scene synchronization", "dock UI and GPU viewport preparation");
        ed.shortcuts(&ctx);

        egui::Panel::top("menu")
            .frame(
                egui::Frame::new()
                    .fill(theme::MENU_BAR)
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ui, |ui| ed.studio_menu_bar(ui));
        egui::Panel::top("testbar")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ui, |ui| ed.studio_top_row(ui));
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            )
            .show(ui, |ui| ed.studio_ribbon(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_DARK)
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ui, |ui| ed.status_bar(ui));

        // Editor::start_run requests this when a playtest begins.
        if std::mem::take(&mut self.ed.switch_to_viewport) {
            if let Some(loc) = self.dock.find_tab(&Tab::Viewport) {
                let _ = self.dock.set_active_tab(loc);
            }
        }

        let _dock_scope = crate::bench::scope("dock_panels");
        let windows: Vec<(String, String)> = self.panels.popped.iter().map(|w| (w.key.clone(), w.title())).collect();
        let mut dock_rect = egui::Rect::NOTHING;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ui, |ui| {
                dock_rect = ui.max_rect();
                DockArea::new(&mut self.dock)
                    .style(theme::dock_style(ui.style()))
                    .show_add_buttons(false)
                    .show_leaf_collapse_buttons(false)
                    .show_leaf_close_all_buttons(false)
                    .show_inside(ui, &mut Viewer { ed: &mut self.ed, window: None, windows: &windows });
            });
        drop(_dock_scope);
        self.reset_document_tabs();
        self.panel_windows(&ctx, &windows, dock_rect);
        self.reset_document_tabs();
        // Window > Explorer / Properties / Output / Avatar Settings toggles
        // (in a panel window too), new panels where Studio docks them
        // (src/panels.rs, never over the 3D view), and which panels are
        // open (their checkmarks next frame).
        use crate::panels::Location;
        // `toolbox:ui:width:<px>` (Toolbox layout checks): the side-by-side
        // split holding the Toolbox gives it that width.
        if let Some(px) = self.ed.toolbox.ui.want_width.take() {
            // The split changes after this frame's draw: a layout report
            // waits for the next one (src/toolbox/glue.rs).
            self.ed.toolbox.ui.action_seq += 1;
            if let Some(path) = self.dock.find_tab(&Tab::Toolbox) {
                let mut node = path.node;
                let tree = &mut self.dock[path.surface];
                while let Some(parent) = node.parent() {
                    if let egui_dock::Node::Horizontal(sp) = &mut tree[parent] {
                        let w = sp.rect.width();
                        if w > px {
                            let f = (px / w).clamp(0.05, 0.95);
                            sp.fraction = if node.is_left() { f } else { 1.0 - f };
                        }
                        break;
                    }
                    node = parent;
                }
            }
        }
        for (tab, want) in std::mem::take(&mut self.ed.ui.panel_requests) {
            let found = self.panels.locate(&self.dock, &tab);
            let open = want.unwrap_or(found.is_none());
            match (found, open) {
                (Some(_), false) => {
                    self.panels.take(&mut self.dock, &tab);
                }
                (Some(Location::Main(at)), true) => {
                    let _ = self.dock.set_active_tab(at);
                }
                (Some(Location::Popped(i, at)), true) => {
                    let w = &mut self.panels.popped[i];
                    let _ = w.dock.set_active_tab(at);
                    w.focus = true;
                }
                (None, true) => {
                    if self.panels.restore_window(&self.dock, tab) {
                        // Restored the hidden panel in its original OS window.
                    } else if let Some(size) = crate::plugin_host::widgets_ui::float_size(&tab) {
                        // A plugin's Float widget: a floating window of the dock
                        // (it can be docked by its tab), centred on the main
                        // window as Studio's (src/plugin_host/widgets_ui.rs).
                        let size = egui::vec2(size[0].max(60.0), size[1].max(40.0)).min(dock_rect.size() - egui::vec2(8.0, 8.0));
                        let at = dock_rect.center() - size / 2.0;
                        let surface = self.dock.add_window(vec![tab]);
                        if let Some(w) = self.dock.get_window_state_mut(surface) {
                            w.set_position(at).set_size(size);
                        }
                    } else if crate::panels::placement(tab.key()).floats {
                        self.panels.pop_out(tab, [420.0, 520.0], None);
                    } else {
                        crate::panels::place(&mut self.dock, tab, &self.panels.homes);
                    }
                }
                (None, false) => {}
            }
        }
        {
            crate::plugin_host::widgets_ui::set_dock_area(dock_rect);
            let (dock, panels) = (&self.dock, &self.panels);
            self.ed.plugin_ui_note_open(|t| panels.locate(dock, t).is_some());
            // Float widgets' windows sized so their content is Studio's client area.
            for (tab, size) in self.ed.plugin_float_fixes() {
                if let Some(path) = self.dock.find_tab(&tab) {
                    if let Some(w) = self.dock.get_window_state_mut(path.surface) {
                        w.set_size(size);
                    }
                }
            }
            self.ed.plugin_drag_tick(&ctx);
            self.ed.plugin_widget_shots(&ctx);
        }
        self.ed.ui.panels_open = [
            (Tab::Explorer, "Explorer"),
            (Tab::Properties, "Properties"),
            (Tab::Output, "Output"),
            (Tab::AvatarSettings, "Avatar Settings"),
            (Tab::TerrainEditor, "Terrain Editor"),
            (Tab::Toolbox, "Toolbox"),
        ]
        .into_iter()
        .filter(|(t, _)| self.panels.locate(&self.dock, t).is_some())
        .map(|(_, n)| n)
        .collect();
        let ed = &mut self.ed;
        ed.open_script_tabs(&mut self.dock, &mut self.panels);
        crate::panels::record_homes(&self.dock, &mut self.panels.homes);
        self.ed.ui.popped = self.panels.popped.iter().map(|w| (w.key.clone(), w.tabs())).collect();
        // A script tab in front of the 3D view (plugin toolbar buttons
        // without ClickableWhenViewportHidden grey out: ui_ribbon.rs).
        self.ed.ui.viewport_hidden = self.dock.find_main_surface_tab(&Tab::Viewport).is_some_and(|(n, t)| {
            matches!(&self.dock.main_surface()[n], egui_dock::Node::Leaf(l) if l.active != t)
        });
        self.ed.ui.viewport_group = self
            .dock
            .find_main_surface_tab(&Tab::Viewport)
            .and_then(|(n, _)| self.dock.main_surface()[n].tabs().map(|t| t.to_vec()))
            .unwrap_or_default();
        if let Some(s) = self.layout.as_mut() {
            s.tick(&ctx, &self.dock, &self.panels, &self.ed);
        }
        let ed = &mut self.ed;
        ed.insert_popup_ui(&ctx);
        ed.about_ui(&ctx);
        ed.plugins_ui(&ctx);
        ed.studio_settings_ui(&ctx);
        ed.user_screenshot_tick(&ctx);
        crate::runtime::domains::assets_cloud::capture::frame_tick(&ctx, ed.viewport_rect);
        ed.confirm_ui(&ctx);
        ed.account_window(&ctx);
        ed.character_builder_window(&ctx);
        ed.cloud_windows(&ctx);
        self.watchdog.stage("dock UI", "harness and screenshot readback");
        self.finish_frame(&ctx, frame_start, dt);
    }
}

impl RustudioApp {
    /// The end of every frame, the editor's and the no-UI mode's: the
    /// harness (shots, input, --luau), control, --bench, and repaints.
    fn finish_frame(&mut self, ctx: &egui::Context, frame_start: Instant, dt: f32) {
        let ctx = ctx.clone();
        let ed = &mut self.ed;
        ed.harness_tick(&ctx);
        ed.control_tick(&ctx);
        ed.mcp_tick(&ctx);
        ed.ui_shots_tick(&ctx);
        let work_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
        ed.last_work_ms = work_ms;
        // RUSTUDIO_GPU_TIMING: resolve this frame's GPU timestamps, collect finished ones.
        if let Some(rs) = ed.render_state.as_ref() {
            crate::gpu_timing::end_frame(&rs.device, &rs.context);
        }
        let mut bench_done = false;
        if let Some(b) = ed.harness.bench.as_mut() {
            // Edit-mode benchmark: turn the camera slowly so several views count.
            // (RUSTUDIO_BENCH_STILL=1: the camera stays put, an idle editor)
            if !b.play && std::env::var_os("RUSTUDIO_BENCH_STILL").is_none() {
                ed.cam.yaw += crate::mech::BENCH_TURN_DEG_PER_S.to_radians() * dt;
                ed.cam.focus = ed.cam.pos + ed.cam.forward() * (ed.cam.focus - ed.cam.pos).length();
            }
            // Play benches count from Play's start (a live join can take a
            // minute before Play), not from the window opening.
            let ready = !b.play || ed.harness.play_started.is_some();
            if ready && !b.done && b.frame(work_ms) {
                let mut report = b.report();
                if let Some(r) = ed.renderer.as_ref() {
                    report["render"] = serde_json::json!(format!("{:?}", r.stats));
                }
                if let Some(p) = ed.play.as_ref() {
                    let (dynamic, fixed) = p.physics.counts();
                    report["physics"] = serde_json::json!({"assemblies": dynamic, "awake": p.physics.awake(), "fixed_colliders": fixed, "steps": p.physics.steps, "stages": p.physics.stage_times()});
                    let sample: Vec<String> = p
                        .physics
                        .awake_sample(25)
                        .into_iter()
                        .map(|(r, pos, vel, n)| {
                            let mut path = Vec::new();
                            let mut cur = r;
                            while let Some(i) = ed.doc.dom.get_by_ref(cur) {
                                path.push(i.name.clone());
                                cur = i.parent();
                            }
                            path.reverse();
                            let kids: Vec<String> = ed
                                .doc
                                .dom
                                .get_by_ref(r)
                                .map(|i| {
                                    i.children()
                                        .iter()
                                        .filter_map(|c| ed.doc.class_of(*c).map(|k| k.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let size =
                                crate::scene::vec3_of(ed.doc.prop(r, "Size")).unwrap_or_default();
                            format!(
                                "{} n={n} size={size:.1} pos={pos:.1} vel={vel:.1} kids={kids:?}",
                                path.join(".")
                            )
                        })
                        .collect();
                    report["awake_sample"] = serde_json::json!(sample);
                }
                let text = serde_json::to_string_pretty(&report).unwrap_or_default();
                if let Some(out) = &ed.harness.log_out {
                    let log: String = ed
                        .log
                        .iter()
                        .map(|l| format!("{:?}: {}\n", l.level, l.text.replace(crate::output_panel::STACK_MARK, "")))
                        .collect();
                    let _ = std::fs::write(out, log);
                }
                if let Some(out) = &b.out {
                    if let Err(e) = std::fs::write(out, &text) {
                        eprintln!("bench: can't write {}: {e}", out.display());
                    }
                    let _ = std::fs::write(out.with_extension("frames.json"), b.frames_json().to_string());
                }
                println!("{text}");
                bench_done = true;
            }
            ctx.request_repaint();
        }
        if bench_done {
            ed.finish_run(&ctx, "bench");
        }
        let active_input = ctx.input(|i| i.pointer.any_down() || !i.keys_down.is_empty());
        if ed.drag.is_some()
            || ed.is_playing()
            || active_input
            || ed.harness.screenshot.is_some()
            || ed.harness.selftest.is_some()
        {
            ctx.request_repaint();
        } else if ed.assets.busy() || ed.textures.decoding() || ed.account.busy() {
            if !ed.is_playing() && ed.harness.plain() && ed.drag.is_none() {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            } else {
                ctx.request_repaint();
            }
        }
        self.watchdog.complete();
    }

    /// `--no-ui`: the 3D view alone over the whole window, nothing of the
    /// editor built or drawn (no menus, ribbon, dock, Explorer, Properties,
    /// Output, status bar, panel windows or editor shortcuts). In Edit state
    /// the document's view through the edit camera; with --play the game,
    /// with every key going to it (game_keys).
    fn no_ui_frame(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame_start: Instant, dt: f32) {
        let ed = &mut self.ed;
        {
            let _p = crate::bench::scope("scene_sync");
            ed.scene.sync(&mut ed.doc);
        }
        if (ctx.zoom_factor() - ed.ui.zoom).abs() > 1e-3 {
            ctx.set_zoom_factor(ed.ui.zoom);
        }
        // --open-roblox / --team-create once the session is checked (no
        // picker or join windows are drawn).
        ed.cloud_poll(ctx);
        self.watchdog.stage("scene synchronization", "3D view");
        ed.game_keys(ctx);
        // (Studio menu actions need the editor: --action does nothing here,
        // except the edits that need no panel: Ctrl+Z / Ctrl+Y / Delete …,
        // which the plugin checks drive.)
        for a in std::mem::take(&mut ed.harness.ui_due) {
            // The ribbon's tools still switch (a plugin's Activate /
            // Deactivation against them: src/plugin_host/ui_ribbon.rs).
            if matches!(
                a.as_str(),
                "undo" | "redo" | "delete" | "duplicate" | "group" | "ungroup" | "tool_select" | "tool_move" | "tool_scale" | "tool_rotate"
            ) {
                ed.run_action(&a);
                continue;
            }
            // The Toolbox's deep links work without the panel (headless checks).
            if a.starts_with("toolbox:") {
                ed.toolbox_action(&a);
            } else {
                eprintln!("--action {a}: no editor in --no-ui");
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ui, |ui| {
                let _p = crate::bench::scope("viewport");
                ed.viewport_ui(ui);
            });
        // Open plugin widgets as fixed panels over the view (no dock here).
        if ed.play.is_none() {
            ed.plugin_noui_widgets(ctx);
        }
        ed.user_screenshot_tick(ctx);
        crate::runtime::domains::assets_cloud::capture::frame_tick(ctx, ed.viewport_rect);
        self.watchdog.stage("3D view", "harness and screenshot readback");
        self.finish_frame(ctx, frame_start, dt);
    }
}

impl Editor {
    /// The end of a harness run (shots taken, --bench measured, --seconds
    /// reached, EndTest): writes --log and --result, sets the exit code
    /// (--fail-on-error) and closes. Without --result / --fail-on-error /
    /// --no-ui it only closes, as runs always did.
    pub fn finish_run(&mut self, ctx: &egui::Context, reason: &str) {
        if self.harness.finished {
            return;
        }
        self.harness.finished = true;
        crate::plugin_host::widgets_ui::on_exit();
        let errors = self.log.iter().filter(|l| l.level == Level::Error).count();
        let warnings = self.log.iter().filter(|l| l.level == Level::Warn).count();
        let code = i32::from(self.harness.fail_on_error && errors > 0);
        if code != 0 {
            eprintln!("--fail-on-error: {errors} error line(s) in the Output");
        }
        EXIT_CODE.store(code, std::sync::atomic::Ordering::Relaxed);
        if reason != "shots" && reason != "bench" {
            if let Some(out) = &self.harness.log_out {
                let text: String = self
                    .log
                    .iter()
                    .map(|l| format!("{:?}: {}\n", l.level, l.text.replace(crate::output_panel::STACK_MARK, "")))
                    .chain(self.harness.shot_lines.iter().map(|l| format!("{l}\n")))
                    .collect();
                if let Err(e) = std::fs::write(out, text) {
                    eprintln!("log: can't write {}: {e}", out.display());
                }
            }
        }
        if let Some(out) = self.harness.result.clone() {
            let body = self.result_json(reason, errors, warnings, code);
            if let Err(e) = std::fs::write(&out, serde_json::to_string_pretty(&body).unwrap_or_default()) {
                eprintln!("--result: can't write {}: {e}", out.display());
            }
        }
        crate::telemetry::flush();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn result_json(&self, reason: &str, errors: usize, warnings: usize, code: i32) -> serde_json::Value {
        let output: Vec<serde_json::Value> = self
            .log
            .iter()
            .map(|l| {
                let level = match l.level {
                    Level::Info => "info",
                    Level::Warn => "warning",
                    Level::Error => "error",
                };
                serde_json::json!({"level": level, "text": l.text.replace(crate::output_panel::STACK_MARK, "")})
            })
            .collect();
        let mut ms: Vec<f32> = self.harness.run_frames.iter().map(|f| f.0).collect();
        let work: f32 = self.harness.run_frames.iter().map(|f| f.1).sum::<f32>() / self.harness.run_frames.len().max(1) as f32;
        ms.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f32| ms.get(((ms.len() as f32 * p) as usize).min(ms.len().saturating_sub(1))).copied().unwrap_or(0.0);
        let mean = ms.iter().sum::<f32>() / ms.len().max(1) as f32;
        let shots: Vec<String> = self
            .harness
            .shot_lines
            .iter()
            .filter_map(|l| l.strip_prefix("[shot] ").map(str::to_string))
            .collect();
        let value = self
            .harness
            .end_value
            .as_deref()
            .map(|v| serde_json::from_str(v).unwrap_or(serde_json::Value::Null));
        let (cpu_s, peak_rss_mb) = process_usage();
        serde_json::json!({
            "ended": reason,
            "exit_code": code,
            "state": if self.harness.play_started.is_some() { "play" } else { "edit" },
            "no_ui": self.harness.no_ui,
            "end_test_value": value,
            "seconds": self.harness.now(),
            "errors": errors,
            "warnings": warnings,
            "frames": {
                "count": ms.len(),
                "mean_ms": mean,
                "p50_ms": pct(0.5),
                "p99_ms": pct(0.99),
                "max_ms": ms.last().copied().unwrap_or(0.0),
                "mean_work_ms": work,
                "fixed_dt": self.fixed_dt,
            },
            "shots": shots,
            "plugins": self.plugins.result_json(),
            "plugin_toolbars": self.plugin_ui_json(),
            "toolbox": self.toolbox.result_json(),
            "process": {"cpu_s": cpu_s, "peak_rss_mb": peak_rss_mb, "rss_mb": current_rss_mb()},
            "output": output,
        })
    }
}

/// This process's resident memory now (MB; NaN if the query fails).
fn current_rss_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a zeroed counters struct with its size set, as the API asks.
    let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) } != 0 {
        m.WorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        f64::NAN
    }
}

/// This process's CPU time (user + system, s) and peak resident memory (MB).
fn process_usage() -> (f64, f64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (z, z, z, z);
    let secs = |f: FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64 * 1e-7;
    // SAFETY: the current process's pseudo-handle and four FILETIME out-pointers.
    let cpu = if unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) } != 0 {
        secs(kernel) + secs(user)
    } else {
        f64::NAN
    };
    // SAFETY: a zeroed counters struct with its size set, as the API asks.
    let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let peak = if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) } != 0 {
        m.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        f64::NAN
    };
    (cpu, peak)
}

// ------------------------------------------------------------------ harness

impl Editor {
    fn harness_tick(&mut self, ctx: &egui::Context) {
        if self.harness.start_play && self.viewport_rect.is_positive() && self.frame_no >= 2 && !self.place_loading() {
            self.harness.start_play = false;
            self.start_play(crate::runtime::RunMode::Play);
            self.harness.play_started = Some(crate::sim_clock::now());
        }
        if self.harness.play_started.is_some() {
            let now = Instant::now();
            if let Some(last) = self.harness.last_frame.replace(now) {
                self.harness.frame_ms.push((now - last).as_secs_f32() * 1000.0);
            }
        }
        // --result's frame statistics: Edit state from the first frame, Play
        // from its start (the first Play frame clears the Edit ones).
        if self.harness.result.is_some() {
            let now = Instant::now();
            let playing = self.harness.play_started.is_some();
            if playing != self.harness.run_playing {
                self.harness.run_playing = playing;
                self.harness.run_frames.clear();
                self.harness.run_last = None;
            }
            if let Some(last) = self.harness.run_last.replace(now) {
                self.harness.run_frames.push(((now - last).as_secs_f32() * 1000.0, self.last_work_ms));
            }
        }
        if self.harness.finished {
            return;
        }
        // A script ended the run (StudioTestService:EndTest), or --seconds.
        if self.harness.end_value.is_some() {
            self.finish_run(ctx, "EndTest");
            return;
        }
        if let (Some(end), Some(now)) = (self.harness.end_after, self.harness.now()) {
            if now >= end {
                self.finish_run(ctx, "seconds");
                return;
            }
        }
        // Timed Luau (`--luau`), once Play runs.
        let Some(t) = self.harness.now() else { return };
        if self.play.is_some() {
            let due: Vec<(bool, String)> = self
                .harness
                .actions
                .iter()
                .filter_map(|(at, a)| match a {
                    HarnessAction::Luau(server, code) if *at <= t => Some((*server, code.clone())),
                    _ => None,
                })
                .collect();
            if !due.is_empty() {
                self.harness
                    .actions
                    .retain(|(at, a)| !(matches!(a, HarnessAction::Luau(..)) && *at <= t));
                for (server, code) in due {
                    let side = if server {
                        crate::runtime::Side::Server
                    } else {
                        crate::runtime::Side::Client
                    };
                    if let Some(e) = self.run_luau(side, &code) {
                        self.error(format!("--luau: {e}"));
                    }
                }
            }
        }
        if let Some(mut t) = self.harness.selftest.take() {
            let done = t.tick(self, ctx);
            self.harness.selftest = Some(t);
            if done {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        let Some(path) = self.harness.screenshot.clone() else {
            return;
        };
        let now = self.harness.now();
        // `--shots`: the next timed shot, else the single one.
        let multi = self.harness.shot_next < self.harness.shots.len();
        let due = if multi {
            now.is_some_and(|t| t >= self.harness.shots[self.harness.shot_next])
        } else if !self.harness.shots.is_empty() {
            false
        } else {
            match self.harness.shot_after {
                Some(s) => now.is_some_and(|t| t >= s),
                None => self.frame_no >= mech::SCREENSHOT_FRAMES,
            }
        };
        let path = if multi {
            let t = self.harness.shots[self.harness.shot_next];
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            path.with_file_name(format!("{stem}-{t}s.png"))
        } else {
            path
        };
        if due && !self.harness.screenshot_requested {
            self.harness.screenshot_requested = true;
            self.write_asset_report();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } if self.harness.screenshot_requested => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            if let Ok(hdr_path) = std::env::var("RUSTUDIO_DUMP_HDR") {
                if let Some((w,h,data)) = self.renderer.as_ref().zip(self.render_state.as_ref()).and_then(|(r,rs)| r.read_hdr(rs)) {
                    if std::fs::write(&hdr_path, data).is_ok() { eprintln!("HDR {w} {h} RGBA16Float {hdr_path}"); }
                }
            }
            let mut written = Vec::new();
            // (--no-render: an empty picture, nothing to write)
            if !(image.size == [0, 0] && self.harness.no_render) {
                match save_png(&image, &path) {
                    Ok(()) => written.push(path.clone()),
                    Err(e) => eprintln!("screenshot failed: {e}"),
                }
            }
            // Just the 3D view too (FILE.view.png), to compare with Studio's viewport.
            // (--no-ui: the window is the view, one picture is enough.)
            if (self.harness.shot_after.is_some() || multi) && self.viewport_rect.is_positive() && !self.harness.no_ui {
                let ppp = ctx.pixels_per_point();
                let r = self.viewport_rect;
                let (x0, y0) = ((r.min.x * ppp) as usize, (r.min.y * ppp) as usize);
                let (w, h) = (
                    ((r.width() * ppp) as usize).min(image.size[0].saturating_sub(x0)),
                    ((r.height() * ppp) as usize).min(image.size[1].saturating_sub(y0)),
                );
                let pixels = (0..h)
                    .flat_map(|y| (0..w).map(move |x| (x, y)))
                    .map(|(x, y)| image.pixels[(y0 + y) * image.size[0] + x0 + x])
                    .collect();
                let view = egui::ColorImage::new([w, h], pixels);
                let view_path = path.with_extension("view.png");
                if save_png(&view, &view_path).is_ok() {
                    written.push(view_path);
                }
            }
            // Each open plugin widget's content too (FILE.widget-<key>.png).
            if !self.harness.no_ui {
                let ppp = ctx.pixels_per_point();
                for (key, r) in self.plugin_widget_rects() {
                    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
                    let wp = path.with_extension(format!("widget-{safe}.png"));
                    if save_png(&crate::plugin_host::widgets_ui::crop(&image, r, ppp), &wp).is_ok() {
                        written.push(wp);
                    }
                }
            }
            // The written paths, for callers that don't rebuild the names.
            for p in written {
                let p = std::path::absolute(&p).unwrap_or(p);
                let line = format!("[shot] {}", p.display());
                println!("{line}");
                self.harness.shot_lines.push(line);
            }
            if let Some(out) = &self.harness.log_out {
                let text: String = self
                    .log
                    .iter()
                    .map(|l| format!("{:?}: {}\n", l.level, l.text.replace(crate::output_panel::STACK_MARK, "")))
                    .chain(self.harness.shot_lines.iter().map(|l| format!("{l}\n")))
                    .collect();
                if let Err(e) = std::fs::write(out, text) {
                    eprintln!("log: can't write {}: {e}", out.display());
                }
            }
            crate::telemetry::flush();
            if multi {
                self.harness.shot_next += 1;
                self.harness.screenshot_requested = false;
                self.write_frame_times(&path);
                self.write_cursor_state(&path);
                if self.harness.shot_next < self.harness.shots.len() {
                    return;
                }
            }
            self.finish_run(ctx, "shots");
        }
        if self.harness.shot_after.is_none()
            && self.harness.shots.is_empty()
            && self.frame_no > mech::SCREENSHOT_FRAMES + 600
        {
            eprintln!("screenshot timed out");
            std::process::exit(2);
        }
    }
}

impl Editor {
    /// FILE.frames.json beside a shot: frame times since Play started.
    fn write_frame_times(&self, shot: &std::path::Path) {
        let mut ms = self.harness.frame_ms.clone();
        if ms.is_empty() {
            return;
        }
        let mean = ms.iter().sum::<f32>() / ms.len() as f32;
        ms.sort_by(|a, b| a.total_cmp(b));
        let p99 = ms[((ms.len() as f32 * 0.99) as usize).min(ms.len() - 1)];
        let body = serde_json::json!({
            "frames": ms.len(),
            "mean_ms": mean,
            "p99_ms": p99,
            "fps": 1000.0 / mean.max(1e-3),
        });
        let _ = std::fs::write(shot.with_extension("frames.json"), body.to_string());
    }
}

#[derive(Debug, Clone)]
pub enum HarnessAction {
    Click(f32, f32),
    Key(egui::Key),
    /// A key held down (true) or let go (false): `--key NAME@S:HOLD`.
    KeyState(egui::Key, bool),
    /// `--wheel NOTCHES@S` over the 3D view's centre (positive = forward,
    /// zoom in), or `--wheel NOTCHES@S:X,Y` at that view position.
    Wheel(f32, Option<(f32, f32)>),
    /// `--type "TEXT"@S`: typed text (egui Text events: a focused TextBox
    /// receives the characters).
    Text(String),
    /// Shift held down or let go.
    Shift(bool),
    /// The right mouse button over the 3D view's centre, down or up.
    RightButton(bool),
    /// Raw mouse motion (pixels), as a grabbed cursor reports it.
    MouseMotion(f32, f32),
    /// The cursor at a view position (`--ldrag`), and the left button there
    /// going down / up.
    PointerAt(f32, f32),
    LeftButton(f32, f32, bool),
    /// `--luau server|client FILE@S`: run Luau on that side of Play.
    Luau(bool, String),
    /// `--action NAME@S`: a Studio menu action by id (`panel_output` closes
    /// or opens the Output panel, resizing the 3D view mid-run).
    UiAction(String),
}

impl HarnessAction {
    /// "X,Y@S" for clicks, "NAME@S" for a key tap, "NAME@S:HOLD" to hold
    /// it HOLD seconds.
    pub fn parse(s: &str, click: bool) -> Option<Vec<(f32, HarnessAction)>> {
        let (what, at) = s.split_once('@')?;
        if click {
            let at: f32 = at.trim().parse().ok()?;
            let (x, y) = what.split_once(',')?;
            return Some(vec![(
                at,
                HarnessAction::Click(x.trim().parse().ok()?, y.trim().parse().ok()?),
            )]);
        }
        let key = egui::Key::from_name(what.trim())?;
        match at.split_once(':') {
            Some((at, hold)) => {
                let (at, hold): (f32, f32) = (at.trim().parse().ok()?, hold.trim().parse().ok()?);
                Some(vec![
                    (at, HarnessAction::KeyState(key, true)),
                    (at + hold, HarnessAction::KeyState(key, false)),
                ])
            }
            None => Some(vec![(at.trim().parse().ok()?, HarnessAction::Key(key))]),
        }
    }

    /// `--wheel NOTCHES@S` or `--wheel NOTCHES@S:X,Y` (view coordinates).
    pub fn parse_wheel(s: &str) -> Option<(f32, HarnessAction)> {
        let (n, at) = s.split_once('@')?;
        let n: f32 = n.trim().parse().ok()?;
        let (at, pos) = match at.split_once(':') {
            Some((at, xy)) => {
                let (x, y) = xy.split_once(',')?;
                (at, Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))
            }
            None => (at, None),
        };
        Some((at.trim().parse().ok()?, HarnessAction::Wheel(n, pos)))
    }

    /// `--type TEXT@S` (the last '@' splits, so the text may hold one).
    pub fn parse_type(s: &str) -> Option<(f32, HarnessAction)> {
        let (text, at) = s.rsplit_once('@')?;
        Some((at.trim().parse().ok()?, HarnessAction::Text(text.to_string())))
    }

    /// `--ldrag X1,Y1>X2,Y2@S`: left-drag in the 3D view (view coordinates):
    /// press at S, move in 10 steps over 0.5 s, let go at S + 0.6.
    /// `--lhold X,Y@S:HOLD`: the left button pressed at X,Y at S, let go at S + HOLD
    /// (a game's press-and-hold, e.g. carrying a grabbed part).
    pub fn parse_lhold(s: &str) -> Option<Vec<(f32, HarnessAction)>> {
        let (xy, when) = s.split_once('@')?;
        let (at, hold) = when.split_once(':')?;
        let (at, hold): (f32, f32) = (at.trim().parse().ok()?, hold.trim().parse().ok()?);
        let (x, y) = xy.split_once(',')?;
        let (x, y): (f32, f32) = (x.trim().parse().ok()?, y.trim().parse().ok()?);
        Some(vec![
            (at - 0.2, HarnessAction::PointerAt(x, y)),
            (at, HarnessAction::LeftButton(x, y, true)),
            (at + hold, HarnessAction::LeftButton(x, y, false)),
        ])
    }

    pub fn parse_ldrag(s: &str) -> Option<Vec<(f32, HarnessAction)>> {
        let (what, at) = s.split_once('@')?;
        let at: f32 = at.trim().parse().ok()?;
        let (a, b) = what.split_once('>')?;
        let xy = |p: &str| -> Option<(f32, f32)> {
            let (x, y) = p.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        };
        let ((x1, y1), (x2, y2)) = (xy(a)?, xy(b)?);
        let mut v = vec![(at - 0.2, HarnessAction::PointerAt(x1, y1)), (at, HarnessAction::LeftButton(x1, y1, true))];
        for k in 1..=10 {
            let f = k as f32 / 10.0;
            v.push((at + 0.05 * k as f32, HarnessAction::PointerAt(x1 + (x2 - x1) * f, y1 + (y2 - y1) * f)));
        }
        v.push((at + 0.6, HarnessAction::LeftButton(x2, y2, false)));
        Some(v)
    }

    /// `--rdrag DX,DY@S`: right-drag over the view: press at S, move DX,DY
    /// pixels in 10 steps over 0.5 s, let go at S + 0.6.
    pub fn parse_rdrag(s: &str) -> Option<Vec<(f32, HarnessAction)>> {
        let (what, at) = s.split_once('@')?;
        let at: f32 = at.trim().parse().ok()?;
        let (x, y) = what.split_once(',')?;
        let (dx, dy): (f32, f32) = (x.trim().parse().ok()?, y.trim().parse().ok()?);
        let mut v = vec![(at, HarnessAction::RightButton(true))];
        for k in 1..=10 {
            v.push((
                at + 0.05 * k as f32,
                HarnessAction::MouseMotion(dx / 10.0, dy / 10.0),
            ));
        }
        v.push((at + 0.6, HarnessAction::RightButton(false)));
        Some(v)
    }
}

impl Editor {
    /// Timed harness input, injected like real events.
    pub fn harness_inject(&mut self, raw: &mut egui::RawInput) {
        let Some(t) = self.harness.now() else { return };
        // No 3D view yet (the start page): clicks are window coordinates.
        let vp = if self.viewport_rect.is_finite() {
            self.viewport_rect
        } else {
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ZERO)
        };
        // The game holds the mouse (MouseBehavior.LockCenter): a held cursor
        // never travels, so clicks land where it is held and pointer moves
        // are dropped. Moving it would read as a huge mouse
        // delta and fling a first-person camera (only --rdrag's raw motion
        // turns it then).
        let locked = self.lock_center_held();
        // Where the real pointer is now: a held cursor's clicks happen there.
        for e in &raw.events {
            if let egui::Event::PointerMoved(p) = e {
                self.harness.pointer = Some(*p);
            }
        }
        let held_at = self.harness.pointer.unwrap_or(vp.center());
        let mut i = 0;
        while i < self.harness.actions.len() {
            if self.harness.actions[i].0 > t {
                i += 1;
                continue;
            }
            if matches!(self.harness.actions[i].1, HarnessAction::Luau(..)) {
                i += 1;
                continue;
            }
            let (_, a) = self.harness.actions.remove(i);
            match a {
                HarnessAction::Luau(..) => {}
                HarnessAction::Click(x, y) => {
                    let p = if locked { held_at } else { vp.min + egui::vec2(x, y) };
                    if !locked {
                        raw.events.push(egui::Event::PointerMoved(p));
                    }
                    for pressed in [true, false] {
                        raw.events.push(egui::Event::PointerButton {
                            pos: p,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        });
                    }
                }
                HarnessAction::Key(key) => {
                    for pressed in [true, false] {
                        raw.events.push(egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        });
                    }
                }
                HarnessAction::KeyState(key, pressed) => {
                    raw.events.push(egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                HarnessAction::Wheel(n, at) => {
                    let p = at.map_or(vp.center(), |(x, y)| vp.min + egui::vec2(x, y));
                    raw.events.push(egui::Event::PointerMoved(p));
                    raw.events.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(0.0, n),
                        modifiers: egui::Modifiers::NONE,
                        phase: egui::TouchPhase::Move,
                    });
                }
                HarnessAction::Text(text) => raw.events.push(egui::Event::Text(text)),
                HarnessAction::Shift(on) => self.harness.shift = on,
                HarnessAction::RightButton(pressed) => {
                    let p = if locked { held_at } else { vp.center() };
                    if !locked {
                        raw.events.push(egui::Event::PointerMoved(p));
                    }
                    raw.events.push(egui::Event::PointerButton {
                        pos: p,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                HarnessAction::PointerAt(x, y) => {
                    if !locked {
                        raw.events.push(egui::Event::PointerMoved(vp.min + egui::vec2(x, y)));
                    }
                }
                HarnessAction::LeftButton(x, y, pressed) => {
                    let p = if locked { held_at } else { vp.min + egui::vec2(x, y) };
                    if !locked {
                        raw.events.push(egui::Event::PointerMoved(p));
                    }
                    raw.events.push(egui::Event::PointerButton {
                        pos: p,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                HarnessAction::MouseMotion(dx, dy) => {
                    raw.events.push(egui::Event::MouseMoved(egui::vec2(dx, dy)));
                }
                HarnessAction::UiAction(name) => {
                    // plugin-button:TOOLBAR/BUTTON, plugin-action:ID (src/plugin_host);
                    // plugin-prompt:ANSWER answers the oldest plugin dialog.
                    if self.plugin_widget_action(&name, raw) {
                        // plugin-widget:PLUGIN/WIDGET:CMD (plugin_host/widgets_ui.rs)
                    } else if self.harness_pointer(&name, raw) {
                        // ptr:VERB:TARGET (ui_pointer.rs)
                    } else if let Some(ans) = name.strip_prefix("plugin-prompt:") {
                        crate::plugin_host::prompts::answer_oldest(ans);
                    } else if let Some(t) = crate::plugin_host::Trigger::parse(&name) {
                        match t {
                            // A click on the ribbon button (Enabled and a
                            // script tab in front count, as in Studio).
                            crate::plugin_host::Trigger::Button(tb, b) => self.plugin_harness_click(&tb, &b),
                            t => self.plugins.trigger(t),
                        }
                    } else {
                        self.harness.ui_due.push(name)
                    }
                }
            }
        }
        for e in &raw.events {
            if let egui::Event::PointerMoved(p) = e {
                self.harness.pointer = Some(*p);
            }
        }
    }
}

pub fn save_png(image: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    let [w, h] = image.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for c in &image.pixels {
        bytes.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    image::save_buffer(
        path,
        &bytes,
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| e.to_string())
}

/// The sample scene's parts, scripts and lights, into a place document
/// (`--demo`, and the fidelity corpus's demo place). Returns the Ball.
pub fn build_demo_parts(doc: &mut Document) -> Ref {
        let ws = doc.default_parent();
        let parts: [(&str, u32, [f32; 3], [f32; 3], [u8; 3]); 6] = [
            (
                "Tower",
                1,
                [-14.0, 6.0, -10.0],
                [6.0, 12.0, 6.0],
                [196, 112, 70],
            ),
            (
                "Ball",
                0,
                [-4.0, 2.0, -4.0],
                [4.0, 4.0, 4.0],
                [80, 160, 220],
            ),
            (
                "Pipe",
                2,
                [8.0, 1.5, -8.0],
                [10.0, 3.0, 3.0],
                [110, 190, 120],
            ),
            ("Step", 1, [10.0, 0.5, 4.0], [8.0, 1.0, 8.0], [230, 200, 90]),
            (
                "Bridge",
                1,
                [-2.0, 8.0, -18.0],
                [24.0, 1.0, 4.0],
                [150, 150, 165],
            ),
            ("Lamp", 1, [3.0, 3.0, 6.0], [1.0, 6.0, 1.0], [250, 240, 220]),
        ];
        let mut last = Ref::none();
        for (name, shape, p, s, c) in parts {
            let r = doc.insert(
                ws,
                doc::new_part(name, shape, Vec3::from(p), Vec3::from(s), c),
            );
            last = if name == "Ball" { r } else { last };
        }
        let ramp = doc::new_part(
            "Ramp",
            1,
            Vec3::new(-14.0, 2.0, 0.0),
            Vec3::new(6.0, 4.0, 8.0),
            [196, 112, 70],
        )
        .with_class("WedgePart");
        doc.insert(ws, ramp);
        let model = doc
            .insert(ws, InstanceBuilder::new("Model").with_name("Statue"));
        doc.insert(
            model,
            InstanceBuilder::new("MeshPart")
                .with_name("StatueMesh")
                .with_property("Anchored", true)
                .with_property("Size", rbx_dom_weak::types::Vector3::new(4.0, 7.0, 4.0))
                .with_property(
                    "CFrame",
                    rbx_dom_weak::types::CFrame::new(
                        rbx_dom_weak::types::Vector3::new(18.0, 4.5, -14.0),
                        rbx_dom_weak::types::Matrix3::identity(),
                    ),
                )
                .with_property(
                    "Color",
                    rbx_dom_weak::types::Color3uint8::new(170, 140, 210),
                ),
        );
        doc.insert(
            model,
            doc::new_part(
                "Plinth",
                1,
                Vec3::new(18.0, 0.5, -14.0),
                Vec3::new(6.0, 1.0, 6.0),
                [90, 90, 100],
            ),
        );
        let glass = doc::new_part(
            "Glass",
            1,
            Vec3::new(2.0, 3.0, -12.0),
            Vec3::new(8.0, 6.0, 0.5),
            [180, 220, 255],
        )
        .with_property("Transparency", 0.5f32);
        doc.insert(ws, glass);
        let neon = doc::new_part(
            "NeonStrip",
            1,
            Vec3::new(3.0, 6.25, 6.0),
            Vec3::new(2.0, 0.5, 2.0),
            [255, 120, 60],
        )
        .with_property("Material", rbx_dom_weak::types::Enum::from_u32(288));
        doc.insert(ws, neon);
        let sp = doc.find_child(ws, "Script");
        if sp.is_none() {
            let sss = doc
                .find_child(doc.dom.root_ref(), "ServerScriptService")
                .unwrap_or(ws);
            doc.insert(
                sss,
                InstanceBuilder::new("Script")
                    .with_name("Main")
                    .with_property("Source", "print(\"Hello from rustudio\")\n".to_string()),
            );
        }
        last
}

/// The whole pixels the 3D view covers in a screenshot of the window: its
/// rectangle in points scaled by `ppp`, rounded inward, where an edge within
/// a hundredth of a pixel of a whole one counts as on it. With fractional
/// scaling (1.25) an edge at 143.2 points is pixel row 179 but comes out
/// 178.99997 in f32: truncating took row 178, the editor's frame (shots were
/// a pixel low), and plain inward rounding lost a row at the other edge.
/// `(x0, y0, w, h)`, clipped to `size`.
pub fn view_pixels(r: egui::Rect, ppp: f32, size: [usize; 2]) -> (usize, usize, usize, usize) {
    const EPS: f32 = 0.01;
    let start = |v: f32, n: usize| (((v * ppp) - EPS).ceil().max(0.0) as usize).min(n);
    let end = |v: f32, n: usize| (((v * ppp) + EPS).floor().max(0.0) as usize).min(n);
    let (x0, y0) = (start(r.min.x, size[0]), start(r.min.y, size[1]));
    let (x1, y1) = (end(r.max.x, size[0]), end(r.max.y, size[1]));
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

/// RUSTUDIO_FRAME_GRID=HZ (tests only): a frame's time snapped to whole
/// periods of a HZ display, the remainder carried, as a vsynced monitor
/// hands frames out. Headless runs pace frames with a timer that drifts
/// against the 240 Hz world clock; the drift changes on which tick a frame
/// starts (and with it where the Humanoid's 2-tick solves fall) at random
/// from run to run. Without the variable: `dt` as it is.
fn frame_grid(dt: f32) -> f32 {
    use std::sync::{Mutex, OnceLock};
    static HZ: OnceLock<Option<f32>> = OnceLock::new();
    static OWED: Mutex<f32> = Mutex::new(0.0);
    let Some(hz) = *HZ.get_or_init(|| std::env::var("RUSTUDIO_FRAME_GRID").ok().and_then(|v| v.parse::<f32>().ok()).filter(|h| *h > 0.0))
    else {
        return dt;
    };
    let mut owed = OWED.lock().unwrap();
    *owed += dt;
    let n = (*owed * hz).round().max(1.0);
    // (a frame shorter than a period still gets one: a headless pacer runs
    // such frames to catch up, a display never does. No more than a quarter
    // period of that is held against the frames that follow.)
    *owed = (*owed - n / hz).max(-0.25 / hz);
    n / hz
}
