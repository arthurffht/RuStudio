//! The plugin host: runs Roblox Studio plugins against the live edit
//! DataModel (and gives each Play DataModel its own copies, as Studio
//! does). This module documents the
//! API other parts of rustudio use; this header is the design.
//!
//! **One edit VM.** All plugins of the edit DataModel share one Luau VM
//! (as Studio's one ScriptContext per DataModel: `_G` and `shared` are
//! shared by plugins there, measured), made with `Runtime::new` and a
//! `World` in `RunMode::Edit`.
//!
//! **The document is lent, not copied.** Each frame the editor's own
//! `Document` moves into the edit VM's World (`std::mem::replace` with a
//! placeholder, as Play lends its DataModels to their runtimes), plugin code
//! runs, and the document moves back. No copy is made, so a plugin's write
//! is in the document the Explorer, Properties, viewport and undo stack
//! read on the same frame. Writes made outside the VM (the editor, undo,
//! file sync, Team Create) reach plugins as signals through the journal
//! (edit_sync.rs). With no plugin loaded nothing runs and nothing moves.
//!
//! **Plugin instances** are orphans in the document's DOM (Parent nil, not
//! in the DataModel): never saved, not in Explorer, not copied into Play.
//! A new document (another place opened) reloads every plugin into it, as
//! Studio loads plugins per DataModel.
//!
//! **Play.** Studio loads every plugin again in the Play server and client
//! DataModels (each copy with IsEdit false), and the edit DataModel's
//! plugins keep running meanwhile with Heartbeat but no RenderStepped
//! (measured). The host does the same: `load_into_play` from start_play,
//! the edit VM ticks on the saved edit document while Play runs.

pub mod edit_sync;
mod glue;
pub mod widgets_ui;
pub mod permissions;
pub mod prompts;
pub mod script_docs;
pub mod settings;
pub mod source;
pub mod ui_ribbon;
pub mod ui_shortcuts;
pub mod vm;

use std::sync::Arc;

use mlua::Value;
use rbx_dom_weak::types::Ref;
use rbx_dom_weak::WeakDom;

use crate::app::Level;
use crate::doc::Document;
use crate::runtime::{RunMode, Runtime, World};

pub use source::PluginSource;
pub use vm::{EditorRequest, UiItem, UiKind};

/// A plugin's id in this rustudio session (stable across reloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId(pub u32);

/// What `PluginHost::loaded` reports per plugin.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub id: PluginId,
    /// The Plugin instance's Name (`user_X.rbxm`, `cloud_123`).
    pub name: String,
    pub source: PluginSource,
    /// Running in the edit DataModel now.
    pub running: bool,
    /// Load errors (the file, a decode) and script errors from its code.
    pub errors: Vec<String>,
}

struct Spec {
    id: PluginId,
    source: PluginSource,
    name: String,
    tree: Option<Arc<WeakDom>>,
    errors: Vec<String>,
    running: bool,
}

impl Spec {
    fn load_spec(&self) -> Option<vm::LoadSpec> {
        Some(vm::LoadSpec {
            id: self.id,
            name: self.name.clone(),
            asset_id: self.source.asset_id(),
            settings_key: self.source.settings_key(),
            tree: self.tree.clone()?,
        })
    }
}

enum Cmd {
    Load(PluginId),
    Unload(PluginId),
    Reload(PluginId),
    Trigger(Trigger),
}

