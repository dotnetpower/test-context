use super::*;

pub(super) fn parse_vitest_jest_json_output(
    json_bytes: &[u8],
    known_files: &BTreeMap<String, String>,
) -> BTreeMap<String, FileXmlStats> {
    let mut stats_by_file: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(json_bytes) else {
        return stats_by_file;
    };
    let Some(test_results) = val.get("testResults").and_then(serde_json::Value::as_array) else {
        return stats_by_file;
    };

    for file_res in test_results {
        let raw_name = file_res
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        let Some(matched_file) = match_testcase_to_file(Some(&raw_name), "", known_files) else {
            continue;
        };
        let entry = stats_by_file.entry(matched_file.clone()).or_default();
        let file_start = file_res
            .get("startTime")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let file_end = file_res
            .get("endTime")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let file_dur = file_end.saturating_sub(file_start);

        let mut seen_node_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut ambiguous_node_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if let Some(assertions) = file_res
            .get("assertionResults")
            .and_then(serde_json::Value::as_array)
        {
            for case in assertions {
                let title = case
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim();
                let full_name = case
                    .get("fullName")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(title)
                    .trim();
                let status = case
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("passed");
                let dur_ms = case
                    .get("duration")
                    .and_then(serde_json::Value::as_f64)
                    .map(|d| d.max(0.0).round() as u64)
                    .unwrap_or(1)
                    .max(1);
                entry.duration_ms = entry.duration_ms.saturating_add(dur_ms);

                let fail_msg = case
                    .get("failureMessages")
                    .and_then(serde_json::Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let snip = truncate_str(&fail_msg, 2048);

                let outcome = match status {
                    "passed" => {
                        entry.passed += 1;
                        TestOutcome::Passed
                    }
                    "failed" => {
                        entry.failed += 1;
                        TestOutcome::Failed
                    }
                    "skipped" | "pending" | "todo" | "disabled" => {
                        entry.skipped += 1;
                        TestOutcome::Skipped
                    }
                    _ => {
                        entry.errors += 1;
                        TestOutcome::Error
                    }
                };

                let primary_id = if title.is_empty() {
                    matched_file.clone()
                } else {
                    format!("{matched_file}::{title}")
                };
                if !outcome.is_green() && !entry.failed_nodeids.contains(&primary_id) {
                    entry.failed_nodeids.push(primary_id.clone());
                }
                // Never let duplicate test titles silently overwrite an earlier failure with a pass.
                if !seen_node_keys.insert(primary_id.clone()) {
                    ambiguous_node_keys.insert(primary_id.clone());
                    entry.node_results.remove(&primary_id);
                } else if !ambiguous_node_keys.contains(&primary_id) {
                    entry.node_results.insert(
                        primary_id.clone(),
                        CachedNodeResult {
                            nodeid: primary_id.clone(),
                            outcome,
                            duration_ms: dur_ms,
                            output_snippet: snip.clone(),
                        },
                    );
                }
                if !full_name.is_empty() && full_name != title {
                    let full_id = format!("{matched_file}::{full_name}");
                    if !seen_node_keys.insert(full_id.clone()) {
                        ambiguous_node_keys.insert(full_id.clone());
                        entry.node_results.remove(&full_id);
                    } else if !ambiguous_node_keys.contains(&full_id) {
                        entry.node_results.insert(
                            full_id.clone(),
                            CachedNodeResult {
                                nodeid: full_id,
                                outcome,
                                duration_ms: dur_ms,
                                output_snippet: snip,
                            },
                        );
                    }
                }
            }
        }
        if entry.duration_ms == 0 && file_dur > 0 {
            entry.duration_ms = file_dur;
        }
        let file_status = file_res
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if file_status == "failed" && entry.failed == 0 && entry.errors == 0 {
            // Preserve suite-level failures (e.g. failing afterAll/teardown or unhandled rejection)
            // even when individual assertions passed, and clear node-level passing entries.
            entry.errors += 1;
            entry.node_results.clear();
            if !entry.failed_nodeids.contains(&matched_file) {
                entry.failed_nodeids.push(matched_file);
            }
        }
    }

    stats_by_file
}

pub(super) fn parse_node_test_output(
    combined_out: &str,
    test_file: &str,
    duration_ms: u64,
    overall_success: bool,
) -> FileXmlStats {
    parse_node_test_output_with_truncation(combined_out, test_file, duration_ms, overall_success, false)
}

pub(super) fn parse_node_test_output_with_truncation(
    combined_out: &str,
    test_file: &str,
    duration_ms: u64,
    overall_success: bool,
    output_truncated: bool,
) -> FileXmlStats {
    let mut stats = FileXmlStats {
        duration_ms: duration_ms.max(1),
        ..FileXmlStats::default()
    };
    let mut cases: Vec<(String, TestOutcome)> = Vec::new();

    for raw_line in combined_out.lines() {
        let line = raw_line.trim();
        let (is_ok, rest) = if let Some(r) = line.strip_prefix("ok ") {
            (true, r)
        } else if let Some(r) = line.strip_prefix("not ok ") {
            (false, r)
        } else {
            continue;
        };
        let Some((_, title_part)) = rest.split_once(" - ") else {
            continue;
        };
        let (clean_title, directive) = match title_part.split_once(" # ") {
            Some((t, dir)) => (t.trim(), dir.trim().to_ascii_lowercase()),
            None => (title_part.trim(), String::new()),
        };
        if clean_title.is_empty() {
            continue;
        }
        let outcome = if directive.starts_with("skip") || directive.starts_with("todo") {
            stats.skipped += 1;
            TestOutcome::Skipped
        } else if is_ok {
            stats.passed += 1;
            TestOutcome::Passed
        } else {
            stats.failed += 1;
            TestOutcome::Failed
        };
        cases.push((clean_title.to_string(), outcome));
    }

    let case_count = cases.len().max(1) as u64;
    let per_case_ms = (duration_ms / case_count).max(1);
    let snippet = truncate_str(combined_out, 2048);

    // Track duplicate test titles: if the same `file::title` appears multiple times in one file,
    // suppress it from `node_results` so a later passing duplicate can never overwrite an earlier
    // failure and replay a false pass.
    let mut seen_nodeids: BTreeSet<String> = BTreeSet::new();
    let mut ambiguous_nodeids: BTreeSet<String> = BTreeSet::new();
    for (title, outcome) in cases {
        let nodeid = format!("{test_file}::{title}");
        if outcome == TestOutcome::Failed && !stats.failed_nodeids.contains(&nodeid) {
            stats.failed_nodeids.push(nodeid.clone());
        }
        if !seen_nodeids.insert(nodeid.clone()) {
            ambiguous_nodeids.insert(nodeid.clone());
            stats.node_results.remove(&nodeid);
            continue;
        }
        if !ambiguous_nodeids.contains(&nodeid) {
            stats.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome,
                    duration_ms: per_case_ms,
                    output_snippet: if outcome.is_green() {
                        String::new()
                    } else {
                        snippet.clone()
                    },
                },
            );
        }
    }

    // Fail closed whenever the process exited nonzero without an attributed failure OR when
    // stdout/stderr hit the capture cap (a trailing failure may have been truncated away!).
    if output_truncated || (!overall_success && stats.failed == 0 && stats.errors == 0) {
        if combined_out.contains("SyntaxError")
            || combined_out.contains("Cannot find module")
            || combined_out.contains("ERR_MODULE_NOT_FOUND")
            || combined_out.contains("TransformError")
            || combined_out.contains("TS2")
        {
            stats.errors = stats.errors.max(1);
        } else {
            stats.failed = stats.failed.max(1);
        }
        if !stats.failed_nodeids.iter().any(|n| n == test_file) {
            stats.failed_nodeids.push(test_file.to_string());
        }
        if output_truncated {
            stats.node_results.clear();
        }
    }

    stats
}

