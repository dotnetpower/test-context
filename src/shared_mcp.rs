use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::runner::{clean_path_or_target, normalize_target_for_root, strip_line_or_fragment_suffix};
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

const INSTRUCTIONS: &str = "Use test_run before shell pytest/cargo/vitest/jest/go test in registered workspaces (cached passes return in <5ms; only uncached deltas run). Use test_affected to inspect affected tests, cache state, and cached lint issues. Pass explicit absolute `root` when known.";
const MCP_WORKERS: usize = 4;
const MCP_MAX_TEXT_BYTES: usize = 12 * 1024;
const MCP_TRIM_THRESHOLD_BYTES: usize = 256 * 1024;

fn read_next_line_poll_fd(
    fd: std::os::fd::RawFd,
    pending: &mut Vec<u8>,
    draining_oversized: &mut bool,
    line_out: &mut Vec<u8>,
    output_broken: &std::sync::atomic::AtomicBool,
) -> std::io::Result<usize> {
    use std::sync::atomic::Ordering;
    line_out.clear();
    let mut chunk = [0u8; 4096];
    loop {
        if *draining_oversized {
            if let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                pending.drain(..nl + 1);
                *draining_oversized = false;
            } else {
                pending.clear();
            }
        }
        if !*draining_oversized {
            if let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                let take = (nl + 1).min(MAX_MESSAGE + 1);
                line_out.extend_from_slice(&pending[..take]);
                pending.drain(..nl + 1);
                return Ok(line_out.len());
            }
            if pending.len() > MAX_MESSAGE {
                // Report oversized line immediately so caller receives -32600 without deadlock;
                // remaining bytes up to the next '\n' are discarded on the next call.
                line_out.extend_from_slice(&pending[..MAX_MESSAGE + 1]);
                pending.clear();
                *draining_oversized = true;
                return Ok(line_out.len());
            }
        }
        if output_broken.load(Ordering::Acquire) {
            return Ok(0);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 100) };
        if rc == 0 {
            continue;
        }
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        let n = unsafe {
            libc::read(
                fd,
                chunk.as_mut_ptr().cast::<libc::c_void>(),
                chunk.len(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            if *draining_oversized || pending.is_empty() {
                pending.clear();
                *draining_oversized = false;
                return Ok(0);
            }
            let take = pending.len().min(MAX_MESSAGE + 1);
            line_out.extend_from_slice(&pending[..take]);
            pending.clear();
            return Ok(line_out.len());
        }
        pending.extend_from_slice(&chunk[..n as usize]);
    }
}

pub fn serve_stdio() -> Result<()> {
    let stdout = std::io::stdout();
    serve_inner(std::io::empty(), stdout, Some(libc::STDIN_FILENO))
}

pub fn serve(input: impl BufRead, output: impl Write + Send) -> Result<()> {
    serve_inner(input, output, None)
}

