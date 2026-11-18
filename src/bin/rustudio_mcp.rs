//! `rustudio-mcp`: standalone Windows console stdio MCP server & multi-instance
//! OmniMCP bridge for RuStudio.
//!
//! Intentionally compiled WITHOUT `#![windows_subsystem = "windows"]` so
//! stdin/stdout JSON-RPC 2.0 communication works cleanly.
//!
//! NEVER write logs or diagnostics to stdout — stdout is strictly reserved for
//! JSON-RPC frames (except one-shot CLI helpers like `--list-instances` / `--help`).

#[path = "../mcp/protocol.rs"]
mod protocol;

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use protocol::{
    instances_dir, is_process_alive, list_live_instances_in, remove_instance_record,
    BridgeRequest, BridgeResponse, InstanceRecord, JsonRpcRequest, JsonRpcResponse, PromptArgument,
    PromptDef, PromptMessage, ResourceContent, ResourceDef, ToolCallResult, ToolContent, ToolDef,
};
use serde_json::{json, Map, Value};

const BRIDGE_IO_TIMEOUT: Duration = Duration::from_secs(125);
const PING_TIMEOUT: Duration = Duration::from_millis(800);

/// Wire framing detected from the incoming stdio stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Newline,
    ContentLength,
}

/// Mutable per-session routing state for `rustudio-mcp`.
#[derive(Debug, Clone)]
struct BridgeSession {
    explicit_port: Option<u16>,
    explicit_pid: Option<u32>,
    selected_pid: Option<u32>,
    selected_port: Option<u16>,
    instances_dir: PathBuf,
}

impl BridgeSession {
    fn new(explicit_port: Option<u16>, explicit_pid: Option<u32>) -> Self {
        Self {
            explicit_port,
            explicit_pid,
            selected_pid: None,
            selected_port: None,
            instances_dir: instances_dir(),
        }
    }

    #[cfg(test)]
    fn with_instances_dir(
        instances_dir: PathBuf,
        explicit_port: Option<u16>,
        explicit_pid: Option<u32>,
    ) -> Self {
        Self {
            explicit_port,
            explicit_pid,
            selected_pid: None,
            selected_port: None,
            instances_dir,
        }
    }

    fn live_instances(&self) -> Vec<InstanceRecord> {
        list_live_instances_in(&self.instances_dir)
    }

    fn record_path_for(&self, pid: u32) -> PathBuf {
        self.instances_dir.join(format!("{pid}.json"))
    }

    fn remove_record_for(&self, pid: u32) {
        let _ = std::fs::remove_file(self.record_path_for(pid));
        remove_instance_record(pid);
    }

    /// Resolves target `(port, known_pid, source_label)` in strict precedence order:
    /// 1. Explicit per-call `studio_id` (PID, port, `pid:<N>`, `port:<P>`, `127.0.0.1:<P>`, or title)
    /// 2. Session selection (`studio_select_instance` / `studio_launch(select: true)`)
    /// 3. CLI `--port` / `--pid`
    /// 4. Environment `RUSTUDIO_MCP_PORT`
    /// 5. Most recently started live instance from `instances_dir`
    fn resolve_target(&self, studio_id: Option<&Value>) -> Result<(u16, Option<u32>, &'static str), String> {
        let live = self.live_instances();

        if let Some(sid) = studio_id.filter(|v| !v.is_null()) {
            let (port, pid) = resolve_studio_id(sid, &live)?;
            return Ok((port, pid, "studio_id"));
        }

        if let Some(pid) = self.selected_pid {
            if !is_process_alive(pid) {
                return Err(format!(
                    "selected RuStudio instance (pid {pid}) is no longer running; call studio_list_instances, studio_select_instance, or studio_launch"
                ));
            }
            if let Some(port) = self.selected_port {
                return Ok((port, Some(pid), "session_selected"));
            }
            if let Some(rec) = live.iter().find(|r| r.pid == pid) {
                return Ok((rec.port, Some(rec.pid), "session_selected_pid"));
            }
            return Err(format!(
                "selected RuStudio instance (pid {pid}) has no active port; call studio_list_instances or studio_select_instance"
            ));
        }

        if let Some(port) = self.selected_port {
            let pid = live.iter().find(|r| r.port == port).map(|r| r.pid);
            return Ok((port, pid, "session_selected_port"));
        }

        if let Some(port) = self.explicit_port {
            let pid = live.iter().find(|r| r.port == port).map(|r| r.pid);
            return Ok((port, pid, "cli_port"));
        }

        if let Some(pid) = self.explicit_pid {
            return live
                .iter()
                .find(|r| r.pid == pid)
                .map(|r| (r.port, Some(r.pid), "cli_pid"))
                .ok_or_else(|| format!("no live RuStudio instance with pid {pid}"));
        }

        if let Ok(env_port) = std::env::var("RUSTUDIO_MCP_PORT") {
            if let Ok(p) = env_port.trim().parse::<u16>() {
                if p > 0 {
                    let pid = live.iter().find(|r| r.port == p).map(|r| r.pid);
                    return Ok((p, pid, "env_port"));
                }
            }
        }

        live.into_iter()
            .next()
            .map(|r| (r.port, Some(r.pid), "most_recent"))
            .ok_or_else(|| {
                "no live RuStudio instance found (call studio_launch or start rustudio.exe first)".into()
            })
    }

    fn active_target_json(&self) -> Value {
        match self.resolve_target(None) {
            Ok((port, pid, source)) => json!({
                "port": port,
                "pid": pid,
                "source": source,
            }),
            Err(_) => Value::Null,
        }
    }
}

fn resolve_studio_id(sid: &Value, live: &[InstanceRecord]) -> Result<(u16, Option<u32>), String> {
    if let Some(n) = sid.as_u64() {
        return resolve_numeric_studio_id(n, live);
    }
    let Some(raw) = sid.as_str() else {
        return Err("studio_id must be a PID integer, TCP port, or instance identifier string".into());
    };
    let s = raw.trim();
    if s.is_empty() {
        return Err("studio_id must not be empty".into());
    }
    if let Some(rest) = s.strip_prefix("pid:").or_else(|| s.strip_prefix("PID:")) {
        let pid: u32 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid PID in studio_id '{s}'"))?;
        return live
            .iter()
            .find(|r| r.pid == pid)
            .map(|r| (r.port, Some(r.pid)))
            .ok_or_else(|| format!("no live RuStudio instance with pid {pid}"));
    }
    if let Some(rest) = s
        .strip_prefix("port:")
        .or_else(|| s.strip_prefix("PORT:"))
        .or_else(|| s.strip_prefix("127.0.0.1:"))
        .or_else(|| s.strip_prefix("localhost:"))
    {
        let port: u16 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid port in studio_id '{s}'"))?;
        if port == 0 {
            return Err("port 0 is not a valid studio_id".into());
        }
        let pid = live.iter().find(|r| r.port == port).map(|r| r.pid);
        return Ok((port, pid));
    }
    if let Ok(n) = s.parse::<u64>() {
        return resolve_numeric_studio_id(n, live);
    }
    // Fallback: match by exact title (case-insensitive), then unique substring.
    let exact: Vec<&InstanceRecord> = live
        .iter()
        .filter(|r| r.title.eq_ignore_ascii_case(s))
        .collect();
    if exact.len() == 1 {
        return Ok((exact[0].port, Some(exact[0].pid)));
    }
    let lower = s.to_ascii_lowercase();
    let partial: Vec<&InstanceRecord> = live
        .iter()
        .filter(|r| r.title.to_ascii_lowercase().contains(&lower))
        .collect();
    if partial.len() == 1 {
        return Ok((partial[0].port, Some(partial[0].pid)));
    }
    if partial.len() > 1 {
        return Err(format!("studio_id '{s}' matched multiple instances; specify PID or port"));
    }
    Err(format!("no live RuStudio instance matching studio_id '{s}'"))
}

fn resolve_numeric_studio_id(n: u64, live: &[InstanceRecord]) -> Result<(u16, Option<u32>), String> {
    if let Ok(pid) = u32::try_from(n) {
        if let Some(rec) = live.iter().find(|r| r.pid == pid) {
            return Ok((rec.port, Some(rec.pid)));
        }
    }
    if let Ok(port) = u16::try_from(n) {
        if port > 0 {
            if let Some(rec) = live.iter().find(|r| r.port == port) {
                return Ok((rec.port, Some(rec.pid)));
            }
            // Allow direct port routing even if no registry record exists.
            return Ok((port, None));
        }
    }
    Err(format!("no live RuStudio instance matching studio_id '{n}'"))
}

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
                let session = BridgeSession::new(explicit_port, explicit_pid);
                let val = collect_instances_payload(&session);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&val).unwrap_or_else(|_| "{}".into())
                );
                return;
            }
            "-h" | "--help" => {
                println!(
                    "rustudio-mcp — OmniMCP stdio proxy & multi-instance bridge for RuStudio\n\n\
                     usage: rustudio-mcp [--port PORT | --pid PID | --list-instances]"
                );
                return;
            }
            "--version" | "-V" => {
                println!("rustudio-mcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("[rustudio-mcp] ignoring unknown flag: {other}");
            }
        }
    }

    let mut session = BridgeSession::new(explicit_port, explicit_pid);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout.lock();

    serve_stdio(&mut reader, &mut out, &mut session);
}

fn serve_stdio(reader: &mut impl BufRead, out: &mut impl Write, session: &mut BridgeSession) {
    loop {
        match read_next_frame(reader) {
            Ok(None) => break,
            Ok(Some((payload, framing))) => {
                if let Some(resp_json) = process_json_payload(&payload, session) {
                    if let Err(e) = write_framed(out, framing, &resp_json) {
                        eprintln!("[rustudio-mcp] failed writing stdout response: {e}");
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("[rustudio-mcp] stdio framing read error: {e}");
                let resp = JsonRpcResponse::err(Value::Null, -32700, format!("framing error: {e}"));
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = write_framed(out, Framing::Newline, &s);
                }
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                }
            }
        }
    }
}