/// A UI event the harness fires by name.
#[derive(Clone, Debug)]
pub enum Trigger {
    /// PluginToolbarButton.Click: (toolbar name, button id or text).
    Button(String, String),
    /// PluginAction.Triggered by its id (as given to CreatePluginAction)
    /// or its full ActionId.
    Action(String),
    /// Answers the open PluginMenu:ShowAsync popup on top (see
    /// `domains::plugin::menus::answer_top` for the choice syntax).
    Menu(String),
    /// Fires `event` on a plugin object of the edit VM (the ribbon's clicks).
    Fire(Ref, &'static str),
    /// Deactivates a plugin (the person picked a ribbon tool).
    Deactivate(PluginId),
}

impl Trigger {
    /// `plugin-button:TOOLBAR/BUTTON` or `plugin-action:ACTIONID` (the
    /// harness's --action names).
    pub fn parse(name: &str) -> Option<Trigger> {
        if let Some(rest) = name.strip_prefix("plugin-button:") {
            let (tb, b) = rest.rsplit_once('/')?;
            return Some(Trigger::Button(tb.to_string(), b.to_string()));
        }
        if let Some(m) = name.strip_prefix("plugin-menu:") {
            return Some(Trigger::Menu(m.to_string()));
        }
        name.strip_prefix("plugin-action:").map(|a| Trigger::Action(a.to_string()))
    }
}

struct EditVm {
    rt: Runtime,
    world: Option<World>,
    doc_id: u64,
    bound: bool,
    gc_t: f64,
}

pub struct PluginHost {
    enabled: bool,
    specs: Vec<Spec>,
    next: u32,
    edit: Option<EditVm>,
    sync: edit_sync::EditSync,
    cmds: Vec<Cmd>,
    log: Vec<(Level, String)>,
    requests: Vec<EditorRequest>,
    activation_seen: u64,
    activation_changed: bool,
}

impl Default for PluginHost {
    fn default() -> Self {
        // HTTP / script-injection permissions come from the plugin store
        // (once: a check registered later, e.g. by a test, stays).
        static STORE_CHECKS: std::sync::Once = std::sync::Once::new();
        STORE_CHECKS.call_once(permissions::use_store_checks);
        PluginHost {
            enabled: !disabled_by_env(),
            specs: Vec::new(),
            next: 1,
            edit: None,
            sync: Default::default(),
            cmds: Vec::new(),
            log: Vec::new(),
            requests: Vec::new(),
            activation_seen: 0,
            activation_changed: false,
        }
    }
}

/// RUSTUDIO_NO_PLUGINS=1 (or --no-plugins): no plugin loads at all.
pub fn disabled_by_env() -> bool {
    std::env::var_os("RUSTUDIO_NO_PLUGINS").is_some_and(|v| !v.is_empty() && v != "0")
}

impl PluginHost {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Nothing loaded and nothing queued: the editor skips the tick.
    pub fn is_idle(&self) -> bool {
        self.specs.is_empty() && self.cmds.is_empty() && self.edit.is_none() && self.log.is_empty()
    }