fn serve_inner<R: BufRead>(
    mut input: R,
    output: impl Write + Send,
    poll_stdin_fd: Option<std::os::fd::RawFd>,
) -> Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        let _ = libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    let output = Arc::new(Mutex::new(std::io::BufWriter::with_capacity(
        64 * 1024,
        output,
    )));
    let output_broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let default_root = Arc::new(std::env::current_dir().ok());

    std::thread::scope(|scope| -> Result<()> {
        let (tx, rx) = sync_channel::<Value>(MCP_WORKERS * 4);
        let (ctrl_tx, ctrl_rx) = sync_channel::<Value>(16);
        let (cancel_tx, cancel_rx) = sync_channel::<Value>(16);
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..MCP_WORKERS {
            let rx_w = Arc::clone(&rx);
            let out_w = Arc::clone(&output);
            let broken_w = Arc::clone(&output_broken);
            let root_w = Arc::clone(&default_root);
            scope.spawn(move || {
                use std::sync::atomic::Ordering;
                loop {
                    if broken_w.load(Ordering::Acquire) {
                        return;
                    }
                    let req = {
                        let guard = rx_w.lock().unwrap_or_else(|p| p.into_inner());
                        guard.recv()
                    };
                    let Ok(parsed) = req else {
                        return;
                    };
                    if broken_w.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(reply) = dispatch_with_root(parsed, root_w.as_ref().as_ref()) {
                        let serialized = reply.to_string();
                        let large = serialized.len() >= MCP_TRIM_THRESHOLD_BYTES;
                        let write_err = {
                            let mut out = out_w.lock().unwrap_or_else(|p| p.into_inner());
                            writeln!(out, "{serialized}")
                                .and_then(|()| out.flush())
                                .is_err()
                        };
                        if write_err {
                            broken_w.store(true, Ordering::Release);
                            return;
                        }
                        if large {
                            let _ = daemon::trim_allocator();
                        }
                    }
                }
            });
        }
        // Dedicated control worker so `test_cancel` / `test_status` / `test_workspaces`
        // never starve behind long-running `test_run` calls occupying all base workers.
        {
            let out_w = Arc::clone(&output);
            let broken_w = Arc::clone(&output_broken);
            let root_w = Arc::clone(&default_root);
            scope.spawn(move || {
                use std::sync::atomic::Ordering;
                while let Ok(parsed) = ctrl_rx.recv() {
                    if broken_w.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(reply) = dispatch_with_root(parsed, root_w.as_ref().as_ref()) {
                        let serialized = reply.to_string();
                        let write_err = {
                            let mut out = out_w.lock().unwrap_or_else(|p| p.into_inner());
                            writeln!(out, "{serialized}")
                                .and_then(|()| out.flush())
                                .is_err()
                        };
                        if write_err {
                            broken_w.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
            });
        }
        // Dedicated cancel worker isolated from both `test_run` and `test_status(load=true)`
        {
            let out_w = Arc::clone(&output);
            let broken_w = Arc::clone(&output_broken);
            let root_w = Arc::clone(&default_root);
            scope.spawn(move || {
                use std::sync::atomic::Ordering;
                while let Ok(parsed) = cancel_rx.recv() {
                    if broken_w.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(reply) = dispatch_with_root(parsed, root_w.as_ref().as_ref()) {
                        let serialized = reply.to_string();
                        let write_err = {
                            let mut out = out_w.lock().unwrap_or_else(|p| p.into_inner());
                            writeln!(out, "{serialized}")
                                .and_then(|()| out.flush())
                                .is_err()
                        };
                        if write_err {
                            broken_w.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
            });
        }
        // Drop parent's handle to `rx` so if all workers exit on broken stdout, `tx.send`
        // fails immediately instead of deadlocking on a full bounded channel.
        drop(rx);

        let mut line = Vec::new();
        let mut fd_pending = Vec::new();
        let mut draining_oversized = false;
        loop {
            if output_broken.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            line.clear();
            if line.capacity() > 64 * 1024 {
                line.shrink_to(16 * 1024);
            }
            let read = if let Some(fd) = poll_stdin_fd {
                read_next_line_poll_fd(
                    fd,
                    &mut fd_pending,
                    &mut draining_oversized,
                    &mut line,
                    &output_broken,
                )?
            } else {
                (&mut input)
                    .take(MAX_MESSAGE as u64 + 1)
                    .read_until(b'\n', &mut line)?
            };
            if read == 0 {
                break;
            }
            if line.len() > MAX_MESSAGE {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32600, "message": "MCP message exceeds MAX_MESSAGE" }
                });
                let mut out = output.lock().unwrap_or_else(|p| p.into_inner());
                writeln!(out, "{err}")?;
                out.flush()?;
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
                    let mut out = output.lock().unwrap_or_else(|p| p.into_inner());
                    writeln!(out, "{err}")?;
                    out.flush()?;
                    continue;
                }
            };
            let is_tool_call = parsed.get("method").and_then(Value::as_str) == Some("tools/call");
            if is_tool_call {
                let tool_name = parsed
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_status_load = tool_name == "test_status"
                    && parsed
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("load"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let send_res = if tool_name == "test_cancel" {
                    cancel_tx.try_send(parsed)
                } else if matches!(tool_name, "test_workspaces" | "ping")
                    || (tool_name == "test_status" && !is_status_load)
                {
                    ctrl_tx.try_send(parsed)
                } else {
                    tx.try_send(parsed)
                };
                match send_res {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(rejected)) => {
                        if let Some(id) = rejected.get("id").cloned() {
                            let err = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32000,
                                    "message": "MCP server overloaded; retry"
                                }
                            });
                            let mut out = output.lock().unwrap_or_else(|p| p.into_inner());
                            writeln!(out, "{err}")?;
                            out.flush()?;
                        }
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                }
            } else if let Some(reply) = dispatch_with_root(parsed, default_root.as_ref().as_ref()) {
                let mut out = output.lock().unwrap_or_else(|p| p.into_inner());
                writeln!(out, "{reply}")?;
                out.flush()?;
            }
        }
        drop(tx);
        drop(ctrl_tx);
        drop(cancel_tx);
        Ok(())
    })
}

