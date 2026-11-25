use super::*;

pub(super) fn resolve_module_attributed_dynamic_deps(graph: &DependencyGraph, record: &mut CachedTestRecord) {
    if !record
        .dynamic_dependencies
        .iter()
        .any(|d| d.starts_with("__tctx_mod_dep__:"))
    {
        return;
    }
    let trans_deps: std::collections::HashSet<String> = graph
        .transitive_deps_for(&record.test_file)
        .into_iter()
        .collect();
    let mut resolved = BTreeSet::new();
    for entry in std::mem::take(&mut record.dynamic_dependencies) {
        if let Some(rest) = entry.strip_prefix("__tctx_mod_dep__:") {
            if let Some((mod_rel, data_rel)) = rest.split_once("::")
                && trans_deps.contains(mod_rel)
            {
                resolved.insert(data_rel.to_string());
            }
        } else {
            resolved.insert(entry);
        }
    }
    record.dynamic_dependencies = resolved.into_iter().collect();
}

/// Returns `(modified_during_run, has_untrackable_dep)`:
/// - `modified_during_run`: a newly discovered tracked runtime dep was modified at/after run start;
/// - `has_untrackable_dep`: a captured runtime dep could not be indexed (missing/oversized/excluded),
///   so caching this run would miss future creation or edits of that path.
pub(super) fn classify_runtime_deps_stability(
    guard: &RootState,
    record: &CachedTestRecord,
    prev_known: &BTreeSet<String>,
) -> (bool, bool) {
    let run_start_i64 = i64::try_from(record.started_unix_ms).unwrap_or(i64::MAX);
    let mut modified_during_run = false;
    let mut has_untrackable = false;
    for dep in &record.dynamic_dependencies {
        let Some(node) = guard.graph.files.get(dep) else {
            has_untrackable = true;
            continue;
        };
        if !prev_known.contains(dep) {
            let mtime_ms = node.stamp.map(|st| {
                st.mtime_sec
                    .saturating_mul(1000)
                    .saturating_add(st.mtime_nsec / 1_000_000)
            });
            // Also inspect inode `ctime` so writes with preserved or backdated `mtime`
            // (e.g. `cp -p` / `utimensat` / archive unpack) are still detected as mid-run edits.
            #[cfg(unix)]
            let ctime_ms = {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(guard.workspace.root.join(dep))
                    .ok()
                    .map(|m| {
                        m.ctime()
                            .saturating_mul(1000)
                            .saturating_add(m.ctime_nsec() / 1_000_000)
                    })
            };
            #[cfg(not(unix))]
            let ctime_ms: Option<i64> = None;
            if mtime_ms.is_none_or(|m| m >= run_start_i64)
                || ctime_ms.is_some_and(|c| c >= run_start_i64)
            {
                modified_during_run = true;
            }
        }
    }
    (modified_during_run, has_untrackable)
}

pub(super) fn authorized_clippy_fix_crates(
    root: &Path,
    norm_targets: &[String],
    all_files: &BTreeMap<String, crate::ast_graph::FileNode>,
) -> BTreeSet<String> {
    let mut all_crates = BTreeSet::new();
    for k in all_files.keys() {
        if k.ends_with(".rs") {
            all_crates.insert(crate::ast_graph::rust_crate_dir(k));
        }
    }
    let mut authorized = BTreeSet::new();
    if all_crates.is_empty() {
        return authorized;
    }
    for raw in norm_targets {
        if raw.contains("::") {
            continue;
        }
        let trimmed = raw.trim().replace('\\', "/");
        if trimmed == "." || trimmed == "./" {
            return all_crates;
        }
        let norm = trimmed.trim_start_matches("./").trim_end_matches('/');
        if norm.is_empty() {
            continue;
        }
        // Individual files (e.g. `app.py`, `src/lib.rs`, `foo.ts`) never authorize crate-wide clippy --fix.
        if !root.join(norm).is_dir() {
            continue;
        }
        let prefix = format!("{norm}/");
        for crate_dir in &all_crates {
            if crate_dir == norm || crate_dir.starts_with(&prefix) {
                authorized.insert(crate_dir.clone());
            }
        }
    }
    authorized
}

