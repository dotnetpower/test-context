use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_go_test_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    environment_hash: &str,
) -> Result<(Vec<CachedTestRecord>, String)> {
    const MAX_GO_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
    let run_started_unix_ms = now_unix_ms();
    let go_cmd = crate::catalog::detect_go_cmd(&spec.root);
    let Some((program, base_args)) = go_cmd.split_first() else {
        return Err(invalid("empty go_cmd").into());
    };
    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs);

    // Group explicit node selectors (`a_test.go::TestA`, `b_test.go::TestB`) per test file so
    // EVERY requested selector executes and is strictly attributed to its target file.
    let mut selectors_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in invocation_targets {
        let (file_part, node_opt) = split_target_file_and_node_on_disk(&spec.root, t);
        if let Some(node) = node_opt {
            let norm_f = file_part.trim().trim_start_matches("./").replace('\\', "/");
            let trimmed_node = node.trim();
            if !norm_f.is_empty() && !trimmed_node.is_empty() && dep_hashes.contains_key(&norm_f) {
                selectors_by_file
                    .entry(norm_f)
                    .or_default()
                    .push(trimmed_node.to_string());
            }
        }
    }

    let k_trimmed = k_expr.map(str::trim).filter(|k| !k.is_empty());
    let mut records = Vec::new();
    let mut all_outputs = Vec::new();

    for (test_file, sels) in &selectors_by_file {
        let Some(dep_hash) = dep_hashes.get(test_file) else {
            continue;
        };
        let mod_dir = find_enclosing_go_module_dir(&spec.root, test_file);
        let mod_workdir = if mod_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&mod_dir)
        };
        let pkg_arg = go_pkg_arg_relative_to_mod(test_file, &mod_dir);
        // Verify that every requested top-level Go test symbol is actually declared in `test_file`
        // (respecting any `-overlay` replacement) so `a_test.go::TestB` never passes via `b_test.go`.
        let tf_bytes =
            read_go_test_file_with_overlay(&spec.root, &mod_workdir, test_file, extra_args)?;
        let owned_symbols: BTreeSet<String> = crate::ast_graph::parse_polyglot_source(
            test_file,
            &String::from_utf8_lossy(&tf_bytes),
        )
        .test_symbols
        .into_iter()
        .collect();
        let mut owned_sels: Vec<&String> = Vec::new();
        let mut undeclared_sels: Vec<String> = Vec::new();
        for s in sels {
            let top = s.split('/').next().unwrap_or(s);
            if owned_symbols.contains(top) {
                owned_sels.push(s);
            } else {
                undeclared_sels.push(s.clone());
            }
        }
        if !undeclared_sels.is_empty() {
            return Err(invalid(format!(
                "MISSING_TEST_SELECTOR: Undeclared Go test selector(s) in {test_file}: {}",
                undeclared_sels.join(", ")
            ))
            .into());
        }
        let mut has_unmatched_selector = false;
        // When `-k` (Go slash-separated regex, e.g. `TestFoo/bar` or `Test.*`) is combined with
        // explicit node selectors, query `go test -list <k_top>` on the top-level segment (since
        // `go test -list` only enumerates top-level tests) so subtest filters like `TestFoo/bar`
        // are preserved and run instead of being discarded.
        let k_branches: Vec<Vec<String>> = k_trimmed
            .map(|k| {
                split_go_run_alternations(k)
                    .into_iter()
                    .map(|branch| branch.into_iter().map(go_testing_rewrite).collect())
                    .collect()
            })
            .unwrap_or_default();
        let valid_sels: Vec<&String> = if let Some(k) = k_trimmed
            && !owned_sels.is_empty()
        {
            let k_top = go_top_list_regex_for_branches(&k_branches);
            let mut list_cmd = Command::new(program);
            list_cmd.current_dir(&mod_workdir);
            list_cmd.args(base_args);
            list_cmd.arg("test").arg("-list").arg(&k_top);
            for arg in extract_go_build_flags(extra_args) {
                list_cmd.arg(arg);
            }
            list_cmd.arg(&pkg_arg);
            let list_out = run_command_with_timeout_tracked(
                &mut list_cmd,
                timeout,
                2 * 1024 * 1024,
                active_pgid,
                active_pgids,
            )?;
            if list_out.timed_out {
                return Err(invalid(format!(
                    "go test -list timed out after {}s",
                    spec.policy.pytest_timeout_secs
                ))
                .into());
            }
            if list_out.cancelled {
                return Err(
                    invalid("TEST_CANCELLED: go test -list child process group was terminated")
                        .into(),
                );
            }
            if list_out.stdout.len() >= 2 * 1024 * 1024 {
                return Err(invalid("go test -list output truncated (>2 MiB)").into());
            }
            if !list_out.status.success() {
                let err_txt = String::from_utf8_lossy(&list_out.stderr);
                return Err(invalid(format!(
                    "go test -list failed while applying -k regex '{k}' for {test_file}: {}",
                    truncate_str(err_txt.trim(), 512)
                ))
                .into());
            }
            let listed_set: BTreeSet<String> = String::from_utf8_lossy(&list_out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("ok ") && !l.starts_with("? "))
                .map(ToOwned::to_owned)
                .collect();
            owned_sels
                .into_iter()
                .filter(|s| {
                    let top = s.split('/').next().unwrap_or(s);
                    listed_set.contains(top) || listed_set.contains(s.as_str())
                })
                .collect()
        } else {
            owned_sels
        };
        if valid_sels.is_empty() {
            return Err(invalid(format!(
                "MISSING_TEST_SELECTOR: No declared Go test symbol matched in {test_file}: {}",
                sels.join(", ")
            ))
            .into());
        }
        // Reject user `-run` in `extra_args` when node-scoped selectors are active so a user
        // `-run` appended afterward can NEVER override the scoped selector and run sibling tests!
        if extra_args_have_go_run_flag(extra_args) {
            return Err(invalid(
                "CONFLICTING_GO_RUN_FLAG: cannot combine -run in pytest_args with node-scoped Go selectors; use k instead",
            )
            .into());
        }
        // Build per-selector `-run` regexes preserving subtest segments AND evaluating every
        // top-level `|` alternation branch and `/` subtest segment (e.g. `TestA/x|TestB/y`,
        // `TestFoo/[/a]`) with Go RE2 regex semantics while rejecting malformed/unsupported syntax!
        let has_any_k_sub = k_branches.iter().any(|b| b.len() > 1);
        #[derive(PartialEq, Eq)]
        struct GoRunInvocation {
            run_regex: String,
            selector: Option<String>,
            branch_segs: Vec<String>,
            req_depth: usize,
        }
        let mut run_invocations: Vec<GoRunInvocation> = Vec::new();
        if has_any_k_sub {
            // Validate every segment of every branch up front so malformed syntax like `[z-a]`
            // or `TestFoo/[` ALWAYS fails explicitly!
            for branch in &k_branches {
                for seg_pat in branch {
                    if let Err(err) = go_re2_segment_matches(seg_pat, "") {
                        return Err(invalid(format!(
                            "INVALID_GO_RUN_REGEX pattern='{}': {err}",
                            k_trimmed.unwrap_or("")
                        ))
                        .into());
                    }
                }
            }
            for s in &valid_sels {
                let s_parts: Vec<String> = s.split('/').map(go_testing_rewrite).collect();
                let s_top = s_parts[0].as_str();
                for branch in &k_branches {
                    if branch.is_empty() {
                        continue;
                    }
                    // Check top-level segment matches `s_top`
                    match go_re2_segment_matches(&branch[0], s_top) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(err) => {
                            return Err(invalid(format!(
                                "INVALID_GO_RUN_REGEX pattern='{}': {err}",
                                k_trimmed.unwrap_or("")
                            ))
                            .into());
                        }
                    }
                    // Check overlapping subtest segments
                    let mut all_matched = true;
                    for (idx_s, k_seg) in branch[1..].iter().enumerate() {
                        if let Some(s_seg) = s_parts.get(idx_s + 1) {
                            match go_re2_segment_matches(k_seg, s_seg) {
                                Ok(true) => {}
                                Ok(false) => {
                                    all_matched = false;
                                    break;
                                }
                                Err(err) => {
                                    return Err(invalid(format!(
                                        "INVALID_GO_RUN_REGEX pattern='{}': {err}",
                                        k_trimmed.unwrap_or("")
                                    ))
                                    .into());
                                }
                            }
                        }
                    }
                    if all_matched {
                        let req_depth = branch.len().max(s_parts.len());
                        let mut parts: Vec<String> = s_parts
                            .iter()
                            .map(|seg| format!("^{}$", escape_go_regex_literal(seg)))
                            .collect();
                        if branch.len() > s_parts.len() {
                            for deeper in &branch[s_parts.len()..] {
                                parts.push(deeper.clone());
                            }
                        }
                        let inv = GoRunInvocation {
                            run_regex: parts.join("/"),
                            selector: Some(s_parts.join("/")),
                            branch_segs: branch.clone(),
                            req_depth,
                        };
                        if !run_invocations.contains(&inv) {
                            run_invocations.push(inv);
                        }
                    }
                }
            }
        } else if valid_sels.iter().any(|s| s.contains('/')) {
            for s in &valid_sels {
                run_invocations.push(GoRunInvocation {
                    run_regex: s
                        .split('/')
                        .map(|seg| format!("^{}$", escape_go_regex_literal(&go_testing_rewrite(seg))))
                        .collect::<Vec<_>>()
                        .join("/"),
                    selector: None,
                    branch_segs: Vec::new(),
                    req_depth: 1,
                });
            }
        } else {
            run_invocations.push(GoRunInvocation {
                run_regex: format!(
                    "^(?:{})$",
                    valid_sels
                        .iter()
                        .map(|s| escape_go_regex_literal(s))
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                selector: None,
                branch_segs: Vec::new(),
                req_depth: 1,
            });
        }
        let mut agg_stats = FileXmlStats::default();
        let mut agg_success = true;
        let mut agg_pkg_failed = false;
        let mut agg_truncated = false;
        let mut agg_any_events = false;
        let mut agg_snippets = Vec::new();
        let mut skipped_node_ids: BTreeSet<String> = BTreeSet::new();
        for inv in &run_invocations {
            let mut command = Command::new(program);
            command.current_dir(&mod_workdir);
            command.args(base_args);
            command.arg("test").arg("-json");
            let has_count_flag = extra_args
                .iter()
                .any(|a| a == "-count" || a.starts_with("-count="));
            if !has_count_flag {
                command.arg("-count=1");
            }
            command.arg("-run").arg(&inv.run_regex);
            for arg in extra_args {
                command.arg(arg);
            }
            command.arg(&pkg_arg);
            let output = run_command_with_timeout_tracked(
                &mut command,
                timeout,
                MAX_GO_OUTPUT_BYTES,
                active_pgid,
                active_pgids,
            )?;
            if output.timed_out {
                return Err(invalid(format!(
                    "go test execution timed out after {}s",
                    spec.policy.pytest_timeout_secs
                ))
                .into());
            }
            if output.cancelled {
                return Err(
                    invalid("TEST_CANCELLED: go test child process group was terminated").into(),
                );
            }
            if output.stdout.len() >= MAX_GO_OUTPUT_BYTES
                || output.stderr.len() >= MAX_GO_OUTPUT_BYTES
            {
                agg_truncated = true;
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
                all_outputs.push(snippet.clone());
                agg_snippets.push(snippet);
            }
            let mut single_map = BTreeMap::new();
            single_map.insert(test_file.clone(), dep_hash.clone());
            let parsed_go = parse_go_test_json_output_with_pkg_failures(
                &stdout_str,
                &single_map,
                &spec.root,
                &mod_workdir,
                extra_args,
            );
            if !output.status.success() {
                agg_success = false;
            }
            if !parsed_go.failed_pkg_dirs.is_empty() {
                agg_pkg_failed = true;
            }
            if let Some(st) = parsed_go.stats_by_file.get(test_file) {
                // Record skipped ancestor events BEFORE depth filtering so a skipped parent
                // (`TestFoo` calling `t.Skip()`) under a subtest selector + `-k` is recognized as
                // skipped rather than falsely reported as `MISSING_TEST_SELECTOR`!
                for (nid, nres) in &st.node_results {
                    if nres.outcome == TestOutcome::Skipped {
                        skipped_node_ids.insert(nid.clone());
                    }
                }
                // Validate terminal events against the EXACT `(selector, branch)` that produced
                // THIS invocation so a passing intermediate parent (`TestFoo/bar`) under a deeper
                // branch (`TestFoo/bar/qux`) can NEVER satisfy a shorter sibling branch's depth
                // (`TestFoo/nope`)!
                if has_any_k_sub {
                    let Some(inv_sel) = inv.selector.as_deref() else {
                        continue;
                    };
                    let exact = format!("{test_file}::{inv_sel}");
                    let sub_p = format!("{exact}/");
                    let mut matched_any = false;
                    for (nid, nres) in &st.node_results {
                        let ev_name = nid.split_once("::").map_or(nid.as_str(), |(_, n)| n);
                        let ev_segs: Vec<&str> = ev_name.split('/').collect();
                        if ev_segs.len() < inv.req_depth {
                            continue;
                        }
                        if nid != &exact && !nid.starts_with(&sub_p) {
                            continue;
                        }
                        let branch_ok = inv.branch_segs.iter().enumerate().all(|(j, pat)| {
                            ev_segs
                                .get(j)
                                .is_some_and(|seg| go_re2_segment_matches(pat, seg) == Ok(true))
                        });
                        if !branch_ok {
                            continue;
                        }
                        matched_any = true;
                        match nres.outcome {
                            TestOutcome::Passed => agg_stats.passed += 1,
                            TestOutcome::Failed => {
                                agg_stats.failed += 1;
                                if !agg_stats.failed_nodeids.contains(nid) {
                                    agg_stats.failed_nodeids.push(nid.clone());
                                }
                            }
                            TestOutcome::Skipped => agg_stats.skipped += 1,
                            TestOutcome::Error => agg_stats.errors += 1,
                        }
                        agg_stats.duration_ms = agg_stats.duration_ms.saturating_add(nres.duration_ms);
                        agg_stats.node_results.insert(nid.clone(), nres.clone());
                    }
                    if matched_any {
                        agg_any_events = true;
                    }
                } else {
                    agg_any_events = true;
                    agg_stats.passed += st.passed;
                    agg_stats.failed += st.failed;
                    agg_stats.skipped += st.skipped;
                    agg_stats.errors += st.errors;
                    agg_stats.duration_ms = agg_stats.duration_ms.saturating_add(st.duration_ms);
                    for fnid in &st.failed_nodeids {
                        if !agg_stats.failed_nodeids.contains(fnid) {
                            agg_stats.failed_nodeids.push(fnid.clone());
                        }
                    }
                    for (k, v) in &st.node_results {
                        agg_stats.node_results.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        // Verify that EVERY requested selector (including subtest selectors like
        // `TestParent/missing`) produced a matching terminal event. Distinguish an actually skipped
        // ancestor (`TestParent` called `t.Skip()`) from a passing parent that never ran the
        // requested child (`TestParent/missing`) — the latter MUST return `MISSING_TEST_SELECTOR`
        // rather than reporting a non-failing `Skipped` / `passed` status!
        let mut missing_subtest_sels: Vec<String> = Vec::new();
        let expected_sels: Vec<String> = if has_any_k_sub {
            run_invocations
                .iter()
                .filter_map(|inv| inv.selector.clone())
                .collect()
        } else {
            valid_sels
                .iter()
                .map(|sel| sel.split('/').map(go_testing_rewrite).collect::<Vec<_>>().join("/"))
                .collect()
        };
        for norm_sel in &expected_sels {
            let exact_nid = format!("{test_file}::{norm_sel}");
            let sub_prefix = format!("{exact_nid}/");
            let saw_sel = agg_stats
                .node_results
                .keys()
                .any(|k| k == &exact_nid || k.starts_with(&sub_prefix));
            if !saw_sel {
                // Check BOTH the selector itself (e.g. `TestFoo` with `-k TestFoo/child` when
                // `TestFoo` calls `t.Skip()`) AND every ancestor prefix!
                let mut ancestor_or_self_skipped = skipped_node_ids.contains(&exact_nid)
                    || agg_stats
                        .node_results
                        .get(&exact_nid)
                        .is_some_and(|r| r.outcome == TestOutcome::Skipped);
                let mut anc = norm_sel.as_str();
                while !ancestor_or_self_skipped
                    && let Some((parent, _)) = anc.rsplit_once('/')
                {
                    let parent_nid = format!("{test_file}::{parent}");
                    if skipped_node_ids.contains(&parent_nid)
                        || agg_stats
                            .node_results
                            .get(&parent_nid)
                            .is_some_and(|r| r.outcome == TestOutcome::Skipped)
                    {
                        ancestor_or_self_skipped = true;
                        break;
                    }
                    anc = parent;
                }
                if ancestor_or_self_skipped {
                    has_unmatched_selector = true;
                    agg_any_events = true;
                    if agg_stats.skipped == 0 {
                        agg_stats.skipped = 1;
                    }
                } else {
                    missing_subtest_sels.push(norm_sel.clone());
                }
            }
        }
        if !missing_subtest_sels.is_empty()
            && agg_success
            && !agg_pkg_failed
            && agg_stats.failed == 0
            && agg_stats.errors == 0
        {
            return Err(invalid(format!(
                "MISSING_TEST_SELECTOR: Requested Go selector(s) matched no test events in {test_file}: {}",
                missing_subtest_sels.join(", ")
            ))
            .into());
        }
        let combined_snip = agg_snippets.join("\n");
        let now = now_unix_ms();
        if agg_any_events {
            let pkg_failed_without_test_fail = (!agg_success || agg_pkg_failed)
                && agg_stats.failed == 0
                && agg_stats.errors == 0;
            if pkg_failed_without_test_fail {
                agg_stats.failed = 1;
                if !agg_stats.failed_nodeids.contains(test_file) {
                    agg_stats.failed_nodeids.push(test_file.clone());
                }
            }
            if has_unmatched_selector && agg_stats.skipped == 0 {
                agg_stats.skipped = 1;
            }
            let outcome = if agg_stats.failed > 0 {
                TestOutcome::Failed
            } else if agg_stats.errors > 0 || agg_truncated {
                TestOutcome::Error
            } else if agg_stats.passed > 0
                && agg_stats.skipped == 0
                && !has_unmatched_selector
                && agg_success
                && !agg_pkg_failed
            {
                TestOutcome::Passed
            } else if agg_success && !agg_pkg_failed {
                TestOutcome::Skipped
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: agg_stats.passed,
                failed: agg_stats.failed,
                skipped: agg_stats.skipped,
                errors: agg_stats.errors,
                duration_ms: agg_stats.duration_ms.max(1),
                failed_nodeids: agg_stats.failed_nodeids,
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    combined_snip
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "go_test_json".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: false,
                node_results: agg_stats.node_results,
                external_module_stamps: BTreeMap::new(),
                exit_code: None,
            });
        } else {
            let is_build_err = !agg_success
                && (combined_snip.contains("[build failed]")
                    || combined_snip.contains("[setup failed]")
                    || combined_snip.contains("cannot find package"));
            let outcome = if agg_truncated {
                TestOutcome::Error
            } else if agg_success && !agg_pkg_failed {
                TestOutcome::Skipped
            } else if is_build_err {
                TestOutcome::Error
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: 0,
                failed: usize::from(outcome == TestOutcome::Failed),
                skipped: usize::from(outcome == TestOutcome::Skipped),
                errors: usize::from(outcome == TestOutcome::Error),
                duration_ms: 1,
                failed_nodeids: if outcome.is_green() || outcome == TestOutcome::Skipped {
                    Vec::new()
                } else {
                    vec![test_file.clone()]
                },
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    combined_snip
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "go_test_json".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: false,
                node_results: BTreeMap::new(),
                external_module_stamps: BTreeMap::new(),
                exit_code: None,
            });
        }
    }

    // Group remaining whole-file Go targets by their enclosing `go.mod` directory so standalone
    // nested modules execute from their own module root.
    let mut remaining_by_mod: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (tf, dh) in dep_hashes {
        if !selectors_by_file.contains_key(tf) {
            let mod_dir = find_enclosing_go_module_dir(&spec.root, tf);
            remaining_by_mod
                .entry(mod_dir)
                .or_default()
                .insert(tf.clone(), dh.clone());
        }
    }
    for (mod_dir, remaining_hashes) in remaining_by_mod {
        let mod_workdir = if mod_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&mod_dir)
        };
        let mut files_by_pkg: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for tf in remaining_hashes.keys() {
            files_by_pkg
                .entry(go_pkg_arg_relative_to_mod(tf, &mod_dir))
                .or_default()
                .push(tf.clone());
        }
        // Check whether EVERY requested Go package includes ALL sibling `_test.go` files in that
        // directory; if a subset of `_test.go` files was explicitly selected (e.g. `pkg/a_test.go`
        // while `pkg/b_test.go` also exists), scope each partial package to its requested files'
        // declared test symbols via `-run` so unrelated `b_test.go` tests do not execute!
        let mut full_pkgs: Vec<String> = Vec::new();
        let mut partial_pkg_runs: Vec<(String, String)> = Vec::new();
        for (pkg_arg, pkg_tfs) in &files_by_pkg {
            let pkg_abs = if pkg_arg == "." {
                mod_workdir.clone()
            } else {
                mod_workdir.join(pkg_arg.trim_start_matches("./"))
            };
            let disk_test_count = std::fs::read_dir(&pkg_abs)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    e.path().is_file()
                        && e.file_name()
                            .to_str()
                            .is_some_and(|n| n.ends_with("_test.go"))
                })
                .count();
            if disk_test_count <= pkg_tfs.len() {
                full_pkgs.push(pkg_arg.clone());
            } else {
                // Reject conflicting `-run` in `extra_args` on file-scoped partial Go packages so
                // a user `-run` appended afterward can NEVER override file scoping and run sibling files!
                if extra_args_have_go_run_flag(extra_args) {
                    return Err(invalid(
                        "CONFLICTING_GO_RUN_FLAG: cannot combine -run in pytest_args with file-scoped Go selection in a multi-file package; use k or target the package directory",
                    )
                    .into());
                }
                let mut owned_syms: BTreeSet<String> = BTreeSet::new();
                for tf in pkg_tfs {
                    if let Ok(bytes) =
                        read_go_test_file_with_overlay(&spec.root, &mod_workdir, tf, extra_args)
                    {
                        for sym in crate::ast_graph::parse_polyglot_source(
                            tf,
                            &String::from_utf8_lossy(&bytes),
                        )
                        .test_symbols
                        {
                            owned_syms.insert(sym);
                        }
                    }
                }
                let k_branches_partial: Vec<Vec<String>> = k_trimmed
                    .map(|k| {
                        split_go_run_alternations(k)
                            .into_iter()
                            .map(|branch| branch.into_iter().map(go_testing_rewrite).collect())
                            .collect()
                    })
                    .unwrap_or_default();
                let final_syms: Vec<String> = if let Some(k) = k_trimmed
                    && !owned_syms.is_empty()
                {
                    let k_top = go_top_list_regex_for_branches(&k_branches_partial);
                    let mut list_cmd = Command::new(program);
                    list_cmd.current_dir(&mod_workdir);
                    list_cmd.args(base_args);
                    list_cmd.arg("test").arg("-list").arg(&k_top);
                    for arg in extract_go_build_flags(extra_args) {
                        list_cmd.arg(arg);
                    }
                    list_cmd.arg(pkg_arg);
                    let lo = run_command_with_timeout_tracked(
                        &mut list_cmd,
                        timeout,
                        2 * 1024 * 1024,
                        active_pgid,
                        active_pgids,
                    )?;
                    if lo.timed_out {
                        return Err(invalid(format!(
                            "go test -list timed out after {}s",
                            spec.policy.pytest_timeout_secs
                        ))
                        .into());
                    }
                    if lo.cancelled {
                        return Err(invalid(
                            "TEST_CANCELLED: go test -list child process group was terminated",
                        )
                        .into());
                    }
                    if lo.stdout.len() >= 2 * 1024 * 1024 {
                        return Err(invalid("go test -list output truncated (>2 MiB)").into());
                    }
                    if !lo.status.success() {
                        let msg = String::from_utf8_lossy(&lo.stderr);
                        return Err(invalid(format!(
                            "INVALID_GO_TEST_FILTER k='{k}': {}",
                            msg.trim()
                        ))
                        .into());
                    }
                    let listed: BTreeSet<String> = String::from_utf8_lossy(&lo.stdout)
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with("ok ") && !l.starts_with("? "))
                        .map(str::to_string)
                        .collect();
                    owned_syms
                        .into_iter()
                        .filter(|s| listed.contains(s))
                        .collect()
                } else {
                    owned_syms.into_iter().collect()
                };
                // Build scoped `-run` per top-level alternation branch so `k = "TestA/x|TestB/y"`
                // on `b_test.go` (owning only `TestB`) produces `^TestB$/y` cleanly!
                let scoped_run = if final_syms.is_empty() {
                    "^$".to_string()
                } else if k_branches_partial.iter().any(|b| b.len() > 1) {
                    let mut branch_runs: Vec<String> = Vec::new();
                    for branch in &k_branches_partial {
                        if branch.is_empty() {
                            continue;
                        }
                        let mut matching_tops: Vec<String> = Vec::new();
                        for sym in &final_syms {
                            match go_re2_segment_matches(&branch[0], sym) {
                                Ok(true) => matching_tops.push(escape_go_regex_literal(sym)),
                                Ok(false) => {}
                                Err(err) => {
                                    return Err(invalid(format!(
                                        "INVALID_GO_RUN_REGEX pattern='{}': {err}",
                                        k_trimmed.unwrap_or("")
                                    ))
                                    .into());
                                }
                            }
                        }
                        if !matching_tops.is_empty() {
                            let top_re = format!("^(?:{})$", matching_tops.join("|"));
                            if branch.len() > 1 {
                                branch_runs.push(format!("{top_re}/{}", branch[1..].join("/")));
                            } else {
                                branch_runs.push(top_re);
                            }
                        }
                    }
                    if branch_runs.is_empty() {
                        "^$".to_string()
                    } else {
                        branch_runs.join("|")
                    }
                } else {
                    format!(
                        "^(?:{})$",
                        final_syms
                            .iter()
                            .map(|s| escape_go_regex_literal(s))
                            .collect::<Vec<_>>()
                            .join("|")
                    )
                };
                partial_pkg_runs.push((pkg_arg.clone(), scoped_run));
            }
        }
        let mut go_invocations: Vec<(Vec<String>, Option<String>)> = Vec::new();
        if !full_pkgs.is_empty() {
            go_invocations.push((full_pkgs, k_trimmed.map(str::to_string)));
        }
        for (pkg_arg, scoped_run) in partial_pkg_runs {
            go_invocations.push((vec![pkg_arg], Some(scoped_run)));
        }

        let mut combined_stdout_chunks: Vec<String> = Vec::new();
        let mut combined_stderr_chunks: Vec<String> = Vec::new();
        let mut overall_success = true;
        let mut output_truncated = false;
        let mut agg_duration_ms: u64 = 0;
        for (inv_pkgs, inv_run_filter) in go_invocations {
            let mut command = Command::new(program);
            command.current_dir(&mod_workdir);
            command.args(base_args);
            command.arg("test").arg("-json");
            let has_count_flag = extra_args
                .iter()
                .any(|a| a == "-count" || a.starts_with("-count="));
            if !has_count_flag {
                command.arg("-count=1");
            }
            if let Some(ref rf) = inv_run_filter {
                command.arg("-run").arg(rf);
            }
            for arg in extra_args {
                command.arg(arg);
            }
            for pkg in &inv_pkgs {
                command.arg(pkg);
            }
            let inv_out = run_command_with_timeout_tracked(
                &mut command,
                timeout,
                MAX_GO_OUTPUT_BYTES,
                active_pgid,
                active_pgids,
            )?;
            if inv_out.timed_out {
                return Err(invalid(format!(
                    "go test execution timed out after {}s",
                    spec.policy.pytest_timeout_secs
                ))
                .into());
            }
            if inv_out.cancelled {
                return Err(
                    invalid("TEST_CANCELLED: go test child process group was terminated").into(),
                );
            }
            agg_duration_ms = agg_duration_ms.saturating_add(inv_out.duration_ms);
            if !inv_out.status.success() {
                overall_success = false;
            }
            if inv_out.stdout.len() >= MAX_GO_OUTPUT_BYTES
                || inv_out.stderr.len() >= MAX_GO_OUTPUT_BYTES
            {
                output_truncated = true;
            }
            let so = String::from_utf8_lossy(&inv_out.stdout).into_owned();
            let se = String::from_utf8_lossy(&inv_out.stderr).into_owned();
            if !so.is_empty() {
                combined_stdout_chunks.push(so);
            }
            if !se.is_empty() {
                combined_stderr_chunks.push(se);
            }
        }
        let stdout_str = combined_stdout_chunks.join("\n");
        let stderr_str = combined_stderr_chunks.join("\n");
        let output_duration_ms = agg_duration_ms;
        let output_success = overall_success;
        let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty()) {
            (true, _) => stderr_str.to_string(),
            (_, true) => stdout_str.to_string(),
            (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
        };
        let snippet = truncate_str(&combined_out, 8192);
        if !snippet.trim().is_empty() {
            all_outputs.push(snippet.clone());
        }
        let mut parsed_go = parse_go_test_json_output_with_pkg_failures(
            &stdout_str,
            &remaining_hashes,
            &spec.root,
            &mod_workdir,
            extra_args,
        );
        let overall_success = output_success;
        let now = now_unix_ms();
        let fallback_dur = (output_duration_ms / (remaining_hashes.len().max(1) as u64)).max(1);

        for (test_file, dep_hash) in &remaining_hashes {
            let tf_dir = Path::new(test_file)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            let pkg_passed = parsed_go.passed_pkg_dirs.contains(&tf_dir);
            let pkg_failed = parsed_go.failed_pkg_dirs.contains(&tf_dir)
                || (!overall_success && !pkg_passed);
            let pkg_ambiguous = parsed_go.ambiguous_pkg_dirs.contains(&tf_dir);
            if let Some(mut stats) = parsed_go.stats_by_file.remove(test_file) {
                if pkg_failed && stats.failed == 0 && stats.errors == 0 {
                    stats.failed = 1;
                    if !stats.failed_nodeids.contains(test_file) {
                        stats.failed_nodeids.push(test_file.clone());
                    }
                }
                let outcome = if stats.failed > 0 {
                    TestOutcome::Failed
                } else if stats.errors > 0 || (output_truncated && !pkg_passed) {
                    TestOutcome::Error
                } else if stats.passed > 0
                    && stats.skipped == 0
                    && !pkg_failed
                    && !output_truncated
                {
                    TestOutcome::Passed
                } else if stats.skipped > 0 && overall_success && !pkg_failed && !output_truncated {
                    TestOutcome::Skipped
                } else if overall_success && !pkg_failed && !output_truncated {
                    TestOutcome::Passed
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
                    runner_mode: "go_test_json".to_string(),
                    started_unix_ms: run_started_unix_ms,
                    completed_unix_ms: now,
                    dynamic_dependencies: Vec::new(),
                    dynamic_dependencies_complete: k_trimmed.is_none()
                        && !pkg_failed
                        && !output_truncated
                        && !pkg_ambiguous,
                    node_results: stats.node_results,
                    external_module_stamps: BTreeMap::new(),
                    exit_code: None,
                });
            } else {
                // Zero tests executed for this specific Go test file (e.g. excluded by `//go:build`
                // tag or unmatched `-k`); NEVER cache an unexecuted file as Passed.
                let is_build_err = (!overall_success || pkg_failed)
                    && (combined_out.contains("[build failed]")
                        || combined_out.contains("[setup failed]")
                        || combined_out.contains("cannot find package"));
                let outcome = if output_truncated && !pkg_passed {
                    TestOutcome::Error
                } else if overall_success && !pkg_failed {
                    TestOutcome::Skipped
                } else if is_build_err {
                    TestOutcome::Error
                } else {
                    TestOutcome::Failed
                };
                records.push(CachedTestRecord {
                    test_file: test_file.clone(),
                    dep_hash: dep_hash.clone(),
                    outcome,
                    passed: 0,
                    failed: usize::from(outcome == TestOutcome::Failed),
                    skipped: usize::from(outcome == TestOutcome::Skipped),
                    errors: usize::from(outcome == TestOutcome::Error),
                    duration_ms: fallback_dur,
                    failed_nodeids: if outcome.is_green() || outcome == TestOutcome::Skipped {
                        Vec::new()
                    } else {
                        vec![test_file.clone()]
                    },
                    output_snippet: if outcome.is_green() {
                        String::new()
                    } else {
                        snippet.clone()
                    },
                    updated_unix_ms: now,
                    source: source_label.to_string(),
                    environment_hash: environment_hash.to_string(),
                    tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                    runner_mode: "go_test_json".to_string(),
                    started_unix_ms: run_started_unix_ms,
                    completed_unix_ms: now,
                    dynamic_dependencies: Vec::new(),
                    dynamic_dependencies_complete: false,
                    node_results: BTreeMap::new(),
                    external_module_stamps: BTreeMap::new(),
                    exit_code: None,
                });
            }
        }
    }
    Ok((records, all_outputs.join("\n")))
}

