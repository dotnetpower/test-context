use super::*;

pub(super) fn parse_cargo_test_output(
    combined_out: &str,
    test_file: &str,
    duration_ms: u64,
    overall_success: bool,
) -> FileXmlStats {
    let mut stats = FileXmlStats {
        duration_ms: duration_ms.max(1),
        ..FileXmlStats::default()
    };
    let mut cases: Vec<(String, TestOutcome)> = Vec::new();
    for raw_line in combined_out.lines() {
        let line = raw_line.trim();
        let nextest_parsed = if let Some(after_pass) = line.strip_prefix("PASS [") {
            after_pass
                .split_once(']')
                .map(|(_, name)| (name.trim(), TestOutcome::Passed))
        } else if let Some(after_fail) = line.strip_prefix("FAIL [") {
            after_fail
                .split_once(']')
                .map(|(_, name)| (name.trim(), TestOutcome::Failed))
        } else if let Some(after_skip) = line.strip_prefix("SKIP [") {
            after_skip
                .split_once(']')
                .map(|(_, name)| (name.trim(), TestOutcome::Skipped))
        } else {
            None
        };
        if let Some((raw_nx_name, nx_outcome)) = nextest_parsed {
            let clean_nx = raw_nx_name
                .rsplit(' ')
                .next()
                .unwrap_or(raw_nx_name)
                .trim();
            if !clean_nx.is_empty() {
                match nx_outcome {
                    TestOutcome::Passed => stats.passed += 1,
                    TestOutcome::Failed => stats.failed += 1,
                    TestOutcome::Skipped => stats.skipped += 1,
                    TestOutcome::Error => stats.errors += 1,
                }
                cases.push((clean_nx.to_string(), nx_outcome));
                continue;
            }
        }
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name_part, status_part)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let test_name = name_part.trim();
        if test_name.is_empty() || test_name.contains(' ') {
            continue;
        }
        let status = status_part.trim();
        let outcome = if status == "ok" {
            stats.passed += 1;
            TestOutcome::Passed
        } else if status == "FAILED" {
            stats.failed += 1;
            TestOutcome::Failed
        } else if status == "ignored" || status.starts_with("ignored,") {
            stats.skipped += 1;
            TestOutcome::Skipped
        } else {
            continue;
        };
        cases.push((test_name.to_string(), outcome));
    }

    let case_count = cases.len().max(1) as u64;
    let per_case_ms = (duration_ms / case_count).max(1);
    let snippet = truncate_str(combined_out, 2048);

    // Count short-name collisions within the file (`one::same` + `two::same`) so a passing
    // duplicate can never overwrite a failing short-name alias in `stats.node_results`.
    let mut short_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (full_name, _) in &cases {
        let short_name = full_name.rsplit("::").next().unwrap_or(full_name);
        *short_counts.entry(short_name.to_string()).or_insert(0) += 1;
    }

    for (full_name, outcome) in cases {
        let short_name = full_name.rsplit("::").next().unwrap_or(&full_name);
        let primary_nodeid = format!("{test_file}::{short_name}");
        let full_nodeid = format!("{test_file}::{full_name}");
        let is_unique_short = short_counts.get(short_name).copied().unwrap_or(0) == 1;
        if outcome == TestOutcome::Failed {
            let fail_id = if is_unique_short {
                &primary_nodeid
            } else {
                &full_nodeid
            };
            if !stats.failed_nodeids.contains(fail_id) {
                stats.failed_nodeids.push(fail_id.clone());
            }
        }
        let out_snip = if outcome.is_green() {
            String::new()
        } else {
            snippet.clone()
        };
        if is_unique_short {
            stats.node_results.insert(
                primary_nodeid.clone(),
                CachedNodeResult {
                    nodeid: primary_nodeid.clone(),
                    outcome,
                    duration_ms: per_case_ms,
                    output_snippet: out_snip.clone(),
                },
            );
        }
        if !is_unique_short || full_nodeid != primary_nodeid {
            stats.node_results.insert(
                full_nodeid.clone(),
                CachedNodeResult {
                    nodeid: full_nodeid,
                    outcome,
                    duration_ms: per_case_ms,
                    output_snippet: out_snip,
                },
            );
        }
    }

    // If `cargo test` exited nonzero without any parsed `FAILED` line (e.g. a later test aborted
    // or segfaulted the test binary after an earlier test passed), record an explicit error so
    // the file can NEVER report or cache as passed.
    if stats.failed == 0 && stats.errors == 0 && !overall_success {
        if combined_out.contains("error[E") || combined_out.contains("could not compile") {
            stats.errors = 1;
        } else {
            stats.failed = 1;
        }
        stats.failed_nodeids.push(test_file.to_string());
    }

    stats
}