pub(super) fn execute_ruff_check(
    spec: &Spec,
    rel_files: &[String],
    fix: bool,
) -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
    execute_ruff_check_tracked(spec, rel_files, fix, None)
}

pub(super) fn execute_ruff_check_tracked(
    spec: &Spec,
    rel_files: &[String],
    fix: bool,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let mut checked: BTreeSet<String> = BTreeSet::new();
    if rel_files.is_empty() {
        return (by_file, checked);
    }
    let py_files: Vec<String> = rel_files
        .iter()
        .filter(|f| f.ends_with(".py"))
        .cloned()
        .collect();
    let fe_files: Vec<String> = rel_files
        .iter()
        .filter(|f| crate::ast_graph::is_frontend_source_path(f))
        .cloned()
        .collect();
    let rs_files: Vec<String> = rel_files
        .iter()
        .filter(|f| f.ends_with(".rs"))
        .cloned()
        .collect();

    let active_langs = usize::from(!py_files.is_empty())
        + usize::from(!fe_files.is_empty())
        + usize::from(!rs_files.is_empty());

    let run_py = || -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
        let mut out = BTreeMap::new();
        let mut ok_files = BTreeSet::new();
        if py_files.is_empty() {
            return (out, ok_files);
        }
        let Some(ruff_cmd) = spec.effective_ruff_cmd() else {
            // Ruff not configured/installed in workspace; mark clean so we don't repeatedly re-probe.
            ok_files.extend(py_files.iter().cloned());
            return (out, ok_files);
        };
        let Some((program, base_args)) = ruff_cmd.split_first() else {
            ok_files.extend(py_files.iter().cloned());
            return (out, ok_files);
        };
        let mut command = Command::new(program);
        command.current_dir(&spec.root);
        command.args(base_args);
        command.arg("check");
        if fix {
            command.arg("--fix");
        }
        command.arg("--output-format=json");
        if let Ok(cache_dir) = crate::catalog::cache_dir() {
            command
                .arg("--cache-dir")
                .arg(cache_dir.join("ruff-cache"));
        }
        for f in &py_files {
            command.arg(f);
        }
        if let Ok(output) = run_command_with_timeout_tracked(
            &mut command,
            Duration::from_secs(15),
            2 * 1024 * 1024,
            None,
            active_pgids,
        ) && !output.timed_out
            && !output.cancelled
            && matches!(output.status.code(), Some(0 | 1))
            && serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).is_ok()
        {
            out.extend(parse_ruff_json_output(
                &output.stdout,
                &spec.root,
                &py_files,
            ));
            ok_files.extend(py_files.iter().cloned());
        }
        (out, ok_files)
    };

    let run_fe = || -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
        if fe_files.is_empty() {
            return (BTreeMap::new(), BTreeSet::new());
        }
        let (mut out, mut ok_files, fallback_files) =
            execute_eslint_check(spec, &fe_files, fix, active_pgids);
        if !fallback_files.is_empty() {
            let (biome_out, biome_ok) =
                execute_biome_check(spec, &fallback_files, fix, active_pgids);
            out.extend(biome_out);
            ok_files.extend(biome_ok);
        }
        (out, ok_files)
    };

    let run_rs = || -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
        if rs_files.is_empty() {
            return (BTreeMap::new(), BTreeSet::new());
        }
        execute_clippy_check(spec, &rs_files, fix, active_pgids)
    };

    if active_langs > 1 {
        let ((py_res, py_ok), (fe_res, fe_ok), (rs_res, rs_ok)) = std::thread::scope(|scope| {
            let py_h = scope.spawn(run_py);
            let fe_h = scope.spawn(run_fe);
            let rs_h = scope.spawn(run_rs);
            (
                py_h.join().unwrap_or_default(),
                fe_h.join().unwrap_or_default(),
                rs_h.join().unwrap_or_default(),
            )
        });
        by_file.extend(py_res);
        by_file.extend(fe_res);
        by_file.extend(rs_res);
        checked.extend(py_ok);
        checked.extend(fe_ok);
        checked.extend(rs_ok);
    } else {
        let (py_res, py_ok) = run_py();
        let (fe_res, fe_ok) = run_fe();
        let (rs_res, rs_ok) = run_rs();
        by_file.extend(py_res);
        by_file.extend(fe_res);
        by_file.extend(rs_res);
        checked.extend(py_ok);
        checked.extend(fe_ok);
        checked.extend(rs_ok);
    }

    (by_file, checked)
}

