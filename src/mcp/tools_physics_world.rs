//! Physics, raycasting, spatial query & CSG tools (populated in Stage 2).

use std::sync::mpsc::Sender;
use super::protocol::{BridgeResponse, ToolDef};
use super::server::CallOutcome;

pub fn schemas() -> Vec<ToolDef> {
    Vec::new()
}

pub fn call(
    _ed: &mut crate::app::Editor,
    _ctx: &egui::Context,
    _name: &str,
    _args: &serde_json::Value,
    _reply: &Sender<BridgeResponse>,
) -> Option<CallOutcome> {
    None
}