fn read_next_frame(reader: &mut impl BufRead) -> std::io::Result<Option<(String, Framing)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("content-length:") || lower.starts_with("content-type:") {
            let mut content_length: Option<usize> = parse_content_length_header(trimmed);
            loop {
                let mut hdr = String::new();
                let hn = reader.read_line(&mut hdr)?;
                if hn == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF inside Content-Length headers",
                    ));
                }
                let htrim = hdr.trim();
                if htrim.is_empty() {
                    break;
                }
                if let Some(len) = parse_content_length_header(htrim) {
                    content_length = Some(len);
                }
            }
            let len = content_length.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing Content-Length header",
                )
            })?;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            let body = String::from_utf8(buf).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid UTF-8: {e}"))
            })?;
            return Ok(Some((body, Framing::ContentLength)));
        }

        return Ok(Some((trimmed.to_string(), Framing::Newline)));
    }
}

fn parse_content_length_header(line: &str) -> Option<usize> {
    let (k, v) = line.split_once(':')?;
    if k.trim().eq_ignore_ascii_case("content-length") {
        v.trim().parse::<usize>().ok()
    } else {
        None
    }
}

fn write_framed(out: &mut impl Write, framing: Framing, json_text: &str) -> std::io::Result<()> {
    match framing {
        Framing::Newline => {
            out.write_all(json_text.as_bytes())?;
            out.write_all(b"\n")?;
        }
        Framing::ContentLength => {
            let header = format!("Content-Length: {}\r\n\r\n", json_text.len());
            out.write_all(header.as_bytes())?;
            out.write_all(json_text.as_bytes())?;
        }
    }
    out.flush()
}

fn process_json_payload(payload: &str, session: &mut BridgeSession) -> Option<String> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('[') {
        let reqs: Vec<JsonRpcRequest> = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = JsonRpcResponse::err(Value::Null, -32700, format!("parse error: {e}"));
                return serde_json::to_string(&resp).ok();
            }
        };
        if reqs.is_empty() {
            let resp = JsonRpcResponse::err(Value::Null, -32600, "empty JSON-RPC batch");
            return serde_json::to_string(&resp).ok();
        }
        let mut responses = Vec::new();
        for req in reqs {
            if let Some(id) = req.id.clone() {
                responses.push(handle_rpc(id, &req.method, &req.params, session));
            }
        }
        if responses.is_empty() {
            return None;
        }
        return serde_json::to_string(&responses).ok();
    }

    let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::err(Value::Null, -32700, format!("parse error: {e}"));
            return serde_json::to_string(&resp).ok();
        }
    };

    let id = req.id.clone()?;
    let resp = handle_rpc(id, &req.method, &req.params, session);
    serde_json::to_string(&resp).ok()
}

fn handle_rpc(
    id: Value,
    method: &str,
    params: &Value,
    session: &mut BridgeSession,
) -> JsonRpcResponse {
    match method {
        "initialize" => {
            let client_ver = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            JsonRpcResponse::ok(
                id,
                json!({
                    "protocolVersion": client_ver,
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": true },
                        "prompts": { "listChanged": true }
                    },
                    "serverInfo": {
                        "name": "rustudio-omnimcp",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "RuStudio OmniMCP gives unrestricted AI access to Roblox Studio's DOM, scripts, Luau VMs (Edit/Server/Client), physics, SmoothGrid terrain, GPU viewport screenshots, GUI layout, Rojo sync, and multi-instance Windows process management. If no RuStudio window is open, call studio_launch first or studio_list_instances to discover running editors."
                }),
            )
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        "tools/list" => {
            let tools = list_all_tools(session);
            JsonRpcResponse::ok(id, json!({ "tools": tools }))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                return JsonRpcResponse::err(id, -32602, "missing tool 'name'");
            };
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            if is_bridge_native_tool(name) {
                let res = handle_bridge_tool(name, &arguments, session);
                return JsonRpcResponse::ok(id, serde_json::to_value(res).unwrap_or_default());
            }

            let studio_id = arguments.get("studio_id");
            let port = match session.resolve_target(studio_id) {
                Ok((p, _, _)) => p,
                Err(e) => {
                    let res = ToolCallResult::error(e);
                    return JsonRpcResponse::ok(id, serde_json::to_value(res).unwrap_or_default());
                }
            };
            let forwarded_args = strip_studio_id(&arguments);
            match send_bridge_to_port(
                port,
                BridgeRequest::CallTool {
                    name: name.to_string(),
                    arguments: forwarded_args,
                },
                BRIDGE_IO_TIMEOUT,
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
            let resources = list_all_resources(session);
            JsonRpcResponse::ok(id, json!({ "resources": resources }))
        }
        "resources/templates/list" => JsonRpcResponse::ok(id, json!({ "resourceTemplates": [] })),
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(|v| v.as_str()) else {
                return JsonRpcResponse::err(id, -32602, "missing resource 'uri'");
            };
            if uri == "rustudio://instances" {
                let payload = collect_instances_payload(session);
                let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
                let contents = vec![ResourceContent {
                    uri: uri.to_string(),
                    mime_type: Some("application/json".into()),
                    text: Some(text),
                    blob: None,
                }];
                return JsonRpcResponse::ok(id, json!({ "contents": contents }));
            }

            let studio_id = params.get("studio_id");
            let port = match session.resolve_target(studio_id) {
                Ok((p, _, _)) => p,
                Err(e) => return JsonRpcResponse::err(id, -32001, e),
            };
            match send_bridge_to_port(
                port,
                BridgeRequest::ReadResource {
                    uri: uri.to_string(),
                },
                BRIDGE_IO_TIMEOUT,
            ) {
                Ok(BridgeResponse::ResourceResult { contents }) => {
                    JsonRpcResponse::ok(id, json!({ "contents": contents }))
                }
                Ok(BridgeResponse::Error { code, message }) => {
                    JsonRpcResponse::err(id, code, message)
                }
                Ok(_) => JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                Err(e) => JsonRpcResponse::err(id, -32001, e),
            }
        }
        "prompts/list" => {
            let prompts = list_all_prompts(session);
            JsonRpcResponse::ok(id, json!({ "prompts": prompts }))
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

            if let Ok((port, _, _)) = session.resolve_target(params.get("studio_id")) {
                match send_bridge_to_port(
                    port,
                    BridgeRequest::GetPrompt {
                        name: name.to_string(),
                        arguments: arguments.clone(),
                    },
                    BRIDGE_IO_TIMEOUT,
                ) {
                    Ok(BridgeResponse::PromptResult {
                        description,
                        messages,
                    }) => {
                        return JsonRpcResponse::ok(
                            id,
                            json!({
                                "description": description,
                                "messages": messages,
                            }),
                        );
                    }
                    Ok(BridgeResponse::Error { code, message }) => {
                        return JsonRpcResponse::err(id, code, message);
                    }
                    Ok(_) => return JsonRpcResponse::err(id, -32603, "unexpected bridge response"),
                    Err(_) => {}
                }
            }

            match offline_get_prompt(name, &arguments) {
                Ok((description, messages)) => JsonRpcResponse::ok(
                    id,
                    json!({
                        "description": description,
                        "messages": messages,
                    }),
                ),
                Err(msg) => JsonRpcResponse::err(id, -32602, msg),
            }
        }
        _ => JsonRpcResponse::err(id, -32601, format!("method not found: {method}")),
    }
}

fn strip_studio_id(args: &Value) -> Value {
    match args {
        Value::Object(map) => {
            let mut copy = map.clone();
            copy.remove("studio_id");
            Value::Object(copy)
        }
        Value::Null => json!({}),
        other => other.clone(),
    }
}

fn is_bridge_native_tool(name: &str) -> bool {
    matches!(
        name,
        "studio_list_instances"
            | "studio_launch"
            | "studio_kill_instance"
            | "studio_select_instance"
    )
}

fn handle_bridge_tool(name: &str, args: &Value, session: &mut BridgeSession) -> ToolCallResult {
    match name {
        "studio_list_instances" => ToolCallResult::json(&collect_instances_payload(session)),
        "studio_launch" => tool_studio_launch(args, session),
        "studio_kill_instance" => tool_studio_kill_instance(args, session),
        "studio_select_instance" => tool_studio_select_instance(args, session),
        _ => ToolCallResult::error(format!("unknown bridge tool: {name}")),
    }
}

fn ping_port(port: u16) -> Result<(u32, u16, String, String), String> {
    match send_bridge_to_port(port, BridgeRequest::Ping, PING_TIMEOUT)? {
        BridgeResponse::Pong {
            pid,
            port,
            title,
            mode,
        } => Ok((pid, port, title, mode)),
        BridgeResponse::Error { message, .. } => Err(message),
        _ => Err("unexpected response to Ping".into()),
    }
}

fn collect_instances_payload(session: &BridgeSession) -> Value {
    let records = session.live_instances();
    let mut seen_ports = HashSet::new();
    let mut instances = Vec::new();

    for rec in records {
        seen_ports.insert(rec.port);
        match ping_port(rec.port) {
            Ok((pid, port, title, mode)) => {
                instances.push(json!({
                    "pid": pid,
                    "port": port,
                    "title": title,
                    "place_path": rec.place_path,
                    "mode": mode,
                    "started_unix": rec.started_unix,
                    "responsive": true,
                }));
            }
            Err(err) => {
                instances.push(json!({
                    "pid": rec.pid,
                    "port": rec.port,
                    "title": rec.title,
                    "place_path": rec.place_path,
                    "mode": rec.mode,
                    "started_unix": rec.started_unix,
                    "responsive": false,
                    "ping_error": err,
                }));
            }
        }
    }

    // Also check explicit/env port if not already represented in instance files.
    let extra_port = session.selected_port.or(session.explicit_port).or_else(|| {
        std::env::var("RUSTUDIO_MCP_PORT")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|&p| p > 0)
    });
    if let Some(p) = extra_port {
        if !seen_ports.contains(&p) {
            if let Ok((pid, port, title, mode)) = ping_port(p) {
                instances.push(json!({
                    "pid": pid,
                    "port": port,
                    "title": title,
                    "place_path": Value::Null,
                    "mode": mode,
                    "started_unix": Value::Null,
                    "responsive": true,
                }));
            }
        }
    }

    json!({
        "count": instances.len(),
        "active_target": session.active_target_json(),
        "instances": instances,
    })
}