fn compact_mcp_payload(name: &str, mut val: Value) -> Value {
    let Some(obj) = val.as_object_mut() else {
        return val;
    };
    match name {
        "test_run" => {
            // Preserve cached_files intact for small runs (<=10); only compact large passing lists
            if let Some(arr) = obj.get("cached_files").and_then(Value::as_array)
                && arr.len() > 10
            {
                let total_cached = arr.len();
                let failed_only: Vec<Value> = arr
                    .iter()
                    .filter(|item| item.get("outcome").and_then(Value::as_str) != Some("passed"))
                    .cloned()
                    .collect();
                let omitted = total_cached.saturating_sub(failed_only.len());
                obj.insert("cached_files".into(), Value::Array(failed_only));
                if omitted > 0 {
                    obj.insert("cached_passing_files_omitted".into(), json!(omitted));
                }
            }
            if let Some(arr) = obj.get_mut("executed_files").and_then(Value::as_array_mut) {
                for item in arr.iter_mut() {
                    if let Some(item_obj) = item.as_object_mut() {
                        item_obj.remove("dep_hash");
                    }
                }
                if arr.len() > 25 {
                    let omitted = arr.len() - 25;
                    arr.truncate(25);
                    obj.insert("executed_files_omitted".into(), json!(omitted));
                }
            }
            if let Some(arr) = obj.get_mut("ruff_diagnostics").and_then(Value::as_array_mut)
                && arr.len() > 20
            {
                let omitted = arr.len() - 20;
                arr.truncate(20);
                obj.insert("ruff_diagnostics_omitted".into(), json!(omitted));
            }
            if let Some(arr) = obj.get_mut("failed_nodeids").and_then(Value::as_array_mut)
                && arr.len() > 40
            {
                let omitted = arr.len() - 40;
                arr.truncate(40);
                obj.insert("failed_nodeids_omitted".into(), json!(omitted));
            }
            if let Some(arr) = obj.get_mut("structured_failures").and_then(Value::as_array_mut)
                && arr.len() > 15
            {
                let omitted = arr.len() - 15;
                arr.truncate(15);
                obj.insert("structured_failures_omitted".into(), json!(omitted));
            }
            if let Some(s) = obj.get("stdout_summary").and_then(Value::as_str)
                && s.len() > 6 * 1024
            {
                let mut end = 6 * 1024;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated = format!(
                    "{}\n... [truncated {} bytes of stdout_summary]",
                    &s[..end],
                    s.len() - end
                );
                obj.insert("stdout_summary".into(), Value::String(truncated));
            }
        }
        "test_affected" => {
            let mut extra_paths: Vec<Value> = Vec::new();
            if let Some(arr) = obj.get_mut("affected_tests").and_then(Value::as_array_mut) {
                if arr.len() > 20 {
                    for item in arr.drain(20..) {
                        if let Some(tf) = item.get("test_file").and_then(Value::as_str) {
                            extra_paths.push(Value::String(tf.to_string()));
                        }
                    }
                }
                for item in arr.iter_mut() {
                    if let Some(item_obj) = item.as_object_mut() {
                        item_obj.remove("dep_hash");
                        let mut syms_omitted = 0usize;
                        if let Some(syms) =
                            item_obj.get_mut("test_symbols").and_then(Value::as_array_mut)
                            && syms.len() > 10
                        {
                            syms_omitted = syms.len() - 10;
                            syms.truncate(10);
                        }
                        if syms_omitted > 0 {
                            item_obj.insert("test_symbols_omitted".into(), json!(syms_omitted));
                        }
                    }
                }
            }
            if !extra_paths.is_empty() {
                obj.insert("additional_affected_test_files".into(), Value::Array(extra_paths));
            }
            if let Some(arr) = obj.get_mut("ruff_diagnostics").and_then(Value::as_array_mut)
                && arr.len() > 20
            {
                let omitted = arr.len() - 20;
                arr.truncate(20);
                obj.insert("ruff_diagnostics_omitted".into(), json!(omitted));
            }
        }
        "test_graph" => {
            for key in [
                "direct_imports",
                "direct_imported_by",
                "transitive_dependencies",
                "affected_tests",
                "test_symbols",
                "fixtures_defined",
                "env_vars_referenced",
            ] {
                if let Some(arr) = obj.get_mut(key).and_then(Value::as_array_mut)
                    && arr.len() > 30
                {
                    let omitted = arr.len() - 30;
                    arr.truncate(30);
                    obj.insert(format!("{key}_omitted"), json!(omitted));
                }
            }
        }
        _ => {}
    }
    val
}

fn extract_failed_file_identities(val: &Value) -> (Vec<Value>, u64) {
    let mut paths: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut omitted = 0u64;
    if let Some(obj) = val.as_object() {
        for src_key in ["executed_files", "cached_files"] {
            if let Some(arr) = obj.get(src_key).and_then(Value::as_array) {
                for item in arr {
                    if matches!(
                        item.get("outcome").and_then(Value::as_str),
                        Some("failed" | "error")
                    ) && let Some(tf) = item.get("test_file").and_then(Value::as_str)
                        && seen.insert(tf.to_string())
                    {
                        if paths.len() < 15 && tf.len() <= 512 {
                            paths.push(Value::String(tf.to_string()));
                        } else {
                            omitted = omitted.saturating_add(1);
                        }
                    }
                }
            }
        }
    }
    (paths, omitted)
}