pub(super) fn newest_ts_source_mtime(dir: &Path, budget: &mut usize) -> (Option<std::time::SystemTime>, bool) {
    let mut newest: Option<std::time::SystemTime> = None;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (None, false);
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return (newest, true);
        }
        *budget -= 1;
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let path = entry.path();
        if ft.is_dir() {
            if matches!(
                name_s.as_ref(),
                "node_modules" | "out" | "dist" | "build" | ".git" | "target"
            ) {
                continue;
            }
            let (sub, exhausted) = newest_ts_source_mtime(&path, budget);
            if let Some(sub_mt) = sub
                && newest.is_none_or(|cur| sub_mt > cur)
            {
                newest = Some(sub_mt);
            }
            if exhausted {
                return (newest, true);
            }
        } else if ft.is_file()
            && (name_s.ends_with(".ts")
                || name_s.ends_with(".tsx")
                || name_s.ends_with(".mts")
                || name_s.ends_with(".cts")
                || (name_s.starts_with("tsconfig") && name_s.ends_with(".json")))
            && let Ok(meta) = entry.metadata()
            && let Ok(mt) = meta.modified()
            && newest.is_none_or(|cur| mt > cur)
        {
            newest = Some(mt);
        }
    }
    (newest, false)
}

pub(super) fn resolve_node_test_runnable_path(workspace_root: &Path, test_file: &str) -> Result<String> {
    if let Some(stem) = test_file
        .strip_suffix(".ts")
        .or_else(|| test_file.strip_suffix(".tsx"))
    {
        let verify_fresh = |pkg_dir: &Path, cand: &str| -> Result<String> {
            let out_abs = workspace_root.join(cand);
            let out_mtime = std::fs::metadata(&out_abs)
                .and_then(|m| m.modified())
                .map_err(|e| {
                    invalid(format!(
                        "STALE_COMPILED_JS: cannot stat compiled artifact '{cand}': {e}"
                    ))
                })?;
            let src_mtime = std::fs::metadata(workspace_root.join(test_file))
                .and_then(|m| m.modified())
                .ok();
            let mut budget = 2_048usize;
            let (pkg_newest, scan_exhausted) = newest_ts_source_mtime(pkg_dir, &mut budget);
            if scan_exhausted {
                return Err(invalid(format!(
                    "STALE_COMPILED_JS: freshness check for compiled artifact '{cand}' exceeded 2048-entry scan budget; rebuild TypeScript or run via vitest/tsx"
                ))
                .into());
            }
            let newest_src = match (src_mtime, pkg_newest) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            if let Some(src_mt) = newest_src
                && src_mt > out_mtime
            {
                return Err(invalid(format!(
                    "STALE_COMPILED_JS: compiled artifact '{cand}' is older than TypeScript source for '{test_file}'; rebuild TypeScript (e.g. `npm run compile`) before running node --test"
                ))
                .into());
            }
            Ok(cand.to_string())
        };
        // Check if a compiled JS counterpart exists under `out/` or `dist/` (e.g. `vscode-extension/test/x.test.ts` -> `vscode-extension/out/test/x.test.js`)
        if let Some((pkg_prefix, rel_inside)) = stem.split_once('/') {
            for out_dir in ["out", "dist", "build"] {
                let cand = format!("{pkg_prefix}/{out_dir}/{rel_inside}.js");
                if workspace_root.join(&cand).is_file() {
                    return verify_fresh(&workspace_root.join(pkg_prefix), &cand);
                }
            }
        }
        for out_dir in ["out", "dist", "build"] {
            let cand = format!("{out_dir}/{stem}.js");
            if workspace_root.join(&cand).is_file() {
                return verify_fresh(workspace_root, &cand);
            }
        }
    }
    Ok(test_file.to_string())
}

