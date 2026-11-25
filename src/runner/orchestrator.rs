use super::*;

impl ManagedRoot {
    /// Re-runs once or twice when sources were edited mid-run, so the caller receives a result for the
    /// current content instead of a discarded one. Long attempts are not repeated.
    pub fn run_tests(&self, req: &TestRunRequest) -> Result<TestRunResponse> {
        self.refresh_cache_from_disk_if_changed();
        const MAX_STABILITY_RETRIES: u32 = 2;
        const MAX_RETRYABLE_ATTEMPT: Duration = Duration::from_secs(60);
        let mut attempt = 0;
        loop {
            let attempt_started = Instant::now();
            let allow_partial_retry = attempt < MAX_STABILITY_RETRIES;
            match self.run_tests_once(req, allow_partial_retry) {
                Err(error)
                    if is_source_changed_error(error.as_ref())
                        && attempt < MAX_STABILITY_RETRIES
                        && attempt_started.elapsed() < MAX_RETRYABLE_ATTEMPT
                        && !self.stopped.load(Ordering::Acquire) =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
                }
                result => return result,
            }
        }
    }

    pub(super) fn run_tests_once(
        &self,
        req: &TestRunRequest,
        allow_partial_retry: bool,
    ) -> Result<TestRunResponse> {
        let _fg_guard = ForegroundRunGuard::enter();
        let started = Instant::now();
        struct ScopedRunGuard<'a> {
            root: &'a ManagedRoot,
            cancel_id: Option<String>,
            unscoped_id: Option<u64>,
        }
        impl Drop for ScopedRunGuard<'_> {
            fn drop(&mut self) {
                let mut sc = self
                    .root
                    .scoped_cancels
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if let Some(id) = &self.cancel_id {
                    sc.active.remove(id);
                }
                if let Some(uid) = self.unscoped_id {
                    sc.unscoped_active.remove(&uid);
                }
            }
        }
        let scoped_pgids = Arc::new(Mutex::new(BTreeSet::new()));
        let scoped_cancelled = Arc::new(AtomicBool::new(false));
        let scoped_single_pgid = AtomicI32::new(0);
        // Register cancellation for the ENTIRE request lifecycle — BOTH scoped AND unscoped runs —
        // so resource waits, singleflight background joins, probes, and execution all wake and
        // abort immediately on `cancel_running` without poisoning future unrelated runs!
        let _scoped_guard = if let Some(cid) = req.cancel_id.as_deref() {
            let mut sc = self
                .scoped_cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if sc.pre_cancelled.remove(cid) {
                scoped_cancelled.store(true, Ordering::SeqCst);
                scoped_pgids
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(i32::MIN);
            } else {
                sc.active.insert(
                    cid.to_string(),
                    ScopedRunHandle {
                        pgids: Arc::clone(&scoped_pgids),
                        cancelled: Arc::clone(&scoped_cancelled),
                    },
                );
            }
            drop(sc);
            ScopedRunGuard {
                root: self,
                cancel_id: Some(cid.to_string()),
                unscoped_id: None,
            }
        } else {
            let mut sc = self
                .scoped_cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            sc.next_unscoped_id = sc.next_unscoped_id.wrapping_add(1);
            let uid = sc.next_unscoped_id;
            sc.unscoped_active.insert(
                uid,
                ScopedRunHandle {
                    pgids: Arc::clone(&scoped_pgids),
                    cancelled: Arc::clone(&scoped_cancelled),
                },
            );
            drop(sc);
            ScopedRunGuard {
                root: self,
                cancel_id: None,
                unscoped_id: Some(uid),
            }
        };
        if scoped_cancelled.load(Ordering::Acquire) {
            return Err(invalid("TEST_CANCELLED: request cancelled before execution").into());
        }
        let has_subfile_filter =
            req.k.is_some() || req.mark.is_some() || req.targets.iter().any(|t| t.contains("::"));
        let explicit_fe_runner = req
            .language_filter
            .as_deref()
            .filter(|l| matches!(*l, "jest" | "vitest"));
        let mut fe_runner_overridden_files: BTreeSet<String> = BTreeSet::new();
        let mut rust_shared_target_files: BTreeSet<String> = BTreeSet::new();
        let learn_invocation = !has_subfile_filter && req.pytest_args.is_empty();

        let mut _fg_resource_guard: Option<ForegroundResourceGuard<'_>> = None;

        let mut cached_summaries = Vec::new();
        let mut cached_failure_outputs = Vec::new();
        let mut observed_runner_exit_code: Option<i32> = None;
        let mut to_execute = Vec::new();
        let mut whole_file_covered: BTreeSet<String> = BTreeSet::new();
        let mut dep_hashes_for_exec = BTreeMap::new();
        let mut joined_bg_count = 0usize;
        let mut saved_ms = 0u64;
        let spec: Spec;
        let preload_modules: Vec<String>;
        let environment_hash: String;
        let parallel_delta: bool;
        let capture_dynamic_dependencies: bool;
        let mut target_test_files: Vec<String>;
        struct DirectPyMixResult {
            pre_py_stamps: BTreeMap<String, Option<crate::ast_graph::FileStamp>>,
            pre_collected_stamps: BTreeMap<PathBuf, Option<crate::ast_graph::FileStamp>>,
            runtime_dep_stamps: BTreeMap<PathBuf, Option<crate::ast_graph::FileStamp>>,
            passed: usize,
            skipped: usize,
            duration_ms: u64,
            stdout_summary: String,
        }
        let mut direct_py_mix: Option<DirectPyMixResult> = None;
        let mut strip_py_flags_for_remaining = false;
        let ruff_diagnostics: Vec<RuffDiagnostic>;
        let mut uncovered_rs_checks: Vec<(String, Vec<String>)> = Vec::new();
        let mut unified_rs_check_crates: Vec<String> = Vec::new();
        let mut uncovered_crate_fingerprints: BTreeMap<String, String> = BTreeMap::new();
        let mut cached_watermarks: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut cached_node_snapshots: BTreeMap<String, Vec<(String, TestOutcome)>> = BTreeMap::new();
        let mut executed_watermarks: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let file_invalidation_stamp = |g: &RootState, tf: &str| -> (u64, u64) {
            let local_gen = g
                .invalidation_generation
                .max(g.file_invalidation_generations.get(tf).copied().unwrap_or(0));
            (local_gen, g.cache.invalidation_watermark_for(tf))
        };
        let rust_unified_workspace_mode: bool;

        let norm_targets: Vec<String>;
        self.warm_environment_probes_unlocked();
        {
            let mut guard = self.lock_state();
            guard.last_activity = Instant::now();
            self.drain_fs_events_locked(&mut guard);
            norm_targets = normalize_targets_for_root(&guard.workspace.root, &req.targets);
            // Enforce strict workspace containment on every normalized target BEFORE `ruff_fix`
            // or test execution so traversal paths like `../other/test.py` or `repo/../other/test.py`
            // can NEVER modify or target files outside `guard.workspace.root`.
            let canon_root = std::fs::canonicalize(&guard.workspace.root)
                .unwrap_or_else(|_| guard.workspace.root.clone());
            for nt in &norm_targets {
                let fpart = nt.split("::").next().unwrap_or(nt).trim();
                if fpart.is_empty() || fpart == "." {
                    continue;
                }
                let p = Path::new(fpart);
                if p.is_absolute()
                    || p.components().any(|c| {
                        matches!(
                            c,
                            std::path::Component::ParentDir
                                | std::path::Component::RootDir
                                | std::path::Component::Prefix(_)
                        )
                    })
                {
                    return Err(invalid(format!(
                        "Target '{nt}' escapes workspace root '{}'",
                        guard.workspace.root.display()
                    ))
                    .into());
                }
                let joined = guard.workspace.root.join(p);
                if let Ok(canon_target) = std::fs::canonicalize(&joined)
                    && !canon_target.starts_with(&canon_root)
                {
                    return Err(invalid(format!(
                        "Target '{nt}' resolves outside workspace root '{}'",
                        guard.workspace.root.display()
                    ))
                    .into());
                }
            }
            Self::ingest_explicit_paths_locked(&mut guard, &norm_targets);

            if req.ruff_fix {
                let mut fix_set: BTreeSet<String> = BTreeSet::new();
                for raw in &norm_targets {
                    let raw_trimmed = raw
                        .split("::")
                        .next()
                        .unwrap_or(raw)
                        .trim()
                        .replace('\\', "/");
                    let is_root_equiv = raw_trimmed == "." || raw_trimmed == "./";
                    let norm = raw_trimmed
                        .trim_start_matches("./")
                        .trim_end_matches('/');
                    if norm.ends_with(".py")
                        || norm.ends_with(".rs")
                        || crate::ast_graph::is_frontend_source_path(norm)
                    {
                        fix_set.insert(norm.to_string());
                    } else if is_root_equiv || norm == "." {
                        for k in guard.graph.files.keys() {
                            if k.ends_with(".py")
                                || k.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(k)
                            {
                                fix_set.insert(k.clone());
                            }
                        }
                    } else if !norm.is_empty() {
                        let prefix = format!("{norm}/");
                        for k in guard.graph.files.keys() {
                            if (k.ends_with(".py")
                                || k.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(k))
                                && k.starts_with(&prefix)
                            {
                                fix_set.insert(k.clone());
                            }
                        }
                    }
                }
                // Only fall back to git dirty files when no explicit targets were passed
                if norm_targets.is_empty() && fix_set.is_empty() {
                    let root_clone = guard.workspace.root.clone();
                    drop(guard);
                    let raw_dirty = git_dirty_files(&root_clone).unwrap_or_default();
                    guard = self.lock_state();
                    for norm in expand_dirty_paths_for_graph(&guard.graph, &raw_dirty) {
                        if (norm.ends_with(".py")
                            || norm.ends_with(".rs")
                            || crate::ast_graph::is_frontend_source_path(&norm))
                            && guard.graph.files.contains_key(&norm)
                        {
                            fix_set.insert(norm);
                        }
                    }
                }
                // `cargo clippy --fix` mutates an entire Rust crate rather than individual `.rs` files;
                // only invoke crate-wide clippy --fix for crates explicitly targeted as a root or crate
                // directory (never because an unrelated non-Rust file like `app.py` was passed alongside
                // a single `.rs` file), and reconcile every `.rs` file in any fixed crate.
                let authorized_crates = authorized_clippy_fix_crates(
                    &guard.workspace.root,
                    &norm_targets,
                    &guard.graph.files,
                );
                let fix_files: Vec<String> = fix_set
                    .into_iter()
                    .filter(|f| {
                        !f.ends_with(".rs")
                            || authorized_crates.contains(&crate::ast_graph::rust_crate_dir(f))
                    })
                    .collect();
                if !fix_files.is_empty() {
                    let touched_crates: BTreeSet<String> = fix_files
                        .iter()
                        .filter(|f| f.ends_with(".rs"))
                        .map(|f| crate::ast_graph::rust_crate_dir(f))
                        .collect();
                    let mut reconcile_targets: BTreeSet<String> = fix_files.iter().cloned().collect();
                    if !touched_crates.is_empty() {
                        for k in guard.graph.files.keys() {
                            if k.ends_with(".rs")
                                && touched_crates.contains(&crate::ast_graph::rust_crate_dir(k))
                            {
                                reconcile_targets.insert(k.clone());
                            }
                        }
                    }
                    let reconcile_vec: Vec<String> = reconcile_targets.into_iter().collect();
                    let spec_for_fix = guard.spec.clone();
                    drop(guard);
                    if scoped_cancelled.load(Ordering::Acquire) {
                        return Err(
                            invalid("TEST_CANCELLED: request cancelled before ruff_fix").into(),
                        );
                    }
                    let _ = execute_ruff_check_tracked(
                        &spec_for_fix,
                        &fix_files,
                        true,
                        Some(&scoped_pgids),
                    );
                    if scoped_cancelled.load(Ordering::Acquire) {
                        return Err(
                            invalid("TEST_CANCELLED: request cancelled during ruff_fix").into(),
                        );
                    }
                    guard = self.lock_state();
                    Self::reconcile_live_files_locked(&mut guard, Some(&reconcile_vec));
                    for f in &reconcile_vec {
                        guard.ruff_cache.remove(f);
                    }
                }
            }

            // 1. Resolve target test files (and reconcile live files on disk).
            // Always honor the full-suite contract when `targets` is omitted: do not silently
            // narrow targetless runs to a git subset (which could hide failures elsewhere in the
            // workspace); agents wanting affected-only execution pass modified source paths in `targets`.
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            let effective_targets = norm_targets.clone();
            let initial_targets = guard.graph.resolve_target_test_files(&effective_targets);
            let structure_changed =
                Self::reconcile_live_files_locked(&mut guard, Some(&initial_targets));
            let mut resolved_targets = if structure_changed {
                guard.graph.resolve_target_test_files(&effective_targets)
            } else {
                initial_targets
            };
            if let Some(ref lang) = req.language_filter {
                match lang.as_str() {
                    "python" => resolved_targets.retain(|tf| tf.ends_with(".py")),
                    "rust" => resolved_targets.retain(|tf| tf.ends_with(".rs")),
                    "frontend" | "vitest" | "jest" | "node" => resolved_targets
                        .retain(|tf| crate::ast_graph::is_frontend_source_path(tf)),
                    _ => {}
                }
            }
            if let Some(ref ct) = req.cargo_target {
                let root_ref = &guard.spec.root;
                resolved_targets.retain(|tf| {
                    tf.ends_with(".rs") && rust_file_matches_explicit_cargo_target(root_ref, tf, ct)
                });
            }
            for tf in &resolved_targets {
                if tf.ends_with(".rs") && rust_file_is_shared_across_targets(&guard.spec.root, tf) {
                    rust_shared_target_files.insert(tf.clone());
                }
            }
            if let Some(runner) = explicit_fe_runner {
                for tf in &resolved_targets {
                    if crate::ast_graph::is_frontend_source_path(tf) {
                        let pkg_dir = frontend_workdir_key(&guard.spec.root, tf);
                        let (pkg_cmd, _) = crate::catalog::detect_frontend_runner_for_dir(
                            &guard.spec.root,
                            &pkg_dir,
                            guard.spec.managed_parent.as_deref(),
                        );
                        let pkg_matches = pkg_cmd.iter().any(|a| a.contains(runner));
                        if !pkg_matches {
                            fe_runner_overridden_files.insert(tf.clone());
                        }
                    }
                }
            }
            target_test_files = resolved_targets;
            spec = guard.spec.clone();
            if let Some(ref part) = req.env_partition
                && guard.env_partition.as_deref() != Some(part.as_str())
            {
                guard.env_partition = Some(part.clone());
                let p = Some(part.as_str());
                let env_h = partition_env_hash(&guard.graph.environment_hash, p);
                let non_py_h = partition_env_hash(&guard.graph.non_python_environment_hash, p);
                let fe_h = partition_env_hash(&guard.graph.frontend_environment_hash, p);
                guard
                    .graph
                    .apply_all_environment_hashes(env_h, non_py_h, fe_h);
            }
            environment_hash = guard.graph.environment_hash.clone();
            parallel_delta = guard.spec.policy.parallel_delta_enabled
                && target_test_files.len() > 1
                && !has_serial_frontend_flag(&req.pytest_args)
                && !target_test_files.iter().any(|tf| tf.ends_with(".rs"))
                && target_test_files.iter().all(|test_file| {
                    guard.graph.files.get(test_file).is_some_and(|node| {
                        node.parsed
                            .module_markers
                            .iter()
                            .any(|marker| marker == "parallel_safe")
                    })
                });
            // Always capture dynamic dependencies whenever Python tests execute (including forced
            // `no_cache` / `rerun_failed` runs) so newly added runtime reads never miss tracking.
            capture_dynamic_dependencies = true;

            if target_test_files.is_empty() {
                // Before returning `no_tests`, check if this request targets any testless Cargo
                // crate(s) (e.g. `targets: []` in a crate with 0 `#[test]` functions, or
                // `targets: ["crates/testless"]` / `targets: ["crates"]`): those crates may still
                // have compile errors, doctests, or `harness = false` targets and MUST run Cargo!
                let has_testless_cargo_targets = has_testless_cargo_targets_for_request(
                    &guard.spec.root,
                    req.language_filter.as_deref(),
                    req.cargo_target.as_deref(),
                    &norm_targets,
                );
                let py_probe_targets: Vec<String> = norm_targets
                    .iter()
                    .filter(|t| !is_explicitly_non_python_target(&guard.spec.root, &guard.graph, t))
                    .cloned()
                    .collect();
                let all_targets_explicitly_non_py =
                    !norm_targets.is_empty() && py_probe_targets.is_empty();
                let has_py_context = !guard.graph.conftest_paths.is_empty()
                    || guard.graph.files.keys().any(|p| p.ends_with(".py"))
                    || guard.spec.root.join("pyproject.toml").is_file()
                    || guard.spec.root.join("pytest.ini").is_file();
                let effective_py_cmd_for_probe = spec.effective_pytest_cmd();
                let all_norm_targets_exist = norm_targets.iter().all(|t| {
                    let clean = t
                        .split("::")
                        .next()
                        .unwrap_or(t)
                        .trim()
                        .trim_start_matches("./");
                    clean.is_empty() || guard.spec.root.join(clean).exists()
                });
                let should_probe_empty_pytest = !all_targets_explicitly_non_py
                    && match req.language_filter.as_deref() {
                        Some("python") => {
                            !req.pytest_args.is_empty()
                                || req.k.is_some()
                                || req.mark.is_some()
                                || (!guard.graph.conftest_paths.is_empty() && all_norm_targets_exist)
                                || has_pytest_collection_override(
                                    &req.pytest_args,
                                    &effective_py_cmd_for_probe,
                                )
                        }
                        None => {
                            (!guard.graph.conftest_paths.is_empty() && all_norm_targets_exist)
                                || has_pytest_collection_override(
                                    &req.pytest_args,
                                    &effective_py_cmd_for_probe,
                                )
                                || (has_py_context
                                    && (!req.pytest_args.is_empty()
                                        || req.k.is_some()
                                        || req.mark.is_some()))
                        }
                        _ => false,
                    };
                if should_probe_empty_pytest {
                    // Empty Python suites (targeted OR targetless, CLI OR MCP — even alongside a
                    // testless Cargo crate!) invoke a structured pytest collection probe with lock
                    // released, exact `PYTHONPATH`, configured timeout, BOTH scoped & unscoped
                    // cancellation, atomic resource reservation, and post-run reconciliation!
                    let root_clone = guard.workspace.root.clone();
                    let py_cmd = spec.effective_pytest_cmd();
                    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs.max(1));
                    let is_info_or_collect_only =
                        is_pytest_info_or_collect_only_flag_effective(&req.pytest_args, &py_cmd);
                    let start_gen = guard.generation;
                    let pre_py_stamps: BTreeMap<String, Option<crate::ast_graph::FileStamp>> =
                        guard
                            .graph
                            .files
                            .iter()
                            .filter(|(p, n)| p.ends_with(".py") || n.is_config)
                            .map(|(p, n)| (p.clone(), n.stamp))
                            .collect();
                    let exec_pgids_ref: &Mutex<BTreeSet<i32>> = &scoped_pgids;
                    let exec_single_pgid_ref: &AtomicI32 = &scoped_single_pgid;
                    if !py_cmd.is_empty() {
                        drop(guard);
                        let pythonpath =
                            build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH"));
                        let mut probe_args: Vec<String> = Vec::new();
                        if let Some(ref kv) = req.k {
                            probe_args.push("-k".to_string());
                            probe_args.push(kv.clone());
                        }
                        if let Some(ref mv) = req.mark {
                            probe_args.push("-m".to_string());
                            probe_args.push(mv.clone());
                        }
                        probe_args.extend(req.pytest_args.iter().cloned());
                        probe_args.extend(py_probe_targets.iter().cloned());
                        if is_info_or_collect_only {
                            // Run informational/setup-plan/collect-only requests directly WITHOUT
                            // forcing `--collect-only` so `--setup-plan` emits the actual plan!
                            let empty_res = BTreeSet::new();
                            let erun = run_direct_pytest_structured(
                                &spec.root,
                                &py_cmd,
                                &pythonpath,
                                &probe_args,
                                true,
                                &empty_res,
                                timeout,
                                exec_single_pgid_ref,
                                exec_pgids_ref,
                            )?;
                            if scoped_cancelled.load(Ordering::Acquire) || erun.cancelled {
                                return Err(invalid(
                                    "TEST_CANCELLED: request cancelled during empty-suite pytest info request",
                                )
                                .into());
                            }
                            return Ok(TestRunResponse {
                                root: root_clone,
                                status: if erun.success {
                                    "passed".to_string()
                                } else if erun.exit_code == 5 {
                                    "no_tests".to_string()
                                } else {
                                    "failed".to_string()
                                },
                                mode: "pytest_info".to_string(),
                                total_test_files: 0,
                                cached_files_count: 0,
                                joined_background_files_count: 0,
                                executed_files_count: 0,
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: usize::from(!erun.success && erun.exit_code != 5),
                                duration_ms: erun.duration_ms,
                                saved_ms: 0,
                                cached_files: Vec::new(),
                                executed_files: Vec::new(),
                                failed_nodeids: Vec::new(),
                                ruff_diagnostics: Vec::new(),
                                structured_failures: Vec::new(),
                                suspected_culprit_files: Vec::new(),
                                untested_modified_files: Vec::new(),
                                flaky_tests: Vec::new(),
                                missing_prerequisites: Vec::new(),
                                warnings_summary: Vec::new(),
                                runner_exit_code: Some(if erun.timed_out { 1 } else { erun.exit_code }),
                                stdout_summary: truncate_str(&erun.combined_output, 8192),
                            });
                        }
                        let probe = probe_pytest_collection_structured(
                            &spec.root,
                            &py_cmd,
                            &pythonpath,
                            &probe_args,
                            timeout,
                            exec_single_pgid_ref,
                            exec_pgids_ref,
                        )?;
                        if scoped_cancelled.load(Ordering::Acquire) || probe.cancelled {
                            return Err(invalid(
                                "TEST_CANCELLED: request cancelled during empty-suite pytest probe",
                            )
                            .into());
                        }
                        if probe.exit_code == 0 || (probe.exit_code == 5 && !is_info_or_collect_only) {
                            if probe.exit_code == 0 && is_info_or_collect_only {
                                return Ok(TestRunResponse {
                                    root: root_clone,
                                    status: "passed".to_string(),
                                    mode: "pytest_info".to_string(),
                                    total_test_files: 0,
                                    cached_files_count: 0,
                                    joined_background_files_count: 0,
                                    executed_files_count: 0,
                                    passed: 0,
                                    failed: 0,
                                    skipped: 0,
                                    errors: 0,
                                    duration_ms: probe.duration_ms,
                                    saved_ms: 0,
                                    cached_files: Vec::new(),
                                    executed_files: Vec::new(),
                                    failed_nodeids: Vec::new(),
                                    ruff_diagnostics: Vec::new(),
                                    structured_failures: Vec::new(),
                                    suspected_culprit_files: Vec::new(),
                                    untested_modified_files: Vec::new(),
                                    flaky_tests: Vec::new(),
                                    missing_prerequisites: Vec::new(),
                                    warnings_summary: Vec::new(),
                                    runner_exit_code: Some(0),
                                    stdout_summary: probe.combined_output,
                                });
                            }
                            let (collected_files, collected_markers) = if probe.exit_code == 0 {
                                let Some(meta) = probe.collected_meta else {
                                    return Err(invalid(
                                        "PYTEST_COLLECTION_UNVERIFIABLE: structured pytest collection output was truncated or unavailable",
                                    )
                                    .into());
                                };
                                meta
                            } else {
                                (Vec::new(), BTreeSet::new())
                            };
                            // Reserve resources atomically for any markers discovered on newly
                            // collected tests (plus a conservative serial lock) before execution!
                            reserve_combined_pytest_resources(
                                self,
                                &collected_markers,
                                &mut _fg_resource_guard,
                                true,
                                &scoped_cancelled,
                            )?;
                            if scoped_cancelled.load(Ordering::Acquire) {
                                return Err(invalid(
                                    "TEST_CANCELLED: request cancelled before direct pytest execution",
                                )
                                .into());
                            }
                            let pre_collected_stamps: BTreeMap<
                                PathBuf,
                                Option<crate::ast_graph::FileStamp>,
                            > = collected_files
                                .iter()
                                .map(|p| (p.clone(), crate::ast_graph::file_stamp(p)))
                                .collect();
                            let erun = run_direct_pytest_with_resource_retry(
                                self,
                                &spec.root,
                                &py_cmd,
                                &pythonpath,
                                &probe_args,
                                false,
                                timeout,
                                exec_single_pgid_ref,
                                exec_pgids_ref,
                                &mut _fg_resource_guard,
                                &scoped_cancelled,
                            )?;
                            if scoped_cancelled.load(Ordering::Acquire) || erun.cancelled {
                                return Err(invalid(
                                    "TEST_CANCELLED: request cancelled during direct pytest execution",
                                )
                                .into());
                            }
                            let ecode = erun.exit_code;
                            let combined_e = erun.combined_output;
                            let ok_e = erun.success;
                            if !ok_e && ecode == 5 {
                                // Execution confirmed 0 tests collected: fall through to testless
                                // Cargo (if present) or standard `no_tests` response!
                                if has_testless_cargo_targets {
                                    strip_py_flags_for_remaining = true;
                                }
                            } else {
                            let (exec_passed, exec_failed, exec_skipped, exec_errors, runtime_dep_stamps) =
                                if let Some(meta) = erun.exec_meta {
                                    if ok_e && meta.mutated_during_run {
                                        return Err(invalid(
                                            "SOURCE_CHANGED_DURING_TEST: runtime dependency modified while direct pytest ran; retry once edits settle",
                                        )
                                        .into());
                                    }
                                    (
                                        meta.passed,
                                        meta.failed,
                                        meta.skipped,
                                        meta.errors,
                                        meta.pre_stamps,
                                    )
                                } else if ok_e {
                                    return Err(invalid(
                                        "PYTEST_EXEC_META_UNVERIFIABLE: direct pytest execution metadata was unavailable",
                                    )
                                    .into());
                                } else {
                                    (0, usize::from(ecode != 5), 0, 0, BTreeMap::new())
                                };
                            if ok_e {
                                let mut post_guard = self.lock_state();
                                self.drain_fs_events_locked(&mut post_guard);
                                let check_files: Vec<String> =
                                    pre_py_stamps.keys().cloned().collect();
                                Self::reconcile_live_files_inner_locked(
                                    &mut post_guard,
                                    Some(&check_files),
                                    false,
                                );
                                let collected_mutated = pre_collected_stamps
                                    .iter()
                                    .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
                                let runtime_mutated = runtime_dep_stamps
                                    .iter()
                                    .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
                                let mutated = collected_mutated
                                    || runtime_mutated
                                    || post_guard.generation != start_gen
                                    || pre_py_stamps.iter().any(|(p, st)| {
                                        crate::ast_graph::file_stamp(
                                            &post_guard.workspace.root.join(p),
                                        ) != *st
                                    });
                                if mutated {
                                    return Err(invalid(
                                        "SOURCE_CHANGED_DURING_TEST: source modified while direct pytest ran; retry once edits settle",
                                    )
                                    .into());
                                }
                            }
                            if !ok_e || !has_testless_cargo_targets {
                                return Ok(TestRunResponse {
                                    root: root_clone,
                                    status: if ok_e {
                                        "passed".to_string()
                                    } else {
                                        "failed".to_string()
                                    },
                                    mode: "direct_pytest_fallback".to_string(),
                                    total_test_files: 0,
                                    cached_files_count: 0,
                                    joined_background_files_count: 0,
                                    executed_files_count: usize::from(ok_e),
                                    passed: exec_passed,
                                    failed: exec_failed,
                                    skipped: exec_skipped,
                                    errors: exec_errors,
                                    duration_ms: erun.duration_ms,
                                    saved_ms: 0,
                                    cached_files: Vec::new(),
                                    executed_files: Vec::new(),
                                    failed_nodeids: Vec::new(),
                                    ruff_diagnostics: Vec::new(),
                                    structured_failures: Vec::new(),
                                    suspected_culprit_files: Vec::new(),
                                    untested_modified_files: Vec::new(),
                                    flaky_tests: Vec::new(),
                                    missing_prerequisites: Vec::new(),
                                    warnings_summary: Vec::new(),
                                    runner_exit_code: Some(if erun.timed_out { 1 } else { ecode }),
                                    stdout_summary: truncate_str(&combined_e, 8192),
                                });
                            }
                            // Python passed and testless Cargo targets also need to run: retain
                            // Python's stamps (including runtime-imported gitignored modules!)
                            // and exact counts for final revalidation & aggregation!
                            direct_py_mix = Some(DirectPyMixResult {
                                pre_py_stamps,
                                pre_collected_stamps,
                                runtime_dep_stamps,
                                passed: exec_passed,
                                skipped: exec_skipped,
                                duration_ms: erun.duration_ms,
                                stdout_summary: truncate_str(&combined_e, 8192),
                            });
                            strip_py_flags_for_remaining = true;
                            }
                        } else if probe.exit_code != 5 {
                            return Ok(TestRunResponse {
                                root: root_clone,
                                status: "failed".to_string(),
                                mode: "pytest_args_error".to_string(),
                                total_test_files: 0,
                                cached_files_count: 0,
                                joined_background_files_count: 0,
                                executed_files_count: 0,
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: 1,
                                duration_ms: probe.duration_ms,
                                saved_ms: 0,
                                cached_files: Vec::new(),
                                executed_files: Vec::new(),
                                failed_nodeids: Vec::new(),
                                ruff_diagnostics: Vec::new(),
                                structured_failures: Vec::new(),
                                suspected_culprit_files: Vec::new(),
                                untested_modified_files: Vec::new(),
                                flaky_tests: Vec::new(),
                                missing_prerequisites: Vec::new(),
                                warnings_summary: Vec::new(),
                                runner_exit_code: Some(probe.exit_code),
                                stdout_summary: probe.combined_output,
                            });
                        } else if has_testless_cargo_targets {
                            strip_py_flags_for_remaining = true;
                        }
                        guard = self.lock_state();
                    }
                }
                if !has_testless_cargo_targets {
                if !norm_targets.is_empty() {
                    let all_existing_untested_sources = !has_subfile_filter
                        && norm_targets.iter().all(|raw| {
                            let clean = raw
                                .trim()
                                .trim_start_matches("./")
                                .trim_end_matches('/')
                                .replace('\\', "/");
                            if clean.is_empty() {
                                return false;
                            }
                            if guard.graph.files.contains_key(&clean) {
                                return true;
                            }
                            let pfx = format!("{clean}/");
                            guard
                                .graph
                                .files
                                .range(pfx.clone()..)
                                .take_while(|(k, _)| k.starts_with(&pfx))
                                .next()
                                .is_some()
                        });
                    if !all_existing_untested_sources {
                        let suggestions = guard.graph.suggest_closest_targets(&norm_targets);
                        let hint = if suggestions.is_empty() {
                            String::new()
                        } else {
                            format!("; did_you_mean=[{}]", suggestions.join(", "))
                        };
                        return Err(invalid(format!(
                            "NO_TESTS_MATCHED targets={}; check pytest python_files/testpaths or cargo test targets configuration or target spelling{hint}",
                            norm_targets.join(",")
                        ))
                        .into());
                    }
                    let untested = guard.graph.untested_modified_files(&norm_targets);
                    let root_clone = guard.workspace.root.clone();
                    let lint_targets: Vec<String> = guard
                        .graph
                        .files
                        .keys()
                        .filter(|k| {
                            (k.ends_with(".py")
                                || k.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(k))
                                && norm_targets.iter().any(|raw| {
                                    let clean = raw
                                        .trim()
                                        .trim_start_matches("./")
                                        .trim_end_matches('/')
                                        .replace('\\', "/");
                                    k == &&clean || k.starts_with(&format!("{clean}/"))
                                })
                        })
                        .cloned()
                        .collect();
                    drop(guard);
                    if !lint_targets.is_empty() {
                        self.ensure_ruff_cached_unlocked(&lint_targets);
                    }
                    let mut guard = self.lock_state();
                    let diags = Self::collect_ruff_diagnostics_locked(&mut guard, &lint_targets);
                    let mut out_lines = Vec::new();
                    if !diags.is_empty() {
                        out_lines.push(format!(
                            "[ruff] {} pre-flight lint/syntax issue(s) detected:",
                            diags.len()
                        ));
                        for diag in diags.iter().take(10) {
                            let fix_tag = if diag.fixable { " [fixable]" } else { "" };
                            out_lines.push(format!(
                                "  {}:{}:{}: {} {}{}",
                                diag.file, diag.row, diag.column, diag.code, diag.message, fix_tag
                            ));
                        }
                    }
                    out_lines.push(format!(
                        "=== no tests defined yet for {} in {:.3}s ===",
                        norm_targets.join(","),
                        started.elapsed().as_secs_f64()
                    ));
                    let no_tests_exit = if diags.is_empty() { None } else { Some(1) };
                    return Ok(TestRunResponse {
                        root: root_clone,
                        status: if diags.is_empty() {
                            "no_tests".to_string()
                        } else {
                            "failed".to_string()
                        },
                        mode: "no_tests".to_string(),
                        total_test_files: 0,
                        cached_files_count: 0,
                        joined_background_files_count: 0,
                        executed_files_count: 0,
                        passed: 0,
                        failed: 0,
                        skipped: 0,
                        errors: diags.len(),
                        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                        saved_ms: 0,
                        cached_files: Vec::new(),
                        executed_files: Vec::new(),
                        failed_nodeids: Vec::new(),
                        ruff_diagnostics: diags,
                        structured_failures: Vec::new(),
                        suspected_culprit_files: Vec::new(),
                        untested_modified_files: untested,
                        flaky_tests: Vec::new(),
                        missing_prerequisites: Vec::new(),
                        warnings_summary: Vec::new(),
                        runner_exit_code: no_tests_exit,
                        stdout_summary: out_lines.join("\n"),
                    });
                }
                return Ok(TestRunResponse {
                    root: guard.workspace.root.clone(),
                    status: "no_tests".to_string(),
                    mode: "no_tests".to_string(),
                    total_test_files: 0,
                    cached_files_count: 0,
                    joined_background_files_count: 0,
                    executed_files_count: 0,
                    passed: 0,
                    failed: 0,
                    skipped: 0,
                    errors: 0,
                    duration_ms: 0,
                    saved_ms: 0,
                    cached_files: Vec::new(),
                    executed_files: Vec::new(),
                    failed_nodeids: Vec::new(),
                    ruff_diagnostics: Vec::new(),
                    structured_failures: Vec::new(),
                    suspected_culprit_files: Vec::new(),
                    untested_modified_files: Vec::new(),
                    flaky_tests: Vec::new(),
                    missing_prerequisites: Vec::new(),
                    warnings_summary: Vec::new(),
                    runner_exit_code: None,
                    stdout_summary: "=== no tests matched in 0.00s ===".to_string(),
                });
                }
            }

            let requested_resources: BTreeSet<String> = target_test_files
                .iter()
                .flat_map(|test_file| test_resource_keys(&guard.graph, test_file).into_iter())
                .collect();
            let resource_deadline = Instant::now() + Duration::from_secs(30);
            while (!requested_resources.is_disjoint(&guard.background_resources)
                || requested_resources
                    .iter()
                    .any(|r| guard.foreground_resources.contains_key(r)))
                && Instant::now() < resource_deadline
                && !scoped_cancelled.load(Ordering::Acquire)
            {
                guard = self.wait_state_timeout(guard, Duration::from_millis(50));
            }
            if scoped_cancelled.load(Ordering::Acquire) {
                return Err(invalid("TEST_CANCELLED: request cancelled while waiting for resources").into());
            }
            let busy_resources: Vec<String> = requested_resources
                .iter()
                .filter(|r| {
                    guard.background_resources.contains(*r)
                        || guard.foreground_resources.contains_key(*r)
                })
                .cloned()
                .collect();
            if !busy_resources.is_empty() {
                return Err(invalid(format!(
                    "TEST_RESOURCE_BUSY resources={}",
                    busy_resources.join(",")
                ))
                .into());
            }
            if !requested_resources.is_empty() {
                for r in &requested_resources {
                    *guard.foreground_resources.entry(r.clone()).or_insert(0) += 1;
                }
                _fg_resource_guard = Some(ForegroundResourceGuard {
                    root: self,
                    keys: requested_resources.into_iter().collect(),
                });
            }

            // Route collection-changing pytest overrides (`-o python_files=spec_*.py`,
            // `-opython_files=spec_*.py`, `PYTEST_ADDOPTS`, `-c ...`, `--doctest-modules`,
            // `--pyargs`, `--version`, etc.) through pytest's own discovery AFTER acquiring
            // resource reservations — independent of indexed Python test presence (so mixed
            // Rust + gitignored `spec_*.py` workspaces NEVER skip Python!), preserving
            // source-target expansion, checking probe cancellation before executing, reserving
            // resources for newly collected markers, and continuing to remaining Rust/FE/testless
            // Cargo targets when Python passes or exits 5!
            let effective_py_cmd = spec.effective_pytest_cmd();
            let has_py_override =
                has_pytest_collection_override(&req.pytest_args, &effective_py_cmd);
            let has_non_py_target = target_test_files.iter().any(|f| !f.ends_with(".py"));
            let has_testless_cargo_work = has_testless_cargo_targets_for_request(
                &guard.spec.root,
                req.language_filter.as_deref(),
                req.cargo_target.as_deref(),
                &norm_targets,
            );
            let has_non_py_work = has_non_py_target || has_testless_cargo_work;
            if has_py_override
                && direct_py_mix.is_none()
                && req.language_filter.as_deref().is_none_or(|l| l == "python")
            {
                let root_clone = guard.workspace.root.clone();
                let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs.max(1));
                let start_gen = guard.generation;
                let pre_py_stamps: BTreeMap<String, Option<crate::ast_graph::FileStamp>> = guard
                    .graph
                    .files
                    .iter()
                    .filter(|(p, n)| p.ends_with(".py") || n.is_config)
                    .map(|(p, n)| (p.clone(), n.stamp))
                    .collect();
                // Preserve source-target expansion (e.g. `src/app.py` with `-o log_cli=true` runs
                // its dependent tests!) while passing suite/directory/test targets verbatim to pytest!
                let mut direct_py_targets: Vec<String> = Vec::new();
                if !norm_targets.is_empty() {
                    let mut seen_dt: BTreeSet<String> = BTreeSet::new();
                    for nt in &norm_targets {
                        let clean = nt
                            .split("::")
                            .next()
                            .unwrap_or(nt)
                            .trim()
                            .trim_start_matches("./")
                            .trim_end_matches('/')
                            .replace('\\', "/");
                        if guard
                            .graph
                            .files
                            .get(&clean)
                            .is_some_and(|n| !n.is_test && clean.ends_with(".py"))
                        {
                            let mut has_py_deps = false;
                            for dep_t in
                                guard.graph.resolve_target_test_files(std::slice::from_ref(nt))
                            {
                                if dep_t.ends_with(".py") {
                                    has_py_deps = true;
                                    if seen_dt.insert(dep_t.clone()) {
                                        direct_py_targets.push(dep_t);
                                    }
                                }
                            }
                            // Track dependency presence separately from deduplication so two
                            // overlapping source targets sharing a test NEVER pass the second
                            // source file to pytest!
                            if (!has_py_deps || nt.contains("::")) && seen_dt.insert(nt.clone()) {
                                direct_py_targets.push(nt.clone());
                            }
                        } else if !is_explicitly_non_python_target(
                            &guard.spec.root,
                            &guard.graph,
                            nt,
                        ) && seen_dt.insert(nt.clone())
                        {
                            direct_py_targets.push(nt.clone());
                        }
                    }
                }
                // When explicit targets were supplied and NONE of them are Python targets (e.g.
                // `targets: ["crates/foo/src/lib.rs"]` or `targets: ["pkg/foo.go"]` with a pytest
                // override), do NOT pass an empty target list to pytest!
                let should_run_direct_py = norm_targets.is_empty() || !direct_py_targets.is_empty();
                if !should_run_direct_py {
                    strip_py_flags_for_remaining = true;
                } else {
                let exec_pgids_ref: &Mutex<BTreeSet<i32>> = &scoped_pgids;
                let exec_single_pgid_ref: &AtomicI32 = &scoped_single_pgid;
                if !effective_py_cmd.is_empty() {
                    drop(guard);
                    let pythonpath = build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH"));
                    let mut direct_args: Vec<String> = Vec::new();
                    if let Some(ref kv) = req.k {
                        direct_args.push("-k".to_string());
                        direct_args.push(kv.clone());
                    }
                    if let Some(ref mv) = req.mark {
                        direct_args.push("-m".to_string());
                        direct_args.push(mv.clone());
                    }
                    direct_args.extend(req.pytest_args.iter().cloned());
                    direct_args.extend(direct_py_targets.iter().cloned());
                    let is_info_only = is_pytest_info_or_collect_only_flag_effective(
                        &req.pytest_args,
                        &effective_py_cmd,
                    );
                    let mut pre_collected_stamps: BTreeMap<
                        PathBuf,
                        Option<crate::ast_graph::FileStamp>,
                    > = BTreeMap::new();
                    if !is_info_only {
                        let probe = probe_pytest_collection_structured(
                            &spec.root,
                            &effective_py_cmd,
                            &pythonpath,
                            &direct_args,
                            timeout,
                            exec_single_pgid_ref,
                            exec_pgids_ref,
                        )?;
                        if scoped_cancelled.load(Ordering::Acquire) || probe.cancelled {
                            return Err(invalid(
                                "TEST_CANCELLED: request cancelled during pytest collection probe",
                            )
                            .into());
                        }
                        if probe.exit_code == 5 {
                            // Do NOT treat an empty `--collect-only` probe as proof that execution
                            // would be empty (a plugin may filter items only under `--collect-only`!):
                            // still reserve the serial lock and execute via
                            // `run_direct_pytest_with_resource_retry`, which validates actual
                            // execution-time markers before running tests and exits 5 if truly empty!
                            let empty_markers: BTreeSet<String> = BTreeSet::new();
                            reserve_combined_pytest_resources(
                                self,
                                &empty_markers,
                                &mut _fg_resource_guard,
                                true,
                                &scoped_cancelled,
                            )?;
                            if scoped_cancelled.load(Ordering::Acquire) {
                                return Err(invalid(
                                    "TEST_CANCELLED: request cancelled before direct pytest execution",
                                )
                                .into());
                            }
                        } else if probe.exit_code != 0 {
                            return Ok(TestRunResponse {
                                root: root_clone,
                                status: "failed".to_string(),
                                mode: "direct_pytest_override".to_string(),
                                total_test_files: 0,
                                cached_files_count: 0,
                                joined_background_files_count: 0,
                                executed_files_count: 0,
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: 1,
                                duration_ms: probe.duration_ms,
                                saved_ms: 0,
                                cached_files: Vec::new(),
                                executed_files: Vec::new(),
                                failed_nodeids: Vec::new(),
                                ruff_diagnostics: Vec::new(),
                                structured_failures: Vec::new(),
                                suspected_culprit_files: Vec::new(),
                                untested_modified_files: Vec::new(),
                                flaky_tests: Vec::new(),
                                missing_prerequisites: Vec::new(),
                                warnings_summary: Vec::new(),
                                runner_exit_code: Some(probe.exit_code),
                                stdout_summary: truncate_str(&probe.combined_output, 8192),
                            });
                        } else {
                            let Some((collected_files, collected_markers)) = probe.collected_meta
                            else {
                                return Err(invalid(
                                    "PYTEST_COLLECTION_UNVERIFIABLE: structured pytest collection output was truncated or unavailable",
                                )
                                .into());
                            };
                            // Atomically acquire the complete combined resource set (releasing any
                            // previously held indexed resources first) so AB-BA deadlocks are
                            // impossible!
                            reserve_combined_pytest_resources(
                                self,
                                &collected_markers,
                                &mut _fg_resource_guard,
                                true,
                                &scoped_cancelled,
                            )?;
                            if scoped_cancelled.load(Ordering::Acquire) {
                                return Err(invalid(
                                    "TEST_CANCELLED: request cancelled before direct pytest execution",
                                )
                                .into());
                            }
                            for p in &collected_files {
                                pre_collected_stamps
                                    .insert(p.clone(), crate::ast_graph::file_stamp(p));
                            }
                        }
                    }
                    {
                        let erun = run_direct_pytest_with_resource_retry(
                            self,
                            &spec.root,
                            &effective_py_cmd,
                            &pythonpath,
                            &direct_args,
                            is_info_only,
                            timeout,
                            exec_single_pgid_ref,
                            exec_pgids_ref,
                            &mut _fg_resource_guard,
                            &scoped_cancelled,
                        )?;
                        if scoped_cancelled.load(Ordering::Acquire) || erun.cancelled {
                            return Err(invalid(
                                "TEST_CANCELLED: request cancelled during direct pytest execution",
                            )
                            .into());
                        }
                        let ecode = erun.exit_code;
                        let combined_e = erun.combined_output;
                        let ok_e = erun.success;
                        let (exec_passed, exec_failed, exec_skipped, exec_errors, runtime_dep_stamps) =
                            if is_info_only {
                                (0, usize::from(!ok_e), 0, 0, BTreeMap::new())
                            } else if let Some(meta) = erun.exec_meta {
                                if ok_e && meta.mutated_during_run {
                                    return Err(invalid(
                                        "SOURCE_CHANGED_DURING_TEST: runtime dependency modified while direct pytest ran; retry once edits settle",
                                    )
                                    .into());
                                }
                                (
                                    meta.passed,
                                    meta.failed,
                                    meta.skipped,
                                    meta.errors,
                                    meta.pre_stamps,
                                )
                            } else if ok_e {
                                return Err(invalid(
                                    "PYTEST_EXEC_META_UNVERIFIABLE: direct pytest execution metadata was unavailable",
                                )
                                .into());
                            } else {
                                (0, usize::from(ecode != 5), 0, 0, BTreeMap::new())
                            };
                        if ok_e && !is_info_only {
                            let mut post_guard = self.lock_state();
                            self.drain_fs_events_locked(&mut post_guard);
                            let check_files: Vec<String> =
                                pre_py_stamps.keys().cloned().collect();
                            Self::reconcile_live_files_inner_locked(
                                &mut post_guard,
                                Some(&check_files),
                                false,
                            );
                            let collected_mutated = pre_collected_stamps
                                .iter()
                                .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
                            let runtime_mutated = runtime_dep_stamps
                                .iter()
                                .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
                            let mutated = collected_mutated
                                || runtime_mutated
                                || post_guard.generation != start_gen
                                || pre_py_stamps.iter().any(|(p, st)| {
                                    crate::ast_graph::file_stamp(
                                        &post_guard.workspace.root.join(p),
                                    ) != *st
                                });
                            if mutated {
                                return Err(invalid(
                                    "SOURCE_CHANGED_DURING_TEST: source modified while direct pytest ran; retry once edits settle",
                                )
                                .into());
                            }
                        }
                        if !has_non_py_work || (!ok_e && ecode != 5) || is_info_only {
                            return Ok(TestRunResponse {
                                root: root_clone,
                                status: if ok_e {
                                    "passed".to_string()
                                } else if ecode == 5 {
                                    "no_tests".to_string()
                                } else {
                                    "failed".to_string()
                                },
                                mode: "direct_pytest_override".to_string(),
                                total_test_files: 0,
                                cached_files_count: 0,
                                joined_background_files_count: 0,
                                executed_files_count: usize::from(ok_e && !is_info_only),
                                passed: exec_passed,
                                failed: exec_failed,
                                skipped: exec_skipped,
                                errors: exec_errors,
                                duration_ms: erun.duration_ms,
                                saved_ms: 0,
                                cached_files: Vec::new(),
                                executed_files: Vec::new(),
                                failed_nodeids: Vec::new(),
                                ruff_diagnostics: Vec::new(),
                                structured_failures: Vec::new(),
                                suspected_culprit_files: Vec::new(),
                                untested_modified_files: Vec::new(),
                                flaky_tests: Vec::new(),
                                missing_prerequisites: Vec::new(),
                                warnings_summary: Vec::new(),
                                runner_exit_code: Some(if erun.timed_out { 1 } else { ecode }),
                                stdout_summary: truncate_str(&combined_e, 8192),
                            });
                        }
                        // Mixed Python + non-Python (or testless Cargo) suite where Python passed
                        // or collected 0 tests: retain Python stamps (including runtime-imported
                        // gitignored modules!) & exact counts for final revalidation/aggregation!
                        if ok_e {
                            direct_py_mix = Some(DirectPyMixResult {
                                pre_py_stamps,
                                pre_collected_stamps,
                                runtime_dep_stamps,
                                passed: exec_passed,
                                skipped: exec_skipped,
                                duration_ms: erun.duration_ms,
                                stdout_summary: truncate_str(&combined_e, 8192),
                            });
                        }
                        guard = self.lock_state();
                        target_test_files.retain(|f| !f.ends_with(".py"));
                        strip_py_flags_for_remaining = true;
                    }
                }
                }
            }

            // 2. If any targeted test file is currently in flight in the background worker for the exact current dep_hash,
            // wait briefly (singleflight join) so we don't spawn a duplicate test process!
            if !req.no_cache {
                let wait_deadline = Instant::now() + Duration::from_secs(30);
                let target_flight_keys: Vec<(String, String)> = target_test_files
                    .iter()
                    .filter_map(|tf| {
                        guard
                            .graph
                            .dep_hash_for_test_or_explicit(tf)
                            .map(|dh| (tf.clone(), flight_key(tf, &dh)))
                    })
                    .collect();
                let mut joined_files: BTreeSet<String> = BTreeSet::new();
                loop {
                    if scoped_cancelled.load(Ordering::Acquire) {
                        return Err(
                            invalid("TEST_CANCELLED: request cancelled while joining background run")
                                .into(),
                        );
                    }
                    let mut any_in_flight = false;
                    for (tf, fk) in &target_flight_keys {
                        if guard.in_flight.contains(fk) {
                            any_in_flight = true;
                            joined_files.insert(tf.clone());
                        }
                    }
                    if !any_in_flight || Instant::now() >= wait_deadline {
                        break;
                    }
                    let remaining = wait_deadline.saturating_duration_since(Instant::now());
                    guard =
                        self.wait_state_timeout(guard, remaining.min(Duration::from_millis(500)));
                }
                joined_bg_count = joined_files.len();
            }
            if scoped_cancelled.load(Ordering::Acquire) {
                return Err(invalid("TEST_CANCELLED: request cancelled before returning results").into());
            }
            // If another process updated/invalidated the on-disk WAL/snapshot while we waited on
            // resources or singleflight join, refresh the cache BEFORE serving any cache hits.
            if guard.cache.disk_changed_since_sync() {
                drop(guard);
                self.refresh_cache_from_disk_if_changed();
                guard = self.lock_state();
            }
            // If we released the state lock while waiting on resources or singleflight join,
            // drain queued watcher events and reconcile live files before serving cached results.
            self.drain_fs_events_locked(&mut guard);
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            Self::reconcile_live_files_locked(&mut guard, Some(&target_test_files));

            // Determine which resolved test files are covered as whole files (by a directory,
            // source file, or full test file path) so overlapping targets like
            // `["tests", "tests/test_a.py::test_one"]` never narrow `test_a.py` to just `test_one`.
            let non_nodeid_targets: Vec<String> = norm_targets
                .iter()
                .filter(|t| {
                    !crate::ast_graph::split_target_file_and_node_in_files(t, &guard.graph.files).1
                })
                .cloned()
                .collect();
            if !non_nodeid_targets.is_empty() {
                whole_file_covered.extend(
                    guard
                        .graph
                        .resolve_target_test_files(&non_nodeid_targets),
                );
            }

            // Detect whether this request spans >= 2 Cargo workspace crates with unified features
            // BEFORE cache partitioning (including testless selected packages from `norm_targets`
            // or full-workspace runs!) so isolated per-crate cached passes NEVER short-circuit a
            // unified multi-package request. Also record any selected testless workspace crates
            // AND uncovered testable targets (e.g., a testless `src/main.rs` or `harness = false`
            // target in a package-scoped run) so compile errors or custom-harness failures are
            // never masked by cached passes!
            rust_unified_workspace_mode = if req
                .language_filter
                .as_deref()
                .is_none_or(|lf| lf == "rust")
            {
                let is_ws = is_cargo_workspace_root(&guard.spec.root);
                let rs_crate_dirs: Vec<String> = if is_ws {
                    requested_workspace_crate_dirs(
                        &guard.spec.root,
                        &norm_targets,
                        &target_test_files,
                    )
                } else if guard.spec.root.join("Cargo.toml").is_file()
                    && (norm_targets.is_empty()
                        || norm_targets
                            .iter()
                            .any(|t| t == "." || t == "./"))
                {
                    vec![".".to_string()]
                } else {
                    Vec::new()
                };
                let covered_rs_files: BTreeSet<String> = target_test_files
                    .iter()
                    .filter(|tf| tf.ends_with(".rs"))
                    .cloned()
                    .collect();
                let tested_crate_dirs: BTreeSet<String> = covered_rs_files
                    .iter()
                    .map(|tf| {
                        let d = rust_crate_dir_on_disk(&guard.spec.root, tf);
                        if d.is_empty() { ".".to_string() } else { d }
                    })
                    .collect();
                if let Some(ct) = req.cargo_target.as_deref() {
                    let targs: Vec<String> = if ct == "lib" {
                        vec!["--lib".to_string()]
                    } else if ct == "bins" {
                        vec!["--bins".to_string()]
                    } else if ct == "doc" {
                        vec!["--doc".to_string()]
                    } else if let Some(b) = ct.strip_prefix("bin:") {
                        vec!["--bin".to_string(), b.to_string()]
                    } else if let Some(t) = ct.strip_prefix("test:") {
                        vec!["--test".to_string(), t.to_string()]
                    } else {
                        Vec::new()
                    };
                    if !targs.is_empty() {
                        for cd in &rs_crate_dirs {
                            if ct != "doc" && tested_crate_dirs.contains(cd) {
                                continue;
                            }
                            let cdir_abs = if cd == "." {
                                guard.spec.root.clone()
                            } else {
                                guard.spec.root.join(cd)
                            };
                            if cargo_manifest_matches_explicit_target(&cdir_abs, ct) {
                                uncovered_rs_checks.push((cd.clone(), targs.clone()));
                            }
                        }
                    }
                } else {
                    let crate_is_pkg_scoped = |cd: &str| -> bool {
                        if norm_targets.is_empty() {
                            return true;
                        }
                        norm_targets.iter().any(|t| {
                            let clean = t
                                .split("::")
                                .next()
                                .unwrap_or(t)
                                .trim()
                                .trim_start_matches("./")
                                .trim_end_matches('/');
                            clean.is_empty()
                                || clean == "."
                                || clean == cd
                                || (cd != "." && cd.starts_with(&format!("{clean}/")))
                        })
                    };
                    for cd in &rs_crate_dirs {
                        let cdir_abs = if cd == "." {
                            guard.spec.root.clone()
                        } else {
                            guard.spec.root.join(cd)
                        };
                        if !tested_crate_dirs.contains(cd) {
                            if cargo_manifest_has_any_testable_target(&cdir_abs) {
                                uncovered_rs_checks.push((cd.clone(), Vec::new()));
                            }
                        } else if crate_is_pkg_scoped(cd) {
                            for missing_targs in collect_uncovered_rust_targets_for_crate(
                                &cdir_abs,
                                cd,
                                &covered_rs_files,
                                &req.pytest_args,
                            ) {
                                uncovered_rs_checks.push((cd.clone(), missing_targs));
                            }
                        }
                    }
                }
                let unified = is_ws
                    && rs_crate_dirs.len() >= 2
                    && workspace_needs_unified_cargo_features(
                        &guard.spec.root,
                        &rs_crate_dirs,
                        &req.pytest_args,
                    );
                if unified {
                    unified_rs_check_crates = rs_crate_dirs
                        .into_iter()
                        .filter(|cd| {
                            let cdir_abs = if cd == "." {
                                guard.spec.root.clone()
                            } else {
                                guard.spec.root.join(cd)
                            };
                            cargo_manifest_has_any_testable_target(&cdir_abs)
                        })
                        .collect();
                }
                // Snapshot live fingerprints for EVERY crate scheduled for uncovered/testless Cargo
                // checks (including all workspace Cargo.toml/Cargo.lock/Rust sources + non-python
                // env hash) so mid-check edits to a custom harness or testless crate NEVER return a
                // stale pass!
                let mut crates_to_fp: BTreeSet<String> = BTreeSet::new();
                for (cd, _) in &uncovered_rs_checks {
                    crates_to_fp.insert(cd.clone());
                }
                for cd in &unified_rs_check_crates {
                    crates_to_fp.insert(cd.clone());
                }
                for cd in crates_to_fp {
                    uncovered_crate_fingerprints.insert(
                        cd.clone(),
                        Self::snapshot_rust_crate_fingerprint(&guard, &cd),
                    );
                }
                unified
            } else {
                false
            };

            // 3. Partition targeted test files into Cached vs To-Execute
            for tf in &target_test_files {
                let Some(dep_hash) = guard.graph.dep_hash_for_test_or_explicit(tf) else {
                    continue;
                };
                let is_unified_rs_bypass = rust_unified_workspace_mode && tf.ends_with(".rs");
                let requested_nodeids: Vec<String> = if whole_file_covered.contains(tf) {
                    Vec::new()
                } else {
                    norm_targets
                        .iter()
                        .filter_map(|t| {
                            let (norm, is_node) =
                                crate::ast_graph::split_target_file_and_node_in_files(
                                    t,
                                    &guard.graph.files,
                                );
                            if is_node && &norm == tf {
                                Some(t.trim_start_matches("./").replace('\\', "/"))
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                let is_node_cache_eligible = guard.graph.files.get(tf).is_some_and(|node| {
                    node.parsed
                        .module_markers
                        .iter()
                        .any(|marker| marker == "node_cache_safe")
                        || ((tf.ends_with(".rs")
                            || crate::ast_graph::is_frontend_source_path(tf))
                            && !node
                                .parsed
                                .markers
                                .iter()
                                .any(|marker| marker == "serial"))
                });
                if !req.no_cache
                    && !is_unified_rs_bypass
                    && !fe_runner_overridden_files.contains(tf)
                    && !rust_shared_target_files.contains(tf)
                    && Self::is_test_cacheable_in_graph(&guard, tf)
                    && req.k.is_none()
                    && req.mark.is_none()
                    && req.pytest_args.is_empty()
                    && !requested_nodeids.is_empty()
                    && is_node_cache_eligible
                    && let Some(record) = guard.cache.get_matching(tf, &dep_hash)
                    && record.dynamic_dependencies_complete
                    && explicit_fe_runner.is_none_or(|r| record.runner_mode == r)
                {
                    // Match EVERY cached node that the runner's selector semantics would execute
                    // (exact match PLUS `::sel` suffix for Rust and ` sel` suffix for Frontend)
                    // so a passing exact match can never hide a failing suffix-matched test!
                    let is_rs_file = tf.ends_with(".rs");
                    let is_vitest_or_jest_rec =
                        matches!(record.runner_mode.as_str(), "vitest" | "jest");
                    let mut matched_nodes: Vec<&CachedNodeResult> = Vec::new();
                    let mut seen_matched: BTreeSet<&str> = BTreeSet::new();
                    let mut all_selectors_matched = true;
                    for nodeid in &requested_nodeids {
                        let sel = nodeid
                            .strip_prefix(&format!("{tf}::"))
                            .unwrap_or(nodeid.as_str());
                        let mut sel_found = false;
                        for (k, v) in &record.node_results {
                            let k_sel = k.strip_prefix(&format!("{tf}::")).unwrap_or(k.as_str());
                            // Match runner-specific execution semantics:
                            // - Rust: exact or `::sel` suffix
                            // - Vitest/Jest (`^(?:.* )?sel$`): exact or ` sel` suffix
                            // - Node (`^sel$`): exact title match ONLY!
                            let matches = k == nodeid
                                || (is_rs_file
                                    && (k_sel == sel || k_sel.ends_with(&format!("::{sel}"))))
                                || (is_vitest_or_jest_rec
                                    && (k_sel == sel || k_sel.ends_with(&format!(" {sel}"))));
                            if matches {
                                sel_found = true;
                                if seen_matched.insert(k.as_str()) {
                                    matched_nodes.push(v);
                                }
                            }
                        }
                        if !sel_found {
                            all_selectors_matched = false;
                            break;
                        }
                    }
                    if is_rs_file {
                        // Deduplicate bare short-name aliases (`test_mul_unit`) PER TEST: only
                        // drop a bare alias when a qualified match ending with `::{alias}` AND
                        // carrying the exact same outcome is already present in `matched_nodes`.
                        // That prevents double-counting `calc::tests::foo` + `foo` without ever
                        // dropping a distinct root-level failing test alongside a nested pass.
                        let prefix = format!("{tf}::");
                        let qualified: Vec<(String, TestOutcome)> = matched_nodes
                            .iter()
                            .filter_map(|n| {
                                let s = n.nodeid.strip_prefix(&prefix)?;
                                s.contains("::").then(|| (s.to_string(), n.outcome))
                            })
                            .collect();
                        if !qualified.is_empty() {
                            matched_nodes.retain(|n| {
                                let Some(s) = n.nodeid.strip_prefix(&prefix) else {
                                    return true;
                                };
                                if s.contains("::") {
                                    return true;
                                }
                                let suffix = format!("::{s}");
                                !qualified
                                    .iter()
                                    .any(|(q, q_out)| q.ends_with(&suffix) && *q_out == n.outcome)
                            });
                        }
                    }
                    if all_selectors_matched
                        && !matched_nodes.is_empty()
                        && ((record.outcome.is_green()
                            && matched_nodes.iter().all(|n| n.outcome.is_green()))
                            || !req.rerun_failed)
                    {
                        let mut passed = 0;
                        let mut failed = 0;
                        let mut skipped = 0;
                        let mut errors = 0;
                        let mut duration_ms = 0;
                        let mut failed_nodeids = Vec::new();
                        let mut node_snap: Vec<(String, TestOutcome)> = Vec::new();
                        for node in matched_nodes {
                            node_snap.push((node.nodeid.clone(), node.outcome));
                            duration_ms += node.duration_ms;
                            match node.outcome {
                                TestOutcome::Passed => passed += 1,
                                TestOutcome::Failed => {
                                    failed += 1;
                                    failed_nodeids.push(node.nodeid.clone());
                                }
                                TestOutcome::Skipped => skipped += 1,
                                TestOutcome::Error => {
                                    errors += 1;
                                    failed_nodeids.push(node.nodeid.clone());
                                }
                            }
                        }
                        saved_ms = saved_ms.saturating_add(duration_ms);
                        cached_watermarks.insert(tf.clone(), file_invalidation_stamp(&guard, tf));
                        cached_node_snapshots.insert(tf.clone(), node_snap);
                        cached_summaries.push(FileRunSummary {
                            test_file: tf.clone(),
                            dep_hash: record.dep_hash.clone(),
                            outcome: if failed > 0 {
                                TestOutcome::Failed
                            } else if errors > 0 {
                                TestOutcome::Error
                            } else if passed > 0 {
                                TestOutcome::Passed
                            } else {
                                TestOutcome::Skipped
                            },
                            source: format!("cached_node({})", record.source),
                            passed,
                            failed,
                            skipped,
                            errors,
                            duration_ms,
                            failed_nodeids,
                        });
                        continue;
                    }
                }
                let can_use_cache = !req.no_cache
                    && !is_unified_rs_bypass
                    && !fe_runner_overridden_files.contains(tf)
                    && !rust_shared_target_files.contains(tf)
                    && Self::is_test_cacheable_in_graph(&guard, tf)
                    && req.k.is_none()
                    && req.mark.is_none()
                    && req.pytest_args.is_empty()
                    && requested_nodeids.is_empty();

                if can_use_cache
                    && let Some(rec) = guard.cache.get_matching(tf, &dep_hash)
                    && rec.dynamic_dependencies_complete
                    && explicit_fe_runner.is_none_or(|r| rec.runner_mode == r)
                    && (rec.outcome.is_green() || (!req.rerun_failed && !has_subfile_filter))
                {
                    saved_ms = saved_ms.saturating_add(rec.duration_ms);
                    if !rec.outcome.is_green() {
                        if let Some(code) = rec.exit_code
                            && code != 0
                        {
                            observed_runner_exit_code.get_or_insert(code);
                        } else if rec.test_file.ends_with(".rs") {
                            observed_runner_exit_code.get_or_insert(101);
                        }
                    }
                    if !rec.outcome.is_green() && !rec.output_snippet.trim().is_empty() {
                        cached_failure_outputs.push(rec.output_snippet.clone());
                    }
                    cached_watermarks.insert(tf.clone(), file_invalidation_stamp(&guard, tf));
                    cached_summaries.push(FileRunSummary {
                        test_file: tf.clone(),
                        dep_hash: rec.dep_hash.clone(),
                        outcome: rec.outcome,
                        source: format!("cached({})", rec.source),
                        passed: rec.passed,
                        failed: rec.failed,
                        skipped: rec.skipped,
                        errors: rec.errors,
                        duration_ms: rec.duration_ms,
                        failed_nodeids: rec.failed_nodeids.clone(),
                    });
                    continue;
                }
                // Remove from background pending queue since we will run it right now
                guard.pending_queue.retain(|p| p != tf);
                guard.in_flight.insert(flight_key(tf, &dep_hash));
                dep_hashes_for_exec.insert(tf.clone(), dep_hash);
                to_execute.push(tf.clone());
            }

            // Prioritize previously failed test files first (Fail-First scheduling), then faster files
            sort_targets_fail_first(&mut to_execute, &guard.cache);

            if !cached_summaries.is_empty() {
                guard.cache.total_cache_hits = guard
                    .cache
                    .total_cache_hits
                    .saturating_add(cached_summaries.len() as u64);
                guard.cache.total_saved_ms = guard.cache.total_saved_ms.saturating_add(saved_ms);
            }
            preload_modules = if to_execute.iter().any(|tf| tf.ends_with(".py")) {
                guard.graph.external_preload_modules_for_tests(&to_execute)
            } else {
                Vec::new()
            };

            let executing_rust = to_execute.iter().any(|tf| tf.ends_with(".rs"));
            let mut ruff_scope: BTreeSet<String> = BTreeSet::new();
            for raw in &norm_targets {
                let norm = raw
                    .split("::")
                    .next()
                    .unwrap_or(raw)
                    .trim()
                    .trim_start_matches("./")
                    .replace('\\', "/");
                if (norm.ends_with(".py")
                    || norm.ends_with(".rs")
                    || crate::ast_graph::is_frontend_source_path(&norm))
                    && guard.graph.files.contains_key(&norm)
                {
                    ruff_scope.insert(norm);
                }
            }
            for tf in &target_test_files {
                if tf.ends_with(".py")
                    || tf.ends_with(".rs")
                    || crate::ast_graph::is_frontend_source_path(tf)
                {
                    ruff_scope.insert(tf.clone());
                }
                if let Some(ids) = guard.graph.transitive_dep_ids_for_test.get(tf) {
                    for &id in ids {
                        if let Some(dep_path) = guard.graph.file_index_to_path.get(id as usize)
                            && (dep_path.ends_with(".py")
                                || dep_path.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(dep_path))
                        {
                            ruff_scope.insert(dep_path.clone());
                        }
                    }
                }
            }
            let to_check_unlocked: Vec<String> = if req.ruff_fix || !guard.pending_ruff_files.is_empty() {
                let pending: Vec<String> = guard
                    .pending_ruff_files
                    .iter()
                    .filter(|f| {
                        ruff_scope.contains(f.as_str()) && (!executing_rust || !f.ends_with(".rs"))
                    })
                    .cloned()
                    .collect();
                for f in &pending {
                    guard.pending_ruff_files.remove(f);
                }
                if req.ruff_fix {
                    ruff_scope.iter().cloned().collect()
                } else {
                    pending
                }
            } else {
                Vec::new()
            };
            let ruff_scope_vec: Vec<String> = ruff_scope.into_iter().collect();
            drop(guard);
            if !to_check_unlocked.is_empty() {
                if scoped_cancelled.load(Ordering::Acquire) {
                    let mut g = self.lock_state();
                    for (tf, dh) in &dep_hashes_for_exec {
                        g.in_flight.remove(&flight_key(tf, dh));
                    }
                    self.condvar.notify_all();
                    return Err(invalid("TEST_CANCELLED: request cancelled before lint").into());
                }
                self.ensure_ruff_cached_unlocked_tracked(&to_check_unlocked, Some(&scoped_pgids));
                if scoped_cancelled.load(Ordering::Acquire) {
                    let mut g = self.lock_state();
                    for (tf, dh) in &dep_hashes_for_exec {
                        g.in_flight.remove(&flight_key(tf, dh));
                    }
                    self.condvar.notify_all();
                    return Err(invalid("TEST_CANCELLED: request cancelled during lint").into());
                }
            }
            if scoped_cancelled.load(Ordering::Acquire) {
                let mut g = self.lock_state();
                for (tf, dh) in &dep_hashes_for_exec {
                    g.in_flight.remove(&flight_key(tf, dh));
                }
                self.condvar.notify_all();
                return Err(invalid("TEST_CANCELLED: request cancelled before returning results").into());
            }
            let mut guard = self.lock_state();
            ruff_diagnostics = Self::collect_ruff_diagnostics_locked(&mut guard, &ruff_scope_vec);
            // If we released the lock to run pre-flight lint AND no tests will execute (cached-only
            // path), revalidate all selected cached summaries before returning so edits made during
            // lint never return stale cached passes.
            if !to_check_unlocked.is_empty() && to_execute.is_empty() && !cached_summaries.is_empty() {
                if guard.cache.disk_changed_since_sync() {
                    drop(guard);
                    self.refresh_cache_from_disk_if_changed();
                    guard = self.lock_state();
                }
                self.drain_fs_events_locked(&mut guard);
                self.maybe_foreground_safety_reconcile_locked(&mut guard);
                Self::reconcile_live_files_locked(&mut guard, Some(&target_test_files));
                let incomplete_after_lint = Self::is_graph_incomplete(&guard);
                let stale_after_lint: Vec<String> = cached_summaries
                    .iter()
                    .filter(|s| {
                        let live_wm = file_invalidation_stamp(&guard, &s.test_file);
                        let prev_wm = cached_watermarks
                            .get(&s.test_file)
                            .copied()
                            .unwrap_or((0, 0));
                        let cache_matches = live_wm == prev_wm
                            && guard
                                .cache
                                .get_matching(&s.test_file, &s.dep_hash)
                                .is_some_and(|rec| {
                                    if s.source.starts_with("cached_node(") {
                                        cached_node_snapshots
                                            .get(&s.test_file)
                                            .is_some_and(|snap| {
                                                snap.iter().all(|(nid, expected_out)| {
                                                    rec.node_results
                                                        .get(nid)
                                                        .is_some_and(|nr| nr.outcome == *expected_out)
                                                })
                                            })
                                    } else {
                                        rec.outcome == s.outcome
                                            && rec.passed == s.passed
                                            && rec.failed == s.failed
                                            && rec.errors == s.errors
                                            && rec.skipped == s.skipped
                                            && rec.failed_nodeids == s.failed_nodeids
                                    }
                                });
                        incomplete_after_lint
                            || guard
                                .graph
                                .dep_hash_for_test_or_explicit(&s.test_file)
                                .as_deref()
                                != Some(s.dep_hash.as_str())
                            || Self::test_touches_unreadable_file(&guard, &s.test_file)
                            || !cache_matches
                    })
                    .map(|s| s.test_file.clone())
                    .collect();
                if !stale_after_lint.is_empty() {
                    return Err(invalid(format!(
                        "SOURCE_CHANGED_DURING_TEST files={}; cached results invalidated during pre-flight lint, retry once edits settle",
                        stale_after_lint.join(",")
                    ))
                    .into());
                }
            }
        }

        // 4. Execute delta tests if any remain uncached
        let mut executed_summaries = Vec::new();
        let mut delta_stdout = String::new();
        if !to_execute.is_empty() {
            let (had_cargo_lock_before_exec, before_non_lock_fp, start_inv_watermarks) = {
                let guard = self.lock_state();
                (
                    Self::snapshot_cargo_lock_existence(&guard, &to_execute),
                    Self::snapshot_non_lock_dep_fingerprints(&guard, &to_execute),
                    Self::snapshot_invalidation_watermarks(&guard, &to_execute),
                )
            };
            // Build invocation targets per resolved file in `to_execute`: keep explicit nodeids
            // (`file::node`) for files requested with node selectors, and pass the file path itself
            // for files resolved via directory/source/file targets so mixed targets never drop tests.
            let invocation_targets: Vec<String> = if norm_targets.iter().any(|t| t.contains("::")) {
                let mut inv = Vec::new();
                for ex in &to_execute {
                    if whole_file_covered.contains(ex) {
                        inv.push(ex.clone());
                        continue;
                    }
                    let prefix = format!("{ex}::");
                    let mut matched_nodeids = false;
                    for t in &norm_targets {
                        if t.starts_with(&prefix) {
                            inv.push(t.clone());
                            matched_nodeids = true;
                        }
                    }
                    if !matched_nodeids {
                        inv.push(ex.clone());
                    }
                }
                inv
            } else {
                to_execute.clone()
            };

            let plugin_manifest = pytest_plugin_manifest_for_targets(&spec, &invocation_targets);
            if scoped_cancelled.load(Ordering::Acquire) {
                let mut guard = self.lock_state();
                for (test_file, dep_hash) in &dep_hashes_for_exec {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                self.condvar.notify_all();
                return Err(invalid("TEST_CANCELLED: pytest process group was terminated").into());
            }
            let exec_pgids_ref: &Mutex<BTreeSet<i32>> = &scoped_pgids;
            let exec_single_pgid_ref: &AtomicI32 = &scoped_single_pgid;
            let mut exec_extra_args = if strip_py_flags_for_remaining {
                if to_execute.iter().all(|f| f.ends_with(".rs")) {
                    filter_cargo_args_from_mixed(&req.pytest_args)
                } else {
                    strip_pytest_only_flags_for_non_py(&req.pytest_args)
                }
            } else {
                req.pytest_args.clone()
            };
            if let Some(runner) = explicit_fe_runner {
                exec_extra_args.push(format!("__tctx_fe_runner__={runner}"));
            }
            if let Some(ref ct) = req.cargo_target {
                exec_extra_args.push(format!("__tctx_cargo_target__={ct}"));
            }
            if to_execute.iter().any(|p| p.ends_with(".rs")) {
                // Preserve original request targets (including directory targets & empty
                // full-workspace requests) so `execute_cargo_test_batch` never mistakes expanded
                // `.rs` files for explicit file-scoped selectors or drops testless selected packages!
                if norm_targets.is_empty() {
                    exec_extra_args.push("__tctx_orig_empty__".to_string());
                } else {
                    for ot in &norm_targets {
                        exec_extra_args.push(format!("__tctx_orig_target__={ot}"));
                    }
                }
            }
            let exec_res = if parallel_delta && invocation_targets.len() > 1 {
                execute_pytest_parallel(
                    &spec,
                    &invocation_targets,
                    &dep_hashes_for_exec,
                    req.k.as_deref(),
                    req.mark.as_deref(),
                    &exec_extra_args,
                    &preload_modules,
                    &environment_hash,
                    &plugin_manifest,
                    capture_dynamic_dependencies,
                    exec_pgids_ref,
                )
            } else {
                execute_pytest_batch(
                    &spec,
                    &invocation_targets,
                    &dep_hashes_for_exec,
                    req.k.as_deref(),
                    req.mark.as_deref(),
                    &exec_extra_args,
                    "on_demand",
                    Some(&self.zygote),
                    Some(exec_single_pgid_ref),
                    Some(exec_pgids_ref),
                    &preload_modules,
                    &environment_hash,
                    &plugin_manifest,
                    capture_dynamic_dependencies,
                )
            };
            if scoped_cancelled.load(Ordering::Acquire) {
                let mut guard = self.lock_state();
                for (test_file, dep_hash) in &dep_hashes_for_exec {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                self.condvar.notify_all();
                return Err(invalid("TEST_CANCELLED: pytest process group was terminated").into());
            }
            let (records, raw_out) = match exec_res {
                Ok(pair) => pair,
                Err(err) => {
                    let mut guard = self.lock_state();
                    for (tf, dh) in &dep_hashes_for_exec {
                        guard.in_flight.remove(&flight_key(tf, dh));
                    }
                    self.condvar.notify_all();
                    return Err(err);
                }
            };

            self.warm_environment_probes_unlocked();
            let mut guard = self.lock_state();
            for (tf, dh) in &dep_hashes_for_exec {
                guard.in_flight.remove(&flight_key(tf, dh));
            }
            // Refresh cross-process WAL/snapshot updates BEFORE post-execution revalidation so
            // any invalidation (either of cached summaries OR of in-flight executed files) is
            // observed before caching or returning results.
            if guard.cache.disk_changed_since_sync() {
                drop(guard);
                self.refresh_cache_from_disk_if_changed();
                guard = self.lock_state();
            }
            // Drain queued watcher events and revalidate external configs BEFORE post-execution
            // reconciliation so any file, config, or external Cargo/Go env edit during execution
            // is immediately reflected in the graph.
            self.drain_fs_events_locked(&mut guard);
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            let has_rs_exec = target_test_files.iter().any(|f| f.ends_with(".rs"));
            // Reconcile ALL targeted tests (both executed AND previously cached in a hybrid run)
            // so a cached dependency modified while `to_execute` ran is never returned stale.
            Self::reconcile_live_files_inner_locked(
                &mut guard,
                Some(&target_test_files),
                has_rs_exec,
            );
            // Compute non-lock instability BEFORE syncing newly created `Cargo.lock` by comparing
            // per-test non-lock dependency fingerprints (unaffected by brand-new `Cargo.lock`
            // creation, but catching every source, config, env, or graph edge edit).
            let after_non_lock_fp =
                Self::snapshot_non_lock_dep_fingerprints(&guard, &to_execute);
            let mut non_lock_unstable: BTreeSet<String> = dep_hashes_for_exec
                .iter()
                .filter(|(test_file, expected_hash)| {
                    if test_file.ends_with(".rs") {
                        before_non_lock_fp.get(*test_file) != after_non_lock_fp.get(*test_file)
                            || Self::test_touches_unreadable_file(&guard, test_file)
                    } else {
                        guard.graph.dep_hash_for_test_or_explicit(test_file).as_deref()
                            != Some(expected_hash.as_str())
                            || Self::test_touches_unreadable_file(&guard, test_file)
                    }
                })
                .map(|(test_file, _)| test_file.clone())
                .collect();
            let mut records = records;
            let existing_lock_modified = Self::sync_cargo_locks_after_exec_locked(
                &mut guard,
                &to_execute,
                &had_cargo_lock_before_exec,
                &non_lock_unstable,
                &mut records,
            );
            // Update `dep_hashes_for_exec` ONLY for Rust tests that were NOT already unstable
            // due to non-lock edits and ONLY when `cargo test` created a brand-new `Cargo.lock`.
            if !existing_lock_modified && has_rs_exec {
                for rec in &records {
                    if rec.test_file.ends_with(".rs")
                        && !non_lock_unstable.contains(&rec.test_file)
                    {
                        dep_hashes_for_exec.insert(rec.test_file.clone(), rec.dep_hash.clone());
                    }
                }
            }
            let mut unstable_set: BTreeSet<String> = dep_hashes_for_exec
                .iter()
                .filter(|(test_file, expected_hash)| {
                    guard.graph.dep_hash_for_test_or_explicit(test_file).as_deref()
                        != Some(expected_hash.as_str())
                        || Self::test_touches_unreadable_file(&guard, test_file)
                        || (existing_lock_modified && test_file.ends_with(".rs"))
                })
                .map(|(test_file, _)| test_file.clone())
                .collect();
            unstable_set.append(&mut non_lock_unstable);
            let graph_incomplete_after_exec = Self::is_graph_incomplete(&guard);
            let mut stale_cached_files: Vec<String> = Vec::new();
            cached_summaries.retain(|summary| {
                let is_node_summary = summary.source.starts_with("cached_node(");
                let live_wm = file_invalidation_stamp(&guard, &summary.test_file);
                let prev_wm = cached_watermarks
                    .get(&summary.test_file)
                    .copied()
                    .unwrap_or((0, 0));
                let still_fresh = !graph_incomplete_after_exec
                    && live_wm == prev_wm
                    && guard
                        .graph
                        .dep_hash_for_test_or_explicit(&summary.test_file)
                        .as_deref()
                        == Some(summary.dep_hash.as_str())
                    && !Self::test_touches_unreadable_file(&guard, &summary.test_file)
                    && guard
                        .cache
                        .get_matching(&summary.test_file, &summary.dep_hash)
                        .is_some_and(|rec| {
                            if is_node_summary {
                                rec.dynamic_dependencies_complete
                                    && !rec.node_results.is_empty()
                                    && (rec.outcome.is_green() || !req.rerun_failed)
                                    && cached_node_snapshots
                                        .get(&summary.test_file)
                                        .is_some_and(|snap| {
                                            snap.iter().all(|(nid, expected_out)| {
                                                rec.node_results
                                                    .get(nid)
                                                    .is_some_and(|nr| nr.outcome == *expected_out)
                                            })
                                        })
                            } else {
                                rec.outcome == summary.outcome
                                    && rec.passed == summary.passed
                                    && rec.failed == summary.failed
                                    && rec.errors == summary.errors
                                    && rec.skipped == summary.skipped
                                    && rec.failed_nodeids == summary.failed_nodeids
                            }
                        });
                if !still_fresh {
                    stale_cached_files.push(summary.test_file.clone());
                }
                still_fresh
            });
            for stale_tf in &stale_cached_files {
                unstable_set.insert(stale_tf.clone());
            }
            let mut requeued = 0usize;
            for test_file in unstable_set.iter().rev() {
                if background_eligible_with_cache(&guard, test_file)
                    && !Self::test_touches_unreadable_file(&guard, test_file)
                {
                    guard.pending_queue.retain(|queued| queued != test_file);
                    guard.pending_queue.push_front(test_file.clone());
                    requeued += 1;
                }
            }

            let mut stable_records: Vec<CachedTestRecord> = records
                .into_iter()
                .filter(|rec| !unstable_set.contains(&rec.test_file))
                .collect();
            if !stable_records.is_empty() {
                let mut runtime_changed = false;
                let mut runtime_dep_modified_during_run = BTreeSet::new();
                let mut runtime_dep_untrackable = BTreeSet::new();
                let workspace_snapshot = guard.workspace.clone();
                let policy_snapshot = guard.spec.policy.clone();
                for record in &mut stable_records {
                    resolve_module_attributed_dynamic_deps(&guard.graph, record);
                    let prev_known: BTreeSet<String> = guard
                        .graph
                        .runtime_deps_for_test
                        .get(&record.test_file)
                        .cloned()
                        .unwrap_or_default();
                    let changed_this = guard.graph.apply_runtime_dependencies(
                        &workspace_snapshot,
                        &policy_snapshot,
                        &record.test_file,
                        &record.dynamic_dependencies,
                    );
                    if changed_this {
                        runtime_changed = true;
                    }
                    let (modified_during_run, has_untrackable) =
                        classify_runtime_deps_stability(&guard, record, &prev_known);
                    if modified_during_run {
                        runtime_dep_modified_during_run.insert(record.test_file.clone());
                    } else if has_untrackable {
                        runtime_dep_untrackable.insert(record.test_file.clone());
                    }
                }
                if runtime_changed {
                    guard.graph.recompute_edges();
                    let environment_hash = guard.graph.environment_hash.clone();
                    guard.graph.apply_environment_hash(environment_hash);
                    for record in &mut stable_records {
                        if let Some(dep_hash) =
                            guard.graph.dep_hash_for_test_or_explicit(&record.test_file)
                        {
                            record.dep_hash = dep_hash;
                        }
                    }
                }
                let policy = guard.spec.policy.clone();
                for rec in stable_records {
                    if runtime_dep_modified_during_run.contains(&rec.test_file)
                        || Self::was_invalidated_since(
                            &guard,
                            &rec.test_file,
                            &start_inv_watermarks,
                        )
                    {
                        // A runtime dependency changed OR an explicit invalidation arrived while
                        // this test was executing; discard the pre-invalidation result as unstable
                        // so we never return a stale pre-invalidation pass!
                        if background_eligible_with_cache(&guard, &rec.test_file) {
                            guard.pending_queue.retain(|queued| queued != &rec.test_file);
                            guard.pending_queue.push_front(rec.test_file.clone());
                            requeued += 1;
                        }
                        unstable_set.insert(rec.test_file);
                        continue;
                    }
                    executed_watermarks.insert(
                        rec.test_file.clone(),
                        file_invalidation_stamp(&guard, &rec.test_file),
                    );
                    if !rec.outcome.is_green()
                        && let Some(code) = rec.exit_code
                        && code != 0
                    {
                        observed_runner_exit_code.get_or_insert(code);
                    }
                    executed_summaries.push(FileRunSummary {
                        test_file: rec.test_file.clone(),
                        dep_hash: rec.dep_hash.clone(),
                        outcome: rec.outcome,
                        source: "executed".to_string(),
                        passed: rec.passed,
                        failed: rec.failed,
                        skipped: rec.skipped,
                        errors: rec.errors,
                        duration_ms: rec.duration_ms,
                        failed_nodeids: rec.failed_nodeids.clone(),
                    });
                    if !has_subfile_filter
                        && !req.skip_cache_write
                        && !fe_runner_overridden_files.contains(&rec.test_file)
                        && !rust_shared_target_files.contains(&rec.test_file)
                        && Self::is_test_cacheable_in_graph(&guard, &rec.test_file)
                        && req.pytest_args.is_empty()
                        && rec.dynamic_dependencies_complete
                        && !runtime_dep_untrackable.contains(&rec.test_file)
                        && !Self::was_invalidated_since(
                            &guard,
                            &rec.test_file,
                            &start_inv_watermarks,
                        )
                    {
                        guard.cache.insert(rec, &policy);
                    }
                }
                guard.cache.total_delta_runs = guard.cache.total_delta_runs.saturating_add(1);
            }

            if !unstable_set.is_empty() {
                let unstable_list: Vec<String> = unstable_set.into_iter().collect();
                // NEVER return `"passed"` when any requested test file was modified mid-run and
                // left unverified: always fail explicitly so callers retry against the settled tree.
                let _ = allow_partial_retry;
                self.condvar.notify_all();
                drop(guard);
                self.flush_cache_detached();
                return Err(invalid(format!(
                    "SOURCE_CHANGED_DURING_TEST files={}; result discarded ({requeued} requeued for background); the file was edited while tests ran, retry once edits settle",
                    unstable_list.join(",")
                ))
                .into());
            }
            delta_stdout = raw_out;
            self.condvar.notify_all();
            drop(guard);
            self.flush_cache_detached();
        }

        // Verify any selected testless Cargo workspace members OR uncovered testable targets
        // (e.g., a testless `src/main.rs` or `harness = false` target) via `cargo test` so
        // compile errors, doctests, and custom-harness panics are NEVER masked by cached passes!
        // In unified workspace mode where `execute_cargo_test_batch` ran at least one `.rs` file,
        // the unified `cargo test -p ...` already compiled & executed all selected members; if NO
        // `.rs` files executed (e.g. only testless Rust crates + passing Python tests), run ONE
        // unified `cargo test -p ...` across `unified_rs_check_crates`!
        let executed_rs_in_delta = to_execute.iter().any(|f| f.ends_with(".rs"));
        let mut ran_uncovered_cargo_check = false;
        let check_pgids_ref: &Mutex<BTreeSet<i32>> = &scoped_pgids;
        let cargo_raw_args =
            if strip_py_flags_for_remaining || req.language_filter.as_deref() != Some("rust") {
                filter_cargo_args_from_mixed(&req.pytest_args)
            } else {
                req.pytest_args.clone()
            };
        let norm_cargo_args = normalize_cargo_config_args(&spec.root, &cargo_raw_args);
        let sep_idx = norm_cargo_args.iter().position(|a| a == "--");
        let extra_cargo_flags: Vec<String> = norm_cargo_args[..sep_idx.unwrap_or(norm_cargo_args.len())]
            .iter()
            .filter(|a| {
                !a.starts_with("__tctx_orig_target__=") && a.as_str() != "__tctx_orig_empty__"
            })
            .cloned()
            .collect();
        let harness_tail_args: Vec<String> = sep_idx
            .map(|idx| norm_cargo_args[idx + 1..].to_vec())
            .unwrap_or_default();
        let append_cargo_filters = |cmd: &mut Command| {
            if let Some(ref k) = req.k {
                cmd.arg(k);
            }
            for a in &extra_cargo_flags {
                cmd.arg(a);
            }
            let wants_ignored = req.mark.as_deref().is_some_and(|m| m.contains("ignore"))
                && !harness_tail_args.iter().any(|a| a == "--ignored");
            if !harness_tail_args.is_empty() || wants_ignored {
                cmd.arg("--");
                for a in &harness_tail_args {
                    cmd.arg(a);
                }
                if wants_ignored {
                    cmd.arg("--ignored");
                }
            }
        };
        if rust_unified_workspace_mode && !executed_rs_in_delta && !unified_rs_check_crates.is_empty()
        {
            let cargo_cmd = spec.effective_cargo_cmd();
            if let Some((prog, base_args)) = cargo_cmd.split_first() {
                let mut pkgs: Vec<(String, String)> = Vec::new();
                for cd in &unified_rs_check_crates {
                    let cdir_abs = if cd == "." {
                        spec.root.clone()
                    } else {
                        spec.root.join(cd)
                    };
                    let Some(pn) = cargo_manifest_package_name(&cdir_abs)
                        .or_else(|| cargo_manifest_package_name_via_metadata(&cdir_abs))
                    else {
                        return Err(invalid(format!(
                            "UNRESOLVED_CARGO_PACKAGE_NAME: Cannot resolve Cargo package name for selected workspace crate '{cd}'"
                        ))
                        .into());
                    };
                    pkgs.push((cd.clone(), pn));
                }
                if !pkgs.is_empty() {
                    let compile_timeout = cargo_compile_timeout_for(&spec);
                    let exec_timeout = Duration::from_secs(spec.policy.pytest_timeout_secs.max(1));
                    let extra_has_no_run = extra_cargo_flags.iter().any(|a| a == "--no-run");
                    let extra_has_doc = extra_cargo_flags.iter().any(|a| a == "--doc")
                        || req.cargo_target.as_deref() == Some("doc");
                    let append_req_cargo_target = |c: &mut Command, skip_doc: bool| {
                        if let Some(ct) = req.cargo_target.as_deref() {
                            if ct == "lib" {
                                c.arg("--lib");
                            } else if ct == "bins" {
                                c.arg("--bins");
                            } else if ct == "doc" {
                                if !skip_doc {
                                    c.arg("--doc");
                                }
                            } else if let Some(b) = ct.strip_prefix("bin:") {
                                c.arg("--bin").arg(b);
                            } else if let Some(t) = ct.strip_prefix("test:") {
                                c.arg("--test").arg(t);
                            }
                        }
                    };
                    ran_uncovered_cargo_check = true;
                    let mut precompile_failed = false;
                    let unified_timeout = if extra_has_no_run {
                        compile_timeout
                    } else {
                        // Always precompile under `compile_timeout` and execute under `exec_timeout`
                        // so `TCTX_CARGO_COMPILE_TIMEOUT_SECS < exec_timeout` NEVER truncates
                        // execution AND `exec_timeout < compile_timeout` NEVER lets harnesses hang!
                        let mut pre_cmd = Command::new(prog);
                        pre_cmd.current_dir(&spec.root);
                        pre_cmd.args(base_args);
                        if extra_has_doc {
                            pre_cmd.arg("build").arg("--lib");
                        } else {
                            pre_cmd.arg("test").arg("--no-run");
                        }
                        for (_, pn) in &pkgs {
                            pre_cmd.arg("-p").arg(pn);
                        }
                        if !extra_has_doc {
                            append_req_cargo_target(&mut pre_cmd, true);
                        }
                        for a in &extra_cargo_flags {
                            if a != "--no-run" && (!extra_has_doc || (a != "--doc" && a != "--no-fail-fast")) {
                                pre_cmd.arg(a);
                            }
                        }
                        let pre_out = run_command_with_timeout_tracked(
                            &mut pre_cmd,
                            compile_timeout,
                            4 * 1024 * 1024,
                            None,
                            Some(check_pgids_ref),
                        )?;
                        if scoped_cancelled.load(Ordering::Acquire) || pre_out.cancelled {
                            return Err(invalid(
                                "TEST_CANCELLED: request cancelled during unified workspace precompile",
                            )
                            .into());
                        }
                        if !pre_out.status.success() || pre_out.timed_out {
                            precompile_failed = true;
                            observed_runner_exit_code.get_or_insert(
                                pre_out.status.code().filter(|&c| c != 0).unwrap_or(101),
                            );
                            let stderr_s = String::from_utf8_lossy(&pre_out.stderr);
                            let stdout_s = String::from_utf8_lossy(&pre_out.stdout);
                            let msg = if !stderr_s.trim().is_empty() {
                                stderr_s.into_owned()
                            } else {
                                stdout_s.into_owned()
                            };
                            if !delta_stdout.is_empty() {
                                delta_stdout.push('\n');
                            }
                            delta_stdout.push_str(&truncate_str(&msg, 4096));
                            for (cd, pn) in &pkgs {
                                let label = if cd == "." {
                                    format!("Cargo.toml ({pn})")
                                } else {
                                    format!("{cd}/Cargo.toml ({pn})")
                                };
                                executed_summaries.push(FileRunSummary {
                                    test_file: label,
                                    dep_hash: String::new(),
                                    outcome: TestOutcome::Error,
                                    source: "testless_cargo_check".to_string(),
                                    passed: 0,
                                    failed: 0,
                                    skipped: 0,
                                    errors: 1,
                                    duration_ms: pre_out.duration_ms.max(1),
                                    failed_nodeids: vec![format!("cargo_check:{pn}")],
                                });
                            }
                        }
                        if extra_has_doc {
                            compile_timeout.saturating_add(exec_timeout)
                        } else {
                            exec_timeout
                        }
                    };
                    if !precompile_failed {
                    let mut cmd = Command::new(prog);
                    cmd.current_dir(&spec.root);
                    cmd.args(base_args);
                    cmd.arg("test");
                    for (_, pn) in &pkgs {
                        cmd.arg("-p").arg(pn);
                    }
                    append_req_cargo_target(&mut cmd, false);
                    append_cargo_filters(&mut cmd);
                    let out = run_command_with_timeout_tracked(
                        &mut cmd,
                        unified_timeout,
                        4 * 1024 * 1024,
                        None,
                        Some(check_pgids_ref),
                    )?;
                    if scoped_cancelled.load(Ordering::Acquire) || out.cancelled {
                        return Err(invalid(
                            "TEST_CANCELLED: request cancelled during unified workspace check",
                        )
                        .into());
                    }
                    if !out.status.success() || out.timed_out {
                        let stderr_s = String::from_utf8_lossy(&out.stderr);
                        let stdout_s = String::from_utf8_lossy(&out.stdout);
                        let msg = if !stderr_s.trim().is_empty() {
                            stderr_s.into_owned()
                        } else {
                            stdout_s.into_owned()
                        };
                        if !delta_stdout.is_empty() {
                            delta_stdout.push('\n');
                        }
                        delta_stdout.push_str(&truncate_str(&msg, 4096));
                        observed_runner_exit_code
                            .get_or_insert(out.status.code().filter(|&c| c != 0).unwrap_or(101));
                        for (cd, pn) in &pkgs {
                            let label = if cd == "." {
                                format!("Cargo.toml ({pn})")
                            } else {
                                format!("{cd}/Cargo.toml ({pn})")
                            };
                            executed_summaries.push(FileRunSummary {
                                test_file: label,
                                dep_hash: String::new(),
                                outcome: TestOutcome::Error,
                                source: "testless_cargo_check".to_string(),
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: 1,
                                duration_ms: out.duration_ms.max(1),
                                failed_nodeids: vec![format!("cargo_check:{pn}")],
                            });
                        }
                    } else if cached_summaries.is_empty() && executed_summaries.is_empty() {
                        for (cd, pn) in &pkgs {
                            let label = if cd == "." {
                                format!("Cargo.toml ({pn})")
                            } else {
                                format!("{cd}/Cargo.toml ({pn})")
                            };
                            executed_summaries.push(FileRunSummary {
                                test_file: label,
                                dep_hash: String::new(),
                                outcome: TestOutcome::Passed,
                                source: "testless_cargo_check".to_string(),
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: 0,
                                duration_ms: out.duration_ms.max(1),
                                failed_nodeids: Vec::new(),
                            });
                        }
                    }
                    }
                }
            }
        } else if !rust_unified_workspace_mode && !uncovered_rs_checks.is_empty() {
            let cargo_cmd = spec.effective_cargo_cmd();
            if let Some((prog, base_args)) = cargo_cmd.split_first() {
                let compile_timeout = cargo_compile_timeout_for(&spec);
                let exec_timeout = Duration::from_secs(spec.policy.pytest_timeout_secs.max(1));
                let extra_has_no_run = extra_cargo_flags.iter().any(|a| a == "--no-run");
                for (cd, target_args) in &uncovered_rs_checks {
                    let cdir_abs = if cd == "." {
                        spec.root.clone()
                    } else {
                        spec.root.join(cd)
                    };
                    let pn_opt = cargo_manifest_package_name(&cdir_abs)
                        .or_else(|| cargo_manifest_package_name_via_metadata(&cdir_abs));
                    if pn_opt.is_none() && cd == "." && is_cargo_workspace_root(&spec.root) {
                        // A workspace root package excluded from `default-members` would run the
                        // wrong package if executed without `-p`; fail closed explicitly!
                        executed_summaries.push(FileRunSummary {
                            test_file: "Cargo.toml (.)".to_string(),
                            dep_hash: String::new(),
                            outcome: TestOutcome::Error,
                            source: "testless_cargo_check".to_string(),
                            passed: 0,
                            failed: 0,
                            skipped: 0,
                            errors: 1,
                            duration_ms: 1,
                            failed_nodeids: vec!["cargo_check:unresolved_root_package".to_string()],
                        });
                        continue;
                    }
                    let pn_label = pn_opt.as_deref().unwrap_or(cd.as_str());
                    // Run member-scoped checks from `cdir_abs` (matching `execute_cargo_test_batch`)
                    // so member-local `.cargo/config.toml` and `rust-toolchain.toml` are honored!
                    let is_build_subcmd = target_args.first().map(String::as_str) == Some("__cargo_build__");
                    let actual_targs = if is_build_subcmd {
                        &target_args[1..]
                    } else {
                        target_args.as_slice()
                    };
                    let group_has_no_run =
                        extra_has_no_run || actual_targs.iter().any(|a| a == "--no-run");
                    let is_doc_group = actual_targs.iter().any(|a| a == "--doc");
                    // When executing tests/harnesses with a shorter execution timeout than compile
                    // timeout, precompile first under `compile_timeout` and execute under `exec_timeout`!
                    ran_uncovered_cargo_check = true;
                    let run_timeout = if is_build_subcmd || group_has_no_run {
                        compile_timeout
                    } else {
                        // Always precompile under `compile_timeout` and execute under `exec_timeout`!
                        let mut pre_cmd = Command::new(prog);
                        pre_cmd.current_dir(&cdir_abs);
                        pre_cmd.args(base_args);
                        if is_doc_group {
                            // `cargo test --doc` links against the non-test library (`cargo build --lib`),
                            // NOT `#[cfg(test)]` unit tests (so a broken unit-test module never fails `--doc`!).
                            pre_cmd.arg("build").arg("--lib");
                            if let Some(ref pn) = pn_opt {
                                pre_cmd.arg("-p").arg(pn);
                            }
                        } else {
                            pre_cmd.arg("test").arg("--no-run");
                            if let Some(ref pn) = pn_opt {
                                pre_cmd.arg("-p").arg(pn);
                            }
                            for ta in actual_targs {
                                if ta != "--no-run" {
                                    pre_cmd.arg(ta);
                                }
                            }
                        }
                        for a in &extra_cargo_flags {
                            if a != "--no-run" && (!is_doc_group || a != "--no-fail-fast") {
                                pre_cmd.arg(a);
                            }
                        }
                        let pre_out = run_command_with_timeout_tracked(
                            &mut pre_cmd,
                            compile_timeout,
                            4 * 1024 * 1024,
                            None,
                            Some(check_pgids_ref),
                        )?;
                        if scoped_cancelled.load(Ordering::Acquire) || pre_out.cancelled {
                            return Err(invalid(
                                "TEST_CANCELLED: request cancelled during uncovered Cargo precompile",
                            )
                            .into());
                        }
                        if !pre_out.status.success() || pre_out.timed_out {
                            let stderr_s = String::from_utf8_lossy(&pre_out.stderr);
                            let stdout_s = String::from_utf8_lossy(&pre_out.stdout);
                            let msg = if !stderr_s.trim().is_empty() {
                                stderr_s.into_owned()
                            } else {
                                stdout_s.into_owned()
                            };
                            if !delta_stdout.is_empty() {
                                delta_stdout.push('\n');
                            }
                            delta_stdout.push_str(&truncate_str(&msg, 4096));
                            observed_runner_exit_code.get_or_insert(
                                pre_out.status.code().filter(|&c| c != 0).unwrap_or(101),
                            );
                            let label = if cd == "." {
                                format!("Cargo.toml ({pn_label})")
                            } else {
                                format!("{cd}/Cargo.toml ({pn_label})")
                            };
                            executed_summaries.push(FileRunSummary {
                                test_file: label,
                                dep_hash: String::new(),
                                outcome: TestOutcome::Error,
                                source: "testless_cargo_check".to_string(),
                                passed: 0,
                                failed: 0,
                                skipped: 0,
                                errors: 1,
                                duration_ms: pre_out.duration_ms.max(1),
                                failed_nodeids: vec![format!("cargo_check:{pn_label}")],
                            });
                            continue;
                        }
                        if is_doc_group {
                            compile_timeout.saturating_add(exec_timeout)
                        } else {
                            exec_timeout
                        }
                    };
                    let mut cmd = Command::new(prog);
                    cmd.current_dir(&cdir_abs);
                    cmd.args(base_args);
                    cmd.arg(if is_build_subcmd { "build" } else { "test" });
                    if let Some(ref pn) = pn_opt {
                        cmd.arg("-p").arg(pn);
                    }
                    for ta in actual_targs {
                        cmd.arg(ta);
                    }
                    if is_build_subcmd {
                        for a in &extra_cargo_flags {
                            if a != "--no-run" && a != "--no-fail-fast" {
                                cmd.arg(a);
                            }
                        }
                    } else if group_has_no_run {
                        // Preserve `--no-run` for `cargo test` so compile-only requests NEVER
                        // execute custom harnesses!
                        if !actual_targs.iter().any(|a| a == "--no-run") {
                            cmd.arg("--no-run");
                        }
                        for a in &extra_cargo_flags {
                            if a != "--no-run" {
                                cmd.arg(a);
                            }
                        }
                    } else {
                        append_cargo_filters(&mut cmd);
                    }
                    let out = run_command_with_timeout_tracked(
                        &mut cmd,
                        run_timeout,
                        4 * 1024 * 1024,
                        None,
                        Some(check_pgids_ref),
                    )?;
                    if scoped_cancelled.load(Ordering::Acquire) || out.cancelled {
                        return Err(invalid(
                            "TEST_CANCELLED: request cancelled during uncovered Cargo target check",
                        )
                        .into());
                    }
                    if !out.status.success() || out.timed_out {
                        observed_runner_exit_code
                            .get_or_insert(out.status.code().filter(|&c| c != 0).unwrap_or(101));
                        let stderr_s = String::from_utf8_lossy(&out.stderr);
                        let stdout_s = String::from_utf8_lossy(&out.stdout);
                        let msg = if !stderr_s.trim().is_empty() {
                            stderr_s.into_owned()
                        } else {
                            stdout_s.into_owned()
                        };
                        if !delta_stdout.is_empty() {
                            delta_stdout.push('\n');
                        }
                        delta_stdout.push_str(&truncate_str(&msg, 4096));
                        let label = if cd == "." {
                            format!("Cargo.toml ({pn_label})")
                        } else {
                            format!("{cd}/Cargo.toml ({pn_label})")
                        };
                        executed_summaries.push(FileRunSummary {
                            test_file: label,
                            dep_hash: String::new(),
                            outcome: TestOutcome::Error,
                            source: "testless_cargo_check".to_string(),
                            passed: 0,
                            failed: 0,
                            skipped: 0,
                            errors: 1,
                            duration_ms: out.duration_ms.max(1),
                            failed_nodeids: vec![format!("cargo_check:{pn_label}")],
                        });
                    } else if cached_summaries.is_empty() && executed_summaries.is_empty() {
                        let label = if cd == "." {
                            format!("Cargo.toml ({pn_label})")
                        } else {
                            format!("{cd}/Cargo.toml ({pn_label})")
                        };
                        executed_summaries.push(FileRunSummary {
                            test_file: label,
                            dep_hash: String::new(),
                            outcome: TestOutcome::Passed,
                            source: "testless_cargo_check".to_string(),
                            passed: 0,
                            failed: 0,
                            skipped: 0,
                            errors: 0,
                            duration_ms: out.duration_ms.max(1),
                            failed_nodeids: Vec::new(),
                        });
                    }
                }
            }
        }

        // Revalidate ALL cached, previously-executed, AND uncovered/testless Cargo crates AFTER
        // slow uncovered Cargo checks (e.g. doctests, custom harnesses, example builds) complete:
        // 1. Refresh external environment probes (`$CARGO_HOME/config.toml`, rustc/cargo/env vars)
        // 2. Refresh WAL/cache invalidations from disk so concurrent `test_invalidate` is honored
        // 3. Reconcile live source files & verify cached/executed `dep_hash`, watermarks, AND
        //    uncovered crate fingerprints (so mid-check edits to a custom harness NEVER pass stale)!
        if ran_uncovered_cargo_check {
            self.warm_environment_probes_unlocked();
            let mut guard = self.lock_state();
            if guard.cache.disk_changed_since_sync() {
                drop(guard);
                self.refresh_cache_from_disk_if_changed();
                guard = self.lock_state();
            }
            self.drain_fs_events_locked(&mut guard);
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            let mut reconcile_paths: Vec<String> = target_test_files.clone();
            if !uncovered_crate_fingerprints.is_empty() {
                for p in guard.graph.files.keys() {
                    reconcile_paths.push(p.clone());
                }
            }
            Self::reconcile_live_files_inner_locked(
                &mut guard,
                Some(&reconcile_paths),
                true,
            );
            let mut unstable_files: Vec<String> = Vec::new();
            for (cd, before_fp) in &uncovered_crate_fingerprints {
                let after_fp = Self::snapshot_rust_crate_fingerprint(&guard, cd);
                if after_fp != *before_fp {
                    let label = if cd == "." {
                        "Cargo.toml".to_string()
                    } else {
                        format!("{cd}/Cargo.toml")
                    };
                    if !unstable_files.contains(&label) {
                        unstable_files.push(label);
                    }
                }
            }
            for cs in &cached_summaries {
                if cs.dep_hash.is_empty() {
                    continue;
                }
                let live_hash = guard.graph.dep_hash_for_test_or_explicit(&cs.test_file);
                let live_wm = file_invalidation_stamp(&guard, &cs.test_file);
                let prev_wm = cached_watermarks
                    .get(&cs.test_file)
                    .copied()
                    .unwrap_or((0, 0));
                let cache_still_matches = live_wm == prev_wm
                    && guard
                        .cache
                        .get_matching(&cs.test_file, &cs.dep_hash)
                        .is_some_and(|rec| {
                            if cs.source.starts_with("cached_node(") {
                                cached_node_snapshots
                                    .get(&cs.test_file)
                                    .is_some_and(|snap| {
                                        snap.iter().all(|(nid, expected_out)| {
                                            rec.node_results
                                                .get(nid)
                                                .is_some_and(|nr| nr.outcome == *expected_out)
                                        })
                                    })
                            } else {
                                rec.outcome == cs.outcome
                                    && rec.passed == cs.passed
                                    && rec.failed == cs.failed
                                    && rec.errors == cs.errors
                                    && rec.skipped == cs.skipped
                                    && rec.failed_nodeids == cs.failed_nodeids
                            }
                        });
                if (live_hash.as_deref() != Some(cs.dep_hash.as_str())
                    || !cache_still_matches
                    || Self::test_touches_unreadable_file(&guard, &cs.test_file))
                    && !unstable_files.contains(&cs.test_file)
                {
                    unstable_files.push(cs.test_file.clone());
                }
            }
            for es in &executed_summaries {
                if es.dep_hash.is_empty() {
                    continue;
                }
                let live_hash = guard.graph.dep_hash_for_test_or_explicit(&es.test_file);
                let live_wm = file_invalidation_stamp(&guard, &es.test_file);
                let prev_wm = executed_watermarks
                    .get(&es.test_file)
                    .copied()
                    .unwrap_or(live_wm);
                if (live_hash.as_deref() != Some(es.dep_hash.as_str())
                    || live_wm != prev_wm
                    || Self::test_touches_unreadable_file(&guard, &es.test_file))
                    && !unstable_files.contains(&es.test_file)
                {
                    unstable_files.push(es.test_file.clone());
                }
            }
            if !unstable_files.is_empty() {
                drop(guard);
                return Err(invalid(format!(
                    "SOURCE_CHANGED_DURING_TEST files={}; test results or environment invalidated while uncovered Cargo check ran, retry once edits settle",
                    unstable_files.join(",")
                ))
                .into());
            }
        }

        // Revalidate retained direct-Python results after remaining Cargo/Frontend/testless checks
        // finish so mid-run Python edits never report a stale pass, and merge Python counts/output!
        if let Some(py_mix) = direct_py_mix {
            let mut post_guard = self.lock_state();
            self.drain_fs_events_locked(&mut post_guard);
            let check_files: Vec<String> = py_mix.pre_py_stamps.keys().cloned().collect();
            Self::reconcile_live_files_inner_locked(&mut post_guard, Some(&check_files), false);
            let collected_mutated = py_mix
                .pre_collected_stamps
                .iter()
                .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
            let runtime_dep_mutated = py_mix
                .runtime_dep_stamps
                .iter()
                .any(|(p, st)| crate::ast_graph::file_stamp(p) != *st);
            let indexed_mutated = py_mix.pre_py_stamps.iter().any(|(p, st)| {
                crate::ast_graph::file_stamp(&post_guard.workspace.root.join(p)) != *st
            });
            let new_py_or_cfg_appeared = post_guard
                .graph
                .files
                .iter()
                .any(|(p, n)| (p.ends_with(".py") || n.is_config) && !py_mix.pre_py_stamps.contains_key(p));
            drop(post_guard);
            if collected_mutated || runtime_dep_mutated || indexed_mutated || new_py_or_cfg_appeared {
                return Err(invalid(
                    "SOURCE_CHANGED_DURING_TEST: Python source or config modified while remaining suite ran; retry once edits settle",
                )
                .into());
            }
            executed_summaries.push(FileRunSummary {
                test_file: "<direct_pytest>".to_string(),
                dep_hash: String::new(),
                outcome: if py_mix.skipped > 0 && py_mix.passed == 0 {
                    TestOutcome::Skipped
                } else {
                    TestOutcome::Passed
                },
                source: "direct_pytest_override".to_string(),
                passed: py_mix.passed,
                failed: 0,
                skipped: py_mix.skipped,
                errors: 0,
                duration_ms: py_mix.duration_ms.max(1),
                failed_nodeids: Vec::new(),
            });
            if !py_mix.stdout_summary.trim().is_empty() {
                if delta_stdout.is_empty() {
                    delta_stdout = py_mix.stdout_summary;
                } else {
                    delta_stdout = format!("{}\n{}", py_mix.stdout_summary, delta_stdout);
                }
            }
        }

        // 5. Aggregate final counts and build pytest-compatible summary
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut errors = 0usize;
        let mut failed_nodeids = Vec::new();
        let mut failed_nodeids_seen = std::collections::HashSet::new();
        let mut any_failed_file = false;

        for item in cached_summaries.iter().chain(executed_summaries.iter()) {
            passed += item.passed;
            failed += item.failed;
            skipped += item.skipped;
            errors += item.errors;
            if !item.outcome.is_green() {
                any_failed_file = true;
            }
            for nid in &item.failed_nodeids {
                if failed_nodeids_seen.insert(nid.as_str()) {
                    failed_nodeids.push(nid.clone());
                }
            }
        }

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = if any_failed_file || failed > 0 || errors > 0 {
            "failed".to_string()
        } else {
            "passed".to_string()
        };
        if status == "passed" && learn_invocation {
            let mut guard = self.lock_state();
            guard.cache.record_invocation(&target_test_files);
        }
        let mode = if executed_summaries.is_empty() {
            "instant_cache_hit".to_string()
        } else if cached_summaries.is_empty() {
            "full_run".to_string()
        } else {
            "hybrid_delta".to_string()
        };

        let errors_segment = if errors > 0 {
            format!(", {errors} errors")
        } else {
            String::new()
        };
        let summary_line = format!(
            "=== {} ({} passed, {} failed{}, {} skipped) in {:.3}s [test-context: {} cached files, {} executed files, saved {:.2}s] ===",
            status.to_uppercase(),
            passed,
            failed,
            errors_segment,
            skipped,
            duration_ms as f64 / 1000.0,
            cached_summaries.len(),
            executed_summaries.len(),
            saved_ms as f64 / 1000.0
        );
        let mut output_parts = Vec::new();
        if !ruff_diagnostics.is_empty() {
            let mut ruff_lines = vec![format!(
                "[ruff] {} pre-flight lint/syntax issue(s) detected (pass ruff_fix=true or run `tctx ruff --fix` to auto-fix safe rules):",
                ruff_diagnostics.len()
            )];
            for diag in ruff_diagnostics.iter().take(10) {
                let fix_tag = if diag.fixable { " [fixable]" } else { "" };
                ruff_lines.push(format!(
                    "  {}:{}:{}: {} {}{}",
                    diag.file, diag.row, diag.column, diag.code, diag.message, fix_tag
                ));
            }
            output_parts.push(ruff_lines.join("\n"));
        }
        for cached_out in cached_failure_outputs {
            if !cached_out.trim().is_empty() {
                output_parts.push(cached_out.trim().to_string());
            }
        }
        let effective_delta_stdout = if status == "passed" && skipped == 0 {
            compact_passed_delta_stdout(&delta_stdout)
        } else {
            delta_stdout.trim().to_string()
        };
        if !effective_delta_stdout.is_empty() {
            output_parts.push(effective_delta_stdout);
        }
        if skipped > 0 {
            output_parts.push(format!(
                "[test-context] {skipped} skipped tests are never cached or treated as verified. TCTX runs pytest in the daemon environment, so tests that skip for a missing variable or service (see the SKIPPED lines above) must be run directly with the project's own wrapper."
            ));
        }
        if cached_summaries.iter().any(|item| !item.outcome.is_green()) {
            output_parts.push(
                "[test-context] A failure above was replayed from the cache. Pass rerun_failed (or no_cache) to re-execute it if it may depend on environment."
                    .to_string(),
            );
        }
        output_parts.push(summary_line);
        let stdout_summary = output_parts.join("\n");

        let mut structured_failures = Vec::new();
        let mut suspected_culprit_files = BTreeSet::new();
        let mut untested_modified_files = Vec::new();
        let flaky_tests: Vec<String>;
        let missing_prerequisites = if skipped > 0 {
            extract_missing_prerequisites(&stdout_summary)
        } else {
            Vec::new()
        };
        let warnings_summary = extract_warnings_summary(&stdout_summary);

        let dirty_for_failure: Vec<String> = if status == "failed" {
            git_dirty_files(&spec.root).unwrap_or_default()
        } else {
            Vec::new()
        };
        let target_set: std::collections::HashSet<&str> =
            target_test_files.iter().map(String::as_str).collect();
        {
            let guard = self.lock_state();
            flaky_tests = if target_set.is_empty() {
                guard.cache.flaky_test_files()
            } else {
                guard
                    .cache
                    .flaky_test_files()
                    .into_iter()
                    .filter(|tf| target_set.contains(tf.as_str()))
                    .collect()
            };
            if status == "failed" {
                for item in cached_summaries.iter().chain(executed_summaries.iter()) {
                    if !item.outcome.is_green() {
                        structured_failures.extend(extract_structured_failures(
                            &item.test_file,
                            &item.failed_nodeids,
                            &stdout_summary,
                        ));
                        let deps: std::collections::HashSet<String> = guard
                            .graph
                            .transitive_deps_for(&item.test_file)
                            .into_iter()
                            .collect();
                        for norm in expand_dirty_paths_for_graph(&guard.graph, &dirty_for_failure) {
                            if deps.contains(&norm) || norm == item.test_file {
                                suspected_culprit_files.insert(norm);
                            }
                        }
                        for raw_t in &req.targets {
                            let norm = raw_t
                                .split("::")
                                .next()
                                .unwrap_or(raw_t)
                                .trim()
                                .trim_start_matches("./")
                                .replace('\\', "/");
                            if !norm.is_empty() && deps.contains(&norm) && norm != item.test_file {
                                suspected_culprit_files.insert(norm);
                            }
                        }
                    }
                }
            }
            if !req.targets.is_empty() {
                untested_modified_files = guard.graph.untested_modified_files(&req.targets);
            }
        }

        let final_runner_exit_code = observed_runner_exit_code.or_else(|| {
            if status == "failed" {
                if req.language_filter.as_deref() == Some("rust") {
                    Some(101)
                } else {
                    Some(1)
                }
            } else {
                None
            }
        });
        Ok(TestRunResponse {
            root: spec.root,
            status,
            mode,
            total_test_files: target_test_files.len(),
            cached_files_count: cached_summaries.len(),
            joined_background_files_count: joined_bg_count,
            executed_files_count: executed_summaries.len(),
            passed,
            failed,
            skipped,
            errors,
            duration_ms,
            saved_ms,
            cached_files: cached_summaries,
            executed_files: executed_summaries,
            failed_nodeids,
            ruff_diagnostics,
            structured_failures,
            suspected_culprit_files: suspected_culprit_files.into_iter().collect(),
            untested_modified_files,
            flaky_tests,
            missing_prerequisites,
            warnings_summary,
            runner_exit_code: final_runner_exit_code,
            stdout_summary,
        })
    }
}