pub(super) fn ensure_node_dir_in_path(command: &mut Command) {
    let node_bin = crate::catalog::detect_node_bin();
    let node_path = Path::new(&node_bin);
    let Some(node_dir) = node_path.parent() else {
        return;
    };
    if node_dir.as_os_str().is_empty() || !node_dir.is_dir() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&current).collect();
    if !dirs.iter().any(|d| d == node_dir) {
        dirs.insert(0, node_dir.to_path_buf());
        if let Ok(joined) = std::env::join_paths(dirs) {
            command.env("PATH", joined);
        }
    }
}

pub(super) fn execute_clippy_check(
    spec: &Spec,
    rs_files: &[String],
    fix: bool,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let mut checked: BTreeSet<String> = BTreeSet::new();
    if std::env::var_os("TCTX_CLIPPY_BIN").is_some_and(|v| v == "none" || v == "0") {
        checked.extend(rs_files.iter().cloned());
        return (by_file, checked);
    }
    let cargo_cmd = spec.effective_cargo_cmd();
    let Some((program, base_args)) = cargo_cmd.split_first() else {
        checked.extend(rs_files.iter().cloned());
        return (by_file, checked);
    };
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in rs_files {
        let crate_dir = crate::ast_graph::rust_crate_dir(f);
        groups.entry(crate_dir).or_default().push(f.clone());
    }
    for (crate_dir, group_files) in groups {
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        if !workdir.join("Cargo.toml").is_file() {
            checked.extend(group_files);
            continue;
        }
        let mut command = Command::new(program);
        command.current_dir(&workdir);
        command.args(base_args);
        command.arg("clippy");
        command.arg("--all-targets");
        command.arg("--no-deps");
        command.arg("--quiet");
        if fix {
            command.arg("--fix");
            command.arg("--allow-dirty");
            command.arg("--allow-staged");
        }
        command.arg("--message-format=json");
        if let Ok(output) = run_command_with_timeout_tracked(
            &mut command,
            Duration::from_secs(45),
            4 * 1024 * 1024,
            None,
            active_pgids,
        ) && !output.timed_out
            && !output.cancelled
        {
            let text = String::from_utf8_lossy(&output.stdout);
            let saw_json_line = text
                .lines()
                .any(|l| l.trim().starts_with('{') && serde_json::from_str::<serde_json::Value>(l.trim()).is_ok());
            if output.status.success() || saw_json_line {
                for (file, diags) in parse_clippy_json_output_for_crate(
                    &output.stdout,
                    &spec.root,
                    &crate_dir,
                    &group_files,
                ) {
                    by_file.entry(file).or_default().extend(diags);
                }
                checked.extend(group_files);
            }
        }
    }
    (by_file, checked)
}

pub fn parse_clippy_json_output(
    stdout: &[u8],
    workspace_root: &Path,
    known_rel_files: &[String],
) -> BTreeMap<String, Vec<RuffDiagnostic>> {
    parse_clippy_json_output_for_crate(stdout, workspace_root, "", known_rel_files)
}