    /// Turns all plugin loading off (--no-plugins): loaded ones unload at
    /// the next frame, later loads are refused.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            let ids: Vec<PluginId> = self.specs.iter().map(|s| s.id).collect();
            for id in ids {
                self.unload(id);
            }
        }
    }

    /// Adds a plugin; it starts in the edit DataModel at the next frame.
    /// The file is read now: a read or decode error comes back here (and
    /// is listed in `loaded()`).
    pub fn load(&mut self, source: PluginSource) -> Result<PluginId, String> {
        if !self.enabled {
            return Err("plugins are turned off (--no-plugins / RUSTUDIO_NO_PLUGINS)".into());
        }
        let id = PluginId(self.next);
        self.next += 1;
        let name = source.plugin_name();
        let (tree, errors) = match source.read() {
            Ok(t) => (Some(t), Vec::new()),
            Err(e) => (None, vec![e]),
        };
        let err = errors.first().cloned();
        self.specs.push(Spec { id, source, name: name.clone(), tree, errors, running: false });
        match err {
            Some(e) => {
                self.log.push((Level::Error, format!("Plugin {name} failed to load: {e}")));
                Err(e)
            }
            None => {
                self.cmds.push(Cmd::Load(id));
                Ok(id)
            }
        }
    }

    /// Loads every plugin file in `dir`, in Studio's order.
    pub fn load_dir(&mut self, dir: &std::path::Path) -> Vec<Result<PluginId, String>> {
        match source::dir_order(dir) {
            Ok(list) => list.into_iter().map(|src| self.load(src)).collect(),
            Err(e) => {
                self.log.push((Level::Error, format!("Plugins folder: {e}")));
                vec![Err(e)]
            }
        }
    }

    /// Stops and removes a plugin (Unloading fires first, at the next frame).
    pub fn unload(&mut self, id: PluginId) {
        self.cmds.push(Cmd::Unload(id));
    }

    /// Reads the plugin's source again and restarts it (unload, then load).
    pub fn reload(&mut self, id: PluginId) {
        self.cmds.push(Cmd::Reload(id));
    }

    pub fn loaded(&self) -> Vec<PluginInfo> {
        self.specs
            .iter()
            .map(|s| PluginInfo {
                id: s.id,
                name: s.name.clone(),
                source: s.source.clone(),
                running: s.running,
                errors: s.errors.clone(),
            })
            .collect()
    }

    /// `--result`'s "plugins": each plugin's name, source, whether it runs,
    /// and its load / script errors.
    pub fn result_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.specs
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id.0,
                        "name": s.name,
                        "source": s.source.path().map(|p| p.display().to_string()),
                        "running": s.running,
                        "errors": s.errors,
                    })
                })
                .collect(),
        )
    }

    /// Fires a toolbar button's Click or a PluginAction's Triggered by name
    /// at the next frame (the harness's `--action plugin-button:…`).
    pub fn trigger(&mut self, t: Trigger) {
        self.cmds.push(Cmd::Trigger(t));
    }

    /// Output lines from the host and the edit VM since the last call.
    pub fn take_log(&mut self) -> Vec<(Level, String)> {
        std::mem::take(&mut self.log)
    }

    /// What plugins asked of the editor since the last call.
    pub fn take_requests(&mut self) -> Vec<EditorRequest> {
        std::mem::take(&mut self.requests)
    }

    /// True once after a plugin activated or deactivated (the editor drops
    /// its own tool when one activates: `active_plugin`).
    pub fn take_activation_change(&mut self) -> bool {
        std::mem::take(&mut self.activation_changed)
    }

    /// The edit VM's Lua state, for reading plugin UI state between frames
    /// (no document entered: don't call into the runtime with it).
    pub fn edit_lua(&self) -> Option<&mlua::Lua> {
        self.edit.as_ref().map(|e| &e.rt.lua)
    }

    /// The activated plugin in the edit DataModel, if any.
    pub fn active_plugin(&self) -> Option<PluginId> {
        let e = self.edit.as_ref()?;
        e.rt.lua.app_data_ref::<vm::PluginVm>()?.active
    }

    /// The edit VM's plugin UI objects (toolbars, buttons, actions, menus,
    /// widgets, mice) for the UI.
    pub fn ui_items(&self) -> Vec<UiItem> {
        self.edit
            .as_ref()
            .and_then(|e| e.rt.lua.app_data_ref::<vm::PluginVm>().map(|v| v.ui.clone()))
            .unwrap_or_default()
    }

    /// Runs `f` with the edit VM's runtime and the edit document entered
    /// (the UI's entry for input: PluginMouse events, widget input,
    /// button clicks). Output goes to the host's log.
    pub fn with_edit<R>(&mut self, doc: &mut Document, f: impl FnOnce(&Runtime) -> R) -> Option<R> {
        let e = self.edit.as_mut()?;
        if e.doc_id != doc.id {
            return None;
        }
        let mut w = e.world.take()?;
        // Edits the editor made since the last frame reach the plugins
        // first (signals, deleted instances lent back): otherwise the
        // after_frame below would drop their marks, and threads this resumes
        // would find a deleted instance gone.
        let pending = self.sync.take(doc);
        w.doc = Some(std::mem::replace(doc, Document::placeholder()));
        // (the plugins' GUI channel: widgets_ui.rs)
        let _ch = crate::gui::share_channel(1);
        e.rt.enter(w);
        let lent = pending.fire(&e.rt);
        if let Ok(mut w) = crate::runtime::world(&e.rt.lua) {
            for r in lent {
                w.orphans.remove(&r);
            }
        }
        let r = f(&e.rt);
        e.rt.dispatch();
        let mut w = e.rt.leave();
        *doc = w.doc.take().expect("edit document returned");
        self.log.extend(std::mem::take(&mut w.output));
        e.world = Some(w);
        self.sync.after_frame(doc);
        Some(r)
    }

    /// One edit frame: pending loads / unloads, edits made outside the VM
    /// as signals, RunService's edit-mode events (Heartbeat; RenderStepped
    /// and PreRender too unless Play is running, as Studio's edit
    /// DataModel), then due waits. `doc` is the edit document (the saved
    /// one during Play).
    pub fn tick(&mut self, doc: &mut Document, dt: f64, rendering: bool, realtime: bool) {
        if self.specs.is_empty() && self.cmds.is_empty() && self.edit.is_none() {
            return;
        }
        let was_clean = !doc.dirty();
        let rev = doc.revision;
        // A different document: the plugins load again into it.
        if self.edit.as_ref().is_some_and(|e| e.doc_id != doc.id) {
            self.edit = None;
            self.sync.reset();
            script_docs::reset();
            for s in &mut self.specs {
                s.running = false;
            }
            let ids: Vec<PluginId> = self.specs.iter().map(|s| s.id).collect();
            let mut cmds: Vec<Cmd> = ids.into_iter().map(Cmd::Load).collect();
            cmds.append(&mut self.cmds);
            self.cmds = cmds;
        }
        if self.edit.is_none() {
            if self.specs.is_empty() {
                self.cmds.clear();
                return;
            }
            match new_vm(RunMode::Edit) {
                Ok((rt, world)) => {
                    self.edit = Some(EditVm { rt, world: Some(world), doc_id: doc.id, bound: false, gc_t: 0.0 })
                }
                Err(e) => {
                    self.log.push((Level::Error, format!("Could not start the plugin runtime: {e}")));
                    self.cmds.clear();
                    return;
                }
            }
        }
        studio_place_name(doc);
        let pending = self.sync.take(doc);
        let cmds = std::mem::take(&mut self.cmds);
        let specs = &mut self.specs;
        let log = &mut self.log;
        let e = self.edit.as_mut().expect("edit vm");
        let Some(mut w) = e.world.take() else { return };
        let _ch = crate::gui::share_channel(1);
        w.realtime = realtime;
        w.signal_mode = crate::runtime::signal_timing::SignalMode::of(doc);
        w.doc = Some(std::mem::replace(doc, Document::placeholder()));
        e.rt.enter(w);
        if !e.bound {
            e.bound = true;
            if let Err(err) = e.rt.bind_globals().and_then(|_| edit_loadstring(&e.rt)) {
                log.push((Level::Error, crate::runtime::describe_error(&err)));
            }
        }
        let lent = pending.fire(&e.rt);
        let mut loaded_any = false;
        for c in cmds {
            match c {
                Cmd::Load(id) => loaded_any |= run_load(&e.rt, specs, id, log),
                Cmd::Unload(id) => {
                    vm::unload(&e.rt, id);
                    specs.retain(|s| s.id != id);
                }
                Cmd::Reload(id) => {
                    vm::unload(&e.rt, id);
                    if let Some(s) = specs.iter_mut().find(|s| s.id == id) {
                        s.running = false;
                        s.errors.clear();
                        match s.source.read() {
                            Ok(t) => s.tree = Some(t),
                            Err(err) => {
                                log.push((Level::Error, format!("Plugin {} failed to load: {err}", s.name)));
                                s.errors.push(err);
                                s.tree = None;
                            }
                        }
                    }
                    loaded_any |= run_load(&e.rt, specs, id, log);
                }
                Cmd::Trigger(t) => fire_trigger(&e.rt, &t, log),
            }
        }
        if loaded_any {
            vm::start_later(&e.rt);
        }
        let rs = crate::runtime::world(&e.rt.lua).ok().and_then(|w| {
            let d = w.d();
            d.find_child(d.dom.root_ref(), "RunService")
        });
        if let Some(rs) = rs {
            e.rt.fire(rs, "Heartbeat", vec![Value::Number(dt)]);
            if rendering {
                e.rt.fire(rs, "RenderStepped", vec![Value::Number(dt)]);
                e.rt.fire(rs, "PreRender", vec![Value::Number(dt)]);
            }
        }
        e.rt.step(dt);
        // A selected instance a plugin destroyed or unparented leaves the
        // selection, and SelectionChanged fires (Studio: Destroy of a
        // selected part fires once).
        if let Ok(mut w) = crate::runtime::world(&e.rt.lua) {
            let d = w.d();
            let root = d.dom.root_ref();
            let keep: Vec<_> = d.selection.iter().copied().filter(|r| d.is_ancestor(root, *r)).collect();
            if keep.len() != d.selection.len() {
                w.dm().selection = keep;
                drop(w);
                if let Some(mut v) = vm::vm(&e.rt.lua) {
                    v.selection_fires += 1;
                }
            }
        }
        // Selection:Set / Add / Remove of this frame, after the callers yielded.
        let fires = vm::vm(&e.rt.lua).map(|mut v| std::mem::take(&mut v.selection_fires)).unwrap_or(0);
        if fires > 0 {
            let sel = crate::runtime::world(&e.rt.lua).ok().and_then(|w| {
                let d = w.d();
                d.find_child(d.dom.root_ref(), "Selection")
            });
            if let Some(sel) = sel {
                for _ in 0..fires {
                    e.rt.fire(sel, "SelectionChanged", vec![]);
                }
                e.rt.dispatch();
            }
        }
        // The person switched rustudio's theme: settings().Studio.ThemeChanged.
        match crate::runtime::domains::plugin::theme::watch_theme(&e.rt.lua) {
            Ok(true) => e.rt.dispatch(),
            Ok(false) => {}
            Err(err) => log.push((Level::Error, crate::runtime::describe_error(&err))),
        }
        // Source writes that waited for a script-injection prompt.
        if permissions::retry_source_writes(&e.rt.lua) {
            e.rt.dispatch();
        }
        match crate::runtime::domains::plugin::script_editor::fire_events(&e.rt.lua) {
            Ok(true) => e.rt.dispatch(),
            Ok(false) => {}
            Err(err) => log.push((Level::Error, crate::runtime::describe_error(&err))),
        }
        match crate::runtime::domains::plugin::studio::apply_pending_tool(&e.rt.lua) {
            Ok(true) => e.rt.dispatch(),
            Ok(false) => {}
            Err(err) => log.push((Level::Error, crate::runtime::describe_error(&err))),
        }
        // ChangeHistoryService's OnUndo / OnRedo / recording events of this
        // frame (plugins' and the editor's Ctrl+Z), after the callers yielded.
        match crate::runtime::domains::plugin::history::fire_events(&e.rt.lua) {
            Ok(true) => e.rt.dispatch(),
            Ok(false) => {}
            Err(err) => log.push((Level::Error, crate::runtime::describe_error(&err))),
        }
        // Instances the editor removed stay lent (in the document,
        // parentless): Studio's deleted instances are still there for a
        // plugin holding them (Parent nil, properties readable); undo / redo
        // take them from there (doc.rs), and the change history pins them.
        let lent: Vec<_> = lent.into_iter().chain(crate::runtime::domains::plugin::history::take_lent(&e.rt.lua)).collect();
        if let Ok(mut w) = crate::runtime::world(&e.rt.lua) {
            for r in lent {
                w.orphans.remove(&r);
            }
        }
        e.gc_t += dt;
        if e.gc_t > 2.0 {
            e.gc_t = 0.0;
            let mut pins = vm::pinned(&e.rt.lua);
            // (What the change history can still bring back stays.)
            if let Ok(w) = crate::runtime::world(&e.rt.lua) {
                pins.extend(w.d().history_refs());
            }
            e.rt.collect(&|r| pins.contains(&r), crate::mech::GC_BUDGET);
        }
        e.rt.gc_step(dt);
        let (reqs, seq) = match vm::vm(&e.rt.lua) {
            Some(mut v) => (std::mem::take(&mut v.requests), v.activation_seq),
            None => (Vec::new(), 0),
        };
        let mut w = e.rt.leave();
        *doc = w.doc.take().expect("edit document returned");
        let out = std::mem::take(&mut w.output);
        e.world = Some(w);
        attribute_errors(specs, &out);
        log.extend(out);
        self.requests.extend(reqs);
        if seq != self.activation_seen {
            self.activation_seen = seq;
            self.activation_changed = true;
        }
        let touched = self.sync.after_frame(doc);
        self.sync.note_selection(doc);
        // Plugin bookkeeping (its own instances, out of the DataModel)
        // doesn't make the place unsaved.
        if was_clean && doc.revision != rev && !touched {
            doc.mark_saved_at(doc.revision);
        }
    }

    /// Loads every running plugin into a Play DataModel's runtime (call
    /// with that side entered, after bind_globals), as Studio loads them in
    /// the Play server and client. Their Output is the side's.
    pub fn load_into_play(&self, rt: &Runtime) {
        if !self.enabled {
            return;
        }
        let mut any = false;
        for s in self.specs.iter().filter(|s| s.running) {
            if let Some(spec) = s.load_spec() {
                if let Err(e) = vm::load(rt, &spec) {
                    if let Ok(mut w) = crate::runtime::world(&rt.lua) {
                        w.output.push((Level::Error, format!("Plugin {} failed to load: {e}", s.name)));
                    }
                }
                any = true;
            }
        }
        if any {
            vm::start_later(rt);
            rt.dispatch();
        }
    }

    /// The Play DataModel is closing: its plugins get Unloading (Studio
    /// unloads plugins with their DataModel).
    pub fn unload_from_play(&self, rt: &Runtime) {
        let ids: Vec<PluginId> = rt
            .lua
            .app_data_ref::<vm::PluginVm>()
            .map(|v| v.plugins.iter().map(|p| p.id).collect())
            .unwrap_or_default();
        for id in ids {
            vm::unload(rt, id);
        }
    }
}