pub(super) fn resolve_go_package_rel_dir(
    pkg: &str,
    go_modules: &[(String, String)],
    known_dirs: &BTreeSet<String>,
) -> Option<String> {
    for (mod_name, mod_dir) in go_modules {
        if let Some(rest) = pkg.strip_prefix(mod_name.as_str())
            && (rest.is_empty() || rest.starts_with('/'))
        {
            let sub = rest.trim_start_matches('/');
            let dir = if mod_dir.is_empty() {
                sub.to_string()
            } else if sub.is_empty() {
                mod_dir.clone()
            } else {
                format!("{mod_dir}/{sub}")
            };
            return Some(dir);
        }
    }
    let mut non_empty: Vec<&String> = known_dirs.iter().filter(|d| !d.is_empty()).collect();
    non_empty.sort_by_key(|d| std::cmp::Reverse(d.len()));
    for dir in non_empty {
        if pkg == dir.as_str() || pkg.ends_with(&format!("/{dir}")) {
            return Some(dir.clone());
        }
    }
    if known_dirs.contains("") {
        return Some(String::new());
    }
    None
}

pub(super) struct ParsedGoJsonBatch {
    pub(super) stats_by_file: BTreeMap<String, FileXmlStats>,
    pub(super) passed_pkg_dirs: BTreeSet<String>,
    pub(super) failed_pkg_dirs: BTreeSet<String>,
    pub(super) ambiguous_pkg_dirs: BTreeSet<String>,
}