pub(super) fn parse_clippy_json_output_for_crate(
    stdout: &[u8],
    workspace_root: &Path,
    crate_dir: &str,
    known_rel_files: &[String],
) -> BTreeMap<String, Vec<RuffDiagnostic>> {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let text = String::from_utf8_lossy(stdout);
    let root_str = workspace_root.to_string_lossy().replace('\\', "/");
    let root_prefix = format!("{}/", root_str.trim_end_matches('/'));
    let crate_prefix = if crate_dir.is_empty() {
        String::new()
    } else {
        format!("{}/", crate_dir.trim_matches('/'))
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if val.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg_obj) = val.get("message") else {
            continue;
        };
        let level = msg_obj
            .get("level")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let Some(spans) = msg_obj.get("spans").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let Some(primary_span) = spans
            .iter()
            .find(|s| s.get("is_primary").and_then(serde_json::Value::as_bool) == Some(true))
            .or_else(|| spans.first())
        else {
            continue;
        };
        let raw_file = primary_span
            .get("file_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        let mut rel_file = if let Some(stripped) = raw_file.strip_prefix(&root_prefix) {
            stripped.to_string()
        } else {
            raw_file.trim_start_matches("./").to_string()
        };
        if !crate_prefix.is_empty()
            && !rel_file.starts_with(&crate_prefix)
            && !workspace_root.join(&rel_file).is_file()
        {
            let cand = format!("{crate_prefix}{rel_file}");
            if workspace_root.join(&cand).is_file()
                || known_rel_files.iter().any(|k| k == &cand)
            {
                rel_file = cand;
            }
        }
        if !known_rel_files.is_empty() && !known_rel_files.iter().any(|k| k == &rel_file) {
            continue;
        }
        let row = primary_span
            .get("line_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as usize;
        let column = primary_span
            .get("column_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as usize;
        let code = msg_obj
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("clippy")
            .to_string();
        let message = msg_obj
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let fixable = msg_obj
            .get("children")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ch| {
                ch.iter().any(|c| {
                    c.get("spans")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|sp| {
                            sp.iter()
                                .any(|s| s.get("suggested_replacement").is_some_and(|r| !r.is_null()))
                        })
                })
            });
        by_file.entry(rel_file.clone()).or_default().push(RuffDiagnostic {
            file: rel_file,
            row,
            column,
            code,
            message,
            fixable,
            fix_message: None,
        });
    }
    by_file
}