fn locate_rustudio_exe() -> Result<PathBuf, String> {
    if let Ok(env_exe) = std::env::var("RUSTUDIO_EXE") {
        let p = PathBuf::from(env_exe);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let sibling = dir.join("rustudio.exe");
            if sibling.is_file() {
                return Ok(sibling);
            }
            let sibling_no_ext = dir.join("rustudio");
            if sibling_no_ext.is_file() {
                return Ok(sibling_no_ext);
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("rustudio.exe");
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    Err("could not locate rustudio.exe next to rustudio-mcp.exe or on PATH".into())
}

fn format_window_size(val: &Value) -> Result<String, String> {
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        let (w, h) = trimmed
            .split_once('x')
            .or_else(|| trimmed.split_once('X'))
            .ok_or_else(|| format!("window_size must be 'WxH', got '{trimmed}'"))?;
        let wn: u32 = w.trim().parse().map_err(|_| format!("invalid width in '{trimmed}'"))?;
        let hn: u32 = h.trim().parse().map_err(|_| format!("invalid height in '{trimmed}'"))?;
        if wn == 0 || hn == 0 {
            return Err("window_size width and height must be > 0".into());
        }
        return Ok(format!("{wn}x{hn}"));
    }
    if let Some(arr) = val.as_array() {
        if arr.len() == 2 {
            let w = arr[0].as_u64().ok_or("window_size[0] must be integer")?;
            let h = arr[1].as_u64().ok_or("window_size[1] must be integer")?;
            if w == 0 || h == 0 {
                return Err("window_size dimensions must be > 0".into());
            }
            return Ok(format!("{w}x{h}"));
        }
    }
    Err("window_size must be 'WxH' string or [width, height] array".into())
}

fn tool_studio_launch(args: &Value, session: &mut BridgeSession) -> ToolCallResult {
    let exe = match locate_rustudio_exe() {
        Ok(p) => p,
        Err(e) => return ToolCallResult::error(e),
    };

    let place_path = args
        .get("place_path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let demo = args.get("demo").and_then(|v| v.as_bool()).unwrap_or(false);
    let play = args.get("play").and_then(|v| v.as_bool()).unwrap_or(false);
    let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(false);
    let no_render = args.get("no_render").and_then(|v| v.as_bool()).unwrap_or(false);
    let overlays = args.get("overlays").and_then(|v| v.as_bool()).unwrap_or(false);
    let no_plugins = args.get("no_plugins").and_then(|v| v.as_bool()).unwrap_or(false);
    let select = args.get("select").and_then(|v| v.as_bool()).unwrap_or(true);
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_f64())
        .unwrap_or(15.0)
        .clamp(1.0, 60.0);

    let explicit_port = match args.get("port").filter(|v| !v.is_null()) {
        Some(v) => match v.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|&p| p > 0) {
            Some(p) => Some(p),
            None => return ToolCallResult::error("port must be an integer 1..=65535"),
        },
        None => None,
    };

    let mut cli: Vec<String> = Vec::new();
    if let Some(ref p) = place_path {
        cli.push(p.clone());
    }
    if demo {
        cli.push("--demo".into());
    }
    if play {
        cli.push("--play".into());
    }
    // Note: rustudio.exe rejects --headless / --no-render without --no-ui.
    if no_render {
        cli.push("--no-render".into());
        cli.push("--no-ui".into());
    } else if headless {
        cli.push("--headless".into());
        cli.push("--no-ui".into());
    }
    if overlays {
        cli.push("--overlays".into());
    }
    if no_plugins {
        cli.push("--no-plugins".into());
    }
    if let Some(ws) = args.get("window_size").filter(|v| !v.is_null()) {
        match format_window_size(ws) {
            Ok(formatted) => {
                cli.push("--window".into());
                cli.push(formatted);
            }
            Err(e) => return ToolCallResult::error(e),
        }
    }
    if let Some(p) = explicit_port {
        cli.push("--mcp-port".into());
        cli.push(p.to_string());
    }
    if let Some(dt) = args.get("fixed_dt").filter(|v| !v.is_null()) {
        let dt_str = match dt {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => return ToolCallResult::error("fixed_dt must be a number or string like '1/60'"),
        };
        if !dt_str.is_empty() {
            cli.push("--fixed-dt".into());
            cli.push(dt_str);
        }
    }

    let mut cmd = Command::new(&exe);
    cmd.args(&cli)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Isolate stdio & console on Windows so `rustudio.exe`'s AttachConsole never
    // touches `rustudio-mcp.exe`'s JSON-RPC stdout pipe.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        if headless || no_render {
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        } else {
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("failed to spawn {}: {e}", exe.display())),
    };
    let child_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return ToolCallResult::error(format!(
                "rustudio.exe (pid {child_pid}) exited prematurely with {status}"
            ));
        }

        let live = session.live_instances();
        let candidate_port = live
            .iter()
            .find(|r| r.pid == child_pid)
            .map(|r| r.port)
            .or(explicit_port);

        if let Some(port) = candidate_port {
            if let Ok((pid, pong_port, title, mode)) = ping_port(port) {
                if pid == child_pid || explicit_port.is_some() {
                    if select {
                        session.selected_pid = Some(pid);
                        session.selected_port = Some(pong_port);
                    }
                    return ToolCallResult::json(&json!({
                        "pid": pid,
                        "port": pong_port,
                        "title": title,
                        "mode": mode,
                        "place_path": place_path,
                        "headless": headless || no_render,
                        "no_render": no_render,
                        "selected": select,
                    }));
                }
            }
        }

        if Instant::now() >= deadline {
            return ToolCallResult::error(format!(
                "timed out after {timeout_secs:.1}s waiting for rustudio.exe (pid {child_pid}) OmniMCP bridge to become ready"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tool_studio_kill_instance(args: &Value, session: &mut BridgeSession) -> ToolCallResult {
    let live = session.live_instances();
    let pid: u32 = if let Some(p) = args.get("pid").and_then(|v| v.as_u64()) {
        match u32::try_from(p) {
            Ok(pid) if pid > 0 => pid,
            _ => return ToolCallResult::error("pid must be a positive integer"),
        }
    } else if let Some(sid) = args.get("studio_id").filter(|v| !v.is_null()) {
        match resolve_studio_id(sid, &live) {
            Ok((_port, Some(pid))) => pid,
            Ok((port, None)) => match ping_port(port) {
                Ok((pid, _, _, _)) => pid,
                Err(e) => return ToolCallResult::error(e),
            },
            Err(e) => return ToolCallResult::error(e),
        }
    } else {
        return ToolCallResult::error("studio_kill_instance requires 'pid' (or 'studio_id')");
    };

    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let known_port = live.iter().find(|r| r.pid == pid).map(|r| r.port);
    let mut method = "already_exited";

    if is_process_alive(pid) {
        let mut exited = false;
        if !force {
            if let Some(port) = known_port {
                let _ = send_bridge_to_port(
                    port,
                    BridgeRequest::CallTool {
                        name: "studio_quit".into(),
                        arguments: json!({}),
                    },
                    Duration::from_secs(2),
                );
                for _ in 0..25 {
                    if !is_process_alive(pid) {
                        exited = true;
                        method = "graceful";
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        if !exited && is_process_alive(pid) {
            #[cfg(windows)]
            {
                match terminate_windows_pid(pid) {
                    Ok(()) => {
                        method = "terminate_process";
                    }
                    Err(e) => return ToolCallResult::error(e),
                }
            }
            #[cfg(not(windows))]
            {
                return ToolCallResult::error(format!(
                    "graceful shutdown did not exit pid {pid} and TerminateProcess is Windows-only"
                ));
            }
        }
    }

    session.remove_record_for(pid);
    if session.selected_pid == Some(pid) || known_port.is_some() && session.selected_port == known_port
    {
        session.selected_pid = None;
        session.selected_port = None;
    }

    ToolCallResult::json(&json!({
        "pid": pid,
        "killed": true,
        "alive_after": is_process_alive(pid),
        "method": method,
        "record_removed": true,
    }))
}

#[cfg(windows)]
fn terminate_windows_pid(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE,
    };
    const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;
    // SAFETY: valid PID checked nonzero; handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_RIGHT, 0, pid);
        if handle.is_null() {
            if !is_process_alive(pid) {
                return Ok(());
            }
            return Err(format!("OpenProcess failed for pid {pid}"));
        }
        let ok = TerminateProcess(handle, 1);
        if ok != 0 {
            let _ = WaitForSingleObject(handle, 2000);
        }
        CloseHandle(handle);
        if ok == 0 && is_process_alive(pid) {
            return Err(format!("TerminateProcess failed for pid {pid}"));
        }
        Ok(())
    }
}

fn tool_studio_select_instance(args: &Value, session: &mut BridgeSession) -> ToolCallResult {
    if args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
        session.selected_pid = None;
        session.selected_port = None;
        return ToolCallResult::json(&json!({
            "cleared": true,
            "active_target": session.active_target_json(),
        }));
    }

    let live = session.live_instances();
    let (target_port, target_pid) = if let Some(pid_u64) = args.get("pid").and_then(|v| v.as_u64()) {
        let Ok(pid) = u32::try_from(pid_u64) else {
            return ToolCallResult::error("invalid pid");
        };
        let Some(rec) = live.iter().find(|r| r.pid == pid) else {
            return ToolCallResult::error(format!("no live RuStudio instance with pid {pid}"));
        };
        (rec.port, Some(rec.pid))
    } else if let Some(port_u64) = args.get("port").and_then(|v| v.as_u64()) {
        let Some(port) = u16::try_from(port_u64).ok().filter(|&p| p > 0) else {
            return ToolCallResult::error("port must be 1..=65535");
        };
        let pid = live.iter().find(|r| r.port == port).map(|r| r.pid);
        (port, pid)
    } else if let Some(sid) = args.get("studio_id").filter(|v| !v.is_null()) {
        match resolve_studio_id(sid, &live) {
            Ok(pair) => pair,
            Err(e) => return ToolCallResult::error(e),
        }
    } else {
        return ToolCallResult::error(
            "studio_select_instance requires 'pid', 'port', 'studio_id', or 'clear: true'",
        );
    };

    match ping_port(target_port) {
        Ok((pid, port, title, mode)) => {
            session.selected_pid = target_pid.or(Some(pid));
            session.selected_port = Some(port);
            ToolCallResult::json(&json!({
                "selected": {
                    "pid": pid,
                    "port": port,
                    "title": title,
                    "mode": mode,
                },
                "active_target": session.active_target_json(),
            }))
        }
        Err(e) => ToolCallResult::error(format!(
            "target RuStudio on port {target_port} did not respond to Ping: {e}"
        )),
    }
}

fn send_bridge_to_port(
    port: u16,
    req: BridgeRequest,
    timeout: Duration,
) -> Result<BridgeResponse, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(3)))
        .map_err(|e| format!("failed to connect to RuStudio OmniMCP on 127.0.0.1:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
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

fn list_all_tools(session: &BridgeSession) -> Vec<ToolDef> {
    let mut tools = bridge_native_tools();
    let mut seen: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();

    let live_tools = session
        .resolve_target(None)
        .ok()
        .and_then(|(port, _, _)| {
            send_bridge_to_port(port, BridgeRequest::ListTools, Duration::from_secs(3)).ok()
        })
        .and_then(|resp| match resp {
            BridgeResponse::Tools { tools } if !tools.is_empty() => Some(tools),
            _ => None,
        });

    let engine_tools = live_tools.unwrap_or_else(offline_engine_tools);
    for t in engine_tools {
        if seen.insert(t.name.clone()) {
            tools.push(with_studio_id_schema(t));
        }
    }
    tools
}

fn with_studio_id_schema(mut tool: ToolDef) -> ToolDef {
    if let Value::Object(ref mut root) = tool.input_schema {
        let props = root
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(ref mut map) = props {
            map.entry("studio_id").or_insert_with(|| {
                json!({
                    "type": ["string", "integer"],
                    "description": "Optional target RuStudio PID, TCP port, 'pid:<N>', 'port:<P>', or place title overriding studio_select_instance for this call."
                })
            });
        }
    }
    tool
}

fn list_all_resources(session: &BridgeSession) -> Vec<ResourceDef> {
    let mut out = vec![ResourceDef {
        uri: "rustudio://instances".into(),
        name: "Running RuStudio Instances".into(),
        description: "List of live RuStudio OmniMCP instances on this machine and the currently selected target.".into(),
        mime_type: Some("application/json".into()),
    }];
    let mut seen: HashSet<String> = out.iter().map(|r| r.uri.clone()).collect();

    let live_res = session
        .resolve_target(None)
        .ok()
        .and_then(|(port, _, _)| {
            send_bridge_to_port(port, BridgeRequest::ListResources, Duration::from_secs(3)).ok()
        })
        .and_then(|resp| match resp {
            BridgeResponse::Resources { resources } if !resources.is_empty() => Some(resources),
            _ => None,
        });

    for r in live_res.unwrap_or_else(offline_engine_resources) {
        if seen.insert(r.uri.clone()) {
            out.push(r);
        }
    }
    out
}

fn list_all_prompts(session: &BridgeSession) -> Vec<PromptDef> {
    let live_prompts = session
        .resolve_target(None)
        .ok()
        .and_then(|(port, _, _)| {
            send_bridge_to_port(port, BridgeRequest::ListPrompts, Duration::from_secs(3)).ok()
        })
        .and_then(|resp| match resp {
            BridgeResponse::Prompts { prompts } if !prompts.is_empty() => Some(prompts),
            _ => None,
        });

    live_prompts.unwrap_or_else(offline_engine_prompts)
}

fn tool_def(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    }
}

fn bridge_native_tools() -> Vec<ToolDef> {
    vec![
        tool_def(
            "studio_list_instances",
            "List all running RuStudio instances discovered via %LOCALAPPDATA%\\rustudio\\mcp-instances, verify TCP liveness via Ping, and show the active routing target.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        tool_def(
            "studio_launch",
            "Launch a new RuStudio instance (windowed or headless/offscreen), wait until its OmniMCP TCP bridge responds to Ping, and optionally select it as the active target.",
            json!({
                "type": "object",
                "properties": {
                    "place_path": { "type": "string", "description": "Optional .rbxl/.rbxlx/.rbxm/.rbxmx file or Rojo project folder to open." },
                    "demo": { "type": "boolean", "description": "Start with RuStudio's built-in demo scene." },
                    "play": { "type": "boolean", "description": "Start Play (F5) immediately after loading." },
                    "headless": { "type": "boolean", "description": "Run offscreen GPU rendering without a visible window (--headless --no-ui)." },
                    "no_render": { "type": "boolean", "description": "Run without a GPU or window (--no-render --no-ui) for ultra-fast simulation/script work." },
                    "overlays": { "type": "boolean", "description": "Keep editor overlays under headless/no-ui Edit mode." },
                    "window_size": { "type": ["string", "array"], "description": "Viewport/window size as 'WxH' or [w, h] (e.g. '1280x720')." },
                    "port": { "type": "integer", "description": "Explicit localhost TCP port for OmniMCP (--mcp-port)." },
                    "fixed_dt": { "type": ["string", "number"], "description": "Deterministic simulation timestep (e.g. '1/60' or 0.0166667)." },
                    "no_plugins": { "type": "boolean", "description": "Disable loading user/installed Studio plugins." },
                    "select": { "type": "boolean", "description": "Automatically select the launched instance for this MCP session (default: true)." },
                    "timeout_secs": { "type": "number", "description": "Maximum seconds to wait for bridge readiness (default: 15, max: 60)." }
                }
            }),
        ),
        tool_def(
            "studio_kill_instance",
            "Terminate a running RuStudio instance by PID (graceful quit first, then Windows TerminateProcess fallback) and clean up its instance record.",
            json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Windows process ID of the RuStudio instance to terminate." },
                    "studio_id": { "type": ["string", "integer"], "description": "Alternative PID/port identifier if pid is not provided." },
                    "force": { "type": "boolean", "description": "Skip graceful quit and immediately call TerminateProcess (default: false)." }
                }
            }),
        ),
        tool_def(
            "studio_select_instance",
            "Select the default target RuStudio instance (by pid, port, or studio_id) for subsequent OmniMCP calls in this session.",
            json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Target RuStudio PID." },
                    "port": { "type": "integer", "description": "Target OmniMCP localhost TCP port." },
                    "studio_id": { "type": ["string", "integer"], "description": "Identifier string or number (PID, port, 'pid:N', 'port:P', or title)." },
                    "clear": { "type": "boolean", "description": "Clear session pin and revert to automatic most-recent instance routing." }
                }
            }),
        ),
    ]
}