/// The edit DataModel's Name as Studio's (measured in Studio: "Place1" for a
/// new place; a file's name for a file, a Roblox place's title for one);
/// Play's copies are "Game" (doc.rs clone_for_play). Not journaled: it is
/// not part of the place.
fn studio_place_name(doc: &mut Document) {
    let name = match (&doc.path, &doc.cloud) {
        // (a local file: its name without the extension; unmeasured)
        (Some(p), _) => p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Place1".into()),
        (None, Some(_)) => doc.title.clone(),
        (None, None) => "Place1".into(),
    };
    let root = doc.dom.root_ref();
    if let Some(i) = doc.dom.get_by_ref_mut(root) {
        if i.name != name {
            i.name = name;
        }
    }
}

/// loadstring works for plugins in the edit DataModel (measured in Studio:
/// `return 1+1` → 2 there; Play DataModels keep refusing it).
fn edit_loadstring(rt: &Runtime) -> mlua::Result<()> {
    let lua = &rt.lua;
    let f = lua.create_function(|lua, (src, name): (mlua::LuaString, Option<String>)| {
        if crate::runtime::security::current(lua) != crate::runtime::security::PLUGIN_IDENT {
            return Err(mlua::Error::runtime("loadstring() is not available"));
        }
        let src = src.to_str()?.to_string();
        let env = crate::globals_parity::new_env(lua, None)?;
        let chunk = lua.load(&src).set_name(name.unwrap_or_else(|| src.clone())).set_environment(env);
        match chunk.into_function() {
            Ok(f) => Ok((Value::Function(f), Value::Nil)),
            Err(e) => Ok((Value::Nil, Value::String(lua.create_string(crate::runtime::describe_error(&e))?))),
        }
    })?;
    let shared: mlua::Table = lua.named_registry_value("rustudio_shared")?;
    shared.raw_set("loadstring", f.clone())?;
    lua.globals().raw_set("loadstring", f)?;
    Ok(())
}