pub(super) fn execute_biome_check(
    spec: &Spec,
    fe_files: &[(String, bool)],
    fix: bool,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> (BTreeMap<String, Vec<RuffDiagnostic>>, BTreeSet<String>) {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let mut checked: BTreeSet<String> = BTreeSet::new();
    let mut groups: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
    for (f, eslint_failed) in fe_files {
        let pkg_dir = frontend_workdir_key(&spec.root, f);
        let entry = groups.entry(pkg_dir).or_insert_with(|| (Vec::new(), false));
        entry.0.push(f.clone());
        entry.1 |= *eslint_failed;
    }
    for (pkg_dir, (group, eslint_failed)) in groups {
        let Some((biome_bin, workdir)) =
            crate::catalog::detect_biome_cmd_for_dir(&spec.root, &pkg_dir)
        else {
            // Only mark clean when neither ESLint nor Biome is installed for this package.
            // If ESLint was installed and crashed/timed out, leave uncached.
            if !eslint_failed {
                checked.extend(group);
            }
            continue;
        };
        let mut command = Command::new(biome_bin);
        ensure_node_dir_in_path(&mut command);
        command.current_dir(&workdir);
        command.arg("check");
        if fix {
            command.arg("--write");
        }
        command.arg("--reporter=json");
        for f in &group {
            let arg_path = if !pkg_dir.is_empty() && workdir != spec.root {
                f.strip_prefix(&format!("{pkg_dir}/")).unwrap_or(f)
            } else {
                f.as_str()
            };
            command.arg(arg_path);
        }
        if let Ok(output) = run_command_with_timeout_tracked(
            &mut command,
            Duration::from_secs(15),
            2 * 1024 * 1024,
            None,
            active_pgids,
        ) && !output.timed_out
            && !output.cancelled
            && matches!(output.status.code(), Some(0 | 1))
            && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .ok()
                .and_then(|v| v.get("diagnostics").and_then(serde_json::Value::as_array).map(|_| ()))
                .is_some()
        {
            by_file.extend(parse_biome_json_output(&output.stdout, &spec.root, &group));
            checked.extend(group);
        }
    }
    (by_file, checked)
}

pub fn parse_biome_json_output(
    stdout: &[u8],
    workspace_root: &Path,
    known_rel_files: &[String],
) -> BTreeMap<String, Vec<RuffDiagnostic>> {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return by_file;
    };
    let Some(diags) = val.get("diagnostics").and_then(serde_json::Value::as_array) else {
        return by_file;
    };
    let root_str = workspace_root.to_string_lossy().replace('\\', "/");
    let root_prefix = format!("{}/", root_str.trim_end_matches('/'));
    for d in diags {
        let raw_file = d
            .get("location")
            .and_then(|l| l.get("path"))
            .and_then(|p| p.get("file").or(Some(p)))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        let rel_file = if let Some(stripped) = raw_file.strip_prefix(&root_prefix) {
            stripped.to_string()
        } else {
            let clean = raw_file.trim_start_matches("./");
            known_rel_files
                .iter()
                .find(|k| raw_file.ends_with(k.as_str()) || k.ends_with(clean))
                .cloned()
                .unwrap_or_else(|| clean.to_string())
        };
        let code = d
            .get("category")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("biome")
            .to_string();
        let message = d
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let fixable = d
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tags| tags.iter().any(|t| t.as_str() == Some("fixable")));
        by_file.entry(rel_file.clone()).or_default().push(RuffDiagnostic {
            file: rel_file,
            row: 1,
            column: 1,
            code,
            message,
            fixable,
            fix_message: None,
        });
    }
    by_file
}

pub(super) type EslintCheckResult = (
    BTreeMap<String, Vec<RuffDiagnostic>>,
    BTreeSet<String>,
    Vec<(String, bool)>,
);

pub(super) fn execute_eslint_check(
    spec: &Spec,
    fe_files: &[String],
    fix: bool,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> EslintCheckResult {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let mut checked: BTreeSet<String> = BTreeSet::new();
    let mut fallback_files: Vec<(String, bool)> = Vec::new();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in fe_files {
        let pkg_dir = if let Some((first, _)) = f.split_once('/')
            && spec.root.join(first).join("package.json").is_file()
        {
            first.to_string()
        } else {
            String::new()
        };
        groups.entry(pkg_dir).or_default().push(f.clone());
    }
    for (pkg_dir, group) in groups {
        let Some((eslint_bin, workdir)) =
            crate::catalog::detect_eslint_cmd_for_dir(&spec.root, &pkg_dir)
        else {
            fallback_files.extend(group.into_iter().map(|f| (f, false)));
            continue;
        };
        let mut command = Command::new(eslint_bin);
        ensure_node_dir_in_path(&mut command);
        command.current_dir(&workdir);
        command.arg("--format=json");
        if fix {
            command.arg("--fix");
        }
        for f in &group {
            let arg_path = if !pkg_dir.is_empty() {
                f.strip_prefix(&format!("{pkg_dir}/")).unwrap_or(f)
            } else {
                f.as_str()
            };
            command.arg(arg_path);
        }
        match run_command_with_timeout_tracked(
            &mut command,
            Duration::from_secs(20),
            2 * 1024 * 1024,
            None,
            active_pgids,
        ) {
            Ok(output)
                if !output.timed_out
                    && !output.cancelled
                    && matches!(output.status.code(), Some(0 | 1))
                    && serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).is_ok() =>
            {
                by_file.extend(parse_eslint_json_output(&output.stdout, &spec.root, &group));
                checked.extend(group);
            }
            _ => {
                // ESLint crashed/timed out/malformed JSON; allow Biome fallback, but mark eslint_failed=true
                // so missing Biome won't silently cache these files as clean.
                fallback_files.extend(group.into_iter().map(|f| (f, true)));
            }
        }
    }
    (by_file, checked, fallback_files)
}