fn offline_engine_resources() -> Vec<ResourceDef> {
    vec![
        ResourceDef {
            uri: "rustudio://state".into(),
            name: "RuStudio Live State".into(),
            description: "Current editor mode (Edit/Play/Run), available DataModels, place metadata, camera, selection, undo stack, FPS, and memory.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://tree/workspace".into(),
            name: "Workspace Hierarchy".into(),
            description: "Hierarchical JSON tree of Workspace instances in the active DataModel.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://tree/game".into(),
            name: "DataModel Hierarchy".into(),
            description: "Hierarchical JSON tree of the full game DataModel (services and descendants).".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://scripts".into(),
            name: "All Scripts Manifest".into(),
            description: "Inventory of every Script, LocalScript, and ModuleScript in the active DataModel.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://output".into(),
            name: "Output Console Log".into(),
            description: "Recent RuStudio Output console lines (Info/Warn/Error) with timestamps and counts.".into(),
            mime_type: Some("application/json".into()),
        },
        ResourceDef {
            uri: "rustudio://selection".into(),
            name: "Current Selection".into(),
            description: "Currently selected instances with refs, paths, tags, attributes, and stored properties.".into(),
            mime_type: Some("application/json".into()),
        },
    ]
}

fn offline_engine_prompts() -> Vec<PromptDef> {
    vec![
        PromptDef {
            name: "omni-game-architect".into(),
            description: "Expert Roblox/Luau systems architect workflow using OmniMCP's DOM, script batching, Edit-during-Play, and verification tools.".into(),
            arguments: vec![
                PromptArgument {
                    name: "goal".into(),
                    description: "The game system, feature, or refactor to architect and build.".into(),
                    required: false,
                },
                PromptArgument {
                    name: "architecture".into(),
                    description: "Optional architectural constraints or patterns.".into(),
                    required: false,
                },
            ],
        },
        PromptDef {
            name: "omni-playtest-debugger".into(),
            description: "Expert playtest, physics, and Luau runtime debugging workflow with deterministic stepping, live VM execution, and log inspection.".into(),
            arguments: vec![PromptArgument {
                name: "symptom".into(),
                description: "Bug, unexpected physics behavior, script error, or test scenario to diagnose.".into(),
                required: false,
            }],
        },
        PromptDef {
            name: "omni-world-builder".into(),
            description: "Expert 3D environment, procedural structure, SmoothGrid terrain, and Lighting/Atmosphere builder with GPU visual verification.".into(),
            arguments: vec![PromptArgument {
                name: "theme".into(),
                description: "Visual style, biome, or environment area to construct and light.".into(),
                required: false,
            }],
        },
        PromptDef {
            name: "omni-ui-designer".into(),
            description: "Expert Roblox ScreenGui/BillboardGui/SurfaceGui designer with responsive layout inspection and visual verification.".into(),
            arguments: vec![PromptArgument {
                name: "screen".into(),
                description: "Target UI screen, HUD, or menu to create or refine.".into(),
                required: false,
            }],
        },
    ]
}