pub(super) fn extract_go_build_flags(extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < extra_args.len() {
        let a = extra_args[idx].as_str();
        if matches!(
            a,
            "-tags" | "--tags" | "-mod" | "--mod" | "-modfile" | "--modfile" | "-overlay" | "--overlay"
        ) {
            out.push(a.to_string());
            if idx + 1 < extra_args.len() {
                idx += 1;
                out.push(extra_args[idx].clone());
            }
        } else if a.starts_with("-tags=")
            || a.starts_with("--tags=")
            || a.starts_with("-mod=")
            || a.starts_with("--mod=")
            || a.starts_with("-modfile=")
            || a.starts_with("--modfile=")
            || a.starts_with("-overlay=")
            || a.starts_with("--overlay=")
            || matches!(
                a,
                "-race" | "--race" | "-msan" | "--msan" | "-asan" | "--asan"
            )
            || a.starts_with("-race=")
            || a.starts_with("--race=")
            || a.starts_with("-msan=")
            || a.starts_with("--msan=")
            || a.starts_with("-asan=")
            || a.starts_with("--asan=")
        {
            out.push(a.to_string());
        }
        idx += 1;
    }
    out
}

/// Queries active compiled `TestGoFiles` / `XTestGoFiles` for a Go package directory when
/// duplicate test symbols exist across build-tagged files (e.g. `foo_linux_test.go` vs
/// `foo_windows_test.go`), forwarding effective build flags (`-tags`, `-mod`, `-race`).
pub(super) fn active_go_test_files_in_dir(
    workspace_root: &Path,
    dir: &str,
    extra_args: &[String],
) -> Option<BTreeSet<String>> {
    let go_cmd = crate::catalog::detect_go_cmd(workspace_root);
    let (program, base_args) = go_cmd.split_first()?;
    let sample_file = if dir.is_empty() {
        "x_test.go".to_string()
    } else {
        format!("{dir}/x_test.go")
    };
    let mod_dir = find_enclosing_go_module_dir(workspace_root, &sample_file);
    let mod_workdir = if mod_dir.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(&mod_dir)
    };
    let pkg_arg = go_pkg_arg_relative_to_mod(&sample_file, &mod_dir);
    let mut cmd = Command::new(program);
    cmd.current_dir(&mod_workdir)
        .args(base_args)
        .arg("list")
        .arg("-json");
    for bf in extract_go_build_flags(extra_args) {
        cmd.arg(bf);
    }
    cmd.arg(&pkg_arg);
    let out = run_command_with_timeout(&mut cmd, Duration::from_secs(5), 256 * 1024).ok()?;
    if !out.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut active = BTreeSet::new();
    for field in ["TestGoFiles", "XTestGoFiles"] {
        if let Some(arr) = val.get(field).and_then(serde_json::Value::as_array) {
            for item in arr.iter().filter_map(serde_json::Value::as_str) {
                let rel = if dir.is_empty() {
                    item.to_string()
                } else {
                    format!("{dir}/{item}")
                };
                active.insert(rel);
            }
        }
    }
    Some(active)
}