pub(super) fn demux_cargo_lib_output(
    combined_out: &str,
    lib_files: &[String],
    duration_ms: u64,
    overall_success: bool,
) -> BTreeMap<String, FileXmlStats> {
    let mut prefixes: Vec<(String, String)> = lib_files
        .iter()
        .filter_map(|tf| rust_module_filter_for_file(tf).map(|pfx| (pfx, tf.clone())))
        .collect();
    // Longest module prefix first so nested submodules (`engine::parser::`) match before parent (`engine::`)
    prefixes.sort_by_key(|(pfx, _)| std::cmp::Reverse(pfx.len()));
    let fallback_lib = lib_files
        .iter()
        .find(|tf| tf.ends_with("src/lib.rs"))
        .cloned();

    let mut raw_cases_by_file: BTreeMap<String, Vec<(String, TestOutcome)>> = BTreeMap::new();
    for tf in lib_files {
        raw_cases_by_file.insert(tf.clone(), Vec::new());
    }

    let mut total_cases = 0usize;
    for raw_line in combined_out.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name_part, status_part)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let test_name = name_part.trim();
        if test_name.is_empty() || test_name.contains(' ') {
            continue;
        }
        let status = status_part.trim();
        let outcome = if status == "ok" {
            TestOutcome::Passed
        } else if status == "FAILED" {
            TestOutcome::Failed
        } else if status == "ignored" || status.starts_with("ignored,") {
            TestOutcome::Skipped
        } else {
            continue;
        };
        let matched_file = prefixes
            .iter()
            .find(|(pfx, _)| test_name.starts_with(pfx.as_str()))
            .map(|(_, tf)| tf.clone())
            .or_else(|| fallback_lib.clone());
        if let Some(tf) = matched_file
            && let Some(bucket) = raw_cases_by_file.get_mut(&tf)
        {
            bucket.push((test_name.to_string(), outcome));
            total_cases += 1;
        }
    }

    let per_case_ms = (duration_ms / (total_cases.max(1) as u64)).max(1);
    let snippet = truncate_str(combined_out, 2048);
    let mut result = BTreeMap::new();

    for tf in lib_files {
        let cases = raw_cases_by_file.remove(tf).unwrap_or_default();
        let file_dur = (per_case_ms * (cases.len().max(1) as u64)).max(1);
        let mut stats = FileXmlStats {
            duration_ms: file_dur,
            ..FileXmlStats::default()
        };
        let mut short_counts: BTreeMap<String, usize> = BTreeMap::new();
        for (full_name, _) in &cases {
            let short_name = full_name.rsplit("::").next().unwrap_or(full_name);
            *short_counts.entry(short_name.to_string()).or_insert(0) += 1;
        }
        for (full_name, outcome) in cases {
            match outcome {
                TestOutcome::Passed => stats.passed += 1,
                TestOutcome::Failed => stats.failed += 1,
                TestOutcome::Skipped => stats.skipped += 1,
                TestOutcome::Error => stats.errors += 1,
            }
            let short_name = full_name.rsplit("::").next().unwrap_or(&full_name);
            let primary_nodeid = format!("{tf}::{short_name}");
            let full_nodeid = format!("{tf}::{full_name}");
            let is_unique_short = short_counts.get(short_name).copied().unwrap_or(0) == 1;
            if outcome == TestOutcome::Failed {
                let fail_id = if is_unique_short {
                    &primary_nodeid
                } else {
                    &full_nodeid
                };
                if !stats.failed_nodeids.contains(fail_id) {
                    stats.failed_nodeids.push(fail_id.clone());
                }
            }
            let out_snip = if outcome.is_green() {
                String::new()
            } else {
                snippet.clone()
            };
            if is_unique_short {
                stats.node_results.insert(
                    primary_nodeid.clone(),
                    CachedNodeResult {
                        nodeid: primary_nodeid.clone(),
                        outcome,
                        duration_ms: per_case_ms,
                        output_snippet: out_snip.clone(),
                    },
                );
            }
            if !is_unique_short || full_nodeid != primary_nodeid {
                stats.node_results.insert(
                    full_nodeid.clone(),
                    CachedNodeResult {
                        nodeid: full_nodeid,
                        outcome,
                        duration_ms: per_case_ms,
                        output_snippet: out_snip,
                    },
                );
            }
        }
        let crate_dir = crate::ast_graph::rust_crate_dir(tf);
        let crate_rel = if crate_dir.is_empty() {
            tf.as_str()
        } else {
            tf.strip_prefix(&format!("{crate_dir}/")).unwrap_or(tf.as_str())
        };
        if stats.passed == 0
            && stats.failed == 0
            && stats.skipped == 0
            && !overall_success
            && (lib_files.len() == 1
                || combined_out.contains(tf.as_str())
                || combined_out.contains(crate_rel))
        {
            if combined_out.contains("error[E") || combined_out.contains("could not compile") {
                stats.errors = 1;
            } else {
                stats.failed = 1;
            }
            stats.failed_nodeids.push(tf.clone());
        }
        result.insert(tf.clone(), stats);
    }
    // Detect abnormal test binary termination (abort/segfault/SIGKILL) INDEPENDENTLY of whether
    // another module already reported a normal failure, because `cargo test` omits the
    // `test result:` summary line or prints `(signal: ...)` when the binary aborts mid-run.
    let abnormal_binary_termination = !overall_success
        && (!combined_out.contains("test result:")
            || combined_out.contains("(signal:")
            || combined_out.contains("SIGABRT")
            || combined_out.contains("SIGSEGV")
            || combined_out.contains("SIGBUS")
            || combined_out.contains("SIGILL"));
    if !overall_success
        && !lib_files.is_empty()
        && (abnormal_binary_termination
            || result
                .values()
                .all(|s| s.failed == 0 && s.errors == 0))
    {
        let is_compile_err =
            combined_out.contains("error[E") || combined_out.contains("could not compile");
        for tf in lib_files {
            if let Some(stats) = result.get_mut(tf)
                && stats.failed == 0
                && stats.errors == 0
            {
                if is_compile_err || abnormal_binary_termination {
                    stats.errors = stats.errors.max(1);
                } else {
                    stats.failed = stats.failed.max(1);
                }
                if !stats.failed_nodeids.contains(tf) {
                    stats.failed_nodeids.push(tf.clone());
                }
            }
        }
    }

    result
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RustArtifactKind {
    Lib,
    Bin(String),
    IntegrationTest(String),
}