pub fn parse_eslint_json_output(
    stdout: &[u8],
    workspace_root: &Path,
    known_rel_files: &[String],
) -> BTreeMap<String, Vec<RuffDiagnostic>> {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(stdout) else {
        return by_file;
    };
    let root_str = workspace_root.to_string_lossy().replace('\\', "/");
    let root_prefix = format!("{}/", root_str.trim_end_matches('/'));

    for item in items {
        let raw_file = item
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        let rel_file = if let Some(stripped) = raw_file.strip_prefix(&root_prefix) {
            stripped.to_string()
        } else {
            let clean = raw_file.trim_start_matches("./");
            known_rel_files
                .iter()
                .find(|k| raw_file.ends_with(k.as_str()) || k.ends_with(clean))
                .cloned()
                .unwrap_or_else(|| clean.to_string())
        };
        let Some(messages) = item.get("messages").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for msg in messages {
            let code = msg
                .get("ruleId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("eslint")
                .to_string();
            let message = msg
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let row = msg
                .get("line")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as usize;
            let column = msg
                .get("column")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as usize;
            let fixable = msg.get("fix").is_some_and(|v| !v.is_null());
            by_file.entry(rel_file.clone()).or_default().push(RuffDiagnostic {
                file: rel_file.clone(),
                row,
                column,
                code,
                message,
                fixable,
                fix_message: None,
            });
        }
    }
    by_file
}

pub fn parse_ruff_json_output(
    stdout: &[u8],
    workspace_root: &Path,
    known_rel_files: &[String],
) -> BTreeMap<String, Vec<RuffDiagnostic>> {
    let mut by_file: BTreeMap<String, Vec<RuffDiagnostic>> = BTreeMap::new();
    let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(stdout) else {
        return by_file;
    };
    let root_str = workspace_root.to_string_lossy().replace('\\', "/");
    let root_prefix = format!("{}/", root_str.trim_end_matches('/'));

    for item in items {
        let raw_file = item
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        let rel_file = if let Some(stripped) = raw_file.strip_prefix(&root_prefix) {
            stripped.to_string()
        } else {
            let clean = raw_file.trim_start_matches("./");
            known_rel_files
                .iter()
                .find(|k| raw_file.ends_with(k.as_str()) || k.ends_with(clean))
                .cloned()
                .unwrap_or_else(|| clean.to_string())
        };
        let code = item
            .get("code")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("name").and_then(serde_json::Value::as_str))
            .unwrap_or("RUFF")
            .to_string();
        let message = item
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let row = item
            .get("location")
            .and_then(|l| l.get("row"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as usize;
        let column = item
            .get("location")
            .and_then(|l| l.get("column"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as usize;
        let fix_obj = item.get("fix").filter(|v| !v.is_null());
        let fixable = fix_obj.is_some();
        let fix_message = fix_obj
            .and_then(|f| f.get("message"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);

        by_file.entry(rel_file.clone()).or_default().push(RuffDiagnostic {
            file: rel_file,
            row,
            column,
            code,
            message,
            fixable,
            fix_message,
        });
    }
    by_file
}

pub(super) fn flight_key(test_file: &str, dep_hash: &str) -> String {
    format!("{test_file}@{dep_hash}")
}

pub fn sort_targets_fail_first(targets: &mut [String], cache: &RootCache) {
    targets.sort_by_key(|tf| {
        let prev_failed = cache
            .latest_by_file
            .get(tf)
            .is_some_and(|rec| !rec.outcome.is_green());
        let prev_dur = cache
            .latest_by_file
            .get(tf)
            .map(|rec| rec.duration_ms)
            .unwrap_or(u64::MAX / 2);
        (!prev_failed, prev_dur, tf.clone())
    });
}

pub(super) fn is_source_changed_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.to_string().starts_with("SOURCE_CHANGED_DURING_TEST")
}