fn offline_get_prompt(
    name: &str,
    args: &Map<String, Value>,
) -> Result<(Option<String>, Vec<PromptMessage>), String> {
    let arg = |k: &str, default: &str| -> String {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    let text = match name {
        "omni-game-architect" => format!(
            "No live RuStudio instance is currently running; call `studio_launch` (or `studio_list_instances`) first.\n\n\
             Once connected, architect '{}':\n\
             1. Inspect tree & scripts (`dom_get_tree`, `script_list_all`, `script_read_batch`).\n\
             2. Create modular hierarchies (`dom_create_instances`, `script_write_batch`, `script_patch_batch`, `omni_batch`).\n\
             3. Keep editing source via `datamodel: \"Edit\"` even during Play, then verify with `script_check_syntax`, `play_control`, `output_console`, and `viewport_capture`.",
            arg("goal", "Roblox/Luau game systems")
        ),
        "omni-playtest-debugger" => format!(
            "No live RuStudio instance is currently running; call `studio_launch` first.\n\n\
             Debugging workflow for '{}':\n\
             1. Run `play_control` (`start_play` / `start_run`), step frames via `play_step_frames`, and inspect `output_console`.\n\
             2. Execute diagnostics via `luau_execute` (`Server`/`Client`/`Edit`, `identity: 8` or `2`) and `physics_inspect` / `play_character_control` / `play_input_inject`.",
            arg("symptom", "runtime or physics issue")
        ),
        "omni-world-builder" => format!(
            "No live RuStudio instance is currently running; call `studio_launch` first.\n\n\
             World-building workflow for '{}':\n\
             1. Sculpt/generate terrain (`terrain_generate`, `terrain_fill_shape`, `terrain_sculpt_stroke`).\n\
             2. Build grounded geometry (`world_build_structure`, `world_place_on_surface`, `world_raycast`, `world_spatial_query`).\n\
             3. Tune `lighting_and_atmosphere` and inspect visually with `viewport_camera` + `viewport_capture`.",
            arg("theme", "3D environment")
        ),
        "omni-ui-designer" => format!(
            "No live RuStudio instance is currently running; call `studio_launch` first.\n\n\
             UI design workflow for '{}':\n\
             1. Build responsive `ScreenGui` trees (`dom_create_instances`, `dom_set_properties`).\n\
             2. Audit geometry via `gui_inspect` and test interactively via `play_control`, `play_input_inject`, and `viewport_capture`.",
            arg("screen", "game UI")
        ),
        _ => return Err(format!("unknown prompt: {name}")),
    };
    Ok((
        Some("Offline OmniMCP prompt (connect or launch RuStudio for live context)".into()),
        vec![PromptMessage {
            role: "user".into(),
            content: ToolContent::Text { text },
        }],
    ))
}

fn dm_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["Edit", "Server", "Client", "Active"],
        "description": "Target DataModel ('Edit' works even during Play/Run; default: 'Active')."
    })
}

fn target_schema(desc: &str) -> Value {
    json!({
        "type": ["string", "object"],
        "description": desc
    })
}