fn serialize_mcp_result(name: &str, val: Value) -> String {
    // Preserve full lossless JSON (including dep_hash, test_symbols, test_conventions, cached_files)
    // whenever the compact serialization already fits within the MCP_MAX_TEXT_BYTES budget.
    let raw_text = serde_json::to_string(&val).unwrap_or_else(|_| val.to_string());
    if raw_text.len() <= MCP_MAX_TEXT_BYTES {
        return raw_text;
    }
    // Capture failed/error file identities BEFORE compact_mcp_payload truncates executed_files
    let (pre_failed_files, pre_failed_files_omitted) = extract_failed_file_identities(&val);
    let mut compacted = compact_mcp_payload(name, val);
    if let Some(obj) = compacted.as_object_mut() {
        obj.insert("truncated".into(), Value::Bool(true));
        if (!pre_failed_files.is_empty() || pre_failed_files_omitted > 0)
            && (obj.contains_key("executed_files_omitted")
                || obj.contains_key("cached_passing_files_omitted")
                || obj.contains_key("cached_files_omitted"))
        {
            if !pre_failed_files.is_empty() {
                obj.insert("failed_files".into(), Value::Array(pre_failed_files.clone()));
            }
            if pre_failed_files_omitted > 0 {
                obj.insert(
                    "failed_files_omitted".into(),
                    json!(pre_failed_files_omitted),
                );
            }
        }
    }
    let text = serde_json::to_string(&compacted).unwrap_or_else(|_| compacted.to_string());
    if text.len() <= MCP_MAX_TEXT_BYTES {
        return text;
    }
    // Handle top-level arrays (e.g., `test_workspaces`) by compacting each entry while ALWAYS
    // preserving complete roots/scopes (usable for follow-up calls), dynamically reserving the
    // exact sentinel size, and strictly enforcing MCP_MAX_TEXT_BYTES.
    if let Value::Array(items) = &compacted {
        let total = items.len();
        let mut compact_items: Vec<Value> = Vec::with_capacity(items.len());
        let worst_sentinel = json!({
            "_truncated": true,
            "fields_compacted": true,
            "total": total,
            "omitted": total,
            "hint": "Run `tctx workspaces` CLI for full globs/policy details.",
        })
        .to_string();
        let reserve_sentinel = worst_sentinel.len() + 2;
        let mut approx_bytes = 2usize;
        let mut fields_compacted = false;
        for item in items {
            let entry = if let Some(obj) = item.as_object() {
                if obj.len() > 3 {
                    fields_compacted = true;
                }
                // Keep complete `root` and `scopes` intact so paths remain usable in subsequent calls
                let root_val = obj.get("root").cloned().unwrap_or(Value::Null);
                let scopes_val = obj.get("scopes").cloned().unwrap_or(Value::Null);
                let globs_count = obj
                    .get("globs")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                json!({
                    "root": root_val,
                    "scopes": scopes_val,
                    "globs_count": globs_count,
                })
            } else {
                item.clone()
            };
            let entry_len = serde_json::to_string(&entry).map_or(64, |s| s.len() + 1);
            if approx_bytes + entry_len + reserve_sentinel > MCP_MAX_TEXT_BYTES {
                break;
            }
            approx_bytes += entry_len;
            compact_items.push(entry);
        }
        let omitted = total.saturating_sub(compact_items.len());
        if omitted > 0 || fields_compacted {
            compact_items.push(json!({
                "_truncated": true,
                "fields_compacted": fields_compacted,
                "total": total,
                "omitted": omitted,
                "hint": "Run `tctx workspaces` CLI for full globs/policy details.",
            }));
        }
        let encoded = serde_json::to_string(&Value::Array(compact_items))
            .unwrap_or_else(|_| "[]".to_string());
        if encoded.len() <= MCP_MAX_TEXT_BYTES {
            return encoded;
        }
        return json!([{"_truncated": true, "total": total, "omitted": total, "hint": "Run `tctx workspaces` CLI for full list."}]).to_string();
    }
    // Always preserve valid JSON and actionable failure details by shedding bulky fields progressively
    if let Some(obj) = compacted.as_object_mut() {
        obj.insert("truncated".into(), Value::Bool(true));
        obj.remove("test_conventions");
        for (trim_key, omitted_key) in [
            ("executed_files", Some("executed_files_omitted")),
            ("cached_files", Some("cached_files_omitted")),
            ("structured_failures", Some("structured_failures_omitted")),
            ("ruff_diagnostics", Some("ruff_diagnostics_omitted")),
            ("warnings_summary", Some("warnings_summary_omitted")),
            (
                "additional_affected_test_files",
                Some("additional_affected_test_files_omitted"),
            ),
        ] {
            let mut newly_shed = 0usize;
            if let Some(arr) = obj.get_mut(trim_key).and_then(Value::as_array_mut)
                && arr.len() > 5
            {
                newly_shed = arr.len() - 5;
                arr.truncate(5);
            }
            if newly_shed > 0
                && let Some(ok) = omitted_key
            {
                let prev = obj.get(ok).and_then(Value::as_u64).unwrap_or(0);
                obj.insert(ok.to_string(), json!(prev.saturating_add(newly_shed as u64)));
            }
        }
        if !pre_failed_files.is_empty() && !obj.contains_key("failed_files") {
            obj.insert("failed_files".into(), Value::Array(pre_failed_files.clone()));
        }
        if pre_failed_files_omitted > 0 && !obj.contains_key("failed_files_omitted") {
            obj.insert(
                "failed_files_omitted".into(),
                json!(pre_failed_files_omitted),
            );
        }
        if let Some(s) = obj.get("stdout_summary").and_then(Value::as_str)
            && s.len() > 2048
        {
            let mut end = 2048;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            let shorter = format!("{}\n... [truncated]", &s[..end]);
            obj.insert("stdout_summary".into(), Value::String(shorter));
        }
        let retry =
            serde_json::to_string(obj).unwrap_or_else(|_| Value::Object(obj.clone()).to_string());
        if retry.len() <= MCP_MAX_TEXT_BYTES {
            return retry;
        }
        // Final valid-JSON fallback retaining counts PLUS actionable failure IDs, top diagnostics, and short stdout excerpt
        let mut minimal = serde_json::Map::new();
        minimal.insert("truncated".into(), Value::Bool(true));
        for keep in [
            "root",
            "status",
            "mode",
            "total_test_files",
            "cached_files_count",
            "executed_files_count",
            "passed",
            "failed",
            "skipped",
            "errors",
            "duration_ms",
            "saved_ms",
            "total_affected_test_files",
            "ruff_errors_count",
            "path",
            "dep_hash",
        ] {
            if let Some(v) = obj.get(keep) {
                // Never corrupt paths or hashes by slicing with "..."; omit only if absurdly huge
                let keep_val = match v {
                    Value::String(s) if s.len() > 2048 => None,
                    other => Some(other.clone()),
                };
                if let Some(bounded) = keep_val {
                    minimal.insert(keep.to_string(), bounded);
                }
            }
        }
        // Preserve compact failed/error file paths collected before array truncation
        if !pre_failed_files.is_empty() || pre_failed_files_omitted > 0 {
            if !pre_failed_files.is_empty() {
                minimal.insert("failed_files".into(), Value::Array(pre_failed_files));
            }
            if pre_failed_files_omitted > 0 {
                minimal.insert(
                    "failed_files_omitted".into(),
                    json!(pre_failed_files_omitted),
                );
            }
        }
        if let Some(arr) = obj.get("affected_tests").and_then(Value::as_array) {
            let mut compact_affected: Vec<Value> = Vec::new();
            for item in arr.iter().take(15) {
                if let Some(s) = item.as_str() {
                    // `test_graph` emits `affected_tests` as plain string paths
                    if s.len() <= 512 {
                        compact_affected.push(Value::String(s.to_string()));
                    }
                    continue;
                }
                let Some(tf) = item.get("test_file").and_then(Value::as_str) else {
                    continue;
                };
                // Never alter/truncate paths with "..."; omit oversized paths cleanly instead
                if tf.len() > 512 {
                    continue;
                }
                let sym_count = item
                    .get("test_symbols")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len) as u64
                    + item
                        .get("test_symbols_omitted")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                let mut entry = json!({
                    "test_file": tf,
                    "cache_state": item.get("cache_state").and_then(Value::as_str).unwrap_or("unknown"),
                });
                if sym_count > 0
                    && let Some(eobj) = entry.as_object_mut()
                {
                    eobj.insert("test_symbols_omitted".into(), json!(sym_count));
                }
                compact_affected.push(entry);
            }
            let prior_omitted = obj
                .get("affected_tests_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_known = obj
                .get("total_affected_test_files")
                .and_then(Value::as_u64)
                .unwrap_or((arr.len() as u64).saturating_add(prior_omitted));
            let omitted = total_known.saturating_sub(compact_affected.len() as u64);
            minimal.insert("affected_tests".into(), Value::Array(compact_affected));
            if omitted > 0 {
                minimal.insert("affected_tests_omitted".into(), json!(omitted));
            }
        }
        if let Some(arr) = obj.get("failed_nodeids").and_then(Value::as_array) {
            let kept: Vec<Value> = arr
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| s.len() <= 512)
                .take(15)
                .map(|s| Value::String(s.to_string()))
                .collect();
            let prev_omitted = obj
                .get("failed_nodeids_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_omitted =
                prev_omitted.saturating_add(arr.len().saturating_sub(kept.len()) as u64);
            minimal.insert("failed_nodeids".into(), Value::Array(kept));
            if total_omitted > 0 {
                minimal.insert("failed_nodeids_omitted".into(), json!(total_omitted));
            }
        }
        if let Some(arr) = obj.get("ruff_diagnostics").and_then(Value::as_array) {
            let kept: Vec<Value> = arr
                .iter()
                .take(5)
                .map(|d| {
                    json!({
                        "file": d.get("file").and_then(Value::as_str).filter(|s| s.len() <= 512),
                        "row": d.get("row"),
                        "code": d.get("code"),
                        "message": d.get("message").and_then(Value::as_str).map(|s| truncate_utf8_str(s, 200)),
                    })
                })
                .collect();
            let prev_omitted = obj
                .get("ruff_diagnostics_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_omitted =
                prev_omitted.saturating_add(arr.len().saturating_sub(kept.len()) as u64);
            minimal.insert("ruff_diagnostics".into(), Value::Array(kept));
            if total_omitted > 0 {
                minimal.insert("ruff_diagnostics_omitted".into(), json!(total_omitted));
            }
        }
        if let Some(arr) = obj.get("structured_failures").and_then(Value::as_array) {
            let kept: Vec<Value> = arr
                .iter()
                .take(3)
                .map(|f| {
                    json!({
                        "file": f.get("file").and_then(Value::as_str).filter(|s| s.len() <= 512),
                        "line": f.get("line"),
                        "nodeid": f.get("nodeid").and_then(Value::as_str).filter(|s| s.len() <= 512),
                        "message": f.get("message").and_then(Value::as_str).map(|s| truncate_utf8_str(s, 200)),
                    })
                })
                .collect();
            let prev_omitted = obj
                .get("structured_failures_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_omitted =
                prev_omitted.saturating_add(arr.len().saturating_sub(kept.len()) as u64);
            minimal.insert("structured_failures".into(), Value::Array(kept));
            if total_omitted > 0 {
                minimal.insert("structured_failures_omitted".into(), json!(total_omitted));
            }
        }
        if let Some(s) = obj.get("stdout_summary").and_then(Value::as_str) {
            minimal.insert("stdout_summary".into(), Value::String(truncate_utf8_str(s, 1536)));
        }
        minimal.insert(
            "hint".into(),
            Value::String(
                "Narrow `targets` or run `tctx` CLI directly for untruncated output.".to_string(),
            ),
        );
        // Enforce strict MCP_MAX_TEXT_BYTES while keeping omission counts accurate when shedding arrays
        for (shed_key, omitted_key) in [
            ("stdout_summary", None),
            ("structured_failures", Some("structured_failures_omitted")),
            ("ruff_diagnostics", Some("ruff_diagnostics_omitted")),
            ("failed_nodeids", Some("failed_nodeids_omitted")),
            ("affected_tests", Some("affected_tests_omitted")),
            ("failed_files", Some("failed_files_omitted")),
            ("hint", None),
            ("root", None),
            ("path", None),
        ] {
            let encoded = serde_json::to_string(&minimal)
                .unwrap_or_else(|_| "{\"truncated\":true}".to_string());
            if encoded.len() <= MCP_MAX_TEXT_BYTES {
                return encoded;
            }
            if let Some(removed) = minimal.remove(shed_key)
                && let Some(ok) = omitted_key
                && let Some(arr) = removed.as_array()
                && !arr.is_empty()
            {
                let prev = minimal.get(ok).and_then(Value::as_u64).unwrap_or(0);
                minimal.insert(ok.to_string(), json!(prev.saturating_add(arr.len() as u64)));
            }
        }
        let final_encoded = serde_json::to_string(&Value::Object(minimal))
            .unwrap_or_else(|_| "{\"truncated\":true}".to_string());
        if final_encoded.len() <= MCP_MAX_TEXT_BYTES {
            return final_encoded;
        }
        return "{\"truncated\":true}".to_string();
    }
    json!({ "truncated": true, "operation": name }).to_string()
}

fn truncate_utf8_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
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
            eprintln!(
                "[tctx-mcp] status=initialized version={} protocol={version}",
                env!("CARGO_PKG_VERSION")
            );
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
            let mut arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            normalize_mcp_arguments(name, &mut arguments);
            let root = resolve_tool_root(name, &mut arguments, default_root);
            match root {
                Ok(resolved_root) => {
                    let req = Request {
                        root: resolved_root,
                        name: name.to_string(),
                        arguments,
                        compact: true,
                    };
                    match daemon::request_pooled_or_local(&req) {
                        Ok(val) => Ok(json!({
                            "content": [{
                                "type": "text",
                                "text": serialize_mcp_result(name, val)
                            }],
                            "isError": false
                        })),
                        Err(err) => {
                            let msg = err.to_string();
                            eprintln!(
                                "[tctx-mcp] op={name} outcome=error err={}",
                                msg.lines().next().unwrap_or(&msg)
                            );
                            Ok(json!({
                                "content": [{
                                    "type": "text",
                                    "text": msg
                                }],
                                "isError": true
                            }))
                        }
                    }
                }
                Err(err) => {
                    let msg = err.to_string();
                    eprintln!(
                        "[tctx-mcp] op={name} outcome=root_error err={}",
                        msg.lines().next().unwrap_or(&msg)
                    );
                    Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": msg
                        }],
                        "isError": true
                    }))
                }
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

