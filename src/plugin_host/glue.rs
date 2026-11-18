//! The editor's side of the plugin host: the per-frame tick, Output, the
//! requests plugins make of the editor, and the Play session's plugin copies.

use crate::app::{Editor, Level};

impl Editor {
    /// Once a frame, after the Play tick: the edit DataModel's plugins run
    /// (on the saved edit document while Play runs, without RenderStepped).
    pub fn plugin_tick(&mut self, dt: f64) {
        // --plugin / --plugins-dir / --no-plugins, once the place is open.
        if !self.harness.plugins_started && !self.place_loading() {
            self.harness.plugins_started = true;
            // Studio is signed in before plugins run (GetStudioUserId, the
            // installed list): a --no-ui run lets the session check answer
            // first, as Play's start does (at most 10 s).
            if self.harness.no_ui {
                for (level, text) in self.account.settle_for_play(std::time::Duration::from_millis(50), false) {
                    self.push_log(level, text);
                }
            }
            if self.harness.no_plugins {
                self.plugins.set_enabled(false);
            } else {
                for d in self.harness.plugin_dirs.clone() {
                    let _ = self.plugins.load_dir(&d);
                }
                for (asset_id, path) in self.harness.installed_plugins.clone() {
                    let _ = self.plugins.load(super::PluginSource::Cloud { asset_id, path });
                }
                for f in self.harness.plugin_files.clone() {
                    let _ = self.plugins.load(super::PluginSource::File(f));
                }
            }
        }
        if self.plugins.is_idle() || self.place_loading() {
            self.plugin_ui_sync();
            return;
        }
        // Plugin GUI / viewport input since the last tick (widgets_ui.rs).
        self.plugin_ui.dt = dt as f32;
        self.plugin_ui_flush();
        // (Before the session check answers: the cached signed-in profile, as
        // the plugin store uses; Studio has the id before plugins run.)
        let uid = self.account.user_id().unwrap_or_else(|| {
            if std::env::var_os("RUSTUDIO_NO_SESSION").is_some() { 0 } else { crate::plugin_store::current_user_id() }
        });
        super::settings::set_user_id(uid);
        // Plugin.GridSize / GetJoinMode read the editor's toolbar settings
        // (Play keeps them current while it runs).
        if self.play.is_none() {
            crate::runtime::domains::testing::set_editor_state(crate::runtime::domains::testing::EditorState {
                snap_move: self.snap_move,
                move_inc: self.move_inc,
                snap_rotate: self.snap_rotate,
                rotate_inc: self.rotate_inc,
                local_space: self.local_space,
                collisions: self.collisions,
                join_surfaces: self.join_surfaces,
            });
        }
        // GetSelectedRibbonTool reads the editor's tool; while a plugin is
        // the active tool it keeps the last ribbon tool.
        if self.plugins.active_plugin().is_none() && !matches!(self.tool, crate::app::Tool::None) {
            crate::runtime::domains::plugin::studio::set_ribbon_tool(self.tool.label());
        }
        // Headless with no prompt answers scheduled: permission prompts are
        // refused instead of waiting forever.
        let answers = self.harness.actions.iter().any(|(_, a)| matches!(a, crate::app::HarnessAction::UiAction(n) if n.starts_with("plugin-prompt:")));
        super::permissions::set_auto_deny(self.harness.no_ui && !answers);
        let rendering = self.play.is_none();
        let realtime = self.fixed_dt.is_none() && !crate::sim_clock::on();
        match self.play_saved_doc_mut() {
            Some(doc) => {
                // (Play holds the edit document meanwhile.)
                let doc: *mut crate::doc::Document = doc;
                // SAFETY: `doc` lives in self.play, which plugin_tick
                // doesn't touch while the host runs.
                self.plugins.tick(unsafe { &mut *doc }, dt, rendering, realtime);
            }
            None => self.plugins.tick(&mut self.doc, dt, rendering, realtime),
        }
        self.plugin_ui_after_tick();
        self.plugin_output();
        self.plugin_ui_sync();
    }

    /// Host and plugin Output lines into the Output panel (and --log).
    pub fn plugin_output(&mut self) {
        for n in super::permissions::take_notes() {
            self.push_log(Level::Warn, n);
        }
        for (level, text) in self.plugins.take_log() {
            self.push_log(level, text);
        }
        // ScriptEditorService / plugin:OpenScript: show or close tabs.
        for req in super::script_docs::take_requests() {
            match req {
                super::script_docs::EditorReq::Show(script, line) => {
                    if self.play.is_none() {
                        self.open_script.push(script);
                        if let Some(l) = line {
                            super::script_docs::set_selection(script, (l.max(1), 1, l.max(1), 1), false);
                        }
                    }
                }
                super::script_docs::EditorReq::Close(script) => self.close_script.push(script),
                super::script_docs::EditorReq::Select(..) => {}
            }
        }
        for req in self.plugins.take_requests() {
            match req {
                super::EditorRequest::OpenScript { script, line } => {
                    super::script_docs::open(script, Some(line), false);
                }
                super::EditorRequest::OpenWikiPage(url) => {
                    self.push_log(Level::Info, format!("A plugin opened {url}"));
                    if !self.harness.no_ui {
                        super::prompts::open_url_later(url);
                    }
                }
                // The ribbon's tool changes (Studio's Group / Ungroup act
                // on the selection; the color / material pickers have no
                // rustudio tool and only report back).
                super::EditorRequest::SelectRibbonTool { tool } => {
                    use crate::app::Tool;
                    match tool.as_str() {
                        "Select" => self.tool = Tool::Select,
                        "Move" => self.tool = Tool::Move,
                        "Scale" => self.tool = Tool::Scale,
                        "Rotate" => self.tool = Tool::Rotate,
                        "Transform" => self.tool = Tool::Transform,
                        "None" => self.tool = Tool::None,
                        "Group" => self.run_action("group"),
                        "Ungroup" => self.run_action("ungroup"),
                        _ => {}
                    }
                }
            }
        }
    }
}
