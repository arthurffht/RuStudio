//! OmniMCP — unrestricted AI game engine control bridge for RuStudio.

pub mod protocol;
pub mod resources_prompts;
pub mod server;
pub mod tools_dom;
pub mod tools_editor;
pub mod tools_luau;
pub mod tools_physics_world;
pub mod tools_play;
pub mod tools_project_cloud;
pub mod tools_scripts;
pub mod tools_session;
pub mod tools_terrain;
pub mod tools_viewport;
pub mod variants;
#[cfg(test)]
mod offline_catalog;

use std::sync::mpsc::Sender;

use protocol::{BridgeResponse, ToolCallResult, ToolContent, ToolDef};
use server::CallOutcome;

/// Returns all OmniMCP tool schemas across every domain plus `omni_batch`.
pub fn all_schemas() -> Vec<ToolDef> {
    let mut out = Vec::new();
    out.push(omni_batch_schema());
    out.extend(tools_session::schemas());
    out.extend(tools_dom::schemas());
    out.extend(tools_scripts::schemas());
    out.extend(tools_luau::schemas());
    out.extend(tools_play::schemas());
    out.extend(tools_physics_world::schemas());
    out.extend(tools_terrain::schemas());
    out.extend(tools_viewport::schemas());
    out.extend(tools_editor::schemas());
    out.extend(tools_project_cloud::schemas());
    out
}

fn omni_batch_schema() -> ToolDef {
    ToolDef {
        name: "omni_batch".into(),
        description: "Execute multiple synchronous OmniMCP tool calls sequentially within a single engine frame.".into(),
        input_schema: serde_json::json!({
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
    }
}

/// Dispatches an OmniMCP tool call into the appropriate tool module (or `omni_batch`).
pub fn dispatch_tool(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    name: &str,
    args: &serde_json::Value,
    reply: &Sender<BridgeResponse>,
) -> CallOutcome {
    if name == "omni_batch" {
        return CallOutcome::Immediate(run_omni_batch(ed, ctx, args));
    }
    dispatch_domain(ed, ctx, name, args, reply)
}

fn dispatch_domain(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    name: &str,
    args: &serde_json::Value,
    reply: &Sender<BridgeResponse>,
) -> CallOutcome {
    if let Some(o) = tools_session::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_dom::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_scripts::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_luau::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_play::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_physics_world::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_terrain::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_viewport::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_editor::call(ed, ctx, name, args, reply) { return o; }
    if let Some(o) = tools_project_cloud::call(ed, ctx, name, args, reply) { return o; }
    CallOutcome::Immediate(ToolCallResult::error(format!("unknown tool: {name}")))
}

fn run_omni_batch(
    ed: &mut crate::app::Editor,
    ctx: &egui::Context,
    args: &serde_json::Value,
) -> ToolCallResult {
    let Some(calls) = args.get("calls").and_then(|v| v.as_array()) else {
        return ToolCallResult::error("omni_batch requires 'calls' array");
    };
    let stop_on_error = args
        .get("stop_on_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut results = Vec::with_capacity(calls.len());
    let mut any_error = false;

    for (index, item) in calls.iter().enumerate() {
        let Some(sub_name) = item.get("name").and_then(|v| v.as_str()) else {
            any_error = true;
            results.push(serde_json::json!({
                "index": index,
                "error": "batch item missing 'name'",
                "isError": true,
            }));
            if stop_on_error {
                break;
            }
            continue;
        };
        if sub_name == "omni_batch" {
            any_error = true;
            results.push(serde_json::json!({
                "index": index,
                "name": sub_name,
                "error": "nested omni_batch is not allowed",
                "isError": true,
            }));
            if stop_on_error {
                break;
            }
            continue;
        }
        let sub_args = item
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        let (sub_tx, _sub_rx) = std::sync::mpsc::channel();
        let outcome = dispatch_domain(ed, ctx, sub_name, &sub_args, &sub_tx);
        match outcome {
            CallOutcome::Immediate(res) => {
                if res.is_error {
                    any_error = true;
                }
                let parsed_content: Vec<serde_json::Value> = res
                    .content
                    .iter()
                    .map(|c| match c {
                        ToolContent::Text { text } => {
                            serde_json::from_str::<serde_json::Value>(text)
                                .unwrap_or_else(|_| serde_json::Value::String(text.clone()))
                        }
                        ToolContent::Image { data, mime_type } => serde_json::json!({
                            "type": "image",
                            "mimeType": mime_type,
                            "bytes": data.len(),
                        }),
                    })
                    .collect();
                results.push(serde_json::json!({
                    "index": index,
                    "name": sub_name,
                    "isError": res.is_error,
                    "result": if parsed_content.len() == 1 {
                        parsed_content[0].clone()
                    } else {
                        serde_json::Value::Array(parsed_content)
                    },
                }));
                if res.is_error && stop_on_error {
                    break;
                }
            }
            CallOutcome::Deferred => {
                any_error = true;
                results.push(serde_json::json!({
                    "index": index,
                    "name": sub_name,
                    "isError": true,
                    "error": format!("tool '{sub_name}' is asynchronous and cannot run inside omni_batch"),
                }));
                if stop_on_error {
                    break;
                }
            }
        }
    }

    let mut out = ToolCallResult::json(&serde_json::json!({
        "count": results.len(),
        "results": results,
    }));
    out.is_error = any_error && stop_on_error;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn schema_and_offline_catalog_have_100_percent_parity() {
        let live = all_schemas();
        let offline = offline_catalog::engine_tools();
        assert_eq!(live.len(), 53, "expected 53 OmniMCP engine tools");
        assert_eq!(
            offline.len(),
            live.len(),
            "offline catalog count mismatch ({} vs {})",
            offline.len(),
            live.len()
        );

        let mut seen = HashSet::new();
        for (idx, (l, o)) in live.iter().zip(offline.iter()).enumerate() {
            assert!(
                seen.insert(l.name.clone()),
                "duplicate tool name in all_schemas(): {}",
                l.name
            );
            assert_eq!(l.name, o.name, "tool name mismatch at index {idx}");
            assert_eq!(
                l.description, o.description,
                "description mismatch for tool '{}'",
                l.name
            );
            assert_eq!(
                l.input_schema, o.input_schema,
                "inputSchema mismatch for tool '{}'",
                l.name
            );
        }

        let live_res = resources_prompts::resources();
        let off_res = offline_catalog::engine_resources();
        assert_eq!(live_res.len(), 6);
        assert_eq!(off_res.len(), live_res.len());
        for (l, o) in live_res.iter().zip(off_res.iter()) {
            assert_eq!(l.uri, o.uri);
            assert_eq!(l.name, o.name);
            assert_eq!(l.description, o.description);
            assert_eq!(l.mime_type, o.mime_type);
        }

        let live_prompts = resources_prompts::prompts();
        let off_prompts = offline_catalog::engine_prompts();
        assert_eq!(live_prompts.len(), 4);
        assert_eq!(off_prompts.len(), live_prompts.len());
        for (l, o) in live_prompts.iter().zip(off_prompts.iter()) {
            assert_eq!(l.name, o.name);
            assert_eq!(l.description, o.description);
            assert_eq!(l.arguments.len(), o.arguments.len());
            for (la, oa) in l.arguments.iter().zip(o.arguments.iter()) {
                assert_eq!(la.name, oa.name);
                assert_eq!(la.description, oa.description);
                assert_eq!(la.required, oa.required);
            }
        }
    }
}
