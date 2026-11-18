//! MCP Resources and Prompts (populated in Stage 2).

use super::protocol::{PromptDef, PromptMessage, ResourceContent, ResourceDef};

pub fn resources() -> Vec<ResourceDef> {
    Vec::new()
}

pub fn read_resource(
    _ed: &mut crate::app::Editor,
    uri: &str,
) -> Result<Vec<ResourceContent>, String> {
    Err(format!("unknown resource: {uri}"))
}

pub fn prompts() -> Vec<PromptDef> {
    Vec::new()
}

pub fn get_prompt(
    _ed: &mut crate::app::Editor,
    name: &str,
    _args: &serde_json::Map<String, serde_json::Value>,
) -> Result<(Option<String>, Vec<PromptMessage>), String> {
    Err(format!("unknown prompt: {name}"))
}
