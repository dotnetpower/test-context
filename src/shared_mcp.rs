use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

const INSTRUCTIONS: &str = "TCTX (test-context) is a local Rust AST dependency graph and speculative background pytest runner/cache. Use test_run (#tctxNativeRun) FIRST instead of running shell pytest directly: unchanged tests that already passed in the background return in < 5ms from cache, and only uncached/modified tests run. Use test_affected (#tctxNativeAffected) to see which tests are affected by edited source files and whether the background runner has already verified them.";

pub fn serve(mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let default_root = std::env::current_dir().ok();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut input)
            .take(MAX_MESSAGE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_MESSAGE {
            let err = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32600, "message": "MCP message exceeds MAX_MESSAGE" }
            });
            writeln!(output, "{err}")?;
            output.flush()?;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let parsed: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                writeln!(output, "{err}")?;
                output.flush()?;
                continue;
            }
        };
        if let Some(reply) = dispatch_with_root(parsed, default_root.as_ref()) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
}

pub fn dispatch_with_root(request: Value, default_root: Option<&PathBuf>) -> Option<Value> {
    if !request.is_object() || request["jsonrpc"] != "2.0" || !request["method"].is_string() {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32600, "message": "invalid JSON-RPC request" }
        }));
    }
    let id = request.get("id")?.clone();
    let method = request["method"].as_str()?;
    let params = &request["params"];

    let result = match method {
        "initialize" => {
            let version = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
            let supported = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
            Ok(json!({
                "protocolVersion": if supported.contains(&version) { version } else { "2025-06-18" },
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "test-context",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(crate::mcp::tools()),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing tool name" }
                }));
            };
            let mut arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let root = resolve_tool_root(name, &mut arguments, default_root);
            match root {
                Ok(resolved_root) => {
                    let req = Request {
                        root: resolved_root,
                        name: name.to_string(),
                        arguments,
                        compact: false,
                    };
                    match daemon::request_or_local(&req) {
                        Ok(val) => Ok(json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string())
                            }],
                            "isError": false
                        })),
                        Err(err) => Ok(json!({
                            "content": [{
                                "type": "text",
                                "text": err.to_string()
                            }],
                            "isError": true
                        })),
                    }
                }
                Err(err) => Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": err.to_string()
                    }],
                    "isError": true
                })),
            }
        }
        _ => Err((-32601, format!("unknown method: {method}"))),
    };

    Some(match result {
        Ok(res) => json!({ "jsonrpc": "2.0", "id": id, "result": res }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    })
}

fn resolve_tool_root(
    name: &str,
    arguments: &mut Value,
    default_root: Option<&PathBuf>,
) -> Result<Option<PathBuf>> {
    if name == "test_workspaces" || name == "ping" {
        return Ok(None);
    }
    if let Some(obj) = arguments.as_object_mut()
        && let Some(root_val) = obj.remove("root")
        && let Some(root_str) = root_val.as_str()
        && !root_str.trim().is_empty()
    {
        return Ok(Some(PathBuf::from(root_str).canonicalize()?));
    }
    if let Some(def) = default_root {
        return Ok(Some(def.canonicalize()?));
    }
    if name == "test_status" {
        return Ok(None);
    }
    Err(invalid("workspace root is required when current directory is unavailable").into())
}