fn extract_string_list(val: &Value) -> Vec<String> {
    match val {
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else if trimmed.contains(',') && !trimmed.starts_with('[') {
                trimmed
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            } else {
                vec![trimmed.to_owned()]
            }
        }
        _ => Vec::new(),
    }
}

fn take_alias_strings(
    obj: &mut serde_json::Map<String, Value>,
    aliases: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &alias in aliases {
        if let Some(val) = obj.remove(alias) {
            for item in extract_string_list(&val) {
                if seen.insert(item.clone()) {
                    out.push(item);
                }
            }
        }
    }
    out
}

fn take_first_alias_value(
    obj: &mut serde_json::Map<String, Value>,
    aliases: &[&str],
) -> Option<Value> {
    let mut chosen = None;
    for &alias in aliases {
        if let Some(val) = obj.remove(alias)
            && chosen.is_none()
            && !val.is_null()
        {
            chosen = Some(val);
        }
    }
    chosen
}

pub fn normalize_mcp_arguments(name: &str, arguments: &mut Value) {
    let Some(obj) = arguments.as_object_mut() else {
        return;
    };
    if !obj.contains_key("root")
        && let Some(root_alias) = take_first_alias_value(
            obj,
            &[
                "workspace",
                "workspaceRoot",
                "workspace_root",
                "cwd",
                "project",
                "repo",
            ],
        )
    {
        obj.insert("root".into(), root_alias);
    } else {
        for alias in [
            "workspace",
            "workspaceRoot",
            "workspace_root",
            "cwd",
            "project",
            "repo",
        ] {
            obj.remove(alias);
        }
    }

    match name {
        "test_run" => {
            let existing = obj
                .get("targets")
                .map(extract_string_list)
                .unwrap_or_default();
            let alias_targets = take_alias_strings(
                obj,
                &[
                    "files",
                    "file",
                    "target",
                    "path",
                    "filePath",
                    "file_path",
                    "filepath",
                    "relative_path",
                    "relativePath",
                    "target_file",
                    "targetFile",
                    "uri",
                ],
            );
            if !existing.is_empty() {
                obj.insert("targets".into(), json!(existing));
            } else if !alias_targets.is_empty() {
                obj.insert("targets".into(), json!(alias_targets));
            }
            let filter_alias = take_first_alias_value(
                obj,
                &["filter", "test_filter", "testFilter", "pattern"],
            );
            if !obj.contains_key("k")
                && let Some(v) = filter_alias
            {
                obj.insert("k".into(), v);
            }
            let mark_alias = take_first_alias_value(obj, &["marker", "markers"]);
            if !obj.contains_key("mark")
                && let Some(v) = mark_alias
            {
                obj.insert("mark".into(), v);
            }
            let fix_alias = take_first_alias_value(obj, &["fix", "lint_fix", "lintFix"]);
            if !obj.contains_key("ruff_fix")
                && let Some(v) = fix_alias
            {
                obj.insert("ruff_fix".into(), v);
            }
            let args_alias = take_alias_strings(obj, &["args", "extra_args", "extraArgs"]);
            if let Some(existing_args) = obj.get("pytest_args").map(extract_string_list)
                && !existing_args.is_empty()
            {
                obj.insert("pytest_args".into(), json!(existing_args));
            } else if !args_alias.is_empty() {
                obj.insert("pytest_args".into(), json!(args_alias));
            }
        }
        "test_affected" | "test_invalidate" => {
            let existing = obj
                .get("files")
                .map(extract_string_list)
                .unwrap_or_default();
            let alias_files = take_alias_strings(
                obj,
                &[
                    "targets",
                    "target",
                    "file",
                    "path",
                    "filePath",
                    "file_path",
                    "filepath",
                    "relative_path",
                    "relativePath",
                    "target_file",
                    "targetFile",
                    "uri",
                ],
            );
            if !existing.is_empty() {
                obj.insert("files".into(), json!(existing));
            } else if !alias_files.is_empty() {
                obj.insert("files".into(), json!(alias_files));
            }
        }
        "test_graph" => {
            let existing = obj
                .get("file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            let alias_files = take_alias_strings(
                obj,
                &[
                    "path",
                    "filePath",
                    "file_path",
                    "filepath",
                    "relative_path",
                    "relativePath",
                    "target",
                    "target_file",
                    "targetFile",
                    "uri",
                    "files",
                    "targets",
                ],
            );
            if existing.is_none()
                && let Some(first) = alias_files.into_iter().next()
            {
                obj.insert("file".into(), Value::String(first));
            }
        }
        _ => {}
    }
}