fn new_vm(mode: RunMode) -> Result<(Runtime, World), String> {
    let rt = Runtime::new()?;
    rt.install_side().map_err(|e| crate::runtime::describe_error(&e))?;
    vm::install(&rt.lua);
    let mut w = World::new(Document::empty(), mode);
    w.doc = None;
    Ok((rt, w))
}

/// Loads one spec into the entered edit VM; true when it started.
fn run_load(rt: &Runtime, specs: &mut [Spec], id: PluginId, log: &mut Vec<(Level, String)>) -> bool {
    let Some(s) = specs.iter_mut().find(|s| s.id == id) else { return false };
    let Some(spec) = s.load_spec() else { return false };
    match vm::load(rt, &spec) {
        Ok(_) => {
            s.running = true;
            rt.dispatch();
            true
        }
        Err(e) => {
            log.push((Level::Error, format!("Plugin {} failed to load: {e}", s.name)));
            s.errors.push(e);
            false
        }
    }
}

/// Error lines in Output that come from a plugin's code (its name starts
/// the message's script path, or a stack line's).
fn attribute_errors(specs: &mut [Spec], out: &[(Level, String)]) {
    // An error without a script path (an engine error) waits for its first
    // stack line, which names the script.
    let mut pending: Option<String> = None;
    for (level, text) in out {
        let t = text.trim_start_matches(crate::output_panel::STACK_MARK);
        let msg = t.split("  —").next().unwrap_or(t).to_string();
        if *level == Level::Error {
            pending = None;
            match specs.iter().position(|s| t.starts_with(&format!("{}.", s.name))) {
                Some(i) => specs[i].errors.push(msg),
                None => pending = Some(msg),
            }
        } else if let Some(rest) = t.strip_prefix("Script '") {
            if let Some(m) = pending.take() {
                if let Some(i) = specs.iter().position(|s| rest.starts_with(&format!("{}.", s.name))) {
                    specs[i].errors.push(m);
                }
            }
        } else if !t.starts_with("Stack Begin") {
            pending = None;
        }
    }
}