pub(super) fn parse_go_test_json_output_with_pkg_failures(
    stdout: &str,
    known_files: &BTreeMap<String, String>,
    workspace_root: &Path,
    mod_workdir: &Path,
    extra_args: &[String],
) -> ParsedGoJsonBatch {
    let mut go_modules: Vec<(String, String)> = Vec::new();
    let mut known_dirs: BTreeSet<String> = BTreeSet::new();
    let mut mod_dirs_checked: BTreeSet<String> = BTreeSet::new();
    for tf in known_files.keys() {
        let dir = Path::new(tf)
            .parent()
            .map(normalize_rel_str)
            .unwrap_or_default();
        known_dirs.insert(dir.clone());
        let mut cur: Option<PathBuf> = Some(if dir.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(&dir)
        });
        while let Some(rel_dir) = cur {
            let rel_s = normalize_rel_str(&rel_dir);
            if mod_dirs_checked.insert(rel_s.clone()) {
                let go_mod = if rel_s.is_empty() {
                    workspace_root.join("go.mod")
                } else {
                    workspace_root.join(&rel_s).join("go.mod")
                };
                if let Ok(bytes) = std::fs::read(&go_mod) {
                    let text = String::from_utf8_lossy(&bytes);
                    for raw in text.lines() {
                        let line = raw.split("//").next().unwrap_or("").trim();
                        if let Some(rest) = line.strip_prefix("module")
                            && rest.as_bytes().first().is_some_and(u8::is_ascii_whitespace)
                        {
                            let m = rest.trim().trim_matches(['"', '\'']);
                            if !m.is_empty() {
                                go_modules.push((m.to_string(), rel_s.clone()));
                            }
                        }
                    }
                }
            }
            if rel_s.is_empty() {
                break;
            }
            cur = rel_dir.parent().map(Path::to_path_buf);
        }
    }
    go_modules.sort_by_key(|(m, _)| std::cmp::Reverse(m.len()));

    // Track ALL declaring `*_test.go` files per `(dir, sym)` (including virtual files introduced
    // by `-overlay` JSON replacements) so overlay-created tests are never misattributed.
    let mut has_overlay_flag = false;
    let mut overlay_path_arg: Option<&str> = None;
    {
        let mut oi = 0usize;
        while oi < extra_args.len() {
            let a = extra_args[oi].as_str();
            if matches!(a, "-overlay" | "--overlay") && oi + 1 < extra_args.len() {
                has_overlay_flag = true;
                overlay_path_arg = Some(extra_args[oi + 1].as_str());
                oi += 2;
                continue;
            }
            if let Some(p) = a.strip_prefix("-overlay=").or_else(|| a.strip_prefix("--overlay=")) {
                has_overlay_flag = true;
                overlay_path_arg = Some(p);
            }
            oi += 1;
        }
    }
    let mut symbol_owners: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for dir in &known_dirs {
        let abs_dir = if dir.is_empty() {
            workspace_root.to_path_buf()
        } else {
            workspace_root.join(dir)
        };
        if let Ok(entries) = std::fs::read_dir(&abs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if !name_str.ends_with("_test.go") {
                    continue;
                }
                let rel_tf = if dir.is_empty() {
                    name_str.to_string()
                } else {
                    format!("{dir}/{name_str}")
                };
                let mod_dir = find_enclosing_go_module_dir(workspace_root, &rel_tf);
                let mod_workdir = if mod_dir.is_empty() {
                    workspace_root.to_path_buf()
                } else {
                    workspace_root.join(&mod_dir)
                };
                if let Ok(bytes) =
                    read_go_test_file_with_overlay(workspace_root, &mod_workdir, &rel_tf, extra_args)
                {
                    let parsed = crate::ast_graph::parse_polyglot_source(
                        &rel_tf,
                        &String::from_utf8_lossy(&bytes),
                    );
                    for sym in parsed.test_symbols {
                        let vec = symbol_owners.entry((dir.clone(), sym)).or_default();
                        if !vec.contains(&rel_tf) {
                            vec.push(rel_tf.clone());
                        }
                    }
                }
            }
        }
    }
    // Also enumerate virtual `*_test.go` files created via `-overlay` JSON `"Replace"` map,
    // resolving relative overlay paths against `mod_workdir` (the actual Go command cwd) and
    // failing closed (`overlay_ambiguous`) if the overlay file cannot be read or parsed.
    let mut overlay_ambiguous = false;
    if let Some(ov_raw) = overlay_path_arg {
        let ov_file = if Path::new(ov_raw).is_absolute() {
            PathBuf::from(ov_raw)
        } else {
            mod_workdir.join(ov_raw)
        };
        if let Ok(ov_bytes) = std::fs::read(&ov_file)
            && let Ok(ov_json) = serde_json::from_slice::<serde_json::Value>(&ov_bytes)
            && let Some(replace_map) = ov_json.get("Replace").and_then(serde_json::Value::as_object)
        {
            let ws_canon =
                std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
            for (k, v) in replace_map {
                let Some(repl) = v.as_str() else {
                    continue;
                };
                if repl.is_empty() || !k.ends_with("_test.go") {
                    continue;
                }
                let k_abs = if Path::new(k).is_absolute() {
                    PathBuf::from(k)
                } else {
                    mod_workdir.join(k)
                };
                let rel_opt = k_abs
                    .strip_prefix(workspace_root)
                    .ok()
                    .or_else(|| k_abs.strip_prefix(&ws_canon).ok());
                let Some(rel_p) = rel_opt else {
                    overlay_ambiguous = true;
                    continue;
                };
                // Lexically normalize virtual overlay paths (e.g. `pkg/../pkg/virtual_test.go`
                // -> `pkg/virtual_test.go`) so directory ownership matches Go's package dir.
                let Some(rel_tf) = crate::ast_graph::normalize_lexical_rel(rel_p) else {
                    overlay_ambiguous = true;
                    continue;
                };
                let dir = Path::new(&rel_tf)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                let repl_abs = if Path::new(repl).is_absolute() {
                    PathBuf::from(repl)
                } else {
                    mod_workdir.join(repl)
                };
                if let Ok(bytes) = std::fs::read(&repl_abs) {
                    let parsed = crate::ast_graph::parse_polyglot_source(
                        &rel_tf,
                        &String::from_utf8_lossy(&bytes),
                    );
                    for sym in parsed.test_symbols {
                        let vec = symbol_owners.entry((dir.clone(), sym)).or_default();
                        if !vec.contains(&rel_tf) {
                            vec.push(rel_tf.clone());
                        }
                    }
                } else {
                    overlay_ambiguous = true;
                }
            }
        } else {
            overlay_ambiguous = true;
        }
    }
    // Resolve any ambiguous duplicate-symbol packages via `go list -json`.
    let mut ambiguous_pkg_dirs: BTreeSet<String> = BTreeSet::new();
    if overlay_ambiguous {
        for d in &known_dirs {
            ambiguous_pkg_dirs.insert(d.clone());
        }
    }
    let mut active_files_by_dir: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    for ((dir, _), owners) in &symbol_owners {
        if owners.len() > 1 && !active_files_by_dir.contains_key(dir) {
            let active = active_go_test_files_in_dir(workspace_root, dir, extra_args);
            if active.is_none() {
                ambiguous_pkg_dirs.insert(dir.clone());
            }
            active_files_by_dir.insert(dir.clone(), active);
        }
    }

    let mut stats_by_file: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    let mut passed_pkg_dirs: BTreeSet<String> = BTreeSet::new();
    let mut failed_pkg_dirs: BTreeSet<String> = BTreeSet::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(action) = val.get("Action").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let pkg = val
            .get("Package")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let resolved_dir = resolve_go_package_rel_dir(pkg, &go_modules, &known_dirs);
        if val.get("Test").is_none() {
            if let Some(d) = resolved_dir {
                if action == "fail" {
                    failed_pkg_dirs.insert(d);
                } else if action == "pass" {
                    passed_pkg_dirs.insert(d);
                }
            }
            continue;
        }
        if !matches!(action, "pass" | "fail" | "skip") {
            continue;
        }
        let Some(test_name) = val.get("Test").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let elapsed_ms = val
            .get("Elapsed")
            .and_then(serde_json::Value::as_f64)
            .map(|secs| (secs * 1000.0).max(1.0).round() as u64)
            .unwrap_or(1);

        let top_test = test_name.split('/').next().unwrap_or(test_name);
        let matched_file = if let Some(ref exact_dir) = resolved_dir {
            if let Some(owners) = symbol_owners.get(&(exact_dir.clone(), top_test.to_string())) {
                if owners.len() == 1 {
                    let owner = &owners[0];
                    known_files.contains_key(owner).then(|| owner.clone())
                } else if let Some(Some(active_set)) = active_files_by_dir.get(exact_dir) {
                    let active_owners: Vec<&String> = owners
                        .iter()
                        .filter(|o| active_set.contains(*o))
                        .collect();
                    if active_owners.len() == 1 {
                        let owner = active_owners[0];
                        known_files.contains_key(owner).then(|| owner.clone())
                    } else {
                        ambiguous_pkg_dirs.insert(exact_dir.clone());
                        None
                    }
                } else {
                    ambiguous_pkg_dirs.insert(exact_dir.clone());
                    None
                }
            } else {
                let dir_has_any_owners = symbol_owners.keys().any(|(d, _)| d == exact_dir);
                if dir_has_any_owners || has_overlay_flag {
                    None
                } else {
                    known_files
                        .keys()
                        .find(|tf| {
                            Path::new(tf)
                                .parent()
                                .map(normalize_rel_str)
                                .unwrap_or_default()
                                == *exact_dir
                        })
                        .cloned()
                }
            }
        } else {
            None
        };
        let Some(test_file) = matched_file else {
            continue;
        };
        let entry = stats_by_file.entry(test_file.clone()).or_default();
        entry.duration_ms = entry.duration_ms.saturating_add(elapsed_ms);
        let outcome = match action {
            "pass" => {
                entry.passed += 1;
                TestOutcome::Passed
            }
            "skip" => {
                entry.skipped += 1;
                TestOutcome::Skipped
            }
            _ => {
                entry.failed += 1;
                TestOutcome::Failed
            }
        };
        let nodeid = format!("{test_file}::{test_name}");
        if outcome == TestOutcome::Failed && !entry.failed_nodeids.contains(&nodeid) {
            entry.failed_nodeids.push(nodeid.clone());
        }
        entry.node_results.insert(
            nodeid.clone(),
            CachedNodeResult {
                nodeid,
                outcome,
                duration_ms: elapsed_ms,
                output_snippet: String::new(),
            },
        );
    }
    ParsedGoJsonBatch {
        stats_by_file,
        passed_pkg_dirs,
        failed_pkg_dirs,
        ambiguous_pkg_dirs,
    }
}

#[cfg(test)]
pub(super) fn parse_go_test_json_output(
    stdout: &str,
    known_files: &BTreeMap<String, String>,
    workspace_root: &Path,
) -> BTreeMap<String, FileXmlStats> {
    parse_go_test_json_output_with_pkg_failures(
        stdout,
        known_files,
        workspace_root,
        workspace_root,
        &[],
    )
    .stats_by_file
}