fn maybe_populate_target_from_subpath_root(
    name: &str,
    obj: &mut serde_json::Map<String, Value>,
    spec_root: &Path,
    raw_root: &str,
    candidate_path: &Path,
) {
    let has_subtarget_syntax =
        raw_root.contains("::") || raw_root.contains('#') || raw_root.contains(':');
    let is_subfile = candidate_path != spec_root
        && candidate_path.starts_with(spec_root)
        && (candidate_path.is_file() || has_subtarget_syntax || name == "test_graph");
    if !is_subfile {
        return;
    }
    let rel_target = normalize_target_for_root(spec_root, raw_root);
    if rel_target.is_empty() || rel_target == "." || rel_target.starts_with('/') {
        return;
    }
    let file_only = rel_target.split("::").next().unwrap_or(&rel_target);
    match name {
        "test_run" => {
            let empty = obj
                .get("targets")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            if empty {
                obj.insert("targets".into(), json!([rel_target]));
            }
        }
        "test_affected" | "test_invalidate" => {
            let empty = obj
                .get("files")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            if empty {
                obj.insert("files".into(), json!([file_only]));
            }
        }
        "test_graph" => {
            let empty = obj
                .get("file")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty());
            if empty {
                obj.insert("file".into(), Value::String(file_only.to_owned()));
            }
        }
        _ => {}
    }
}