/// Fires a harness trigger in the entered VM.
fn fire_trigger(rt: &Runtime, t: &Trigger, log: &mut Vec<(Level, String)>) {
    let items = vm::vm(&rt.lua).map(|v| v.ui.clone()).unwrap_or_default();
    let toolbar_name = |r: Ref| {
        items.iter().find_map(|u| match &u.kind {
            UiKind::Toolbar { name } if u.r == r => Some(name.clone()),
            _ => None,
        })
    };
    if let Trigger::Menu(choice) = t {
        let answered = match crate::runtime::world(&rt.lua) {
            Ok(w) => crate::runtime::domains::plugin::menus::answer_top(&rt.lua, w.d(), choice),
            Err(_) => false,
        };
        if !answered {
            log.push((Level::Warn, format!("--action: no plugin menu is open to answer {choice:?}")));
        }
        return;
    }
    if let Trigger::Deactivate(id) = t {
        let root = vm::vm(&rt.lua).and_then(|v| v.by_id(*id).map(|p| p.root));
        if let Some(root) = root {
            if let Err(e) = crate::runtime::domains::plugin::deactivate(&rt.lua, root) {
                log.push((Level::Error, crate::runtime::describe_error(&e)));
            }
            rt.dispatch();
        }
        return;
    }
    if let Trigger::Fire(r, ev) = t {
        // RUSTUDIO_PLUGIN_TRACE: each UI fire and whether anything listens
        // on the object (checks/plugins/ui/check.py's corpus clicks).
        if std::env::var_os("RUSTUDIO_PLUGIN_TRACE").is_some() {
            let listening = crate::runtime::world(&rt.lua).is_ok_and(|w| w.listening.contains(r));
            log.push((Level::Info, format!("plugin-ui: {ev} {r:?} listening {listening}")));
        }
        rt.fire(*r, ev, vec![]);
        rt.dispatch();
        return;
    }
    let target = match t {
        Trigger::Button(tb, b) => items
            .iter()
            .find(|u| match &u.kind {
                UiKind::Button { toolbar, id, text, .. } => {
                    // ("*": any toolbar, for the harness)
                    (tb == "*" || toolbar_name(*toolbar).as_deref() == Some(tb.as_str())) && (id == b || text == b)
                }
                _ => false,
            })
            .map(|u| (u.r, "Click")),
        Trigger::Action(a) => items
            .iter()
            .find(|u| {
                matches!(&u.kind, UiKind::Action { id, .. } if id == a)
                    || crate::runtime::world(&rt.lua).is_ok_and(|w| {
                        matches!(w.d().prop(u.r, "ActionId"), Some(rbx_dom_weak::types::Variant::String(s)) if s == *a)
                    })
            })
            .map(|u| (u.r, "Triggered")),
        Trigger::Menu(_) | Trigger::Fire(..) | Trigger::Deactivate(_) => None,
    };
    match target {
        Some((r, ev)) => {
            rt.fire(r, ev, vec![]);
            rt.dispatch();
        }
        None => log.push((Level::Warn, format!("--action: no plugin {t:?} to fire"))),
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod ui_tests;
#[cfg(test)]
#[path = "tests_services.rs"]
mod tests_services;
#[cfg(test)]
#[path = "tests_webstream.rs"]
mod tests_webstream;