pub(super) const NODE_JSONL_REPORTER: &str = "data:text/javascript,export default async function*(s){for await(const e of s)if(e.type===\"test:pass\"||e.type===\"test:fail\")yield JSON.stringify({t:e.type,f:e.data.file,n:e.data.name,lv:e.data.nesting,fl:e.data.nesting===0&&e.data.name===e.data.file,d:e.data.details?.duration_ms,s:e.data.skip,todo:e.data.todo})+\"\\n\";}";

pub(super) fn parse_node_jsonl_batch_output(
    stdout_str: &str,
    runnable_to_source: &BTreeMap<String, String>,
    known_files: &BTreeMap<String, String>,
) -> BTreeMap<String, FileXmlStats> {
    let mut stats_by_file: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    let mut seen_nodeids: BTreeSet<String> = BTreeSet::new();
    let mut ambiguous_nodeids: BTreeSet<String> = BTreeSet::new();
    let mut file_level_failed: BTreeSet<String> = BTreeSet::new();
    for raw_line in stdout_str.lines() {
        let line = raw_line.trim();
        if !line.starts_with('{') || !line.ends_with('}') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event_type) = val.get("t").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let raw_file = val
            .get("f")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\\', "/");
        if raw_file.is_empty() {
            continue;
        }
        let matched_file = {
            let mut best: Option<(usize, &str)> = None;
            let mut ambiguous = false;
            for (runnable, src) in runnable_to_source {
                if path_matches_suffix_on_boundary(&raw_file, runnable.as_str()) {
                    let score = runnable.len();
                    match best {
                        Some((best_len, _)) if score > best_len => {
                            best = Some((score, src.as_str()));
                            ambiguous = false;
                        }
                        Some((best_len, best_src)) if score == best_len && best_src != src => {
                            ambiguous = true;
                        }
                        None => best = Some((score, src.as_str())),
                        _ => {}
                    }
                }
            }
            if ambiguous {
                None
            } else {
                best.map(|(_, s)| s.to_string())
                    .or_else(|| match_testcase_to_file(Some(&raw_file), "", known_files))
            }
        };
        let Some(test_file) = matched_file else {
            continue;
        };
        let title = val
            .get("n")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        // Identify file-level summary events from reporter metadata (`fl: true` when `nesting === 0
        // && name === file`) or exact match against `raw_file` / empty title — NOT arbitrary
        // `.js`/`.ts` suffixes so a user test legitimately named `foo.js` is preserved!
        let is_file_level_event = val
            .get("fl")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || title.is_empty()
            || title == raw_file
            || title == test_file;
        // Skip file-level summary *pass* events (individual subtests are already counted),
        // but NEVER discard file-level *fail* events (e.g. uncaught top-level exceptions or
        // late async crashes after earlier subtests passed)!
        if is_file_level_event && event_type == "test:pass" {
            continue;
        }
        let dur_ms = val
            .get("d")
            .and_then(serde_json::Value::as_f64)
            .map(|d| d.max(0.0).round() as u64)
            .unwrap_or(1)
            .max(1);
        let is_skip = val
            .get("s")
            .is_some_and(|v| !v.is_null() && v != &serde_json::Value::Bool(false))
            || val
                .get("todo")
                .is_some_and(|v| !v.is_null() && v != &serde_json::Value::Bool(false));

        let entry = stats_by_file.entry(test_file.clone()).or_default();
        entry.duration_ms = entry.duration_ms.saturating_add(dur_ms);
        if is_file_level_event {
            entry.failed += 1;
            if !entry.failed_nodeids.contains(&test_file) {
                entry.failed_nodeids.push(test_file.clone());
            }
            // Clear all per-node results on a file-level crash so `rerun_failed=false` can never
            // replay an earlier passing node from a file that suffered a late uncaught exception!
            entry.node_results.clear();
            file_level_failed.insert(test_file);
            continue;
        }
        let outcome = if is_skip {
            entry.skipped += 1;
            TestOutcome::Skipped
        } else if event_type == "test:pass" {
            entry.passed += 1;
            TestOutcome::Passed
        } else {
            entry.failed += 1;
            TestOutcome::Failed
        };
        let nodeid = format!("{test_file}::{title}");
        if outcome == TestOutcome::Failed && !entry.failed_nodeids.contains(&nodeid) {
            entry.failed_nodeids.push(nodeid.clone());
        }
        // Suppress ambiguous duplicate test titles and any nodes in files with file-level crashes.
        if file_level_failed.contains(&test_file) {
            continue;
        }
        if !seen_nodeids.insert(nodeid.clone()) {
            ambiguous_nodeids.insert(nodeid.clone());
            entry.node_results.remove(&nodeid);
        } else if !ambiguous_nodeids.contains(&nodeid) {
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome,
                    duration_ms: dur_ms,
                    output_snippet: String::new(),
                },
            );
        }
    }
    stats_by_file
}

pub(super) fn has_serial_frontend_flag(extra_args: &[String]) -> bool {
    let mut iter = extra_args.iter();
    while let Some(arg) = iter.next() {
        let trimmed = arg.trim();
        if trimmed == "--runInBand"
            || trimmed == "-i"
            || trimmed.contains("singleThread")
            || trimmed == "--pool=forks"
            || trimmed == "--maxWorkers=1"
            || trimmed == "--maxConcurrency=1"
        {
            return true;
        }
        if trimmed == "--pool"
            && let Some(next) = iter.next()
            && next.trim() == "forks"
        {
            return true;
        }
        if (trimmed == "--maxWorkers" || trimmed == "--maxConcurrency")
            && let Some(next) = iter.next()
            && next.trim() == "1"
        {
            return true;
        }
    }
    false
}