fn resolve_tool_root(
    name: &str,
    arguments: &mut Value,
    default_root: Option<&PathBuf>,
) -> Result<Option<PathBuf>> {
    if name == "test_workspaces" || name == "ping" {
        return Ok(None);
    }
    let specs = crate::catalog::load_arc().unwrap_or_else(|_| Arc::new(Vec::new()));
    if let Some(obj) = arguments.as_object_mut()
        && let Some(root_val) = obj.remove("root")
        && let Some(raw_root) = root_val.as_str()
        && !raw_root.trim().is_empty()
    {
        let cleaned = clean_path_or_target(raw_root);
        let base_part = cleaned.split("::").next().unwrap_or(&cleaned).trim();
        let file_part = strip_line_or_fragment_suffix(base_part);
        if !file_part.contains('/') && !file_part.contains('\\') {
            let exact: Vec<_> = specs
                .iter()
                .filter(|s| s.root.file_name().and_then(|n| n.to_str()) == Some(file_part))
                .collect();
            if exact.len() == 1 {
                return Ok(Some(exact[0].root.clone()));
            }
            let lower = file_part.to_ascii_lowercase();
            let ci: Vec<_> = specs
                .iter()
                .filter(|s| {
                    s.root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.to_ascii_lowercase() == lower)
                })
                .collect();
            if ci.len() == 1 {
                return Ok(Some(ci[0].root.clone()));
            }
        }
        let path_buf = PathBuf::from(file_part);
        let resolved_candidate = path_buf.canonicalize().unwrap_or_else(|_| path_buf.clone());
        if let Some(spec) = specs
            .iter()
            .filter(|s| resolved_candidate.starts_with(&s.root))
            .max_by_key(|s| s.root.components().count())
        {
            maybe_populate_target_from_subpath_root(
                name,
                obj,
                &spec.root,
                raw_root,
                &resolved_candidate,
            );
            return Ok(Some(spec.root.clone()));
        }
        return Ok(Some(resolved_candidate));
    }

    let mut raw_hints: Vec<String> = Vec::new();
    if let Some(obj) = arguments.as_object() {
        if let Some(f) = obj.get("file").and_then(Value::as_str) {
            raw_hints.push(f.to_owned());
        }
        for key in ["targets", "files"] {
            if let Some(arr) = obj.get(key).and_then(Value::as_array) {
                for item in arr.iter().filter_map(Value::as_str) {
                    raw_hints.push(item.to_owned());
                }
            }
        }
    }

    for hint in &raw_hints {
        let cleaned = clean_path_or_target(hint);
        let base_part = cleaned.split("::").next().unwrap_or(&cleaned).trim();
        let file_part = strip_line_or_fragment_suffix(base_part);
        let p = Path::new(file_part);
        if p.is_absolute() {
            let resolved_p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            if let Some(spec) = specs
                .iter()
                .filter(|s| resolved_p.starts_with(&s.root))
                .max_by_key(|s| s.root.components().count())
            {
                return Ok(Some(spec.root.clone()));
            }
        }
    }

    let canon_default = default_root.and_then(|def| def.canonicalize().ok());
    if let Some(canon) = &canon_default
        && let Ok(spec) = crate::registration::prepare_or_resolve_spec_canonical(canon)
    {
        return Ok(Some(spec.root));
    }

    if !specs.is_empty() {
        for hint in &raw_hints {
            let cleaned = clean_path_or_target(hint);
            let base_part = cleaned.split("::").next().unwrap_or(&cleaned).trim();
            let file_part = strip_line_or_fragment_suffix(base_part)
                .replace('\\', "/")
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_string();
            if file_part.is_empty() || file_part.starts_with('/') || file_part.starts_with("..") {
                continue;
            }
            if let Some((first_seg, rest)) = file_part.split_once('/') {
                let by_name: Vec<_> = specs
                    .iter()
                    .filter(|s| s.root.file_name().and_then(|n| n.to_str()) == Some(first_seg))
                    .collect();
                if by_name.len() == 1
                    && (rest.is_empty() || by_name[0].root.join(rest).exists())
                {
                    return Ok(Some(by_name[0].root.clone()));
                }
            }
            let matching: Vec<_> = specs
                .iter()
                .filter(|s| s.root.join(&file_part).exists())
                .collect();
            if matching.len() == 1 {
                return Ok(Some(matching[0].root.clone()));
            }
        }
    }

    if specs.len() == 1 {
        return Ok(Some(specs[0].root.clone()));
    }
    if name == "test_status" {
        return Ok(None);
    }
    if let Some(canon) = canon_default {
        return Ok(Some(canon));
    }
    Err(invalid("workspace root is required when current directory is unavailable").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_subagent_mcp_aliases_and_string_targets() {
        let mut run_args = json!({
            "workspace": "/repo",
            "file": "tests/test_service.py",
            "filter": "test_add",
            "marker": "slow",
            "fix": true,
            "args": "-vv"
        });
        normalize_mcp_arguments("test_run", &mut run_args);
        assert_eq!(run_args["root"], json!("/repo"));
        assert_eq!(run_args["targets"], json!(["tests/test_service.py"]));
        assert_eq!(run_args["k"], json!("test_add"));
        assert_eq!(run_args["mark"], json!("slow"));
        assert_eq!(run_args["ruff_fix"], json!(true));
        assert_eq!(run_args["pytest_args"], json!(["-vv"]));
        assert!(run_args.get("file").is_none());
        assert!(run_args.get("filter").is_none());

        let mut graph_args = json!({
            "filePath": "src/service.py"
        });
        normalize_mcp_arguments("test_graph", &mut graph_args);
        assert_eq!(graph_args["file"], json!("src/service.py"));
        assert!(graph_args.get("filePath").is_none());

        let mut affected_args = json!({
            "targets": "src/a.py, src/b.py"
        });
        normalize_mcp_arguments("test_affected", &mut affected_args);
        assert_eq!(affected_args["files"], json!(["src/a.py", "src/b.py"]));
        assert!(affected_args.get("targets").is_none());
    }
}
