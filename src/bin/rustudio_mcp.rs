//! `rustudio-mcp`: stdio MCP (JSON-RPC 2.0) proxy bridging AI clients to a
//! running `rustudio` OmniMCP TCP server on 127.0.0.1.
//!
//! Intentionally compiled WITHOUT `#![windows_subsystem = "windows"]` so
//! stdin/stdout JSON-RPC communication works cleanly.

#[path = "../mcp/protocol.rs"]
mod protocol;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use protocol::{
    list_live_instances, BridgeRequest, BridgeResponse, JsonRpcRequest, JsonRpcResponse,
};

fn main() {
    let mut explicit_port: Option<u16> = None;
    let mut explicit_pid: Option<u32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                explicit_port = args.next().and_then(|s| s.parse().ok());
            }
            "--pid" => {
                explicit_pid = args.next().and_then(|s| s.parse().ok());
            }
            "--list-instances" => {
                let instances = list_live_instances();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&instances).unwrap_or_else(|_| "[]".into())
                );
                return;
            }
            "-h" | "--help" => {
                println!(
                    "rustudio-mcp — OmniMCP stdio proxy for RuStudio\n\n\
                     usage: rustudio-mcp [--port PORT | --pid PID | --list-instances]"
                );
                return;
            }
            _ => {}
        }
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in BufReader::new(stdin.lock()).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::err(
                    serde_json::Value::Null,
                    -32700,
                    format!("parse error: {e}"),
                );
                write_json_line(&mut out, &resp);
                continue;
            }
        };

        // Notifications have no id and expect no response.
        let Some(id) = req.id.clone() else {
            continue;
        };

        let resp = handle_rpc(id, &req.method, &req.params, explicit_port, explicit_pid);
        write_json_line(&mut out, &resp);
    }
}

fn write_json_line(out: &mut impl Write, resp: &JsonRpcResponse) {
    if let Ok(line) = serde_json::to_string(resp) {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn handle_rpc(
    id: serde_json::Value,
    method: &str,
    params: &serde_json::Value,
    explicit_port: Option<u16>,
    explicit_pid: Option<u32>,
) -> JsonRpcResponse {
    match method {
        "initialize" => {
            let client_ver = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            JsonRpcResponse::ok(
                id,
                serde_json::json!({
                    "protocolVersion": client_ver,
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": true },
                        "prompts": { "listChanged": true }
                    },
                    "serverInfo": {
                        "name": "rustudio-omnimcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
        }
        "ping" => JsonRpcResponse::ok(id, serde_json::json!({})),
        "tools/list" => match send_bridge(explicit_port, explicit_pid, BridgeRequest::ListTools) {
            Ok(BridgeResponse::Tools { tools }) => {
                JsonRpcResponse::ok(id, serde_json::json!({ "tools": tools }))
            }
            Ok(BridgeResponse::Error { code, message }) => JsonRpcResponse::err(id, code, message),
            Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
            Err(e) => JsonRpcResponse::err(id, -32001, e),
        },
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                return JsonRpcResponse::err(id, -32602, "missing tool 'name'");
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match send_bridge(
                explicit_port,
                explicit_pid,
                BridgeRequest::CallTool {
                    name: name.to_string(),
                    arguments,
                },
            ) {
                Ok(BridgeResponse::ToolResult(res)) => {
                    JsonRpcResponse::ok(id, serde_json::to_value(res).unwrap_or_default())
                }
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        "resources/list" => {
            match send_bridge(explicit_port, explicit_pid, BridgeRequest::ListResources) {
                Ok(BridgeResponse::Resources { resources }) => {
                    JsonRpcResponse::ok(id, serde_json::json!({ "resources": resources }))
                }
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(|v| v.as_str()) else {
                return JsonRpcResponse::err(id, -32602, "missing resource 'uri'");
            };
            match send_bridge(
                explicit_port,
                explicit_pid,
                BridgeRequest::ReadResource {
                    uri: uri.to_string(),
                },
            ) {
                Ok(BridgeResponse::ResourceResult { contents }) => {
                    JsonRpcResponse::ok(id, serde_json::json!({ "contents": contents }))
                }
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        "prompts/list" => {
            match send_bridge(explicit_port, explicit_pid, BridgeRequest::ListPrompts) {
                Ok(BridgeResponse::Prompts { prompts }) => {
                    JsonRpcResponse::ok(id, serde_json::json!({ "prompts": prompts }))
                }
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        "prompts/get" => {
            let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                return JsonRpcResponse::err(id, -32602, "missing prompt 'name'");
            };
            let arguments = params
                .get("arguments")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            match send_bridge(
                explicit_port,
                explicit_pid,
                BridgeRequest::GetPrompt {
                    name: name.to_string(),
                    arguments,
                },
            ) {
                Ok(BridgeResponse::PromptResult {
                    description,
                    messages,
                }) => JsonRpcResponse::ok(
                    id,
                    serde_json::json!({
                        "description": description,
                        "messages": messages,
                    }),
                ),
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        _ => JsonRpcResponse::err(id, -32601, format!("method not found: {method}")),
    }
}

fn target_port(explicit_port: Option<u16>, explicit_pid: Option<u32>) -> Result<u16, String> {
    if let Some(p) = explicit_port {
        return Ok(p);
    }
    if let Ok(env_port) = std::env::var("RUSTUDIO_MCP_PORT") {
        if let Ok(p) = env_port.trim().parse::<u16>() {
            return Ok(p);
        }
    }
    let live = list_live_instances();
    if let Some(pid) = explicit_pid {
        return live
            .into_iter()
            .find(|r| r.pid == pid)
            .map(|r| r.port)
            .ok_or_else(|| format!("no live RuStudio instance with pid {pid}"));
    }
    live.into_iter()
        .next()
        .map(|r| r.port)
        .ok_or_else(|| "no live RuStudio instance found (start rustudio.exe first)".into())
}

fn send_bridge(
    explicit_port: Option<u16>,
    explicit_pid: Option<u32>,
    req: BridgeRequest,
) -> Result<BridgeResponse, String> {
    let port = target_port(explicit_port, explicit_pid)?;
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("failed to connect to RuStudio OmniMCP on 127.0.0.1:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(125)));
    let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("failed writing bridge request: {e}"))?;
    let mut resp_line = String::new();
    BufReader::new(stream)
        .read_line(&mut resp_line)
        .map_err(|e| format!("failed reading bridge response: {e}"))?;
    if resp_line.trim().is_empty() {
        return Err("RuStudio OmniMCP bridge closed connection without responding".into());
    }
    serde_json::from_str(&resp_line)
        .map_err(|e| format!("invalid bridge response JSON: {e}"))
}