pub(super) fn frontend_workdir_key(root: &Path, test_file: &str) -> String {
    // Walk up ancestor directories from `test_file`'s parent toward workspace root (excluding
    // workspace root itself) to find the nearest package directory containing `package.json` or
    // a local Vitest/Jest binary.
    let mut cur = Path::new(test_file).parent();
    while let Some(dir) = cur {
        let ds = dir.to_string_lossy().replace('\\', "/");
        if ds.is_empty() || ds == "." {
            break;
        }
        let abs = root.join(&ds);
        if abs.join("package.json").is_file()
            || abs.join("node_modules/.bin/vitest").is_file()
            || abs.join("node_modules/.bin/jest").is_file()
        {
            return ds;
        }
        cur = dir.parent();
    }
    String::new()
}

pub(super) fn resolve_explicit_fe_runner_cmd(root: &Path, pkg_dir: &str, runner: &str) -> Vec<String> {
    let mut search_dirs = Vec::new();
    if !pkg_dir.is_empty() {
        search_dirs.push(root.join(pkg_dir));
    }
    search_dirs.push(root.to_path_buf());
    for dir in search_dirs {
        let local_bin = dir.join("node_modules/.bin").join(runner);
        if local_bin.is_file() {
            let bin = local_bin.to_string_lossy().into_owned();
            return if runner == "vitest" {
                vec![bin, "run".to_string()]
            } else {
                vec![bin]
            };
        }
    }
    if runner == "vitest" {
        vec!["npx".to_string(), "vitest".to_string(), "run".to_string()]
    } else {
        vec!["npx".to_string(), "jest".to_string()]
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_frontend_test_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    _mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    environment_hash: &str,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let run_started_unix_ms = now_unix_ms();
    let mut requested_runner: Option<&str> = None;
    let filtered_extra_args: Vec<String> = extra_args
        .iter()
        .filter_map(|a| {
            if let Some(r) = a.strip_prefix("__tctx_fe_runner__=") {
                requested_runner = Some(r);
                None
            } else if a.starts_with("__tctx_cargo_target__=")
                || a.starts_with("__tctx_cargo_cwd__=")
                || a.starts_with("__tctx_orig_target__=")
                || a == "__tctx_orig_empty__"
            {
                None
            } else {
                Some(a.clone())
            }
        })
        .collect();
    let extra_args = filtered_extra_args.as_slice();
    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs);

    let mut explicit_node_filters_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in invocation_targets {
        let (file_part, node_opt) = split_target_file_and_node_on_disk(&spec.root, t);
        if let Some(node) = node_opt {
            let norm_file = file_part.trim_start_matches("./").replace('\\', "/");
            let node_trimmed = node.trim();
            if !node_trimmed.is_empty() {
                let entry = explicit_node_filters_by_file.entry(norm_file).or_default();
                if !entry.iter().any(|existing| existing == node_trimmed) {
                    entry.push(node_trimmed.to_string());
                }
            }
        }
    }

    let merge_frontend_records = |raw_records: Vec<CachedTestRecord>| -> Vec<CachedTestRecord> {
        let mut merged: BTreeMap<String, CachedTestRecord> = BTreeMap::new();
        for rec in raw_records {
            match merged.get_mut(&rec.test_file) {
                Some(existing) => {
                    existing.passed = existing.passed.saturating_add(rec.passed);
                    existing.failed = existing.failed.saturating_add(rec.failed);
                    existing.skipped = existing.skipped.saturating_add(rec.skipped);
                    existing.errors = existing.errors.saturating_add(rec.errors);
                    existing.duration_ms = existing.duration_ms.saturating_add(rec.duration_ms);
                    for nid in rec.failed_nodeids {
                        if !existing.failed_nodeids.contains(&nid) {
                            existing.failed_nodeids.push(nid);
                        }
                    }
                    existing.node_results.extend(rec.node_results);
                    if !rec.output_snippet.is_empty() {
                        if !existing.output_snippet.is_empty() {
                            existing.output_snippet.push('\n');
                        }
                        existing.output_snippet.push_str(&rec.output_snippet);
                    }
                    existing.outcome = if existing.failed > 0 {
                        TestOutcome::Failed
                    } else if existing.errors > 0 {
                        TestOutcome::Error
                    } else if existing.passed > 0 && existing.skipped == 0 {
                        TestOutcome::Passed
                    } else if existing.skipped > 0 {
                        TestOutcome::Skipped
                    } else {
                        existing.outcome
                    };
                }
                None => {
                    merged.insert(rec.test_file.clone(), rec);
                }
            }
        }
        merged.into_values().collect()
    };

    // Resolve the effective runner and working directory PER package so mixed monorepos (e.g.
    // `packages/web` using Vitest alongside `packages/utils` using `node --test`) never run a
    // package with a sibling package's runner.
    type FrontendPkgGroupKey = (String, Option<(String, bool)>);
    let mut pkg_runner_cache: BTreeMap<String, (Vec<String>, PathBuf)> = BTreeMap::new();
    let mut pkg_groups: BTreeMap<FrontendPkgGroupKey, BTreeMap<String, String>> = BTreeMap::new();
    let mut node_test_pkg_groups: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for (tf, dh) in dep_hashes {
        let pkg_dir = frontend_workdir_key(&spec.root, tf);
        let (pkg_cmd, _) = pkg_runner_cache.entry(pkg_dir.clone()).or_insert_with(|| {
            if let Some(runner) = requested_runner {
                let wd = if !pkg_dir.is_empty() && spec.root.join(&pkg_dir).is_dir() {
                    spec.root.join(&pkg_dir)
                } else {
                    spec.root.clone()
                };
                (resolve_explicit_fe_runner_cmd(&spec.root, &pkg_dir, runner), wd)
            } else {
                crate::catalog::detect_frontend_runner_for_dir(
                    &spec.root,
                    &pkg_dir,
                    spec.managed_parent.as_deref(),
                )
            }
        });
        let Some((prog, base)) = pkg_cmd.split_first() else {
            return Err(invalid("empty frontend_test_cmd").into());
        };
        let pkg_is_vitest_or_jest = prog.ends_with("vitest")
            || prog.ends_with("jest")
            || base.iter().any(|a| a.contains("vitest") || a.contains("jest"));
        if pkg_is_vitest_or_jest {
            if let Some(nodes) = explicit_node_filters_by_file.get(tf) {
                for node in nodes {
                    pkg_groups
                        .entry((pkg_dir.clone(), Some((node.clone(), true))))
                        .or_default()
                        .insert(tf.clone(), dh.clone());
                }
            } else {
                pkg_groups
                    .entry((pkg_dir, k_expr.map(|k| (k.to_string(), false))))
                    .or_default()
                    .insert(tf.clone(), dh.clone());
            }
        } else {
            node_test_pkg_groups
                .entry(pkg_dir)
                .or_default()
                .insert(tf.clone(), dh.clone());
        }
    }

    let mut combined_records = Vec::new();
    let mut combined_snippets = Vec::new();

    if !pkg_groups.is_empty() {
        let pkg_runner_ref = &pkg_runner_cache;
        let run_one_pkg = |pkg_dir: String,
                           name_filter: Option<(String, bool)>,
                           group_hashes: BTreeMap<String, String>,
                           pkg_active_pgid: Option<&AtomicI32>|
         -> Result<(Vec<CachedTestRecord>, Vec<String>)> {
            let mut records = Vec::new();
            let mut all_snippets = Vec::new();
            let Some((group_cmd, workdir)) = pkg_runner_ref.get(&pkg_dir) else {
                return Ok((records, all_snippets));
            };
            let workdir = workdir.clone();
            let Some((grp_prog, grp_base)) = group_cmd.split_first() else {
                return Ok((records, all_snippets));
            };
            let grp_vitest =
                grp_prog.ends_with("vitest") || grp_base.iter().any(|a| a.contains("vitest"));
            let tmp_dir = tempfile::tempdir()?;
            let json_path = tmp_dir.path().join("fe-report.json");
            let mut command = Command::new(grp_prog);
            ensure_node_dir_in_path(&mut command);
            command.current_dir(&workdir);
            command.args(grp_base);
            if grp_vitest {
                command.arg("--reporter=json");
                command.arg(format!("--outputFile={}", json_path.display()));
            } else {
                command.arg("--json");
                command.arg(format!("--outputFile={}", json_path.display()));
                command.arg("--runTestsByPath");
            }
            let exact_node_requested = name_filter.as_ref().is_some_and(|(_, exact)| *exact);
            if let Some((ref f, is_exact)) = name_filter
                && !f.trim().is_empty()
            {
                let pat = if is_exact {
                    format!("^(?:.* )?{}$", escape_js_regex_literal(f.trim()))
                } else {
                    f.clone()
                };
                command.arg("-t").arg(pat);
            }
            for arg in extra_args {
                command.arg(arg);
            }
            for test_file in group_hashes.keys() {
                let rel_arg = if !pkg_dir.is_empty() && workdir != spec.root {
                    test_file
                        .strip_prefix(&format!("{pkg_dir}/"))
                        .unwrap_or(test_file)
                } else {
                    test_file.as_str()
                };
                command.arg(rel_arg);
            }

            let output = run_command_with_timeout_tracked(
                &mut command,
                timeout,
                2 * 1024 * 1024,
                pkg_active_pgid,
                active_pgids,
            )?;
            if output.timed_out {
                return Err(invalid(format!(
                    "frontend test execution timed out after {}s",
                    spec.policy.pytest_timeout_secs
                ))
                .into());
            }
            if output.cancelled {
                return Err(invalid(
                    "TEST_CANCELLED: frontend test child process group was terminated",
                )
                .into());
            }
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty()) {
                (true, _) => stderr_str.to_string(),
                (_, true) => stdout_str.to_string(),
                (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
            };
            let snippet = truncate_str(&combined_out, 8192);
            if !snippet.trim().is_empty() {
                all_snippets.push(snippet.clone());
            }
            let json_bytes = std::fs::read(&json_path).unwrap_or_default();
            let per_file = parse_vitest_jest_json_output(&json_bytes, &group_hashes);
            let overall_success = output.status.success();
            let any_attributed_failure =
                per_file.values().any(|s| s.failed > 0 || s.errors > 0);
            let has_fe_session_error = !overall_success
                && (stderr_str.contains("Unhandled Rejection")
                    || stderr_str.contains("Unhandled Error")
                    || stderr_str.contains("Uncaught Exception")
                    || stderr_str.contains("globalTeardown")
                    || stderr_str.contains("FATAL ERROR")
                    || stderr_str.contains("Worker exited unexpectedly")
                    || stdout_str.contains("Unhandled Rejection")
                    || stdout_str.contains("Unhandled Error")
                    || stdout_str.contains("globalTeardown"));
            // Reject unexplained nonzero exits OR session-level crash/teardown errors (even when
            // another file also failed) so passing sibling files from a broken session never cache green.
            if !overall_success && (!any_attributed_failure || has_fe_session_error) && !per_file.is_empty() {
                return Err(invalid(format!(
                    "FRONTEND_SESSION_FAILED: frontend test runner exited nonzero with session-level error: {}",
                    truncate_str(stderr_str.trim(), 512)
                ))
                .into());
            }
            let now = now_unix_ms();
            let per_file_fallback_ms =
                (output.duration_ms / (group_hashes.len().max(1) as u64)).max(1);

            for (test_file, dep_hash) in &group_hashes {
                if let Some(stats) = per_file.get(test_file) {
                    let missing_requested_node = exact_node_requested
                        && stats.passed == 0
                        && stats.failed == 0
                        && stats.errors == 0;
                    let outcome = if stats.failed > 0 || missing_requested_node {
                        TestOutcome::Failed
                    } else if stats.errors > 0 {
                        TestOutcome::Error
                    } else if stats.passed > 0 && stats.skipped == 0 {
                        TestOutcome::Passed
                    } else if stats.skipped > 0 && overall_success {
                        TestOutcome::Skipped
                    } else if overall_success {
                        TestOutcome::Passed
                    } else {
                        TestOutcome::Failed
                    };
                    let eff_failed = if missing_requested_node {
                        stats.failed.max(1)
                    } else {
                        stats.failed
                    };
                    let mut eff_failed_nodeids = stats.failed_nodeids.clone();
                    if missing_requested_node && eff_failed_nodeids.is_empty() {
                        eff_failed_nodeids.push(test_file.clone());
                    }
                    records.push(CachedTestRecord {
                        test_file: test_file.clone(),
                        dep_hash: dep_hash.clone(),
                        outcome,
                        passed: stats.passed,
                        failed: eff_failed,
                        skipped: stats.skipped,
                        errors: stats.errors,
                        duration_ms: stats.duration_ms.max(1),
                        failed_nodeids: eff_failed_nodeids,
                        output_snippet: if outcome.is_green() {
                            String::new()
                        } else {
                            snippet.clone()
                        },
                        updated_unix_ms: now,
                        source: source_label.to_string(),
                        environment_hash: environment_hash.to_string(),
                        tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                        runner_mode: if grp_vitest {
                            "vitest".to_string()
                        } else {
                            "jest".to_string()
                        },
                        started_unix_ms: run_started_unix_ms,
                        completed_unix_ms: now,
                        dynamic_dependencies: Vec::new(),
                        dynamic_dependencies_complete: true,
                        node_results: stats.node_results.clone(),
                        external_module_stamps: BTreeMap::new(),
                        exit_code: None,
                    });
                } else {
                    let stats = parse_node_test_output(
                        &combined_out,
                        test_file,
                        per_file_fallback_ms,
                        overall_success,
                    );
                    let outcome = if stats.failed > 0 {
                        TestOutcome::Failed
                    } else if stats.errors > 0 {
                        TestOutcome::Error
                    } else if stats.passed > 0 {
                        TestOutcome::Passed
                    } else if overall_success {
                        TestOutcome::Skipped
                    } else {
                        TestOutcome::Failed
                    };
                    records.push(CachedTestRecord {
                        test_file: test_file.clone(),
                        dep_hash: dep_hash.clone(),
                        outcome,
                        passed: stats.passed,
                        failed: stats.failed,
                        skipped: stats.skipped,
                        errors: stats.errors,
                        duration_ms: stats.duration_ms,
                        failed_nodeids: stats.failed_nodeids,
                        output_snippet: if outcome.is_green() {
                            String::new()
                        } else {
                            snippet.clone()
                        },
                        updated_unix_ms: now,
                        source: source_label.to_string(),
                        environment_hash: environment_hash.to_string(),
                        tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                        runner_mode: if grp_vitest {
                            "vitest".to_string()
                        } else {
                            "jest".to_string()
                        },
                        started_unix_ms: run_started_unix_ms,
                        completed_unix_ms: now,
                        dynamic_dependencies: Vec::new(),
                        dynamic_dependencies_complete: true,
                        node_results: stats.node_results,
                        external_module_stamps: BTreeMap::new(),
                        exit_code: None,
                    });
                }
            }
            Ok((records, all_snippets))
        };

        let mut records = Vec::new();
        let mut all_snippets = Vec::new();
        let distinct_pkg_dirs: BTreeSet<&str> =
            pkg_groups.keys().map(|(pkg, _)| pkg.as_str()).collect();
        // Only run multiple frontend packages concurrently when the caller already verified
        // `parallel_delta` (`parallel_delta_enabled` + `parallel_safe` markers, indicated by
        // `on_demand_parallel`), no serial runner flags are present, and each group targets a distinct package.
        let allow_parallel_pkgs = spec.policy.parallel_delta_enabled
            && source_label == "on_demand_parallel"
            && pkg_groups.len() > 1
            && distinct_pkg_dirs.len() == pkg_groups.len()
            && !has_serial_frontend_flag(extra_args);
        if allow_parallel_pkgs {
            const MAX_CONCURRENT_FE_PKGS: usize = 4;
            let worker_count = pkg_groups.len().min(MAX_CONCURRENT_FE_PKGS);
            let queue = Mutex::new(pkg_groups.into_iter().collect::<VecDeque<_>>());
            let run_ref = &run_one_pkg;
            let queue_ref = &queue;
            let worker_results: Vec<Result<(Vec<CachedTestRecord>, Vec<String>)>> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = (0..worker_count)
                        .map(|_| {
                            scope.spawn(move || -> Result<(Vec<CachedTestRecord>, Vec<String>)> {
                                let mut local_recs = Vec::new();
                                let mut local_snips = Vec::new();
                                loop {
                                    let next = {
                                        queue_ref
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner())
                                            .pop_front()
                                    };
                                    let Some(((pkg_dir, name_filter), group_hashes)) = next else {
                                        break;
                                    };
                                    let (recs, snips) =
                                        run_ref(pkg_dir, name_filter, group_hashes, None)?;
                                    local_recs.extend(recs);
                                    local_snips.extend(snips);
                                }
                                Ok((local_recs, local_snips))
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                Err(invalid("frontend test worker panicked").into())
                            })
                        })
                        .collect()
                });
            for res in worker_results {
                let (recs, snips) = res?;
                records.extend(recs);
                all_snippets.extend(snips);
            }
        } else {
            for ((pkg_dir, name_filter), group_hashes) in pkg_groups {
                let (recs, snips) = run_one_pkg(pkg_dir, name_filter, group_hashes, active_pgid)?;
                records.extend(recs);
                all_snippets.extend(snips);
            }
        }
        combined_records.extend(records);
        combined_snippets.extend(all_snippets);
    }

    for (pkg_dir, pkg_hashes) in node_test_pkg_groups {
        let Some((pkg_cmd, pkg_workdir)) = pkg_runner_cache.get(&pkg_dir) else {
            continue;
        };
        let Some((program, base_args)) = pkg_cmd.split_first() else {
            return Err(invalid("empty frontend_test_cmd").into());
        };
        let pkg_arg_for_runnable = |runnable: &str| -> String {
            if !pkg_dir.is_empty() && pkg_workdir != &spec.root {
                if let Some(rel) = runnable.strip_prefix(&format!("{pkg_dir}/")) {
                    return rel.to_string();
                }
                return spec.root.join(runnable).to_string_lossy().into_owned();
            }
            runnable.to_string()
        };
        let has_distinct_node_filters = pkg_hashes
            .keys()
            .any(|tf| explicit_node_filters_by_file.contains_key(tf));
        let shared_name_filter = if has_distinct_node_filters {
            None
        } else {
            k_expr.map(ToOwned::to_owned)
        };

        // Multi-file single-process `node --test` batch execution with inline JSONL demultiplexing
        // (only when no per-file `::node` filters require distinct `--test-name-pattern` arguments).
        let mut batch_handled = false;
        if pkg_hashes.len() > 1 && !has_distinct_node_filters {
            let mut runnable_to_source = BTreeMap::new();
            let mut command = Command::new(program);
            command.current_dir(pkg_workdir);
            command.args(base_args);
            command.arg(format!("--test-reporter={NODE_JSONL_REPORTER}"));
            if let Some(ref f) = shared_name_filter
                && !f.trim().is_empty()
            {
                command.arg(format!("--test-name-pattern={f}"));
            }
            for arg in extra_args {
                command.arg(arg);
            }
            for test_file in pkg_hashes.keys() {
                let runnable = resolve_node_test_runnable_path(&spec.root, test_file)?;
                let cli_arg = pkg_arg_for_runnable(&runnable);
                runnable_to_source.insert(runnable, test_file.clone());
                command.arg(cli_arg);
            }
            const NODE_TEST_MAX_BYTES: usize = 2 * 1024 * 1024;
            if let Ok(output) = run_command_with_timeout_tracked(
                &mut command,
                timeout,
                NODE_TEST_MAX_BYTES,
                active_pgid,
                active_pgids,
            ) {
                if output.timed_out {
                    return Err(invalid(format!(
                        "node --test execution timed out after {}s",
                        spec.policy.pytest_timeout_secs
                    ))
                    .into());
                }
                if output.cancelled {
                    return Err(invalid(
                        "TEST_CANCELLED: node --test child process group was terminated",
                    )
                    .into());
                }
                let output_truncated = output.stdout.len() >= NODE_TEST_MAX_BYTES
                    || output.stderr.len() >= NODE_TEST_MAX_BYTES;
                let stdout_str = String::from_utf8_lossy(&output.stdout);
                let stderr_str = String::from_utf8_lossy(&output.stderr);
                let reporter_unsupported = !output.status.success()
                    && stdout_str.trim().is_empty()
                    && (stderr_str.contains("test-reporter")
                        || stderr_str.contains("ERR_UNSUPPORTED_ESM_URL_SCHEME")
                        || stderr_str.contains("bad option"));
                if !reporter_unsupported {
                    if output_truncated {
                        return Err(invalid(
                            "NODE_TEST_OUTPUT_TRUNCATED: node --test batch output exceeded 2 MiB capture limit; refusing to cache truncated results",
                        )
                        .into());
                    }
                    let mut per_file =
                        parse_node_jsonl_batch_output(&stdout_str, &runnable_to_source, &pkg_hashes);
                    let overall_success = output.status.success();
                    let any_attributed_failure =
                        per_file.values().any(|s| s.failed > 0 || s.errors > 0);
                    // Reject unexplained nonzero exits (e.g. late uncaught exception after subtests
                    // passed) so passing events from a failing batch are never cached as green, and
                    // do NOT re-execute an already-started batch a second time!
                    if !overall_success && !any_attributed_failure {
                        return Err(invalid(format!(
                            "NODE_TEST_BATCH_FAILED: node --test exited nonzero without an attributed per-file failure: {}",
                            truncate_str(stderr_str.trim(), 512)
                        ))
                        .into());
                    }
                    if per_file.len() != pkg_hashes.len() {
                        return Err(invalid(format!(
                            "NODE_TEST_BATCH_INCOMPLETE: expected reports for {} files but parsed {} (refusing to re-execute side-effecting batch)",
                            pkg_hashes.len(),
                            per_file.len()
                        ))
                        .into());
                    }
                    let snippet = truncate_str(stderr_str.trim(), 8192);
                    if !snippet.trim().is_empty() {
                        combined_snippets.push(snippet.clone());
                    }
                    let now = now_unix_ms();
                    for (test_file, dep_hash) in &pkg_hashes {
                        let stats = per_file.remove(test_file).unwrap_or_default();
                        let outcome = if stats.failed > 0 {
                            TestOutcome::Failed
                        } else if stats.errors > 0 {
                            TestOutcome::Error
                        } else if stats.passed > 0 && stats.skipped == 0 {
                            TestOutcome::Passed
                        } else if stats.skipped > 0 && overall_success {
                            TestOutcome::Skipped
                        } else if overall_success {
                            TestOutcome::Passed
                        } else {
                            TestOutcome::Failed
                        };
                        let used_compiled_artifact = runnable_to_source
                            .iter()
                            .any(|(runnable, src)| src == test_file && runnable != test_file);
                        combined_records.push(CachedTestRecord {
                            test_file: test_file.clone(),
                            dep_hash: dep_hash.clone(),
                            outcome,
                            passed: stats.passed,
                            failed: stats.failed,
                            skipped: stats.skipped,
                            errors: stats.errors,
                            duration_ms: stats.duration_ms.max(1),
                            failed_nodeids: stats.failed_nodeids,
                            output_snippet: if outcome.is_green() {
                                String::new()
                            } else {
                                snippet.clone()
                            },
                            updated_unix_ms: now,
                            source: source_label.to_string(),
                            environment_hash: environment_hash.to_string(),
                            tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                            runner_mode: "node_test_batch".to_string(),
                            started_unix_ms: run_started_unix_ms,
                            completed_unix_ms: now,
                            dynamic_dependencies: Vec::new(),
                            // Compiled `out/`/`dist/`/`build/` JS artifacts are excluded from workspace
                            // indexing, so do not serve cached passes when executing a compiled artifact.
                            dynamic_dependencies_complete: !used_compiled_artifact,
                            node_results: stats.node_results,
                            external_module_stamps: BTreeMap::new(),
                            exit_code: None,
                        });
                    }
                    batch_handled = true;
                }
            }
        }

        if batch_handled {
            continue;
        }

        // Fallback: Node.js built-in test runner (`node --test`) per file (and per explicit node filter)
        for (test_file, dep_hash) in &pkg_hashes {
            let filters: Vec<(Option<String>, bool)> =
                match explicit_node_filters_by_file.get(test_file) {
                    Some(nodes) if !nodes.is_empty() => {
                        nodes.iter().cloned().map(|n| (Some(n), true)).collect()
                    }
                    _ => vec![(k_expr.map(ToOwned::to_owned), false)],
                };
            let runnable = resolve_node_test_runnable_path(&spec.root, test_file)?;
            let cli_arg = pkg_arg_for_runnable(&runnable);
            for (file_filter, is_exact_node) in filters {
                let file_started = now_unix_ms();
                let mut command = Command::new(program);
                command.current_dir(pkg_workdir);
                command.args(base_args);
                command.arg("--test-reporter=tap");
                if let Some(ref f) = file_filter
                    && !f.trim().is_empty()
                {
                    let pat = if is_exact_node {
                        format!("^{}$", escape_js_regex_literal(f.trim()))
                    } else {
                        f.clone()
                    };
                    command.arg(format!("--test-name-pattern={pat}"));
                }
                for arg in extra_args {
                    command.arg(arg);
                }
                command.arg(&cli_arg);

                const NODE_FILE_MAX_BYTES: usize = 2 * 1024 * 1024;
                let output = run_command_with_timeout_tracked(
                    &mut command,
                    timeout,
                    NODE_FILE_MAX_BYTES,
                    active_pgid,
                    active_pgids,
                )?;
                if output.timed_out {
                    return Err(invalid(format!(
                        "node --test execution timed out after {}s",
                        spec.policy.pytest_timeout_secs
                    ))
                    .into());
                }
                if output.cancelled {
                    return Err(invalid(
                        "TEST_CANCELLED: node --test child process group was terminated",
                    )
                    .into());
                }
                let output_truncated = output.stdout.len() >= NODE_FILE_MAX_BYTES
                    || output.stderr.len() >= NODE_FILE_MAX_BYTES;
                let stdout_str = String::from_utf8_lossy(&output.stdout);
                let stderr_str = String::from_utf8_lossy(&output.stderr);
                let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty())
                {
                    (true, _) => stderr_str.to_string(),
                    (_, true) => stdout_str.to_string(),
                    (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
                };
                let snippet = truncate_str(&combined_out, 8192);
                if !snippet.trim().is_empty() {
                    combined_snippets.push(snippet.clone());
                }
                let overall_success = output.status.success();
                let mut stats = parse_node_test_output_with_truncation(
                    &combined_out,
                    test_file,
                    output.duration_ms,
                    overall_success,
                    output_truncated,
                );
                if is_exact_node && stats.passed == 0 && stats.failed == 0 && stats.errors == 0 {
                    stats.failed = 1;
                    stats.failed_nodeids.push(test_file.clone());
                }
                let outcome = if stats.failed > 0 {
                    TestOutcome::Failed
                } else if stats.errors > 0 {
                    TestOutcome::Error
                } else if stats.passed > 0 && stats.skipped == 0 {
                    TestOutcome::Passed
                } else if stats.skipped > 0 && overall_success {
                    TestOutcome::Skipped
                } else if overall_success {
                    TestOutcome::Passed
                } else {
                    TestOutcome::Failed
                };
                let now = now_unix_ms();
                combined_records.push(CachedTestRecord {
                    test_file: test_file.clone(),
                    dep_hash: dep_hash.clone(),
                    outcome,
                    passed: stats.passed,
                    failed: stats.failed,
                    skipped: stats.skipped,
                    errors: stats.errors,
                    duration_ms: stats.duration_ms,
                    failed_nodeids: stats.failed_nodeids,
                    output_snippet: if outcome.is_green() {
                        String::new()
                    } else {
                        snippet
                    },
                    updated_unix_ms: now,
                    source: source_label.to_string(),
                    environment_hash: environment_hash.to_string(),
                    tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                    runner_mode: "node_test".to_string(),
                    started_unix_ms: file_started,
                    completed_unix_ms: now,
                    dynamic_dependencies: Vec::new(),
                    // Compiled `out/`/`dist/`/`build/` JS artifacts are excluded from workspace
                    // indexing, so do not serve cached passes when executing a compiled artifact.
                    dynamic_dependencies_complete: &runnable == test_file && !output_truncated,
                    node_results: stats.node_results,
                    external_module_stamps: BTreeMap::new(),
                    exit_code: None,
                });
            }
        }
    }

    Ok((merge_frontend_records(combined_records), combined_snippets.join("\n")))
}