/// Complete offline catalog of all 53 OmniMCP engine tools exposed even when
/// zero `rustudio.exe` windows are currently running.
fn offline_engine_tools() -> Vec<ToolDef> {
    vec![
        // Batch
        tool_def(
            "omni_batch",
            "Execute multiple synchronous OmniMCP tool calls sequentially within a single engine frame.",
            json!({
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "description": "Ordered list of tool invocations ({name, arguments}).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "arguments": { "type": "object" }
                            },
                            "required": ["name"]
                        }
                    },
                    "stop_on_error": { "type": "boolean", "description": "Stop immediately on first error (default: false)." }
                },
                "required": ["calls"]
            }),
        ),

        // Session & Place (6)
        tool_def(
            "studio_get_state",
            "Get complete live editor snapshot: mode (Edit/Play/Run), available DataModels, title, path, dirty, revisions, undo stack, selection, camera, FPS, work ms, memory, solver, and play state.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool_def(
            "place_new",
            "Reset the editor to a fresh Baseplate place (stopping Play first if active and clearing caches/tabs).",
            json!({
                "type": "object",
                "properties": {
                    "force": { "type": "boolean", "description": "Discard unsaved changes without warning." }
                }
            }),
        ),
        tool_def(
            "place_open",
            "Open a local .rbxl/.rbxlx/.rbxm/.rbxmx file or Rojo *.project.json/folder into the live RuStudio editor.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or project folder path to open." }
                },
                "required": ["path"]
            }),
        ),
        tool_def(
            "place_save",
            "Save the Edit or Active place (or selected instances to .rbxm/.rbxmx).",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Destination file path (defaults to current document path)." },
                    "datamodel": dm_schema(),
                    "targets": { "type": "array", "items": target_schema("Optional instance targets for model export (.rbxm/.rbxmx).") }
                }
            }),
        ),
        tool_def(
            "place_diagnostics",
            "Return place statistics, service summary, script count/lines, crash reports, memory usage, and frame timing.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema()
                }
            }),
        ),
        tool_def(
            "studio_quit",
            "Cleanly close the RuStudio instance (optionally saving first).",
            json!({
                "type": "object",
                "properties": {
                    "save_before_quit": { "type": "boolean" }
                }
            }),
        ),

        // DOM (9)
        tool_def(
            "dom_get_tree",
            "Dump hierarchical or flat DataModel tree with configurable depth, class/name/tag filters, and requested properties.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "root": target_schema("Root instance path or ref (default: 'game')."),
                    "max_depth": { "type": "integer", "description": "Maximum traversal depth (default: 4)." },
                    "flat": { "type": "boolean", "description": "Return flat array instead of nested children." },
                    "class": { "type": "string", "description": "Filter by IsA class." },
                    "name_contains": { "type": "string" },
                    "tag": { "type": "string" },
                    "include_properties": { "type": "array", "items": { "type": "string" } }
                }
            }),
        ),
        tool_def(
            "dom_inspect_instance",
            "Inspect all stored DOM properties, reflection defaults (including NotScriptable & RobloxScriptSecurity), Attributes, Tags, Archivable, children, and world bounds for target(s).",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "target": target_schema("Single target path or ref."),
                    "targets": { "type": "array", "items": target_schema("Batch of target paths or refs.") },
                    "include_defaults": { "type": "boolean", "description": "Include reflection database default properties." }
                }
            }),
        ),
        tool_def(
            "dom_query",
            "Unlimited DataModel search by IsA/exact class, name pattern, path prefix, tags, attributes, property values, or 3D sphere/box bounds.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "root": target_schema("Subtree root (default: 'game')."),
                    "is_a": { "type": "string" },
                    "exact_class": { "type": "string" },
                    "name_pattern": { "type": "string" },
                    "path_prefix": { "type": "string" },
                    "has_tag": { "type": "string" },
                    "has_attribute": { "type": "string" },
                    "attribute_equals": { "type": "object" },
                    "property_equals": { "type": "object" },
                    "within_sphere": { "type": "object", "properties": { "center": { "type": "array" }, "radius": { "type": "number" } } },
                    "within_box": { "type": "object", "properties": { "min": { "type": "array" }, "max": { "type": "array" } } },
                    "limit": { "type": "integer" },
                    "select_properties": { "type": "array", "items": { "type": "string" } }
                }
            }),
        ),
        tool_def(
            "dom_create_instances",
            "Batch recursive instance tree builder creating arbitrary nested hierarchies with properties, attributes, and tags in one undoable operation.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "parent": target_schema("Parent path or ref."),
                                "class": { "type": "string" },
                                "name": { "type": "string" },
                                "properties": { "type": "object" },
                                "attributes": { "type": "object" },
                                "tags": { "type": "array", "items": { "type": "string" } },
                                "children": { "type": "array" }
                            },
                            "required": ["class"]
                        }
                    }
                },
                "required": ["items"]
            }),
        ),
        tool_def(
            "dom_set_properties",
            "Batch property, attribute, tag, and name mutator bypassing NotScriptable and capability restrictions across all Roblox Variant types.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": target_schema("Target instance path or ref."),
                                "name": { "type": "string" },
                                "properties": { "type": "object" },
                                "attributes": { "type": "object" },
                                "add_tags": { "type": "array", "items": { "type": "string" } },
                                "remove_tags": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["target"]
                        }
                    }
                },
                "required": ["edits"]
            }),
        ),
        tool_def(
            "dom_reparent_rename_delete",
            "Batch reparent, rename, delete, and clear_children operations in a single call.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "reparent": { "type": "array", "items": { "type": "object", "properties": { "target": target_schema("Target."), "new_parent": target_schema("New parent.") }, "required": ["target", "new_parent"] } },
                    "rename": { "type": "array", "items": { "type": "object", "properties": { "target": target_schema("Target."), "name": { "type": "string" } }, "required": ["target", "name"] } },
                    "delete": { "type": "array", "items": target_schema("Instances to remove.") },
                    "clear_children": { "type": "array", "items": target_schema("Containers whose children will be removed.") }
                }
            }),
        ),
        tool_def(
            "dom_clone_and_transform",
            "Clone instance(s) N times with offset/rotation steps, 2D grid layouts, or explicit transforms.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "target": target_schema("Target to clone."),
                    "targets": { "type": "array", "items": target_schema("Targets to clone.") },
                    "count": { "type": "integer" },
                    "parent": target_schema("Destination parent."),
                    "name_prefix": { "type": "string" },
                    "offset_step": { "type": "array", "items": { "type": "number" } },
                    "rotation_step_deg": { "type": "array", "items": { "type": "number" } },
                    "grid": { "type": "object" },
                    "transforms": { "type": "array" }
                }
            }),
        ),
        tool_def(
            "dom_group_ungroup",
            "Group target instances into a Model or Folder, or ungroup container instances.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "action": { "type": "string", "enum": ["group", "ungroup"] },
                    "targets": { "type": "array", "items": target_schema("Target instances.") },
                    "class": { "type": "string", "enum": ["Model", "Folder"] },
                    "name": { "type": "string" }
                },
                "required": ["action", "targets"]
            }),
        ),
        tool_def(
            "dom_serialize_subtree",
            "Export instances to .rbxmx XML or base64 .rbxm, or import .rbxmx/.rbxm under a parent instance.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "action": { "type": "string", "enum": ["export", "import"] },
                    "targets": { "type": "array", "items": target_schema("Targets for export.") },
                    "format": { "type": "string", "enum": ["rbxmx", "rbxm"] },
                    "parent": target_schema("Parent for import."),
                    "data": { "type": "string", "description": "XML text or base64 .rbxm payload for import." }
                },
                "required": ["action"]
            }),
        ),

        // Scripts (7)
        tool_def(
            "script_list_all",
            "List every Script, LocalScript, and ModuleScript with ref, path, class, lines, bytes, disabled, run_context, and syntax status.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "path_prefix": { "type": "string" }
                }
            }),
        ),
        tool_def(
            "script_read_batch",
            "Read one or many scripts in one call (full source or line range) with line numbers and hash digest.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "targets": { "type": "array", "items": target_schema("Script paths or refs.") },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["targets"]
            }),
        ),
        tool_def(
            "script_write_batch",
            "Atomically create or replace Source across multiple scripts in one call (optionally creating missing Folder parents).",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "scripts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "class": { "type": "string", "enum": ["Script", "LocalScript", "ModuleScript"] },
                                "source": { "type": "string" },
                                "disabled": { "type": "boolean" },
                                "run_context": { "type": "string" },
                                "create_parents": { "type": "boolean" }
                            },
                            "required": ["path", "source"]
                        }
                    }
                },
                "required": ["scripts"]
            }),
        ),
        tool_def(
            "script_patch_batch",
            "Atomic multi-script surgical editor validating all string-replace or line-range edits across all files before committing any.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "files": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": target_schema("Script path or ref."),
                                "edits": { "type": "array" }
                            },
                            "required": ["target", "edits"]
                        }
                    }
                },
                "required": ["files"]
            }),
        ),
        tool_def(
            "script_grep",
            "Unlimited full-text search across all scripts in the DataModel with surrounding context lines.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "query": { "type": "string" },
                    "case_sensitive": { "type": "boolean" },
                    "context_lines": { "type": "integer" },
                    "path_prefix": { "type": "string" },
                    "class_filter": { "type": "string" }
                },
                "required": ["query"]
            }),
        ),
        tool_def(
            "script_check_syntax",
            "Compile-check Luau syntax on target script(s) or every script in the DataModel.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "target": target_schema("Single script target."),
                    "targets": { "type": "array", "items": target_schema("Script targets (omit to check all scripts).") }
                }
            }),
        ),
        tool_def(
            "script_open_tab",
            "Open and focus script tab(s) in RuStudio's code editor.",
            json!({
                "type": "object",
                "properties": {
                    "target": target_schema("Script path or ref."),
                    "targets": { "type": "array", "items": target_schema("Script paths or refs.") }
                }
            }),
        ),

        // Luau (3)
        tool_def(
            "luau_execute",
            "Execute arbitrary Luau code in Edit, Server, or Client VM at selectable security identity (2, 4, 6, or 8; default 8), capturing output, return values, and duration_us.",
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Luau source chunk to execute." },
                    "datamodel": { "type": "string", "enum": ["Edit", "Server", "Client", "Active"] },
                    "identity": { "type": "integer", "enum": [2, 4, 6, 8], "description": "Thread security identity (default: 8)." },
                    "chunk_name": { "type": "string" }
                },
                "required": ["code"]
            }),
        ),
        tool_def(
            "luau_fire_signal",
            "Fire any signal/event (RemoteEvent, BindableEvent, ClickDetector, ProximityPrompt, etc.) on a target instance in Edit, Server, or Client.",
            json!({
                "type": "object",
                "properties": {
                    "target": target_schema("Instance path or ref."),
                    "signal": { "type": "string", "description": "Event/signal name (e.g. 'MouseClick', 'OnServerEvent')." },
                    "args": { "type": "array", "description": "JSON arguments converted to Luau values." },
                    "datamodel": dm_schema()
                },
                "required": ["target", "signal"]
            }),
        ),
        tool_def(
            "luau_vm_stats",
            "Inspect Luau VM memory, GC counts, active thread/slot counts, and SimClock state across Edit, Server, and Client.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema()
                }
            }),
        ),

        // Play & Input (5)
        tool_def(
            "play_control",
            "Start Play (F5), Start Run (F8), Stop (Shift+F5), Restart, or switch viewport/editor view between Server and Client, with optional rig_type and solver.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start_play", "start_run", "stop", "restart", "view_client", "view_server"] },
                    "rig_type": { "type": "string", "enum": ["R15", "R6", "Auto"] },
                    "solver": { "type": "string", "enum": ["studio", "studio-nextgen", "rapier"] }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "play_step_frames",
            "Deterministically step an active Play/Run session by N frames (1..=600) or simulated seconds and return all Output logs emitted during the step.",
            json!({
                "type": "object",
                "properties": {
                    "frames": { "type": "integer", "description": "Number of frames to step (1..=600, default: 1)." },
                    "seconds": { "type": "number", "description": "Alternative target elapsed time in seconds." },
                    "fixed_dt": { "type": "number", "description": "Optional fixed frame delta during step." },
                    "auto_start_play": { "type": "boolean", "description": "Start Play automatically if currently in Edit mode." }
                }
            }),
        ),
        tool_def(
            "play_character_control",
            "Inspect or drive the live Play character: get_state, teleport, move_to, jump, set_stats (Health/MaxHealth/WalkSpeed/JumpPower/JumpHeight), equip/unequip/activate tools, respawn.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get_state", "teleport", "move_to", "jump", "set_stats", "equip_tool", "unequip_tools", "activate_tool", "respawn"] },
                    "position": { "type": "array", "items": { "type": "number" } },
                    "target": target_schema("Optional target instance to teleport/move to or tool to equip."),
                    "stats": { "type": "object" },
                    "tool_name": { "type": "string" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "play_input_inject",
            "Inject keyboard presses/holds, mouse moves/clicks/drags/wheel, or text typing into the active game or editor harness.",
            json!({
                "type": "object",
                "properties": {
                    "events": { "type": "array", "description": "Array of input actions (press, down, up, hold, move, click, right_click, drag, wheel, type_text)." },
                    "action": { "type": "string" },
                    "key": { "type": "string" },
                    "position": { "type": "array", "items": { "type": "number" } },
                    "text": { "type": "string" },
                    "hold_secs": { "type": "number" }
                }
            }),
        ),
        tool_def(
            "output_console",
            "Read, filter, tail, append, or clear RuStudio's Output console log.",
            json!({
                "type": "object",
                "properties": {
                    "level": { "type": "string", "enum": ["Info", "Warn", "Error"] },
                    "contains": { "type": "string" },
                    "since_index": { "type": "integer" },
                    "tail": { "type": "integer" },
                    "clear": { "type": "boolean" },
                    "append": { "type": "object", "properties": { "level": { "type": "string" }, "text": { "type": "string" } } }
                }
            }),
        ),

        // World & Physics (6)
        tool_def(
            "world_raycast",
            "Cast one ray or a batch of rays against Edit, Server, or Client BaseParts, Terrain, and Meshes with Include/Exclude filtering.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "origin": { "type": "array", "items": { "type": "number" } },
                    "direction": { "type": "array", "items": { "type": "number" } },
                    "target_pos": { "type": "array", "items": { "type": "number" } },
                    "rays": { "type": "array" },
                    "filter_mode": { "type": "string", "enum": ["Exclude", "Include"] },
                    "filter_targets": { "type": "array", "items": target_schema("Instances for filter list.") },
                    "ignore_water": { "type": "boolean" }
                }
            }),
        ),
        tool_def(
            "world_spatial_query",
            "Compute exact bounding_box for any instance/Model subtree or run box_overlap / sphere_overlap spatial queries.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "operation": { "type": "string", "enum": ["bounding_box", "box_overlap", "sphere_overlap"] },
                    "target": target_schema("Target for bounding_box."),
                    "cframe": { "type": ["object", "array"] },
                    "size": { "type": "array", "items": { "type": "number" } },
                    "center": { "type": "array", "items": { "type": "number" } },
                    "radius": { "type": "number" },
                    "filter_mode": { "type": "string", "enum": ["Exclude", "Include"] },
                    "filter_targets": { "type": "array", "items": target_schema("Filter instances.") }
                },
                "required": ["operation"]
            }),
        ),
        tool_def(
            "world_place_on_surface",
            "Snap target instance(s) or Model flush onto the surface or terrain below [x, z] using exact bounding-box min_y and optional normal alignment.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "target": target_schema("Single instance or Model."),
                    "targets": { "type": "array", "items": target_schema("Batch of targets.") },
                    "position": { "type": "array", "items": { "type": "number" }, "description": "[x, z] or [x, y, z] raycast start." },
                    "align_to_normal": { "type": "boolean" },
                    "y_offset": { "type": "number" }
                }
            }),
        ),
        tool_def(
            "world_build_structure",
            "High-level procedural 3D builder creating undoable Models of Parts (box_room, wall_run, staircase, ramp, arch, cylinder_ring, scatter).",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "kind": { "type": "string", "enum": ["box_room", "wall_run", "staircase", "ramp", "arch", "cylinder_ring", "scatter"] },
                    "parent": target_schema("Parent container (default: Workspace)."),
                    "name": { "type": "string" },
                    "origin": { "type": "array", "items": { "type": "number" } },
                    "params": { "type": "object" }
                },
                "required": ["kind"]
            }),
        ),
        tool_def(
            "physics_inspect",
            "Inspect live Play/Run physics bodies (Position, AssemblyLinearVelocity, AssemblyAngularVelocity, Mass, Anchored, CanCollide, sleeping/grounded, constraints).",
            json!({
                "type": "object",
                "properties": {
                    "targets": { "type": "array", "items": target_schema("Optional specific parts.") },
                    "limit": { "type": "integer" }
                }
            }),
        ),
        tool_def(
            "physics_manipulate",
            "Set linear/angular velocity, apply impulse/angular impulse, wake bodies, or toggle Anchored/CanCollide on live Server/Client physics parts.",
            json!({
                "type": "object",
                "properties": {
                    "target": target_schema("Target BasePart."),
                    "datamodel": { "type": "string", "enum": ["Server", "Client", "Active"] },
                    "linear_velocity": { "type": "array", "items": { "type": "number" } },
                    "angular_velocity": { "type": "array", "items": { "type": "number" } },
                    "impulse": { "type": "array", "items": { "type": "number" } },
                    "angular_impulse": { "type": "array", "items": { "type": "number" } },
                    "wake": { "type": "boolean" },
                    "anchored": { "type": "boolean" },
                    "can_collide": { "type": "boolean" }
                },
                "required": ["target"]
            }),
        ),

        // Terrain (5)
        tool_def(
            "terrain_inspect",
            "Read Workspace.Terrain properties (WaterColor, WaveSize/Speed, Reflectance, Transparency, Decoration, MaterialColors), chunk bounds, and optional 3D voxel samples.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "region": {
                        "type": "object",
                        "properties": {
                            "min": { "type": "array", "items": { "type": "number" } },
                            "max": { "type": "array", "items": { "type": "number" } },
                            "step": { "type": "number" }
                        }
                    }
                }
            }),
        ),
        tool_def(
            "terrain_fill_shape",
            "Fill or carve ('Air') SmoothGrid terrain shapes (block, ball, cylinder, wedge, water_level) with immediate mesh rebuild and undo support.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "shape": { "type": "string", "enum": ["block", "ball", "cylinder", "wedge", "water_level"] },
                    "material": { "type": "string", "description": "Terrain Enum.Material name (e.g. 'Grass', 'Rock', 'Sand', 'Water', 'Air')." },
                    "center": { "type": "array", "items": { "type": "number" } },
                    "size": { "type": "array", "items": { "type": "number" } },
                    "radius": { "type": "number" },
                    "height": { "type": "number" },
                    "cframe": { "type": ["object", "array"] }
                },
                "required": ["shape", "material"]
            }),
        ),
        tool_def(
            "terrain_sculpt_stroke",
            "Apply Terrain Brush strokes (Add, Subtract, Grow, Erode, Smooth, Flatten, Paint, SeaLevel) at a point or along a stroke path.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "brush": { "type": "string", "enum": ["Add", "Subtract", "Grow", "Erode", "Smooth", "Flatten", "Paint", "SeaLevel"] },
                    "points": { "type": "array", "description": "Array of [x, y, z] stroke positions." },
                    "point": { "type": "array", "items": { "type": "number" } },
                    "brush_size": { "type": "number" },
                    "strength": { "type": "number" },
                    "material": { "type": "string" },
                    "target_plane_y": { "type": "number" }
                },
                "required": ["brush"]
            }),
        ),
        tool_def(
            "terrain_generate",
            "Run RuStudio's procedural biome SmoothGrid terrain generator with seed, position, size, biomes, biome_size, and caves.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "seed": { "type": "integer" },
                    "position": { "type": "array", "items": { "type": "number" } },
                    "size": { "type": "array", "items": { "type": "number" } },
                    "biomes": { "type": "array", "items": { "type": "string" } },
                    "biome_size": { "type": "number" },
                    "caves": { "type": "boolean" }
                }
            }),
        ),
        tool_def(
            "terrain_clear",
            "Clear all SmoothGrid terrain voxels or only a specified {min, max} bounding box.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "region": {
                        "type": "object",
                        "properties": {
                            "min": { "type": "array", "items": { "type": "number" } },
                            "max": { "type": "array", "items": { "type": "number" } }
                        }
                    }
                }
            }),
        ),

        // Viewport & Lighting (3)
        tool_def(
            "viewport_camera",
            "Get or set the editor viewport camera (pos, look_at, yaw_deg, pitch_deg, fov_deg) or focus_target on an instance/Model/selection.",
            json!({
                "type": "object",
                "properties": {
                    "pos": { "type": "array", "items": { "type": "number" } },
                    "look_at": { "type": "array", "items": { "type": "number" } },
                    "yaw_deg": { "type": "number" },
                    "pitch_deg": { "type": "number" },
                    "fov_deg": { "type": "number" },
                    "focus_target": target_schema("Instance or Model to frame (or 'selection').")
                }
            }),
        ),
        tool_def(
            "viewport_capture",
            "Capture a live GPU screenshot of the 3D viewport or full RuStudio window, optionally overriding camera/clean_view and returning PNG image content and/or saving to disk.",
            json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["viewport", "window"] },
                    "camera": { "type": "object" },
                    "clean_view": { "type": "boolean", "description": "Temporarily hide editor grid/gizmos/outlines during capture." },
                    "settle_frames": { "type": "integer", "description": "Frames to wait before capture (default: 2)." },
                    "save_path": { "type": "string", "description": "Optional PNG file output path." },
                    "max_width": { "type": "integer", "description": "Optional downscale max width in pixels." },
                    "return_image": { "type": "boolean", "description": "Return MCP base64 image/png block (default: true)." }
                }
            }),
        ),
        tool_def(
            "lighting_and_atmosphere",
            "Configure Lighting properties, Sky, Atmosphere, Clouds, and post-processing effects (Bloom, Blur, ColorCorrection, DepthOfField, SunRays) or apply lighting presets.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "preset": { "type": "string", "enum": ["daylight", "golden_hour", "cyberpunk_night", "foggy_horror", "studio_neutral"] },
                    "lighting": { "type": "object" },
                    "sky": { "type": "object" },
                    "atmosphere": { "type": "object" },
                    "clouds": { "type": "object" },
                    "effects": { "type": "object" }
                }
            }),
        ),

        // Editor & GUI (4)
        tool_def(
            "editor_selection",
            "Get, set, add, remove, clear, or select_all_of_class on RuStudio's Explorer/Viewport selection.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get", "set", "add", "remove", "clear", "select_all_of_class"] },
                    "targets": { "type": "array", "items": target_schema("Target instances.") },
                    "class": { "type": "string" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "editor_undo_redo",
            "Inspect undo/redo stack status, undo or redo N steps, or clear document edit history.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["status", "undo", "redo", "clear"] },
                    "steps": { "type": "integer" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "editor_ui_and_settings",
            "Configure active transform tool, move/rotate snap increments, collisions, join_surfaces, local_space, show_grid, trigger Studio actions, or read/write studio/rustudio settings.",
            json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "enum": ["Select", "Move", "Scale", "Rotate"] },
                    "snap_move": { "type": "boolean" },
                    "move_inc": { "type": "number" },
                    "snap_rotate": { "type": "boolean" },
                    "rotate_inc": { "type": "number" },
                    "collisions": { "type": "boolean" },
                    "join_surfaces": { "type": "boolean" },
                    "local_space": { "type": "boolean" },
                    "show_grid": { "type": "boolean" },
                    "action": { "type": "string" },
                    "settings": { "type": "object" }
                }
            }),
        ),
        tool_def(
            "gui_inspect",
            "Walk StarterGui, PlayerGui, SurfaceGui, or BillboardGui hierarchies and compute 2D AbsolutePosition/AbsoluteSize rects, visibility, text, ZIndex, and optional point hit-test.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "root": target_schema("Optional GUI root instance."),
                    "screen_size": { "type": "array", "items": { "type": "number" } },
                    "at": { "type": "array", "items": { "type": "number" }, "description": "Optional [x, y] pixel point to hit-test." }
                }
            }),
        ),

        // Project, Plugins, Assets, Reflection (4)
        tool_def(
            "project_rojo",
            "Export open place to a Rojo project folder, compute Rojo sourcemap.json, or build a .project.json folder into a .rbxl file.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["export", "sourcemap", "build"] },
                    "path": { "type": "string", "description": "Project folder or .project.json path." },
                    "out_path": { "type": "string", "description": "Output directory or .rbxl path." },
                    "open": { "type": "boolean" },
                    "include_non_scripts": { "type": "boolean" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "plugin_control",
            "List, load (.lua/.luau/.rbxm/.rbxmx), reload, unload, inspect UI items, or trigger toolbar buttons/actions of Studio plugins.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "load", "reload", "unload", "list_ui", "trigger"] },
                    "path": { "type": "string" },
                    "plugin_id": { "type": "string" },
                    "ui_id": { "type": "string" }
                },
                "required": ["action"]
            }),
        ),
        tool_def(
            "asset_import",
            "Import local .rbxm/.rbxmx models, .fbx 3D models/animations, .obj meshes, or insert a Roblox asset by numeric asset ID into the DataModel.",
            json!({
                "type": "object",
                "properties": {
                    "datamodel": dm_schema(),
                    "path": { "type": "string", "description": "Local model/mesh path (.rbxm/.rbxmx/.fbx/.obj)." },
                    "asset_id": { "type": "integer", "description": "Numeric Roblox asset ID." },
                    "parent": target_schema("Target parent (default: Workspace).")
                }
            }),
        ),
        tool_def(
            "reflection_api_search",
            "Search RuStudio's embedded Roblox reflection database and api-dump.json for classes, full property/method/event schemas (including security & defaults), and Enum items.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring query for class or enum names." },
                    "class": { "type": "string", "description": "Exact Roblox class name to inspect." },
                    "enum_name": { "type": "string", "description": "Exact Roblox Enum name to list items." },
                    "include_inherited": { "type": "boolean" }
                }
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    use std::net::TcpListener;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustudio-mcp-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn offline_tools_list_includes_bridge_and_all_engine_tools() {
        let dir = temp_dir("offline-tools");
        let mut session = BridgeSession::with_instances_dir(dir.clone(), None, None);

        let resp = handle_rpc(json!(1), "tools/list", &json!({}), &mut session);
        let result = resp.result.expect("tools/list result");
        let tools = result["tools"].as_array().expect("tools array");

        let names: HashSet<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();

        let expected = [
            // 4 bridge-native tools
            "studio_list_instances",
            "studio_launch",
            "studio_kill_instance",
            "studio_select_instance",
            // 53 OmniMCP engine tools
            "omni_batch",
            "studio_get_state",
            "place_new",
            "place_open",
            "place_save",
            "place_diagnostics",
            "studio_quit",
            "dom_get_tree",
            "dom_inspect_instance",
            "dom_query",
            "dom_create_instances",
            "dom_set_properties",
            "dom_reparent_rename_delete",
            "dom_clone_and_transform",
            "dom_group_ungroup",
            "dom_serialize_subtree",
            "script_list_all",
            "script_read_batch",
            "script_write_batch",
            "script_patch_batch",
            "script_grep",
            "script_check_syntax",
            "script_open_tab",
            "luau_execute",
            "luau_fire_signal",
            "luau_vm_stats",
            "play_control",
            "play_step_frames",
            "play_character_control",
            "play_input_inject",
            "output_console",
            "world_raycast",
            "world_spatial_query",
            "world_place_on_surface",
            "world_build_structure",
            "physics_inspect",
            "physics_manipulate",
            "terrain_inspect",
            "terrain_fill_shape",
            "terrain_sculpt_stroke",
            "terrain_generate",
            "terrain_clear",
            "viewport_camera",
            "viewport_capture",
            "lighting_and_atmosphere",
            "editor_selection",
            "editor_undo_redo",
            "editor_ui_and_settings",
            "gui_inspect",
            "project_rojo",
            "plugin_control",
            "asset_import",
            "reflection_api_search",
        ];
        for name in expected {
            assert!(names.contains(name), "missing tool '{name}' in offline tools/list");
        }
        assert_eq!(tools.len(), 57);

        // Verify engine tools carry optional studio_id schema while bridge tools do not get polluted.
        let dom_tree = tools.iter().find(|t| t["name"] == "dom_get_tree").unwrap();
        assert!(dom_tree["inputSchema"]["properties"].get("studio_id").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_rpc_dispatch_supports_newline_and_content_length_framing() {
        let dir = temp_dir("framing");
        let mut session = BridgeSession::with_instances_dir(dir.clone(), None, None);

        // 1) Newline-delimited JSON-RPC sequence (initialize, notification, ping, resources/read, prompts/list, unknown)
        let req1 = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}});
        let notif = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        let req2 = json!({"jsonrpc":"2.0","id":2,"method":"ping"});
        let req3 = json!({"jsonrpc":"2.0","id":3,"method":"resources/list"});
        let req4 = json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"rustudio://instances"}});
        let req5 = json!({"jsonrpc":"2.0","id":5,"method":"prompts/list"});
        let req6 = json!({"jsonrpc":"2.0","id":6,"method":"no/such/method"});

        let input = format!("{req1}\n{notif}\n{req2}\n{req3}\n{req4}\n{req5}\n{req6}\n");
        let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
        let mut out = Vec::new();
        serve_stdio(&mut reader, &mut out, &mut session);

        let out_str = String::from_utf8(out).unwrap();
        let lines: Vec<Value> = out_str
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Notification produced no line -> 6 responses total.
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], "rustudio-omnimcp");
        assert_eq!(lines[1]["id"], 2);
        assert!(lines[1]["result"].is_object());
        assert_eq!(lines[2]["id"], 3);
        let res_uris: Vec<&str> = lines[2]["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert!(res_uris.contains(&"rustudio://instances"));
        assert!(res_uris.contains(&"rustudio://state"));
        assert_eq!(lines[3]["id"], 4);
        assert_eq!(lines[3]["result"]["contents"][0]["uri"], "rustudio://instances");
        assert_eq!(lines[4]["id"], 5);
        assert_eq!(lines[4]["result"]["prompts"].as_array().unwrap().len(), 4);
        assert_eq!(lines[5]["id"], 6);
        assert_eq!(lines[5]["error"]["code"], -32601);

        // 2) Content-Length framed input produces Content-Length framed output
        let body = json!({"jsonrpc":"2.0","id":99,"method":"ping"}).to_string();
        let framed_in = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut cl_reader = BufReader::new(Cursor::new(framed_in.into_bytes()));
        let mut cl_out = Vec::new();
        serve_stdio(&mut cl_reader, &mut cl_out, &mut session);

        let cl_str = String::from_utf8(cl_out).unwrap();
        let (hdrs, resp_body) = cl_str.split_once("\r\n\r\n").expect("Content-Length header separator");
        assert!(hdrs.starts_with("Content-Length: "));
        let parsed: Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["id"], 99);
        assert!(parsed["result"].is_object());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn spawn_mock_bridge(label: &'static str) -> (u16, std::sync::mpsc::Receiver<BridgeRequest>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut out = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let req: BridgeRequest = serde_json::from_str(line.trim()).unwrap();
                let _ = tx.send(req.clone());
                let resp = match req {
                    BridgeRequest::Ping => BridgeResponse::Pong {
                        pid: std::process::id(),
                        port,
                        title: label.into(),
                        mode: "Edit".into(),
                    },
                    BridgeRequest::CallTool { name, arguments } => {
                        BridgeResponse::ToolResult(ToolCallResult::json(&json!({
                            "server": label,
                            "tool": name,
                            "arguments": arguments,
                        })))
                    }
                    _ => BridgeResponse::Error {
                        code: -32601,
                        message: "unsupported in mock".into(),
                    },
                };
                let text = serde_json::to_string(&resp).unwrap();
                let _ = writeln!(out, "{text}");
                let _ = out.flush();
            }
        });
        (port, rx)
    }

    #[test]
    fn studio_id_routing_and_stripping_work_across_precedence_levels() {
        let dir = temp_dir("routing");
        let (port_a, rx_a) = spawn_mock_bridge("PlaceA");
        let (port_b, rx_b) = spawn_mock_bridge("PlaceB");

        // Write two live-PID instance records (note: use current PID on newer one,
        // and test port routing + explicit studio_id routing).
        let pid_a = 11111u32; // note: non-existent PID would be pruned if written as json,
                              // so use current PID for one record and direct port for second,
                              // AND test select + studio_id overrides.
        let my_pid = std::process::id();
        let rec = InstanceRecord {
            pid: my_pid,
            port: port_a,
            title: "PlaceA.rbxl".into(),
            place_path: None,
            mode: "Edit".into(),
            started_unix: 200,
        };
        std::fs::write(
            dir.join(format!("{my_pid}.json")),
            serde_json::to_vec(&rec).unwrap(),
        )
        .unwrap();

        let mut session = BridgeSession::with_instances_dir(dir.clone(), None, None);

        // 1) Default routes to most recent live instance (PlaceA), stripping nothing when absent.
        let r1 = handle_rpc(
            json!(1),
            "tools/call",
            &json!({
                "name": "studio_get_state",
                "arguments": { "extra": 42 }
            }),
            &mut session,
        );
        let payload1: Value =
            serde_json::from_str(r1.result.unwrap()["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload1["server"], "PlaceA");
        assert_eq!(payload1["arguments"], json!({"extra": 42}));
        assert!(matches!(rx_a.recv_timeout(Duration::from_secs(1)).unwrap(), BridgeRequest::CallTool { .. }));

        // 2) Select PlaceB by port via `studio_select_instance`.
        let sel = handle_rpc(
            json!(2),
            "tools/call",
            &json!({
                "name": "studio_select_instance",
                "arguments": { "port": port_b }
            }),
            &mut session,
        );
        let sel_txt = sel.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
        assert!(sel_txt.contains("PlaceB"), "unexpected select result: {sel_txt}");
        let _ = rx_b.recv_timeout(Duration::from_secs(1)).unwrap(); // Ping

        // Subsequent call with no studio_id goes to session-selected PlaceB.
        let r2 = handle_rpc(
            json!(3),
            "tools/call",
            &json!({
                "name": "dom_get_tree",
                "arguments": { "max_depth": 2 }
            }),
            &mut session,
        );
        let payload2: Value =
            serde_json::from_str(r2.result.unwrap()["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload2["server"], "PlaceB");
        let _ = rx_b.recv_timeout(Duration::from_secs(1)).unwrap();

        // 3) Per-call `studio_id` by PID overrides session selection back to PlaceA AND strips `studio_id`.
        let r3 = handle_rpc(
            json!(4),
            "tools/call",
            &json!({
                "name": "dom_get_tree",
                "arguments": { "studio_id": my_pid, "max_depth": 3 }
            }),
            &mut session,
        );
        let payload3: Value =
            serde_json::from_str(r3.result.unwrap()["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload3["server"], "PlaceA");
        assert_eq!(payload3["arguments"], json!({"max_depth": 3}));
        assert!(payload3["arguments"].get("studio_id").is_none());
        let _ = rx_a.recv_timeout(Duration::from_secs(1)).unwrap();

        // 4) Per-call `studio_id` by `"port:<B>"` overrides session/PID and strips `studio_id`.
        let _ = pid_a;
        let r4 = handle_rpc(
            json!(5),
            "tools/call",
            &json!({
                "name": "studio_get_state",
                "arguments": { "studio_id": format!("port:{port_b}") }
            }),
            &mut session,
        );
        let payload4: Value =
            serde_json::from_str(r4.result.unwrap()["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload4["server"], "PlaceB");
        assert_eq!(payload4["arguments"], json!({}));
        let _ = rx_b.recv_timeout(Duration::from_secs(1)).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