pub(super) fn parse_cargo_test_executables_json(
    stdout: &[u8],
) -> BTreeMap<RustArtifactKind, PathBuf> {
    let mut map = BTreeMap::new();
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if val.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let is_test_profile = val
            .get("profile")
            .and_then(|p| p.get("test"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if !is_test_profile {
            continue;
        }
        let Some(exe_str) = val.get("executable").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let exe_path = PathBuf::from(exe_str);
        if !exe_path.is_file() {
            continue;
        }
        let Some(target) = val.get("target") else {
            continue;
        };
        let name = target
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kinds: Vec<&str> = target
            .get("kind")
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        if kinds.contains(&"test") {
            map.insert(RustArtifactKind::IntegrationTest(name.to_string()), exe_path);
        } else if kinds.contains(&"bin") {
            map.insert(RustArtifactKind::Bin(name.to_string()), exe_path);
        } else if kinds.iter().any(|k| matches!(*k, "lib" | "rlib" | "cdylib" | "dylib" | "proc-macro")) {
            map.insert(RustArtifactKind::Lib, exe_path);
        }
    }
    map
}

pub fn cargo_compile_timeout_for_policy(policy: &crate::policy::MemoryPolicy) -> Duration {
    let configured = std::env::var("TCTX_CARGO_COMPILE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| (10..=7_200).contains(&s))
        .unwrap_or_else(|| policy.pytest_timeout_secs.max(600));
    Duration::from_secs(configured)
}

pub(super) fn cargo_compile_timeout_for(spec: &Spec) -> Duration {
    cargo_compile_timeout_for_policy(&spec.policy)
}

pub(super) fn apply_cargo_exec_env(cmd: &mut Command, source_label: &str) {
    cmd.env_remove("TCTX_IDLE_TIMEOUT_SECS")
        .env_remove("TCTX_LARGE_ROOT_IDLE_TIMEOUT_SECS")
        .env_remove("TCTX_MAX_RESIDENT_ROOTS")
        .env_remove("TCTX_MAX_RSS_MB");
    if source_label == "background" && std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        cmd.env("CARGO_BUILD_JOBS", "4");
    }
}


