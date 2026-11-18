//! Shared OmniMCP JSON-RPC protocol types, TCP bridge messages, and
//! Windows instance registry (`%LOCALAPPDATA%\rustudio\mcp-instances\<pid>.json`).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// MCP tool definition exposed via `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// MCP resource definition exposed via `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDef {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// MCP resource contents returned by `resources/read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// MCP prompt argument definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

/// MCP prompt definition exposed via `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDef {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

/// MCP prompt message returned by `prompts/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: ToolContent,
}

/// Content item inside a tool call result or prompt message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// Result returned by `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub content: Vec<ToolContent>,
    #[serde(rename = "isError", default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ToolCallResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            is_error: false,
        }
    }

    pub fn json(value: &serde_json::Value) -> Self {
        let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
        Self::text(text)
    }

    pub fn image_png(base64_png: String, caption: Option<String>) -> Self {
        let mut content = Vec::with_capacity(2);
        if let Some(c) = caption {
            content.push(ToolContent::Text { text: c });
        }
        content.push(ToolContent::Image {
            data: base64_png,
            mime_type: "image/png".into(),
        });
        Self {
            content,
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text {
                text: message.into(),
            }],
            is_error: true,
        }
    }
}

/// Request sent over localhost TCP from `rustudio-mcp` to a running `rustudio`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BridgeRequest {
    Ping,
    ListTools,
    CallTool {
        name: String,
        #[serde(default)]
        arguments: serde_json::Value,
    },
    ListResources,
    ReadResource {
        uri: String,
    },
    ListPrompts,
    GetPrompt {
        name: String,
        #[serde(default)]
        arguments: serde_json::Map<String, serde_json::Value>,
    },
}

/// Response returned over localhost TCP from `rustudio` to `rustudio-mcp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BridgeResponse {
    Pong {
        pid: u32,
        port: u16,
        title: String,
        mode: String,
    },
    Tools {
        tools: Vec<ToolDef>,
    },
    ToolResult(ToolCallResult),
    Resources {
        resources: Vec<ResourceDef>,
    },
    ResourceResult {
        contents: Vec<ResourceContent>,
    },
    Prompts {
        prompts: Vec<PromptDef>,
    },
    PromptResult {
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        messages: Vec<PromptMessage>,
    },
    Error {
        code: i32,
        message: String,
    },
}

/// Inbound JSON-RPC 2.0 request (used by `rustudio-mcp` stdio proxy).
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Outbound JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Outbound JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Discovery record written by each running `rustudio` instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceRecord {
    pub pid: u32,
    pub port: u16,
    pub title: String,
    pub place_path: Option<String>,
    pub mode: String,
    pub started_unix: u64,
}

/// Directory storing live OmniMCP instance records (`%LOCALAPPDATA%\rustudio\mcp-instances`).
pub fn instances_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rustudio").join("mcp-instances")
}

/// File path for a given PID's instance record inside `instances_dir()`.
pub fn record_path(pid: u32) -> PathBuf {
    instances_dir().join(format!("{pid}.json"))
}

/// Checks whether a Windows process with `pid` is currently alive.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Atomically writes or refreshes an `InstanceRecord` at `<pid>.json`.
pub fn write_instance_record(rec: &InstanceRecord) -> std::io::Result<PathBuf> {
    let dir = instances_dir();
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{}.json", rec.pid));
    let tmp = dir.join(format!("{}.json.tmp", rec.pid));
    let data = serde_json::to_vec_pretty(rec).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, data)?;
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Removes the `<pid>.json` instance record if present.
pub fn remove_instance_record(pid: u32) {
    let _ = std::fs::remove_file(record_path(pid));
}

/// Lists all currently alive `rustudio` instances from `instances_dir()`,
/// automatically pruning stale records whose processes have exited.
pub fn list_live_instances() -> Vec<InstanceRecord> {
    list_live_instances_in(&instances_dir())
}

pub fn list_live_instances_in(dir: &std::path::Path) -> Vec<InstanceRecord> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(rec) = serde_json::from_slice::<InstanceRecord>(&bytes) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if !is_process_alive(rec.pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        out.push(rec);
    }
    out.sort_by(|a, b| b.started_unix.cmp(&a.started_unix).then_with(|| a.pid.cmp(&b.pid)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive_and_bogus_is_dead() {
        assert!(is_process_alive(std::process::id()));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(u32::MAX - 3));
    }

    #[test]
    fn stale_instance_records_are_pruned() {
        let dir = std::env::temp_dir().join(format!("rustudio-mcp-inst-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let alive = InstanceRecord {
            pid: std::process::id(),
            port: 44777,
            title: "TestPlace.rbxl".into(),
            place_path: Some(r"C:\Places\TestPlace.rbxl".into()),
            mode: "Edit".into(),
            started_unix: 100,
        };
        let dead = InstanceRecord {
            pid: u32::MAX - 3,
            port: 44778,
            title: "Dead.rbxl".into(),
            place_path: None,
            mode: "Edit".into(),
            started_unix: 200,
        };
        std::fs::write(dir.join(format!("{}.json", alive.pid)), serde_json::to_vec(&alive).unwrap()).unwrap();
        std::fs::write(dir.join(format!("{}.json", dead.pid)), serde_json::to_vec(&dead).unwrap()).unwrap();

        let live = list_live_instances_in(&dir);
        assert_eq!(live, vec![alive.clone()]);
        assert!(!dir.join(format!("{}.json", dead.pid)).exists());
        assert!(dir.join(format!("{}.json", alive.pid)).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_roundtrip() {
        let req = BridgeRequest::CallTool {
            name: "get_tree".into(),
            arguments: serde_json::json!({"depth": 2}),
        };
        let text = serde_json::to_string(&req).unwrap();
        let back: BridgeRequest = serde_json::from_str(&text).unwrap();
        match back {
            BridgeRequest::CallTool { name, arguments } => {
                assert_eq!(name, "get_tree");
                assert_eq!(arguments["depth"], 2);
            }
            _ => panic!("unexpected variant"),
        }

        let res = ToolCallResult::json(&serde_json::json!({"ok": true}));
        let resp = BridgeResponse::ToolResult(res);
        let rtext = serde_json::to_string(&resp).unwrap();
        let rback: BridgeResponse = serde_json::from_str(&rtext).unwrap();
        assert!(matches!(rback, BridgeResponse::ToolResult(_)));
    }
}
