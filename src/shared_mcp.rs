use crate::daemon::{self, Request};
use crate::mcp::MAX_MESSAGE;
use crate::runner::{clean_path_or_target, normalize_target_for_root, strip_line_or_fragment_suffix};
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

const INSTRUCTIONS: &str = "TCTX (test-context) is a local polyglot (Python + Rust + Frontend TS/JS/JSX/TSX/Vue/Svelte/CSS/SCSS) AST dependency graph, speculative background pytest/cargo/vitest/jest/node runner, and content-addressed Ruff/ESLint pre-flight checker. Use test_run (#tctxNativeRun) FIRST instead of running shell pytest/cargo test/vitest/jest/ruff/eslint directly: unchanged tests that already passed in the background return in < 5ms from cache, only uncached/modified tests run, and pre-flight Ruff/ESLint diagnostics (with optional ruff_fix=true auto-remediation) are surfaced automatically. Use test_affected (#tctxNativeAffected) to see which tests are affected by edited source files, whether the background runner has already verified them, and any pre-checked lint errors. Always pass the explicit absolute `root` parameter when known (especially in subagents, background sessions, Windows/WSL Agent Sessions, and multi-root setups).";

pub fn serve(mut input: impl BufRead, output: impl Write) -> Result<()> {
    let mut output = std::io::BufWriter::with_capacity(64 * 1024, output);
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
    for &alias in aliases {
        if let Some(val) = obj.remove(alias) {
            for item in extract_string_list(&val) {
                if !out.contains(&item) {
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
    let specs = crate::catalog::load().unwrap_or_default();
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
        if let Ok(spec) = crate::catalog::resolve_spec(&resolved_candidate) {
            maybe_populate_target_from_subpath_root(
                name,
                obj,
                &spec.root,
                raw_root,
                &resolved_candidate,
            );
            return Ok(Some(spec.root));
        }
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
        return Ok(Some(path_buf.canonicalize()?));
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
            if let Ok(spec) = crate::catalog::resolve_spec(p) {
                return Ok(Some(spec.root));
            }
            if let Some(spec) = specs
                .iter()
                .filter(|s| p.starts_with(&s.root))
                .max_by_key(|s| s.root.components().count())
            {
                return Ok(Some(spec.root.clone()));
            }
        }
    }

    let canon_default = default_root.and_then(|def| def.canonicalize().ok());
    if let Some(canon) = &canon_default
        && let Ok(spec) = crate::registration::prepare_or_resolve_spec(canon)
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
