use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_cargo_test_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    environment_hash: &str,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let mut explicit_cargo_target: Option<&str> = None;
    let mut explicit_cargo_cwd: Option<PathBuf> = None;
    let mut orig_request_targets: Vec<String> = Vec::new();
    let mut orig_request_was_empty = false;
    let mut has_orig_metadata = false;
    let filtered_extra_args: Vec<String> = extra_args
        .iter()
        .filter_map(|a| {
            if let Some(ct) = a.strip_prefix("__tctx_cargo_target__=") {
                explicit_cargo_target = Some(ct);
                None
            } else if let Some(cwd) = a.strip_prefix("__tctx_cargo_cwd__=") {
                explicit_cargo_cwd = Some(PathBuf::from(cwd));
                None
            } else if let Some(ot) = a.strip_prefix("__tctx_orig_target__=") {
                has_orig_metadata = true;
                orig_request_targets.push(ot.to_string());
                None
            } else if a == "__tctx_orig_empty__" {
                has_orig_metadata = true;
                orig_request_was_empty = true;
                None
            } else if a.starts_with("__tctx_fe_runner__=") {
                None
            } else {
                Some(a.clone())
            }
        })
        .collect();
    let filtered_extra_args = normalize_cargo_config_args(&spec.root, &filtered_extra_args);
    let extra_args = filtered_extra_args.as_slice();
    let cargo_cmd = spec.effective_cargo_cmd();
    let Some((program, base_args)) = cargo_cmd.split_first() else {
        return Err(invalid("empty cargo_cmd").into());
    };
    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs);
    let compile_timeout = cargo_compile_timeout_for(spec);
    let mut records = Vec::new();
    let mut all_outputs = Vec::new();
    let has_explicit_node = invocation_targets.iter().any(|t| t.contains("::"));

    // Group eligible `--lib` unit test files by crate_dir so multiple unit test modules in the same crate
    // run in a single `cargo test --lib` invocation instead of spawning cargo N times.
    // When `-k` or harness `--` flags (e.g. `--exact`) are present, route through `individual_files`
    // (which reuses the single `--no-run` precompilation and intersects `-k` / `--exact` strictly
    // against each selected file's module ownership via `-- --list`).
    let has_k_or_harness_flags = k_expr.is_some_and(|k| !k.trim().is_empty())
        || extra_args.iter().any(|a| a == "--");
    let mut files_by_crate: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for test_file in dep_hashes.keys() {
        files_by_crate
            .entry(rust_crate_dir_on_disk(&spec.root, test_file))
            .or_default()
            .push(test_file.clone());
    }
    let mut crate_manifest_info: BTreeMap<String, CargoManifestTargets> = BTreeMap::new();
    for crate_dir in files_by_crate.keys() {
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(crate_dir)
        };
        crate_manifest_info.insert(crate_dir.clone(), parse_cargo_manifest_targets(&workdir));
    }
    // Filter out files ONLY when ALL target identities owning the file are disabled (via
    // `test = false` or unsatisfied `required-features`) on default runs (`explicit_cargo_target.is_none()`),
    // so an enabled `[lib]`, `[[test]]`, or `[[bin]]` sharing the same path is NEVER dropped!
    let is_feature_disabled_test_file = |crate_dir: &str, test_file: &str| -> bool {
        if explicit_cargo_target.is_some() {
            return false;
        }
        let Some(manifest) = crate_manifest_info.get(crate_dir) else {
            return false;
        };
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(crate_dir)
        };
        let rel_in = rel_path_in_crate(crate_dir, test_file);
        let is_lib = manifest.lib_path.as_deref() == Some(rel_in);
        let has_enabled_lib = is_lib && manifest.lib_test_enabled;
        let test_names = manifest.custom_tests.get(rel_in);
        let has_enabled_test = test_names.is_some_and(|names| {
            names.iter().any(|tn| {
                !manifest.test_disabled.contains(tn)
                    && manifest
                        .custom_test_required_features
                        .get(tn)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            })
        });
        let bin_names = manifest.custom_bins.get(rel_in);
        let has_enabled_bin = bin_names.is_some_and(|names| {
            names.iter().any(|bn| {
                !manifest.bin_test_disabled.contains(bn)
                    && manifest
                        .custom_bin_required_features
                        .get(bn)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            })
        });
        // Also check auto-discovered targets (`src/main.rs`, `src/bin/*`, `tests/*`) so a disabled
        // `[lib] path = "src/main.rs", test = false` NEVER hides an enabled auto-discovered binary!
        let has_enabled_auto_main_bin = rel_in == "src/main.rs"
            && manifest.main_bin.as_ref().is_some_and(|mb| {
                !manifest.bin_test_disabled.contains(mb)
                    && manifest
                        .custom_bin_required_features
                        .get(mb)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            });
        let has_enabled_auto_src_bin = manifest.autobins
            && rel_in
                .strip_prefix("src/bin/")
                .and_then(rust_bin_name_from_bin_rel)
                .is_some_and(|bn| {
                    !manifest.bin_test_disabled.contains(&bn)
                        && manifest
                            .custom_bin_required_features
                            .get(&bn)
                            .is_none_or(|rf| {
                                cargo_required_features_satisfied(&workdir, rf, extra_args)
                            })
                });
        let has_enabled_auto_integ = manifest.autotests
            && crate::ast_graph::rust_integration_test_target_name(test_file).is_some_and(|tn| {
                !manifest.test_disabled.contains(&tn)
                    && manifest
                        .custom_test_required_features
                        .get(&tn)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            });
        if has_enabled_lib
            || has_enabled_test
            || has_enabled_bin
            || has_enabled_auto_main_bin
            || has_enabled_auto_src_bin
            || has_enabled_auto_integ
        {
            return false;
        }
        if is_lib
            || test_names.is_some_and(|n| !n.is_empty())
            || bin_names.is_some_and(|n| !n.is_empty())
        {
            return true;
        }
        match resolve_rust_child_target_ownership(&workdir, manifest, rel_in, explicit_cargo_target)
        {
            RustTargetChildOwnership::UniqueTest(target_name, _) => {
                manifest.test_disabled.contains(&target_name)
                    || manifest
                        .custom_test_required_features
                        .get(&target_name)
                        .is_some_and(|rf| {
                            !cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            }
            RustTargetChildOwnership::UniqueBin(bin_name, _) if !bin_name.is_empty() => {
                manifest.bin_test_disabled.contains(&bin_name)
                    || manifest
                        .custom_bin_required_features
                        .get(&bin_name)
                        .is_some_and(|rf| {
                            !cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            }
            RustTargetChildOwnership::MultipleBins(bin_list) if !bin_list.is_empty() => bin_list
                .iter()
                .all(|(bn, _)| {
                    !bn.is_empty()
                        && (manifest.bin_test_disabled.contains(bn)
                            || manifest
                                .custom_bin_required_features
                                .get(bn)
                                .is_some_and(|rf| {
                                    !cargo_required_features_satisfied(&workdir, rf, extra_args)
                                }))
                }),
            RustTargetChildOwnership::UniqueLib(_) => !manifest.lib_test_enabled,
            _ => false,
        }
    };
    // When an MCP/Native `test_run` targets >= 2 crates in a Cargo workspace (including testless
    // selected packages!) that requires unified feature resolution, execute a single unified
    // `cargo test -p <pkg1> -p <pkg2> ...` from `spec.root` instead of splitting per-member!
    let exec_test_files: Vec<String> = dep_hashes.keys().cloned().collect();
    let orig_slice_for_unif: &[String] = if has_orig_metadata {
        if orig_request_was_empty {
            &[]
        } else {
            orig_request_targets.as_slice()
        }
    } else {
        invocation_targets
    };
    let selected_crate_dirs_for_unif =
        requested_workspace_crate_dirs(&spec.root, orig_slice_for_unif, &exec_test_files);
    if is_cargo_workspace_root(&spec.root)
        && selected_crate_dirs_for_unif.len() >= 2
        && workspace_needs_unified_cargo_features(
            &spec.root,
            &selected_crate_dirs_for_unif,
            extra_args,
        )
    {
        // Check the ORIGINAL user request targets (NOT the expanded `invocation_targets` `.rs`
        // list!) so directory targets (`["crates/a", "crates/b"]`) and full-workspace runs (`[]`)
        // execute cleanly while explicit `.rs` file/node selections across multiple unified crates
        // fail closed instead of silently widening!
        let has_specific_rs_file_targets = has_explicit_node
            || orig_slice_for_unif
                .iter()
                .any(|t| t.split("::").next().unwrap_or(t).ends_with(".rs"));
        if has_specific_rs_file_targets {
            return Err(invalid(
                "UNIFIED_WORKSPACE_FILE_SCOPING: Selected .rs file or node targets span multiple Cargo workspace crates with unified features; target whole packages/workspace or run each crate separately.",
            )
            .into());
        }
        let mut pkg_names: Vec<String> = Vec::new();
        for crate_dir in &selected_crate_dirs_for_unif {
            let cdir = if crate_dir.is_empty() || crate_dir == "." {
                spec.root.clone()
            } else {
                spec.root.join(crate_dir)
            };
            let Some(pn) = cargo_manifest_package_name(&cdir)
                .or_else(|| cargo_manifest_package_name_via_metadata(&cdir))
            else {
                return Err(invalid(format!(
                    "UNRESOLVED_CARGO_PACKAGE_NAME: Cannot resolve Cargo package name for selected workspace crate '{crate_dir}'"
                ))
                .into());
            };
            if !pkg_names.contains(&pn) {
                pkg_names.push(pn);
            }
        }
        if pkg_names.len() >= 2 {
            let mut cmd = Command::new(program);
            cmd.current_dir(&spec.root);
            apply_cargo_exec_env(&mut cmd, source_label);
            cmd.args(base_args);
            cmd.arg("test");
            for pn in &pkg_names {
                cmd.arg("-p").arg(pn);
            }
            if let Some(ct) = explicit_cargo_target {
                if ct == "lib" {
                    cmd.arg("--lib");
                } else if ct == "bins" {
                    cmd.arg("--bins");
                } else if ct == "doc" {
                    cmd.arg("--doc");
                } else if let Some(b) = ct.strip_prefix("bin:") {
                    cmd.arg("--bin").arg(b);
                } else if let Some(t) = ct.strip_prefix("test:") {
                    cmd.arg("--test").arg(t);
                }
            }
            let sep_idx = extra_args.iter().position(|a| a == "--");
            let cargo_part = sep_idx.map_or(extra_args, |i| &extra_args[..i]);
            let harness_part = sep_idx.map_or(&[][..], |i| &extra_args[i + 1..]);
            for a in cargo_part {
                cmd.arg(a);
            }
            let has_k = k_expr.is_some_and(|k| !k.trim().is_empty());
            let has_ign = mark_expr.is_some_and(|m| m == "ignore" || m == "ignored");
            if has_k || has_ign || !harness_part.is_empty() {
                cmd.arg("--");
                if let Some(k) = k_expr.map(str::trim).filter(|k| !k.is_empty()) {
                    cmd.arg(k);
                }
                if has_ign {
                    cmd.arg("--ignored");
                }
                for a in harness_part {
                    cmd.arg(a);
                }
            }
            let run_started = now_unix_ms();
            let exec_timeout = Duration::from_secs(spec.policy.pytest_timeout_secs.max(1));
            let unified_has_no_run = cargo_part.iter().any(|a| a == "--no-run");
            let unified_has_doc =
                cargo_part.iter().any(|a| a == "--doc") || explicit_cargo_target == Some("doc");
            let unified_run_timeout = if unified_has_no_run {
                compile_timeout
            } else {
                // Always precompile unified workspace under `compile_timeout` and execute under
                // `exec_timeout` so short `TCTX_CARGO_COMPILE_TIMEOUT_SECS` NEVER kills a longer
                // test AND short `exec_timeout` NEVER allows a hanging test to run 600s!
                let mut pre_cmd = Command::new(program);
                pre_cmd.current_dir(&spec.root);
                apply_cargo_exec_env(&mut pre_cmd, source_label);
                pre_cmd.args(base_args);
                if unified_has_doc {
                    pre_cmd.arg("build").arg("--lib");
                } else {
                    pre_cmd.arg("test").arg("--no-run");
                }
                for pn in &pkg_names {
                    pre_cmd.arg("-p").arg(pn);
                }
                if !unified_has_doc
                    && let Some(ct) = explicit_cargo_target
                {
                    if ct == "lib" {
                        pre_cmd.arg("--lib");
                    } else if ct == "bins" {
                        pre_cmd.arg("--bins");
                    } else if let Some(b) = ct.strip_prefix("bin:") {
                        pre_cmd.arg("--bin").arg(b);
                    } else if let Some(t) = ct.strip_prefix("test:") {
                        pre_cmd.arg("--test").arg(t);
                    }
                }
                for a in cargo_part {
                    if a != "--no-run"
                        && (!unified_has_doc || (a != "--doc" && a != "--no-fail-fast"))
                    {
                        pre_cmd.arg(a);
                    }
                }
                let pre_out = run_command_with_timeout_tracked(
                    &mut pre_cmd,
                    compile_timeout,
                    8 * 1024 * 1024,
                    active_pgid,
                    active_pgids,
                )?;
                if pre_out.cancelled {
                    return Err(invalid(
                        "TEST_CANCELLED: cargo test child process group was terminated",
                    )
                    .into());
                }
                if pre_out.timed_out {
                    return Err(invalid(format!(
                        "cargo test unified workspace compilation timed out after {}s",
                        compile_timeout.as_secs()
                    ))
                    .into());
                }
                if !pre_out.status.success() {
                    let stdout_s = String::from_utf8_lossy(&pre_out.stdout);
                    let stderr_s = String::from_utf8_lossy(&pre_out.stderr);
                    let combined = match (stdout_s.trim().is_empty(), stderr_s.trim().is_empty()) {
                        (true, _) => stderr_s.to_string(),
                        (_, true) => stdout_s.to_string(),
                        (false, false) => format!("{}\n{}", stdout_s.trim(), stderr_s.trim()),
                    };
                    let now = now_unix_ms();
                    let snippet = truncate_str(&combined, 8192);
                    for (idx, (tf, dh)) in dep_hashes.iter().enumerate() {
                        records.push(CachedTestRecord {
                            test_file: tf.clone(),
                            dep_hash: dh.clone(),
                            outcome: TestOutcome::Error,
                            passed: 0,
                            failed: 0,
                            skipped: 0,
                            errors: if idx == 0 { 1 } else { 0 },
                            duration_ms: pre_out.duration_ms.max(1),
                            updated_unix_ms: now,
                            started_unix_ms: run_started,
                            completed_unix_ms: now,
                            tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                            source: source_label.to_string(),
                            failed_nodeids: vec![format!("compile:{tf}")],
                            output_snippet: snippet.clone(),
                            environment_hash: environment_hash.to_string(),
                            runner_mode: "cargo_test".to_string(),
                            dynamic_dependencies: Vec::new(),
                            dynamic_dependencies_complete: true,
                            node_results: BTreeMap::new(),
                            external_module_stamps: BTreeMap::new(),
                            exit_code: Some(
                                pre_out
                                    .status
                                    .code()
                                    .filter(|&c| c != 0)
                                    .unwrap_or(101),
                            ),
                        });
                    }
                    return Ok((records, snippet));
                }
                if unified_has_doc {
                    compile_timeout.saturating_add(exec_timeout)
                } else {
                    exec_timeout
                }
            };
            let out = run_command_with_timeout_tracked(
                &mut cmd,
                unified_run_timeout,
                8 * 1024 * 1024,
                active_pgid,
                active_pgids,
            )?;
            if out.cancelled {
                return Err(invalid(
                    "TEST_CANCELLED: cargo test child process group was terminated",
                )
                .into());
            }
            if out.timed_out {
                return Err(invalid(format!(
                    "cargo test unified workspace execution timed out after {}s",
                    unified_run_timeout.as_secs()
                ))
                .into());
            }
            let stdout_s = String::from_utf8_lossy(&out.stdout);
            let stderr_s = String::from_utf8_lossy(&out.stderr);
            let combined = match (stdout_s.trim().is_empty(), stderr_s.trim().is_empty()) {
                (true, _) => stderr_s.to_string(),
                (_, true) => stdout_s.to_string(),
                (false, false) => format!("{}\n{}", stdout_s.trim(), stderr_s.trim()),
            };
            let ok = out.status.success();
            let stats = parse_cargo_test_output(&combined, "workspace", out.duration_ms, ok);
            let now = now_unix_ms();
            let overall_outcome = if !ok || stats.failed > 0 {
                TestOutcome::Failed
            } else if stats.errors > 0 {
                TestOutcome::Error
            } else if stats.passed > 0 && stats.skipped == 0 {
                TestOutcome::Passed
            } else {
                TestOutcome::Skipped
            };
            let snippet = truncate_str(&combined, 8192);
            for (idx, (tf, dh)) in dep_hashes.iter().enumerate() {
                records.push(CachedTestRecord {
                    test_file: tf.clone(),
                    dep_hash: dh.clone(),
                    outcome: overall_outcome,
                    passed: if idx == 0 { stats.passed } else { 0 },
                    failed: if idx == 0 { stats.failed } else { 0 },
                    skipped: if idx == 0 { stats.skipped } else { 0 },
                    errors: if idx == 0 { stats.errors } else { 0 },
                    duration_ms: if idx == 0 { stats.duration_ms.max(1) } else { 1 },
                    failed_nodeids: if idx == 0 {
                        stats.failed_nodeids.clone()
                    } else {
                        Vec::new()
                    },
                    output_snippet: if idx == 0 && !overall_outcome.is_green() {
                        snippet.clone()
                    } else {
                        String::new()
                    },
                    updated_unix_ms: now,
                    source: source_label.to_string(),
                    environment_hash: environment_hash.to_string(),
                    tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                    runner_mode: "cargo_test_unified".to_string(),
                    started_unix_ms: run_started,
                    completed_unix_ms: now,
                    dynamic_dependencies: Vec::new(),
                    // Unified multi-package results are never persisted as standalone per-file cache entries.
                    dynamic_dependencies_complete: false,
                    node_results: BTreeMap::new(),
                    external_module_stamps: BTreeMap::new(),
                    exit_code: if overall_outcome.is_green()
                        || overall_outcome == TestOutcome::Skipped
                    {
                        None
                    } else {
                        Some(out.status.code().filter(|&c| c != 0).unwrap_or(101))
                    },
                });
            }
            return Ok((records, snippet));
        }
    }
    for (crate_dir, crate_files) in &mut files_by_crate {
        crate_files.retain(|tf| !is_feature_disabled_test_file(crate_dir, tf));
    }

    let mut lib_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut individual_files: Vec<String> = Vec::new();

    for test_file in dep_hashes.keys() {
        let crate_dir = rust_crate_dir_on_disk(&spec.root, test_file);
        if is_feature_disabled_test_file(&crate_dir, test_file) {
            continue;
        }
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let rel_in = rel_path_in_crate(&crate_dir, test_file);
        let manifest = crate_manifest_info.get(&crate_dir);
        let has_lib = manifest.is_some_and(|m| m.lib_path.is_some());
        let is_custom_lib_crate = manifest
            .and_then(|m| m.lib_path.as_deref())
            .is_some_and(|lp| lp != "src/lib.rs");
        let is_custom_bin = manifest.is_some_and(|m| m.custom_bins.contains_key(rel_in));
        let is_custom_test = manifest.is_some_and(|m| m.custom_tests.contains_key(rel_in));
        let child_owner = manifest
            .map(|m| {
                resolve_rust_child_target_ownership(&workdir, m, rel_in, explicit_cargo_target)
            })
            .unwrap_or(RustTargetChildOwnership::None);
        let is_non_lib_child = matches!(
            child_owner,
            RustTargetChildOwnership::UniqueTest(_, _)
                | RustTargetChildOwnership::UniqueBin(_, _)
                | RustTargetChildOwnership::MultipleBins(_)
                | RustTargetChildOwnership::Ambiguous
        );
        let autotests = manifest.is_none_or(|m| m.autotests);
        let is_integration_owned = is_custom_test
            || (autotests
                && rust_integration_test_owner(&spec.root, &crate_dir, test_file).is_some());
        let is_under_src = rel_in.starts_with("src/");
        // Only group standard submodule files (`rust_module_filter_for_file(test_file).is_some()`)
        // in crates WITHOUT `#[path]` overrides; crate roots (`src/lib.rs`) and `#[path]` crates
        // run via `individual_files` (reusing the single `--no-run` precompilation) with exact
        // per-file ownership scoping so an unselected sibling module can never run or fail the request.
        let crate_has_custom_path = crate_uses_custom_mod_path(&spec.root, &crate_dir);
        let lib_has_custom_harness =
            manifest.is_some_and(|m| m.harness_false_targets.contains("lib"));
        let is_lib_unit = !has_explicit_node
            && !has_k_or_harness_flags
            && is_under_src
            && rust_module_filter_for_file(test_file).is_some()
            && !crate_has_custom_path
            && !lib_has_custom_harness
            && !is_integration_owned
            && !is_custom_bin
            && !is_custom_lib_crate
            && !is_non_lib_child
            && !test_file.ends_with("src/main.rs")
            && test_file != "src/main.rs"
            && !test_file.contains("src/bin/")
            && has_lib;
        if is_lib_unit {
            lib_groups
                .entry(crate_dir)
                .or_default()
                .push(test_file.clone());
        } else {
            individual_files.push(test_file.clone());
        }
    }

    // When a crate has multiple cargo test invocations (e.g. `--lib` unit tests plus one or more
    // `--test <name>` integration tests), pre-compile all targeted artifacts in a single multi-core
    // `cargo test --no-run` pass so Cargo's jobserver builds and links them concurrently once.
    let mut precompiled_failed_crates: BTreeMap<String, (String, u64, i32)> = BTreeMap::new();
    let mut precompiled_executables: BTreeMap<(String, RustArtifactKind), PathBuf> = BTreeMap::new();
    let mut precompiled_duration_ms: BTreeMap<String, u64> = BTreeMap::new();

    let mut workspace_batched_crates: BTreeSet<String> = BTreeSet::new();
    if files_by_crate.len() >= 2 && extra_args.is_empty() && is_cargo_workspace_root(&spec.root) {
        // Only batch crates that strictly need `--lib` so we never compile unrelated `--tests` or `--bins`
        // across sibling packages; crates with integration tests or bins use per-crate targeted precompilation below.
        let mut ws_packages: Vec<(String, String)> = Vec::new();
        for (crate_dir, crate_files) in &files_by_crate {
            let manifest = crate_manifest_info.get(crate_dir);
            let all_lib = manifest.is_some_and(|m| m.lib_path.is_some())
                && crate_files.iter().all(|tf| {
                    let rel_in = rel_path_in_crate(crate_dir, tf);
                    rust_integration_test_owner(&spec.root, crate_dir, tf).is_none()
                        && !manifest.is_some_and(|m| {
                            m.custom_bins.contains_key(rel_in)
                                || m.custom_tests.contains_key(rel_in)
                        })
                        && !tf.ends_with("src/main.rs")
                        && tf != "src/main.rs"
                        && !tf.contains("src/bin/")
                });
            // Exclude crates with their own crate-local `.cargo/config[.toml]` (or intermediate
            // subdir `.cargo/config[.toml]`) from workspace-root batching so Cargo loads their
            // crate-specific rustflags/env settings from `workdir`.
            if all_lib
                && !crate_has_local_cargo_config(&spec.root, crate_dir)
                && let Some(pkg_name) = manifest.and_then(|m| m.pkg_name.as_ref())
            {
                ws_packages.push((crate_dir.clone(), pkg_name.clone()));
            }
        }
        if ws_packages.len() >= 2 {
            let mut command = Command::new(program);
            command.current_dir(&spec.root);
            apply_cargo_exec_env(&mut command, source_label);
            command.args(base_args);
            command
                .arg("test")
                .arg("--no-run")
                .arg("--message-format=json")
                .arg("--lib");
            for (_, pkg_name) in &ws_packages {
                command.arg("-p").arg(pkg_name);
            }
            let output = run_command_with_timeout_tracked(
                &mut command,
                compile_timeout,
                4 * 1024 * 1024,
                active_pgid,
                active_pgids,
            )?;
            if output.timed_out {
                return Err(invalid(format!(
                    "cargo test compilation timed out after {}s",
                    compile_timeout.as_secs()
                ))
                .into());
            }
            if output.cancelled {
                return Err(invalid(
                    "TEST_CANCELLED: cargo test child process group was terminated",
                )
                .into());
            }
            if output.status.success() {
                let parsed =
                    parse_cargo_test_executables_workspace_json(&output.stdout, &spec.root);
                let per_crate_ms = output.duration_ms / (ws_packages.len() as u64);
                for (crate_dir, _) in &ws_packages {
                    if parsed.contains_key(&(crate_dir.clone(), RustArtifactKind::Lib)) {
                        workspace_batched_crates.insert(crate_dir.clone());
                        precompiled_duration_ms.insert(crate_dir.clone(), per_crate_ms);
                    }
                }
                for (k, v) in parsed {
                    precompiled_executables.insert(k, v);
                }
            }
        }
    }

    for (crate_dir, crate_files) in &files_by_crate {
        if workspace_batched_crates.contains(crate_dir) {
            continue;
        }
        let lib_count = lib_groups.get(crate_dir).map_or(0, Vec::len);
        let indiv_count = crate_files.len().saturating_sub(if lib_count > 1 { lib_count } else { 0 });
        let total_invocations = usize::from(lib_count > 1) + indiv_count;
        if total_invocations == 0 {
            continue;
        }
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(crate_dir)
        };
        let manifest = crate_manifest_info.get(crate_dir);
        let main_bin_opt = manifest.and_then(|m| m.main_bin.clone());
        let has_lib = manifest.is_some_and(|m| m.lib_path.is_some());
        let mut need_lib = lib_count > 0;
        let mut need_bins = false;
        let mut specific_bins = BTreeSet::new();
        let mut specific_tests = BTreeSet::new();
        for tf in crate_files {
            let rel_in = rel_path_in_crate(crate_dir, tf);
            let child_owner = manifest
                .map(|m| {
                    resolve_rust_child_target_ownership(&workdir, m, rel_in, explicit_cargo_target)
                })
                .unwrap_or(RustTargetChildOwnership::None);
            let bin_is_feature_eligible = |bn: &str| -> bool {
                manifest
                    .and_then(|m| m.custom_bin_required_features.get(bn))
                    .is_none_or(|rf| cargo_required_features_satisfied(&workdir, rf, extra_args))
            };
            let test_is_feature_eligible = |tn: &str| -> bool {
                manifest
                    .and_then(|m| m.custom_test_required_features.get(tn))
                    .is_none_or(|rf| cargo_required_features_satisfied(&workdir, rf, extra_args))
            };
            if explicit_cargo_target == Some("lib") {
                need_lib = true;
            } else if explicit_cargo_target == Some("bins") {
                if let Some(custom_bins) = manifest.and_then(|m| m.custom_bins.get(rel_in)) {
                    for bn in custom_bins {
                        if bin_is_feature_eligible(bn) {
                            specific_bins.insert(bn.clone());
                        }
                    }
                } else if let RustTargetChildOwnership::MultipleBins(ref bin_list) = child_owner {
                    for (bn, _) in bin_list {
                        if !bn.is_empty() {
                            if bin_is_feature_eligible(bn) {
                                specific_bins.insert(bn.clone());
                            }
                        } else {
                            need_bins = true;
                        }
                    }
                } else if let RustTargetChildOwnership::UniqueBin(ref bn, _) = child_owner {
                    if !bn.is_empty() {
                        if bin_is_feature_eligible(bn) {
                            specific_bins.insert(bn.clone());
                        }
                    } else {
                        need_bins = true;
                    }
                } else if (tf.ends_with("src/main.rs") || tf == "src/main.rs")
                    && let Some(ref main_bin) = main_bin_opt
                {
                    if bin_is_feature_eligible(main_bin) {
                        specific_bins.insert(main_bin.clone());
                    }
                } else if let Some((_, bin_rel)) = tf.rsplit_once("src/bin/")
                    && let Some(bin_name) = rust_bin_name_from_bin_rel(bin_rel)
                {
                    if bin_is_feature_eligible(&bin_name) {
                        specific_bins.insert(bin_name);
                    }
                } else {
                    need_bins = true;
                }
            } else if let Some(wanted_bin) =
                explicit_cargo_target.and_then(|ct| ct.strip_prefix("bin:"))
            {
                specific_bins.insert(wanted_bin.to_string());
            } else if let Some(wanted_test) =
                explicit_cargo_target.and_then(|ct| ct.strip_prefix("test:"))
            {
                specific_tests.insert(wanted_test.to_string());
            } else if let RustTargetChildOwnership::MultipleBins(ref bin_list) = child_owner {
                for (bn, _) in bin_list {
                    if !bn.is_empty() {
                        if manifest.is_none_or(|m| !m.bin_test_disabled.contains(bn))
                            && bin_is_feature_eligible(bn)
                        {
                            specific_bins.insert(bn.clone());
                        }
                    } else {
                        need_bins = true;
                    }
                }
            } else if let Some(custom_tests) = manifest.and_then(|m| m.custom_tests.get(rel_in)) {
                for tn in custom_tests {
                    let eligible = manifest.is_none_or(|m| !m.test_disabled.contains(tn))
                        && test_is_feature_eligible(tn);
                    if eligible {
                        specific_tests.insert(tn.clone());
                    }
                }
                if manifest.is_some_and(|m| {
                    m.lib_test_enabled && m.lib_path.as_deref() == Some(rel_in)
                }) {
                    need_lib = true;
                }
                if let Some(custom_bins) = manifest.and_then(|m| m.custom_bins.get(rel_in)) {
                    for bn in custom_bins {
                        let eligible = manifest.is_none_or(|m| !m.bin_test_disabled.contains(bn))
                            && bin_is_feature_eligible(bn);
                        if eligible {
                            specific_bins.insert(bn.clone());
                        }
                    }
                }
            } else if let RustTargetChildOwnership::UniqueTest(target_name, _) = child_owner {
                if manifest.is_none_or(|m| !m.test_disabled.contains(&target_name))
                    && test_is_feature_eligible(&target_name)
                {
                    specific_tests.insert(target_name);
                }
            } else if let RustTargetChildOwnership::UniqueBin(bin_name, _) = child_owner {
                if !bin_name.is_empty() {
                    if manifest.is_none_or(|m| !m.bin_test_disabled.contains(&bin_name))
                        && bin_is_feature_eligible(&bin_name)
                    {
                        specific_bins.insert(bin_name);
                    }
                } else {
                    need_bins = true;
                }
            } else if manifest.is_none_or(|m| m.autotests)
                && let Some((target_name, _)) =
                    rust_integration_test_owner(&spec.root, crate_dir, tf)
            {
                specific_tests.insert(target_name);
            } else if let Some(custom_bins) = manifest.and_then(|m| m.custom_bins.get(rel_in)) {
                for bn in custom_bins {
                    let eligible = explicit_cargo_target.is_some()
                        || (manifest.is_none_or(|m| !m.bin_test_disabled.contains(bn))
                            && manifest
                                .and_then(|m| m.custom_bin_required_features.get(bn))
                                .is_none_or(|rf| {
                                    cargo_required_features_satisfied(&workdir, rf, extra_args)
                                }));
                    if eligible {
                        specific_bins.insert(bn.clone());
                    }
                }
                if manifest.is_some_and(|m| {
                    m.lib_test_enabled && m.lib_path.as_deref() == Some(rel_in)
                }) {
                    need_lib = true;
                }
            } else if tf.ends_with("src/main.rs") || tf == "src/main.rs" {
                if let Some(ref main_bin) = main_bin_opt {
                    specific_bins.insert(main_bin.clone());
                } else {
                    need_bins = true;
                }
            } else if let Some((_, bin_rel)) = tf.rsplit_once("src/bin/")
                && let Some(bin_name) = rust_bin_name_from_bin_rel(bin_rel)
            {
                specific_bins.insert(bin_name);
            } else if has_lib
                && (rel_in.starts_with("src/")
                    || manifest
                        .and_then(|m| m.lib_path.as_deref())
                        .is_some_and(|lp| lp == rel_in))
            {
                need_lib = true;
            } else if let Some(ref main_bin) = main_bin_opt {
                specific_bins.insert(main_bin.clone());
            } else {
                need_bins = true;
            }
        }
        let extra_has_pkg = extra_args
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "-p" || a == "--package" || a.starts_with("-p=") || a.starts_with("--package="));
        let (pre_cwd, pre_pkg_for_cwd): (&Path, Option<String>) =
            if let Some(ref ecwd) = explicit_cargo_cwd
                && ecwd != &workdir
                && !extra_has_pkg
                && let Some(pn) = cargo_manifest_package_name(&workdir)
            {
                (ecwd.as_path(), Some(pn))
            } else if !extra_has_pkg
                && is_cargo_workspace_root(&spec.root)
                && let Some(pn) = cargo_manifest_package_name(&workdir)
            {
                (workdir.as_path(), Some(pn))
            } else {
                (workdir.as_path(), None)
            };
        let mut command = Command::new(program);
        command.current_dir(pre_cwd);
        apply_cargo_exec_env(&mut command, source_label);
        command.args(base_args);
        command.arg("test").arg("--no-run").arg("--message-format=json");
        if let Some(ref pn) = pre_pkg_for_cwd {
            command.arg("-p").arg(pn);
        }
        if need_lib {
            command.arg("--lib");
        }
        if need_bins {
            command.arg("--bins");
        }
        for bin_name in &specific_bins {
            command.arg("--bin").arg(bin_name);
        }
        for test_name in &specific_tests {
            command.arg("--test").arg(test_name);
        }
        let mut ea_idx = 0usize;
        while ea_idx < extra_args.len() {
            let arg = &extra_args[ea_idx];
            if arg == "--" {
                break;
            }
            if arg == "--no-run" || arg.starts_with("--message-format=") {
                ea_idx += 1;
                continue;
            }
            if arg == "--message-format" {
                ea_idx += 2;
                continue;
            }
            command.arg(arg);
            ea_idx += 1;
        }
        let output = run_command_with_timeout_tracked(
            &mut command,
            compile_timeout,
            4 * 1024 * 1024,
            active_pgid,
            active_pgids,
        )?;
        if output.timed_out {
            return Err(invalid(format!(
                "cargo test compilation timed out after {}s",
                compile_timeout.as_secs()
            ))
            .into());
        }
        if output.cancelled {
            return Err(
                invalid("TEST_CANCELLED: cargo test child process group was terminated").into(),
            );
        }
        if !output.status.success() {
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty()) {
                (true, _) => stderr_str.to_string(),
                (_, true) => stdout_str.to_string(),
                (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
            };
            let fail_code = output.status.code().filter(|&c| c != 0).unwrap_or(101);
            precompiled_failed_crates
                .insert(crate_dir.clone(), (combined_out, output.duration_ms, fail_code));
        } else {
            precompiled_duration_ms.insert(crate_dir.clone(), output.duration_ms);
            for (kind, exe_path) in parse_cargo_test_executables_json(&output.stdout) {
                precompiled_executables.insert((crate_dir.clone(), kind), exe_path);
            }
        }
    }

    for (crate_dir, group_files) in lib_groups {
        if group_files.len() <= 1 {
            individual_files.extend(group_files);
            continue;
        }
        if precompiled_failed_crates.contains_key(&crate_dir) {
            individual_files.extend(group_files);
            continue;
        }
        let uses_custom_mod_path = crate_uses_custom_mod_path(&spec.root, &crate_dir);
        let run_started_unix_ms = now_unix_ms();
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        // Always execute via `cargo test` (reusing the already-precompiled artifacts in `target/`)
        // rather than invoking the test binary directly, so Cargo faithfully provides its full
        // runtime environment (`CARGO_MANIFEST_DIR`, `CARGO_PKG_*`, `CARGO_BIN_EXE_*`,
        // `LD_LIBRARY_PATH`, `[env]`, and `[target.*].runner`).
        let direct_lib_exe: Option<PathBuf> = None;
        let _ = cargo_config_disables_direct_exe(&spec.root, &crate_dir);
        let mut command = if let Some(ref exe_path) = direct_lib_exe {
            let mut cmd = Command::new(exe_path);
            cmd.current_dir(&workdir);
            apply_cargo_exec_env(&mut cmd, source_label);
            if let Some(k) = k_expr
                && !k.trim().is_empty()
            {
                cmd.arg(k);
            }
            if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                cmd.arg("--ignored");
            }
            if k_expr.is_none_or(|k| k.trim().is_empty())
                && !uses_custom_mod_path
                && let Some(module_filters) = group_files
                    .iter()
                    .map(|tf| rust_module_filter_for_file(tf))
                    .collect::<Option<Vec<String>>>()
                && !module_filters.is_empty()
            {
                cmd.args(&module_filters);
            }
            cmd
        } else {
            let (grp_cargo_extra, grp_harness_extra): (&[String], &[String]) =
                match extra_args.iter().position(|a| a == "--") {
                    Some(sep_idx) => (&extra_args[..sep_idx], &extra_args[sep_idx + 1..]),
                    None => (extra_args, &[]),
                };
            let grp_extra_has_pkg = grp_cargo_extra
                .iter()
                .any(|a| a == "-p" || a == "--package" || a.starts_with("-p=") || a.starts_with("--package="));
            let (grp_cwd, grp_pkg_for_cwd): (&Path, Option<String>) =
                if let Some(ref ecwd) = explicit_cargo_cwd
                    && ecwd != &workdir
                    && !grp_extra_has_pkg
                    && let Some(pn) = cargo_manifest_package_name(&workdir)
                {
                    (ecwd.as_path(), Some(pn))
                } else if !grp_extra_has_pkg
                    && is_cargo_workspace_root(&spec.root)
                    && let Some(pn) = cargo_manifest_package_name(&workdir)
                {
                    (workdir.as_path(), Some(pn))
                } else {
                    (workdir.as_path(), None)
                };
            let mut cmd = Command::new(program);
            cmd.current_dir(grp_cwd);
            apply_cargo_exec_env(&mut cmd, source_label);
            cmd.args(base_args);
            cmd.arg("test").arg("--lib");
            if let Some(ref pn) = grp_pkg_for_cwd {
                cmd.arg("-p").arg(pn);
            }
            if let Some(k) = k_expr
                && !k.trim().is_empty()
            {
                cmd.arg(k);
            }
            for arg in grp_cargo_extra {
                cmd.arg(arg);
            }
            let mut harness_args = Vec::new();
            if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                harness_args.push("--ignored".to_string());
            }
            let mut fallback_to_individual = false;
            if k_expr.is_none_or(|k| k.trim().is_empty())
                && grp_harness_extra.is_empty()
                && !uses_custom_mod_path
                && let Some(module_filters) = group_files
                    .iter()
                    .map(|tf| rust_module_filter_for_file(tf))
                    .collect::<Option<Vec<String>>>()
                && !module_filters.is_empty()
            {
                // Enumerate `-- --list` and pass `-- --exact` with exact prefix-matched test names
                // so libtest substring matching (e.g. `a::` matching `qa::test`) can never execute
                // or fail on unrelated sibling modules.
                let mut list_cmd = Command::new(program);
                list_cmd.current_dir(grp_cwd);
                apply_cargo_exec_env(&mut list_cmd, source_label);
                list_cmd.args(base_args);
                list_cmd.arg("test").arg("--lib");
                if let Some(ref pn) = grp_pkg_for_cwd {
                    list_cmd.arg("-p").arg(pn);
                }
                for arg in grp_cargo_extra {
                    list_cmd.arg(arg);
                }
                list_cmd.arg("--").arg("--list");
                const MAX_GRP_LIST_BYTES: usize = 8 * 1024 * 1024;
                let list_out = run_command_with_timeout_tracked(
                    &mut list_cmd,
                    timeout,
                    MAX_GRP_LIST_BYTES,
                    active_pgid,
                    active_pgids,
                )?;
                if list_out.cancelled {
                    return Err(invalid(
                        "TEST_CANCELLED: cargo test --list child process group was terminated",
                    )
                    .into());
                }
                if list_out.timed_out {
                    return Err(invalid(format!(
                        "cargo test --list timed out after {}s while scoping grouped lib modules",
                        timeout.as_secs()
                    ))
                    .into());
                }
                if list_out.status.success() && list_out.stdout.len() < MAX_GRP_LIST_BYTES {
                    let mut excluded_child_prefixes: Vec<String> = Vec::new();
                    let mut group_ambiguous_prefixes: Vec<(String, String)> = Vec::new();
                    for tf in &group_files {
                        if let Some(mod_pfx) = rust_module_filter_for_file(tf) {
                            let (child_pfxs, amb_pfxs) =
                                sibling_module_prefixes_and_ambiguity_for_root(
                                    &spec.root,
                                    &crate_dir,
                                    tf,
                                );
                            for cp in child_pfxs {
                                excluded_child_prefixes.push(format!("{mod_pfx}{cp}"));
                            }
                            for ap in amb_pfxs {
                                group_ambiguous_prefixes.push((format!("{mod_pfx}{ap}"), tf.clone()));
                            }
                        }
                    }
                    let listed = String::from_utf8_lossy(&list_out.stdout);
                    let mut exact_names: Vec<String> = Vec::new();
                    for line in listed.lines() {
                        let Some(name) = line.trim().strip_suffix(": test") else {
                            continue;
                        };
                        let best_selected_len = module_filters
                            .iter()
                            .filter(|pfx| name.starts_with(pfx.as_str()))
                            .map(String::len)
                            .max()
                            .unwrap_or(0);
                        if best_selected_len == 0 {
                            continue;
                        }
                        // Fail closed when a listed test matches an ambiguous conditional
                        // inline+external module overlap on a selected file (unless owned by a
                        // strictly more specific selected descendant).
                        if let Some((_, owner_file)) = group_ambiguous_prefixes
                            .iter()
                            .filter(|(ap, _)| {
                                name.starts_with(ap.as_str()) && ap.len() > best_selected_len
                            })
                            .max_by_key(|(ap, _)| ap.len())
                        {
                            return Err(invalid(format!(
                                "AMBIGUOUS_CONDITIONAL_MODULE: test '{name}' in {owner_file} belongs to a module declared both externally and inline under conditional compilation; cannot safely scope."
                            ))
                            .into());
                        }
                        // Longest-prefix precedence: if `a.rs` (`a::`) and `a/b/c.rs` (`a::b::c::`)
                        // are selected while intermediate `a/b.rs` (`a::b::`) is excluded,
                        // `a::b::c::t` has best_selected_len (9) > best_excluded_len (6) and is KEPT!
                        let best_excluded_len = excluded_child_prefixes
                            .iter()
                            .filter(|ex| name.starts_with(ex.as_str()))
                            .map(String::len)
                            .max()
                            .unwrap_or(0);
                        if best_selected_len > best_excluded_len {
                            exact_names.push(name.to_string());
                        }
                    }
                    harness_args.push("--exact".to_string());
                    if !exact_names.is_empty() {
                        harness_args.extend(exact_names);
                    } else {
                        // `-- --list` succeeded with 0 matching tests in selected modules; pass an
                        // unmatchable exact filter so libtest never falls back to running substring
                        // matches like `qa::test` for `a::`.
                        harness_args.push("__tctx_no_matching_lib_tests__".to_string());
                    }
                } else {
                    // Never fall back to substring `module_filters`; split into safely scoped
                    // individual runs instead.
                    fallback_to_individual = true;
                }
            }
            if fallback_to_individual {
                individual_files.extend(group_files);
                continue;
            }
            harness_args.extend(grp_harness_extra.iter().cloned());
            if !harness_args.is_empty() {
                cmd.arg("--");
                cmd.args(&harness_args);
            }
            cmd
        };
        // Once `--no-run` precompilation has already succeeded for this crate, enforce the
        // test execution `timeout` (not the 600s `compile_timeout`) so a hanging Rust test
        // terminates promptly at the configured deadline.
        let group_timeout = if direct_lib_exe.is_some()
            || precompiled_duration_ms.contains_key(&crate_dir)
        {
            timeout
        } else {
            compile_timeout
        };
        const CARGO_TEST_MAX_BYTES: usize = 2 * 1024 * 1024;
        let output = run_command_with_timeout_tracked(
            &mut command,
            group_timeout,
            CARGO_TEST_MAX_BYTES,
            active_pgid,
            active_pgids,
        )?;
        if output.timed_out {
            return Err(invalid(format!(
                "cargo test execution timed out after {}s",
                group_timeout.as_secs()
            ))
            .into());
        }
        if output.cancelled {
            return Err(
                invalid("TEST_CANCELLED: cargo test child process group was terminated").into(),
            );
        }
        let output_truncated = output.stdout.len() >= CARGO_TEST_MAX_BYTES
            || output.stderr.len() >= CARGO_TEST_MAX_BYTES;
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
        }
        let overall_success = output.status.success();
        let mut demuxed = if uses_custom_mod_path {
            // Run the crate's `--lib` suite ONCE and conservatively attribute its file-level
            // outcome to each grouped file while suppressing per-node cache entries (since
            // renamed `#[path = "..."]` module names prevent exact file->node attribution).
            let per_file_dur = (output.duration_ms / (group_files.len().max(1) as u64)).max(1);
            group_files
                .iter()
                .map(|tf| {
                    let mut stats =
                        parse_cargo_test_output(&combined_out, tf, per_file_dur, overall_success);
                    stats.node_results.clear();
                    (tf.clone(), stats)
                })
                .collect()
        } else {
            demux_cargo_lib_output(&combined_out, &group_files, output.duration_ms, overall_success)
        };
        let crate_total_files = files_by_crate
            .get(&crate_dir)
            .map_or(group_files.len(), Vec::len)
            .max(1) as u64;
        let compile_share_ms = if direct_lib_exe.is_some() {
            precompiled_duration_ms
                .get(&crate_dir)
                .copied()
                .unwrap_or(0)
                / crate_total_files
        } else {
            0
        };
        let now = now_unix_ms();
        let group_exit_code = output.status.code().filter(|&c| c != 0).unwrap_or(101);
        let mut group_recorded_failure = false;
        let mut first_unreported_group_file: Option<(String, String)> = None;
        let mut first_group_file: Option<(String, String)> = None;
        for test_file in group_files {
            let Some(dep_hash) = dep_hashes.get(&test_file) else {
                continue;
            };
            if first_group_file.is_none() {
                first_group_file = Some((test_file.clone(), dep_hash.clone()));
            }
            let mut stats = demuxed.remove(&test_file).unwrap_or_default();
            if output_truncated {
                stats.node_results.clear();
            }
            // Clear per-node cache entries on ANY file in the grouped `--lib` run that declares
            // external child modules (e.g. `src/lib.rs` OR parent module `src/a.rs` declaring `mod b;`)
            // so a child module's test can never be cached under its parent file's node cache.
            if !sibling_module_prefixes_for_crate_root(&spec.root, &crate_dir, &test_file)
                .is_empty()
            {
                stats.node_results.clear();
            }
            if stats.passed == 0
                && stats.failed == 0
                && stats.skipped == 0
                && stats.errors == 0
                && !overall_success
            {
                if first_unreported_group_file.is_none() {
                    first_unreported_group_file = Some((test_file.clone(), dep_hash.clone()));
                }
                continue;
            }
            let zero_test_filtered = stats.passed == 0
                && stats.failed == 0
                && stats.skipped == 0
                && stats.errors == 0
                && rust_module_filter_for_file(&test_file).is_some();
            let outcome = if stats.failed > 0 {
                TestOutcome::Failed
            } else if stats.errors > 0 {
                TestOutcome::Error
            } else if stats.passed > 0 && stats.skipped == 0 {
                TestOutcome::Passed
            } else if (stats.skipped > 0 || zero_test_filtered) && overall_success {
                TestOutcome::Skipped
            } else if overall_success {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };
            if !outcome.is_green() && outcome != TestOutcome::Skipped {
                group_recorded_failure = true;
            }
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: stats.passed,
                failed: stats.failed,
                skipped: stats.skipped,
                errors: stats.errors,
                duration_ms: stats.duration_ms.max(1).saturating_add(compile_share_ms),
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
                runner_mode: "cargo_test_batch".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: overall_success
                    && !output_truncated
                    && !(stats.passed == 0
                        && stats.failed == 0
                        && stats.skipped == 0
                        && stats.errors == 0
                        && rust_module_filter_for_file(&test_file).is_some()),
                node_results: stats.node_results,
                external_module_stamps: BTreeMap::new(),
                exit_code: if outcome.is_green() || outcome == TestOutcome::Skipped {
                    None
                } else {
                    Some(group_exit_code)
                },
            });
        }
        // If the grouped Cargo process exited nonzero (e.g. panicked/aborted after one test
        // printed `ok` and before another reported), NEVER let the batch aggregate as `"passed"`:
        // record an explicit uncacheable Error for the unreported (or first) group file!
        if !overall_success
            && !group_recorded_failure
            && let Some((err_tf, err_dh)) = first_unreported_group_file.or(first_group_file)
        {
            records.push(CachedTestRecord {
                test_file: err_tf.clone(),
                dep_hash: err_dh,
                outcome: TestOutcome::Error,
                passed: 0,
                failed: 0,
                skipped: 0,
                errors: 1,
                duration_ms: output.duration_ms.max(1),
                failed_nodeids: vec![format!("cargo_process_exit:{err_tf}")],
                output_snippet: snippet.clone(),
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "cargo_test_batch".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: false,
                node_results: BTreeMap::new(),
                external_module_stamps: BTreeMap::new(),
                exit_code: Some(group_exit_code),
            });
        }
    }

    let mut direct_parallel_tasks: Vec<(String, String, PathBuf, PathBuf, Option<String>, u64)> =
        Vec::new();
    let mut sequential_files: Vec<String> = Vec::new();

    for test_file in &individual_files {
        let Some(dep_hash) = dep_hashes.get(test_file) else {
            continue;
        };
        let run_started_unix_ms = now_unix_ms();
        let crate_dir = rust_crate_dir_on_disk(&spec.root, test_file);
        if let Some((combined_out, dur_ms, fail_code)) = precompiled_failed_crates.get(&crate_dir) {
            let snippet = truncate_str(combined_out, 8192);
            if !all_outputs.contains(&snippet) && !snippet.trim().is_empty() {
                all_outputs.push(snippet.clone());
            }
            let stats = parse_cargo_test_output(combined_out, test_file, *dur_ms, false);
            let now = now_unix_ms();
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome: if stats.errors > 0 {
                    TestOutcome::Error
                } else {
                    TestOutcome::Failed
                },
                passed: stats.passed,
                failed: stats.failed,
                skipped: stats.skipped,
                errors: stats.errors,
                duration_ms: stats.duration_ms.max(1),
                failed_nodeids: stats.failed_nodeids,
                output_snippet: snippet,
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "cargo_test".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: true,
                node_results: stats.node_results,
                external_module_stamps: BTreeMap::new(),
                exit_code: Some(*fail_code),
            });
            continue;
        }
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let explicit_node_filter: Option<String> = invocation_targets.iter().find_map(|t| {
            let (f, node) = t.split_once("::")?;
            let norm_f = f.trim().trim_start_matches("./").replace('\\', "/");
            if &norm_f == test_file {
                Some(node.to_string())
            } else {
                None
            }
        });
        let main_bin_opt = crate_manifest_info
            .get(&crate_dir)
            .and_then(|m| m.main_bin.clone());
        let has_lib = crate_manifest_info
            .get(&crate_dir)
            .is_some_and(|m| m.lib_path.is_some());
        let direct_exe = if extra_args.is_empty() {
            let kind_opt = if crate::ast_graph::is_rust_integration_test_path(test_file) {
                crate::ast_graph::rust_integration_test_target_name(test_file)
                    .map(RustArtifactKind::IntegrationTest)
            } else if let Some((_, bin_rel)) = test_file.rsplit_once("src/bin/") {
                Path::new(bin_rel)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| RustArtifactKind::Bin(stem.to_string()))
            } else if test_file.ends_with("src/main.rs") || test_file == "src/main.rs" {
                main_bin_opt.map(RustArtifactKind::Bin)
            } else if has_lib {
                Some(RustArtifactKind::Lib)
            } else {
                main_bin_opt.map(RustArtifactKind::Bin)
            };
            // Always execute via `cargo test` rather than invoking the precompiled binary directly
            // so Cargo provides `CARGO_MANIFEST_DIR`, `CARGO_PKG_*`, `LD_LIBRARY_PATH`, `[env]`, etc.
            let _ = (&kind_opt, &precompiled_executables);
            None::<PathBuf>
        } else {
            None
        };
        if let Some(exe_path) = direct_exe {
            let filter_arg = explicit_node_filter
                .or_else(|| k_expr.map(ToOwned::to_owned))
                .or_else(|| rust_module_filter_for_file(test_file));
            let crate_total_files = files_by_crate
                .get(&crate_dir)
                .map_or(1, Vec::len)
                .max(1) as u64;
            let compile_share_ms = precompiled_duration_ms
                .get(&crate_dir)
                .copied()
                .unwrap_or(0)
                / crate_total_files;
            direct_parallel_tasks.push((
                test_file.clone(),
                dep_hash.clone(),
                workdir,
                exe_path,
                filter_arg,
                compile_share_ms,
            ));
        } else {
            sequential_files.push(test_file.clone());
        }
    }

    if !direct_parallel_tasks.is_empty() {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .clamp(1, 4)
            .min(direct_parallel_tasks.len());
        let chunk_size = direct_parallel_tasks.len().div_ceil(workers).max(1);
        let ignore_flag = mark_expr.is_some_and(|m| m == "ignore" || m == "ignored");
        let chunk_results: Vec<Result<Vec<(CachedTestRecord, String)>>> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = direct_parallel_tasks
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || -> Result<Vec<(CachedTestRecord, String)>> {
                            let mut local_out = Vec::with_capacity(chunk.len());
                            for (
                                test_file,
                                dep_hash,
                                workdir,
                                exe_path,
                                filter_arg,
                                compile_share_ms,
                            ) in chunk
                            {
                                let run_started_unix_ms = now_unix_ms();
                                let mut cmd = Command::new(exe_path);
                                cmd.current_dir(workdir);
                                apply_cargo_exec_env(&mut cmd, source_label);
                                if ignore_flag {
                                    cmd.arg("--ignored");
                                }
                                if let Some(f) = filter_arg
                                    && !f.trim().is_empty()
                                {
                                    cmd.arg(f);
                                }
                                const CARGO_DIRECT_MAX_BYTES: usize = 2 * 1024 * 1024;
                                let output = run_command_with_timeout_tracked(
                                    &mut cmd,
                                    timeout,
                                    CARGO_DIRECT_MAX_BYTES,
                                    None,
                                    active_pgids,
                                )?;
                                if output.timed_out {
                                    return Err(invalid(format!(
                                        "cargo test execution timed out after {}s",
                                        spec.policy.pytest_timeout_secs
                                    ))
                                    .into());
                                }
                                if output.cancelled {
                                    return Err(invalid(
                                        "TEST_CANCELLED: cargo test child process group was terminated",
                                    )
                                    .into());
                                }
                                let output_truncated = output.stdout.len() >= CARGO_DIRECT_MAX_BYTES
                                    || output.stderr.len() >= CARGO_DIRECT_MAX_BYTES;
                                let stdout_str = String::from_utf8_lossy(&output.stdout);
                                let stderr_str = String::from_utf8_lossy(&output.stderr);
                                let combined_out =
                                    match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty())
                                    {
                                        (true, _) => stderr_str.to_string(),
                                        (_, true) => stdout_str.to_string(),
                                        (false, false) => {
                                            format!("{}\n{}", stdout_str.trim(), stderr_str.trim())
                                        }
                                    };
                                let snippet = truncate_str(&combined_out, 8192);
                                let overall_success = output.status.success();
                                let mut stats = parse_cargo_test_output(
                                    &combined_out,
                                    test_file,
                                    output.duration_ms,
                                    overall_success,
                                );
                                if output_truncated {
                                    stats.node_results.clear();
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
                                local_out.push((
                                    CachedTestRecord {
                                        test_file: test_file.clone(),
                                        dep_hash: dep_hash.clone(),
                                        outcome,
                                        passed: stats.passed,
                                        failed: stats.failed,
                                        skipped: stats.skipped,
                                        errors: stats.errors,
                                        duration_ms: stats.duration_ms.saturating_add(*compile_share_ms),
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
                                        runner_mode: "cargo_test_direct".to_string(),
                                        started_unix_ms: run_started_unix_ms,
                                        completed_unix_ms: now,
                                        dynamic_dependencies: Vec::new(),
                                        dynamic_dependencies_complete: !output_truncated
                                            && !(stats.passed == 0
                                                && stats.failed == 0
                                                && stats.skipped == 0
                                                && stats.errors == 0
                                                && rust_module_filter_for_file(test_file).is_some()),
                                        node_results: stats.node_results,
                                        external_module_stamps: BTreeMap::new(),
                                        exit_code: if outcome.is_green()
                                            || outcome == TestOutcome::Skipped
                                        {
                                            None
                                        } else {
                                            Some(
                                                output
                                                    .status
                                                    .code()
                                                    .filter(|&c| c != 0)
                                                    .unwrap_or(101),
                                            )
                                        },
                                    },
                                    snippet,
                                ));
                            }
                            Ok(local_out)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| {
                            Err(invalid("parallel cargo test runner panicked").into())
                        })
                    })
                    .collect()
            });
        for chunk_res in chunk_results {
            for (rec, snippet) in chunk_res? {
                if !snippet.trim().is_empty() {
                    all_outputs.push(snippet);
                }
                records.push(rec);
            }
        }
    }

    let nextest_cmd = if extra_args.is_empty()
        && mark_expr.is_none_or(|m| m != "ignore" && m != "ignored")
    {
        crate::catalog::detect_nextest_cmd(&spec.root)
    } else {
        None
    };

    for test_file in &sequential_files {
        let Some(dep_hash) = dep_hashes.get(test_file) else {
            continue;
        };
        let run_started_unix_ms = now_unix_ms();
        let crate_dir = rust_crate_dir_on_disk(&spec.root, test_file);
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let explicit_node_filters: Vec<String> = invocation_targets
            .iter()
            .filter_map(|t| {
                let (f, node_opt) = split_target_file_and_node_on_disk(&spec.root, t);
                let node = node_opt?;
                let norm_f = f.trim().trim_start_matches("./").replace('\\', "/");
                if &norm_f == test_file {
                    let trimmed = node.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                } else {
                    None
                }
            })
            .collect();

        let (cargo_extra_args, harness_extra_args): (&[String], &[String]) =
            match extra_args.iter().position(|a| a == "--") {
                Some(sep_idx) => (&extra_args[..sep_idx], &extra_args[sep_idx + 1..]),
                None => (extra_args, &[]),
            };

        let manifest = crate_manifest_info.get(&crate_dir);
        let rel_in = rel_path_in_crate(&crate_dir, test_file);
        let main_bin_opt = manifest.and_then(|m| m.main_bin.clone());
        let has_lib = manifest.is_some_and(|m| m.lib_path.is_some());
        let is_lib_root = manifest
            .and_then(|m| m.lib_path.as_deref())
            .is_some_and(|lp| lp == rel_in);
        let custom_bin_names: Vec<String> = manifest
            .and_then(|m| m.custom_bins.get(rel_in))
            .cloned()
            .unwrap_or_default();
        let custom_bin_opt = if let Some(wanted_bin) =
            explicit_cargo_target.and_then(|ct| ct.strip_prefix("bin:"))
        {
            custom_bin_names
                .iter()
                .find(|bn| bn.as_str() == wanted_bin)
                .cloned()
        } else if custom_bin_names.len() == 1 {
            custom_bin_names.first().cloned()
        } else {
            None
        };
        let custom_test_names: Vec<String> = manifest
            .and_then(|m| m.custom_tests.get(rel_in))
            .cloned()
            .unwrap_or_default();
        let eligible_tests: Vec<&String> = custom_test_names
            .iter()
            .filter(|tn| {
                manifest.is_none_or(|m| !m.test_disabled.contains(*tn))
                    && manifest
                        .and_then(|m| m.custom_test_required_features.get(*tn))
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
            })
            .collect();
        let custom_test_opt = if let Some(wanted_test) =
            explicit_cargo_target.and_then(|ct| ct.strip_prefix("test:"))
        {
            custom_test_names
                .iter()
                .find(|tn| tn.as_str() == wanted_test)
                .cloned()
        } else if eligible_tests.len() == 1 {
            eligible_tests.first().map(|s| (*s).clone())
        } else {
            None
        };
        let child_owner = manifest
            .map(|m| {
                resolve_rust_child_target_ownership(&workdir, m, rel_in, explicit_cargo_target)
            })
            .unwrap_or(RustTargetChildOwnership::None);
        let autotests = manifest.is_none_or(|m| m.autotests);
        let autobins = manifest.is_none_or(|m| m.autobins);
        let allow_integration_root = explicit_cargo_target.is_none()
            || explicit_cargo_target.is_some_and(|ct| ct.starts_with("test:"));
        let is_auto_integration_root = allow_integration_root
            && autotests
            && crate::ast_graph::rust_integration_test_target_name(test_file).is_some();
        if child_owner == RustTargetChildOwnership::Ambiguous
            && !is_lib_root
            && custom_bin_opt.is_none()
            && custom_test_opt.is_none()
            && !is_auto_integration_root
        {
            return Err(invalid(format!(
                "AMBIGUOUS_RUST_TARGET_OWNER: {test_file} is mounted as a child module by multiple Cargo targets or module paths; target the owning Cargo test/bin/lib root explicitly."
            ))
            .into());
        }
        let seq_custom_mod_path = crate_uses_custom_mod_path(&spec.root, &crate_dir);
        // Resolve integration test ownership preferring explicit `[[test]]` / `autotests`-gated
        // auto-discovered roots and the actual `mod` tree (which handles `[[test]] path = "..."`
        // children AND `#[path = "real.rs"] mod renamed;` inside `tests/<name>/main.rs`).
        let integration_owner: Option<(String, Option<String>)> = if !allow_integration_root {
            None
        } else if let Some(tn) = custom_test_opt {
            Some((tn, None))
        } else if autotests
            && let Some(tn) = crate::ast_graph::rust_integration_test_target_name(test_file)
        {
            Some((tn, None))
        } else if let RustTargetChildOwnership::UniqueTest(tn, pfx) = &child_owner {
            Some((tn.clone(), Some(pfx.clone())))
        } else if autotests && !seq_custom_mod_path {
            rust_integration_test_owner(&spec.root, &crate_dir, test_file)
        } else {
            if autotests && rust_integration_test_owner(&spec.root, &crate_dir, test_file).is_some()
            {
                return Err(invalid(format!(
                    "AMBIGUOUS_INTEGRATION_MOD_PATH: {test_file} lives under an integration test using #[path = \"...\"] and its module name could not be uniquely resolved; target the integration test root explicitly."
                ))
                .into());
            }
            None
        };
        let resolved_bin_child: Option<(String, String)> =
            if let RustTargetChildOwnership::UniqueBin(bn, pfx) = &child_owner {
                Some((bn.clone(), pfx.clone()))
            } else {
                None
            };
        let resolved_lib_child: Option<String> =
            if let RustTargetChildOwnership::UniqueLib(pfx) = &child_owner {
                Some(pfx.clone())
            } else {
                None
            };
        // Collect ALL enabled target identities when a source file is shared across multiple
        // Cargo targets (e.g. `MultipleBins` under `--bins`, OR default-run `src/lib.rs` shared
        // with an enabled `[[test]]` or `[[bin]]`, OR multiple `[[test]]`/`[[bin]]` on one path),
        // filtering every binary by `required-features` (and `test = false` on default runs) so
        // ineligible binaries are NEVER explicitly invoked!
        let bin_exec_eligible = |bn: &str, enforce_test_enabled: bool| -> bool {
            (!enforce_test_enabled || manifest.is_none_or(|m| !m.bin_test_disabled.contains(bn)))
                && manifest
                    .and_then(|m| m.custom_bin_required_features.get(bn))
                    .is_none_or(|rf| cargo_required_features_satisfied(&workdir, rf, extra_args))
        };
        let multi_targets: Option<Vec<(Vec<String>, Option<String>)>> =
            if let RustTargetChildOwnership::MultipleBins(ref list) = child_owner {
                let enforce_test = explicit_cargo_target.is_none();
                Some(
                    list.iter()
                        .filter(|(bn, _)| bn.is_empty() || bin_exec_eligible(bn, enforce_test))
                        .map(|(bn, pfx)| {
                            let targs = if !bn.is_empty() {
                                vec!["--bin".to_string(), bn.clone()]
                            } else {
                                vec!["--bins".to_string()]
                            };
                            (targs, Some(pfx.clone()))
                        })
                        .collect(),
                )
            } else if explicit_cargo_target == Some("bins") && custom_bin_names.len() > 1 {
                Some(
                    custom_bin_names
                        .iter()
                        .filter(|bn| bin_exec_eligible(bn, false))
                        .map(|bn| (vec!["--bin".to_string(), bn.clone()], None))
                        .collect(),
                )
            } else if explicit_cargo_target.is_none() {
                let mut shared_id_targets: Vec<(Vec<String>, Option<String>)> = Vec::new();
                if is_lib_root && manifest.is_none_or(|m| m.lib_test_enabled) {
                    shared_id_targets.push((vec!["--lib".to_string()], None));
                }
                for tn in &eligible_tests {
                    shared_id_targets.push((vec!["--test".to_string(), (*tn).clone()], None));
                }
                for bn in &custom_bin_names {
                    if bin_exec_eligible(bn, true) {
                        shared_id_targets.push((vec!["--bin".to_string(), bn.clone()], None));
                    }
                }
                // Also include enabled auto-discovered binary owners (`src/main.rs` or
                // `src/bin/*`) so `[lib] path = "src/main.rs"` runs BOTH `--lib` and `--bin`!
                if rel_in == "src/main.rs"
                    && let Some(ref mb) = main_bin_opt
                    && !custom_bin_names.contains(mb)
                    && manifest.is_none_or(|m| !m.bin_test_disabled.contains(mb))
                    && manifest
                        .and_then(|m| m.custom_bin_required_features.get(mb))
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
                {
                    shared_id_targets.push((vec!["--bin".to_string(), mb.clone()], None));
                } else if autobins
                    && let Some(bn) = rel_in
                        .strip_prefix("src/bin/")
                        .and_then(rust_bin_name_from_bin_rel)
                    && !custom_bin_names.contains(&bn)
                    && manifest.is_none_or(|m| !m.bin_test_disabled.contains(&bn))
                    && manifest
                        .and_then(|m| m.custom_bin_required_features.get(&bn))
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(&workdir, rf, extra_args)
                        })
                {
                    shared_id_targets.push((vec!["--bin".to_string(), bn], None));
                }
                if shared_id_targets.len() > 1 {
                    Some(shared_id_targets)
                } else {
                    None
                }
            } else {
                None
            };
        // Fail closed for files outside `src/` that are not owned by ANY declared Cargo target
        // (prevents custom-target helpers like `suites/helper.rs` from silently running `--lib`
        // and caching a false-green pass!).
        let is_standard_src_file = rel_in.starts_with("src/");
        if !is_standard_src_file
            && !is_lib_root
            && custom_bin_opt.is_none()
            && integration_owner.is_none()
            && resolved_bin_child.is_none()
            && resolved_lib_child.is_none()
            && multi_targets.is_none()
        {
            return Err(invalid(format!(
                "UNKNOWN_RUST_TARGET_OWNER: {test_file} is outside src/ and does not match any Cargo [[test]]/[[bin]]/[lib] target in {}; refusing to fall back to --lib.",
                if crate_dir.is_empty() { "Cargo.toml" } else { crate_dir.as_str() }
            ))
            .into());
        }
        // Handle multi-target shared files (multiple binaries under `--bins`, or `[lib]` + `[[test]]`
        // / `[[bin]]` sharing `src/lib.rs`): run EVERY owning target identity with full selector
        // semantics (`explicit_node_filters`, `--exact`, child-module exclusions) and merge results.
        if let Some(ref targets_to_run) = multi_targets {
            let multi_k_trimmed = k_expr.map(str::trim).filter(|k| !k.is_empty());
            let multi_user_exact = harness_extra_args.iter().any(|a| a == "--exact");
            let (raw_child_pfxs, raw_amb_pfxs) =
                sibling_module_prefixes_and_ambiguity_for_root(&spec.root, &crate_dir, test_file);
            let mut matched_selectors: BTreeSet<&str> = BTreeSet::new();
            let mut merged_passed = 0usize;
            let mut merged_failed = 0usize;
            let mut merged_skipped = 0usize;
            let mut merged_errors = 0usize;
            let mut merged_duration = 0u64;
            let mut merged_failed_ids: Vec<String> = Vec::new();
            let mut merged_snips: Vec<String> = Vec::new();
            let mut all_ok = true;
            for (target_flag_args, mod_pfx_opt) in targets_to_run {
                let target_id_key: String = match target_flag_args.as_slice() {
                    [a] if a == "--lib" => "lib".to_string(),
                    [a, b] if a == "--bin" => format!("bin:{b}"),
                    [a, b] if a == "--test" => format!("test:{b}"),
                    _ => String::new(),
                };
                let is_custom_harness = !target_id_key.is_empty()
                    && manifest.is_some_and(|m| m.harness_false_targets.contains(&target_id_key));
                if is_custom_harness && !explicit_node_filters.is_empty() {
                    return Err(invalid(format!(
                        "UNSUPPORTED_HARNESS_SELECTOR: Cannot apply per-test node selectors ({}) to harness = false target `{target_id_key}` in {test_file}.",
                        explicit_node_filters.join(", ")
                    ))
                    .into());
                }
                let multi_extra_has_pkg = cargo_extra_args
                    .iter()
                    .any(|a| a == "-p" || a == "--package" || a.starts_with("-p=") || a.starts_with("--package="));
                let (multi_cwd, multi_pkg_for_cwd): (&Path, Option<String>) =
                    if let Some(ref ecwd) = explicit_cargo_cwd
                        && ecwd != &workdir
                        && !multi_extra_has_pkg
                        && let Some(pn) = cargo_manifest_package_name(&workdir)
                    {
                        (ecwd.as_path(), Some(pn))
                    } else if !multi_extra_has_pkg
                        && is_cargo_workspace_root(&spec.root)
                        && let Some(pn) = cargo_manifest_package_name(&workdir)
                    {
                        (workdir.as_path(), Some(pn))
                    } else {
                        (workdir.as_path(), None)
                    };
                let matched_names: Option<Vec<String>> = if is_custom_harness {
                    // `harness = false` targets do not emit standard libtest `: test` lines under
                    // `-- --list`: delegate direct execution rather than skipping!
                    None
                } else {
                    let mut list_cmd = Command::new(program);
                    list_cmd.current_dir(multi_cwd);
                    apply_cargo_exec_env(&mut list_cmd, source_label);
                    list_cmd.args(base_args);
                    list_cmd.arg("test");
                    if let Some(ref pn) = multi_pkg_for_cwd {
                        list_cmd.arg("-p").arg(pn);
                    }
                    for tfa in target_flag_args {
                        list_cmd.arg(tfa);
                    }
                    for arg in cargo_extra_args {
                        list_cmd.arg(arg);
                    }
                    list_cmd.arg("--").arg("--list");
                    const MAX_MULTI_LIST_BYTES: usize = 8 * 1024 * 1024;
                    let list_out = run_command_with_timeout_tracked(
                        &mut list_cmd,
                        timeout,
                        MAX_MULTI_LIST_BYTES,
                        active_pgid,
                        active_pgids,
                    )?;
                    if list_out.cancelled {
                        return Err(invalid(
                            "TEST_CANCELLED: cargo test --list child process group was terminated",
                        )
                        .into());
                    }
                    if list_out.timed_out {
                        return Err(invalid(format!(
                            "cargo test --list timed out after {}s while scoping targets for {test_file}",
                            timeout.as_secs()
                        ))
                        .into());
                    }
                    if !list_out.status.success() {
                        let err_text = String::from_utf8_lossy(&list_out.stderr);
                        return Err(invalid(format!(
                            "cargo test --list failed for {} ({test_file}): {}",
                            target_flag_args.join(" "),
                            truncate_str(err_text.trim(), 512)
                        ))
                        .into());
                    }
                    if list_out.stdout.len() >= MAX_MULTI_LIST_BYTES {
                        return Err(invalid(format!(
                            "cargo test --list output exceeded {MAX_MULTI_LIST_BYTES} bytes while scoping --bins for {test_file}"
                        ))
                        .into());
                    }
                    let qual_child_pfxs: Vec<String> = raw_child_pfxs
                        .iter()
                        .map(|p| match mod_pfx_opt.as_deref() {
                            Some(pfx) => format!("{pfx}{p}"),
                            None => p.clone(),
                        })
                        .collect();
                    let qual_amb_pfxs: Vec<String> = raw_amb_pfxs
                        .iter()
                        .map(|p| match mod_pfx_opt.as_deref() {
                            Some(pfx) => format!("{pfx}{p}"),
                            None => p.clone(),
                        })
                        .collect();
                    let listed = String::from_utf8_lossy(&list_out.stdout);
                    let mut names_vec: Vec<String> = Vec::new();
                    for line in listed.lines() {
                        let Some(name) = line.trim().strip_suffix(": test") else {
                            continue;
                        };
                        if let Some(mod_pfx) = mod_pfx_opt
                            && !name.starts_with(mod_pfx.as_str())
                        {
                            continue;
                        }
                        if qual_child_pfxs
                            .iter()
                            .any(|sib| name.starts_with(sib.as_str()))
                        {
                            continue;
                        }
                        if qual_amb_pfxs
                            .iter()
                            .any(|amb| name.starts_with(amb.as_str()))
                        {
                            return Err(invalid(format!(
                                "AMBIGUOUS_CONDITIONAL_MODULE: test '{name}' in {test_file} belongs to a module declared both externally and inline under conditional compilation; cannot safely scope."
                            ))
                            .into());
                        }
                        if !explicit_node_filters.is_empty() {
                            let short = name.rsplit("::").next().unwrap_or(name);
                            let mut hit_any = false;
                            for sel in &explicit_node_filters {
                                if name == sel || short == sel || name.ends_with(&format!("::{sel}"))
                                {
                                    matched_selectors.insert(sel.as_str());
                                    hit_any = true;
                                }
                            }
                            if !hit_any {
                                continue;
                            }
                        }
                        if let Some(k) = multi_k_trimmed {
                            let matches_k = if multi_user_exact {
                                name == k
                            } else {
                                name.contains(k)
                            };
                            if !matches_k {
                                continue;
                            }
                        }
                        names_vec.push(name.to_string());
                    }
                    if names_vec.is_empty() {
                        continue;
                    }
                    Some(names_vec)
                };
                let mut bin_cmd = Command::new(program);
                bin_cmd.current_dir(multi_cwd);
                apply_cargo_exec_env(&mut bin_cmd, source_label);
                bin_cmd.args(base_args);
                bin_cmd.arg("test");
                if let Some(ref pn) = multi_pkg_for_cwd {
                    bin_cmd.arg("-p").arg(pn);
                }
                for tfa in target_flag_args {
                    bin_cmd.arg(tfa);
                }
                for arg in cargo_extra_args {
                    bin_cmd.arg(arg);
                }
                if let Some(ref names_vec) = matched_names {
                    bin_cmd.arg("--").arg("--exact");
                    for name in names_vec {
                        bin_cmd.arg(name);
                    }
                    if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                        bin_cmd.arg("--ignored");
                    }
                    for arg in harness_extra_args {
                        if arg != "--exact" {
                            bin_cmd.arg(arg);
                        }
                    }
                } else {
                    let has_ign = mark_expr.is_some_and(|m| m == "ignore" || m == "ignored");
                    if multi_k_trimmed.is_some() || has_ign || !harness_extra_args.is_empty() {
                        bin_cmd.arg("--");
                        if let Some(k) = multi_k_trimmed {
                            bin_cmd.arg(k);
                        }
                        if has_ign {
                            bin_cmd.arg("--ignored");
                        }
                        for arg in harness_extra_args {
                            bin_cmd.arg(arg);
                        }
                    }
                }
                let seq_timeout = if precompiled_duration_ms.contains_key(&crate_dir) {
                    timeout
                } else {
                    compile_timeout
                };
                const CARGO_MULTI_MAX_BYTES: usize = 2 * 1024 * 1024;
                let out = run_command_with_timeout_tracked(
                    &mut bin_cmd,
                    seq_timeout,
                    CARGO_MULTI_MAX_BYTES,
                    active_pgid,
                    active_pgids,
                )?;
                if out.timed_out {
                    return Err(invalid(format!(
                        "cargo test execution timed out after {}s",
                        seq_timeout.as_secs()
                    ))
                    .into());
                }
                if out.cancelled {
                    return Err(invalid(
                        "TEST_CANCELLED: cargo test child process group was terminated",
                    )
                    .into());
                }
                let stdout_s = String::from_utf8_lossy(&out.stdout);
                let stderr_s = String::from_utf8_lossy(&out.stderr);
                let combined = match (stdout_s.trim().is_empty(), stderr_s.trim().is_empty()) {
                    (true, _) => stderr_s.to_string(),
                    (_, true) => stdout_s.to_string(),
                    (false, false) => format!("{}\n{}", stdout_s.trim(), stderr_s.trim()),
                };
                let snip = truncate_str(&combined, 4096);
                if !snip.trim().is_empty() {
                    all_outputs.push(snip.clone());
                    merged_snips.push(snip);
                }
                let ok = out.status.success();
                all_ok &= ok;
                let st = parse_cargo_test_output(&combined, test_file, out.duration_ms, ok);
                merged_passed = merged_passed.saturating_add(st.passed);
                merged_failed = merged_failed.saturating_add(st.failed);
                merged_skipped = merged_skipped.saturating_add(st.skipped);
                merged_errors = merged_errors.saturating_add(st.errors);
                merged_duration = merged_duration.saturating_add(st.duration_ms);
                for fid in st.failed_nodeids {
                    if !merged_failed_ids.contains(&fid) {
                        merged_failed_ids.push(fid);
                    }
                }
            }
            if !explicit_node_filters.is_empty() {
                let missing: Vec<&str> = explicit_node_filters
                    .iter()
                    .filter(|sel| !matched_selectors.contains(sel.as_str()))
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    return Err(invalid(format!(
                        "MISSING_TEST_SELECTOR: No Rust test matched selector(s) in {test_file}: {}",
                        missing.join(", ")
                    ))
                    .into());
                }
            }
            let outcome = if merged_failed > 0 {
                TestOutcome::Failed
            } else if merged_errors > 0 {
                TestOutcome::Error
            } else if merged_passed > 0 && merged_skipped == 0 && all_ok {
                TestOutcome::Passed
            } else if all_ok {
                TestOutcome::Skipped
            } else {
                TestOutcome::Failed
            };
            let now = now_unix_ms();
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: merged_passed,
                failed: merged_failed,
                skipped: merged_skipped,
                errors: merged_errors,
                duration_ms: merged_duration.max(1),
                failed_nodeids: merged_failed_ids,
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    truncate_str(&merged_snips.join("\n"), 8192)
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "cargo_test".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: false,
                node_results: BTreeMap::new(),
                external_module_stamps: BTreeMap::new(),
                exit_code: if outcome.is_green() || outcome == TestOutcome::Skipped {
                    None
                } else {
                    Some(101)
                },
            });
            continue;
        }
        let apply_target_args = |cmd: &mut Command| {
            // Honor explicit Cargo target selection FIRST (e.g. `--lib` or `--bins` when `[lib]`
            // and `[[bin]]` share the same source path!) before inferring target kind from filenames.
            if explicit_cargo_target == Some("lib") {
                cmd.arg("--lib");
            } else if explicit_cargo_target == Some("bins") {
                if let Some(ref custom_bin) = custom_bin_opt {
                    cmd.arg("--bin").arg(custom_bin);
                } else if let Some((ref bin_name, _)) = resolved_bin_child {
                    if !bin_name.is_empty() {
                        cmd.arg("--bin").arg(bin_name);
                    } else {
                        cmd.arg("--bins");
                    }
                } else if (test_file.ends_with("src/main.rs") || test_file == "src/main.rs")
                    && let Some(ref main_bin) = main_bin_opt
                {
                    cmd.arg("--bin").arg(main_bin);
                } else if let Some((_, bin_rel)) = test_file.rsplit_once("src/bin/")
                    && let Some(bin_name) = rust_bin_name_from_bin_rel(bin_rel)
                {
                    cmd.arg("--bin").arg(bin_name);
                } else {
                    cmd.arg("--bins");
                }
            } else if let Some(wanted_bin) =
                explicit_cargo_target.and_then(|ct| ct.strip_prefix("bin:"))
            {
                cmd.arg("--bin").arg(wanted_bin);
            } else if let Some(wanted_test) =
                explicit_cargo_target.and_then(|ct| ct.strip_prefix("test:"))
            {
                cmd.arg("--test").arg(wanted_test);
            } else if let Some((ref target_name, _)) = integration_owner {
                cmd.arg("--test").arg(target_name);
            } else if let Some(ref custom_bin) = custom_bin_opt {
                cmd.arg("--bin").arg(custom_bin);
            } else if let Some((ref bin_name, _)) = resolved_bin_child {
                if !bin_name.is_empty() {
                    cmd.arg("--bin").arg(bin_name);
                } else {
                    cmd.arg("--bins");
                }
            } else if resolved_lib_child.is_some()
                || (is_lib_root && manifest.is_none_or(|m| m.lib_test_enabled))
            {
                cmd.arg("--lib");
            } else if test_file.ends_with("src/main.rs") || test_file == "src/main.rs" {
                if let Some(ref main_bin) = main_bin_opt {
                    cmd.arg("--bin").arg(main_bin);
                } else {
                    cmd.arg("--bins");
                }
            } else if let Some((_, bin_rel)) = test_file.rsplit_once("src/bin/")
                && let Some(bin_name) = rust_bin_name_from_bin_rel(bin_rel)
            {
                cmd.arg("--bin").arg(bin_name);
            } else if has_lib {
                cmd.arg("--lib");
            } else if let Some(ref main_bin) = main_bin_opt {
                cmd.arg("--bin").arg(main_bin);
            } else {
                cmd.arg("--bins");
            }
        };

        let is_integration_test = integration_owner.is_some();
        let file_mod_prefix = if let Some((_, ref sub_pfx)) = integration_owner {
            sub_pfx.clone()
        } else if is_lib_root || custom_bin_opt.is_some() {
            None
        } else if let Some((_, ref pfx)) = resolved_bin_child {
            Some(pfx.clone())
        } else if let Some(ref pfx) = resolved_lib_child {
            Some(pfx.clone())
        } else if seq_custom_mod_path {
            custom_mod_prefix_for_file(&spec.root, &crate_dir, test_file)
        } else {
            rust_module_filter_for_file(test_file)
        };
        let is_auto_bin_root = autobins
            && rel_in.strip_prefix("src/bin/").is_some_and(|bin_rel| {
                !bin_rel.contains('/')
                    || bin_rel
                        .strip_suffix("/main.rs")
                        .is_some_and(|d| !d.is_empty() && !d.contains('/'))
            });
        let is_crate_root_file = !is_integration_test
            && (is_lib_root
                || custom_bin_opt.is_some()
                || (rel_in == "src/main.rs" && main_bin_opt.is_some())
                || is_auto_bin_root);
        if seq_custom_mod_path
            && !is_integration_test
            && !is_crate_root_file
            && file_mod_prefix.is_none()
        {
            // Fail closed: in a crate using `#[path = "..."]`, module ownership could not be
            // uniquely determined for this non-root file (e.g. an unmounted `src/orphan.rs`);
            // never run an unscoped `--lib` suite that could cache unrelated passing tests for it!
            return Err(invalid(format!(
                "UNRESOLVED_RUST_MOD_PATH: Cannot safely determine module ownership for {test_file} because the crate uses custom #[path] modules; target the owning crate root or mounted module explicitly."
            ))
            .into());
        }
        // Resolve declared external child `mod foo;` prefixes AND ambiguous conditional external+inline
        // module overlaps for EVERY module file (crate roots, integration roots, AND intermediate
        // parent modules like `src/a.rs` declaring `mod b;`) by prepending `file_mod_prefix`, so
        // child tests never satisfy parent selectors or pollute parent node caches.
        let (raw_child_prefixes, raw_ambiguous_prefixes) =
            sibling_module_prefixes_and_ambiguity_for_root(&spec.root, &crate_dir, test_file);
        let qualify_child_pfx = |p: String| -> String {
            match file_mod_prefix.as_deref() {
                Some(pfx) => format!("{pfx}{p}"),
                None => p,
            }
        };
        let root_sibling_prefixes: Vec<String> = raw_child_prefixes
            .into_iter()
            .map(&qualify_child_pfx)
            .collect();
        let ambiguous_mod_prefixes: Vec<String> = raw_ambiguous_prefixes
            .into_iter()
            .map(&qualify_child_pfx)
            .collect();
        let single_target_id_key: String = if explicit_cargo_target == Some("lib") {
            "lib".to_string()
        } else if let Some(b) = explicit_cargo_target.and_then(|ct| ct.strip_prefix("bin:")) {
            format!("bin:{b}")
        } else if let Some(t) = explicit_cargo_target.and_then(|ct| ct.strip_prefix("test:")) {
            format!("test:{t}")
        } else if let Some((ref tn, _)) = integration_owner {
            format!("test:{tn}")
        } else if let Some(ref bn) = custom_bin_opt {
            format!("bin:{bn}")
        } else if let Some((ref bn, _)) = resolved_bin_child {
            format!("bin:{bn}")
        } else if resolved_lib_child.is_some()
            || (is_lib_root && manifest.is_none_or(|m| m.lib_test_enabled))
        {
            "lib".to_string()
        } else if (test_file.ends_with("src/main.rs") || test_file == "src/main.rs")
            && let Some(ref mb) = main_bin_opt
        {
            format!("bin:{mb}")
        } else if let Some((_, bin_rel)) = test_file.rsplit_once("src/bin/")
            && let Some(bn) = rust_bin_name_from_bin_rel(bin_rel)
        {
            format!("bin:{bn}")
        } else if has_lib && manifest.is_none_or(|m| m.lib_test_enabled) {
            "lib".to_string()
        } else {
            String::new()
        };
        let is_single_custom_harness = !single_target_id_key.is_empty()
            && manifest.is_some_and(|m| m.harness_false_targets.contains(&single_target_id_key));
        if is_single_custom_harness && !explicit_node_filters.is_empty() {
            return Err(invalid(format!(
                "UNSUPPORTED_HARNESS_SELECTOR: Cannot apply per-test node selectors ({}) to harness = false target `{single_target_id_key}` in {test_file}.",
                explicit_node_filters.join(", ")
            ))
            .into());
        }
        let k_trimmed = k_expr.map(str::trim).filter(|k| !k.is_empty());
        let user_exact_flag = harness_extra_args.iter().any(|a| a == "--exact");
        // Never use `cargo nextest` when `.config/nextest.toml` exists, when explicit node/filter
        // selectors are active, when a module prefix is needed (nextest matches substrings like
        // `a::` -> `qa::test`), when a root file has external child modules to exclude, OR when
        // the target declares `harness = false` (custom harness)!
        let using_nextest = nextest_cmd.is_some()
            && explicit_node_filters.is_empty()
            && k_trimmed.is_none()
            && !user_exact_flag
            && file_mod_prefix.is_none()
            && root_sibling_prefixes.is_empty()
            && ambiguous_mod_prefixes.is_empty()
            && !is_single_custom_harness
            && !has_nextest_config(&spec.root, &crate_dir);
        let ind_extra_has_pkg = cargo_extra_args
            .iter()
            .any(|a| a == "-p" || a == "--package" || a.starts_with("-p=") || a.starts_with("--package="));
        let (cmd_cwd, cmd_pkg_for_cwd): (&Path, Option<String>) =
            if let Some(ref ecwd) = explicit_cargo_cwd
                && ecwd != &workdir
                && !ind_extra_has_pkg
                && let Some(pn) = cargo_manifest_package_name(&workdir)
            {
                (ecwd.as_path(), Some(pn))
            } else if !ind_extra_has_pkg
                && is_cargo_workspace_root(&spec.root)
                && let Some(pn) = cargo_manifest_package_name(&workdir)
            {
                (workdir.as_path(), Some(pn))
            } else {
                (workdir.as_path(), None)
            };
        let mut command = if using_nextest
            && let Some(ref nx) = nextest_cmd
            && let Some((nx_prog, nx_base)) = nx.split_first()
        {
            let mut cmd = Command::new(nx_prog);
            cmd.current_dir(cmd_cwd);
            apply_cargo_exec_env(&mut cmd, source_label);
            cmd.args(nx_base);
            cmd.arg("run").arg("--no-fail-fast");
            if let Some(ref pn) = cmd_pkg_for_cwd {
                cmd.arg("-p").arg(pn);
            }
            cmd
        } else {
            let mut cmd = Command::new(program);
            cmd.current_dir(cmd_cwd);
            apply_cargo_exec_env(&mut cmd, source_label);
            cmd.args(base_args);
            cmd.arg("test");
            if let Some(ref pn) = cmd_pkg_for_cwd {
                cmd.arg("-p").arg(pn);
            }
            cmd
        };
        apply_target_args(&mut command);

        // Scope explicit per-file node selectors, `-k` filters, standalone `--exact`, module-
        // prefixed files (`file_mod_prefix.is_some()`), AND target roots with declared external
        // child modules strictly to tests owned by `test_file` via `-- --list` + `-- --exact`.
        // Skip `-- --list` for `harness = false` targets (which do not emit `: test` lines).
        let mut exact_node_matches: Option<Vec<String>> = None;
        if (!explicit_node_filters.is_empty()
            || k_trimmed.is_some()
            || user_exact_flag
            || file_mod_prefix.is_some()
            || !root_sibling_prefixes.is_empty()
            || !ambiguous_mod_prefixes.is_empty())
            && !using_nextest
            && !is_single_custom_harness
            && (!seq_custom_mod_path || file_mod_prefix.is_some() || is_crate_root_file || is_integration_test)
        {
            let mut list_cmd = Command::new(program);
            list_cmd.current_dir(cmd_cwd);
            apply_cargo_exec_env(&mut list_cmd, source_label);
            list_cmd.args(base_args);
            list_cmd.arg("test");
            if let Some(ref pn) = cmd_pkg_for_cwd {
                list_cmd.arg("-p").arg(pn);
            }
            apply_target_args(&mut list_cmd);
            for arg in cargo_extra_args {
                list_cmd.arg(arg);
            }
            list_cmd.arg("--").arg("--list");
            const MAX_CARGO_LIST_BYTES: usize = 8 * 1024 * 1024;
            let list_out = run_command_with_timeout_tracked(
                &mut list_cmd,
                timeout,
                MAX_CARGO_LIST_BYTES,
                active_pgid,
                active_pgids,
            )?;
            if list_out.cancelled {
                return Err(invalid(
                    "TEST_CANCELLED: cargo test --list child process group was terminated",
                )
                .into());
            }
            if list_out.timed_out {
                return Err(invalid(format!(
                    "cargo test --list timed out after {}s while scoping selectors for {test_file}",
                    timeout.as_secs()
                ))
                .into());
            }
            if !list_out.status.success() {
                let err_text = String::from_utf8_lossy(&list_out.stderr);
                return Err(invalid(format!(
                    "cargo test --list failed while scoping selectors for {test_file}: {}",
                    truncate_str(err_text.trim(), 512)
                ))
                .into());
            }
            if list_out.stdout.len() >= MAX_CARGO_LIST_BYTES {
                return Err(invalid(format!(
                    "cargo test --list output exceeded {MAX_CARGO_LIST_BYTES} bytes while scoping selectors for {test_file}"
                ))
                .into());
            }
            let listed = String::from_utf8_lossy(&list_out.stdout);
            let mut matched = Vec::new();
            let mut matched_selectors: BTreeSet<&str> = BTreeSet::new();
            for line in listed.lines() {
                let Some(name) = line.trim().strip_suffix(": test") else {
                    continue;
                };
                if let Some(ref mod_pfx) = file_mod_prefix
                    && !name.starts_with(mod_pfx.as_str())
                {
                    continue;
                }
                if root_sibling_prefixes
                    .iter()
                    .any(|sib| name.starts_with(sib.as_str()))
                {
                    continue;
                }
                if ambiguous_mod_prefixes
                    .iter()
                    .any(|amb| name.starts_with(amb.as_str()))
                {
                    let hits_req = if !explicit_node_filters.is_empty() {
                        let short = name.rsplit("::").next().unwrap_or(name);
                        explicit_node_filters.iter().any(|sel| {
                            name == sel || short == sel || name.ends_with(&format!("::{sel}"))
                        })
                    } else if let Some(k) = k_trimmed {
                        if user_exact_flag {
                            name == k
                        } else {
                            name.contains(k)
                        }
                    } else {
                        true
                    };
                    if hits_req {
                        return Err(invalid(format!(
                            "AMBIGUOUS_CONDITIONAL_MODULE: test '{name}' in {test_file} belongs to a module declared both externally and inline under conditional compilation; cannot safely scope to {test_file}."
                        ))
                        .into());
                    }
                    continue;
                }
                if !explicit_node_filters.is_empty() {
                    let short = name.rsplit("::").next().unwrap_or(name);
                    let mut hit_any = false;
                    for sel in &explicit_node_filters {
                        if name == sel || short == sel || name.ends_with(&format!("::{sel}")) {
                            matched_selectors.insert(sel.as_str());
                            hit_any = true;
                        }
                    }
                    if !hit_any {
                        continue;
                    }
                }
                if let Some(k) = k_trimmed {
                    let matches_k = if user_exact_flag {
                        name == k
                    } else {
                        name.contains(k)
                    };
                    if !matches_k {
                        continue;
                    }
                }
                matched.push(name.to_string());
            }
            if !explicit_node_filters.is_empty() {
                let missing: Vec<&str> = explicit_node_filters
                    .iter()
                    .filter(|sel| !matched_selectors.contains(sel.as_str()))
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    return Err(invalid(format!(
                        "MISSING_TEST_SELECTOR: No Rust test matched selector(s) in {test_file}: {}",
                        missing.join(", ")
                    ))
                    .into());
                }
            }
            exact_node_matches = Some(matched);
        }
        if let Some(ref matched) = exact_node_matches
            && matched.is_empty()
        {
            // Explicit file-scoped `::node` selector matched 0 tests: fail explicitly rather than
            // aggregating a Skipped record as overall "passed"; `-k` pattern mismatch stays Skipped.
            let is_explicit_node_missing = !explicit_node_filters.is_empty();
            let now = now_unix_ms();
            let msg = format!(
                "No tests matched file-scoped selector(s) in {test_file}: {}",
                if is_explicit_node_missing {
                    explicit_node_filters.join(", ")
                } else {
                    k_trimmed.unwrap_or("").to_string()
                }
            );
            if is_explicit_node_missing {
                return Err(invalid(format!("MISSING_TEST_SELECTOR: {msg}")).into());
            }
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome: TestOutcome::Skipped,
                passed: 0,
                failed: 0,
                skipped: 1,
                errors: 0,
                duration_ms: 1,
                failed_nodeids: Vec::new(),
                output_snippet: msg,
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "cargo_test".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: false,
                node_results: BTreeMap::new(),
                external_module_stamps: BTreeMap::new(),
                exit_code: None,
            });
            continue;
        }

        let single_fallback_filter = if explicit_node_filters.is_empty() {
            k_trimmed
                .map(ToOwned::to_owned)
                .or_else(|| file_mod_prefix.clone())
        } else {
            None
        };
        if using_nextest {
            if let Some(ref f) = single_fallback_filter
                && !f.trim().is_empty()
            {
                command.arg(f);
            }
            for arg in extra_args {
                command.arg(arg);
            }
            if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                command.arg("--run-ignored").arg("ignored-only");
            }
        } else if let Some(ref exact_names) = exact_node_matches {
            // Merge all libtest harness arguments under ONE single `--` separator so existing
            // `--` flags in `extra_args` (e.g. `["--", "--nocapture"]`) never turn `--exact`
            // into a positional filter!
            for arg in cargo_extra_args {
                command.arg(arg);
            }
            command.arg("--").arg("--exact");
            for name in exact_names {
                command.arg(name);
            }
            if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                command.arg("--ignored");
            }
            for arg in harness_extra_args {
                // `--exact` was already emitted above and consumed during `-- --list` resolution;
                // passing it a second time causes libtest to reject the invocation.
                if arg == "--exact" {
                    continue;
                }
                command.arg(arg);
            }
        } else {
            if explicit_node_filters.len() == 1 {
                command.arg(&explicit_node_filters[0]);
            } else if let Some(ref f) = single_fallback_filter
                && !f.trim().is_empty()
            {
                command.arg(f);
            }
            for arg in cargo_extra_args {
                command.arg(arg);
            }
            let needs_harness_sep = explicit_node_filters.len() > 1
                || mark_expr.is_some_and(|m| m == "ignore" || m == "ignored")
                || !harness_extra_args.is_empty();
            if needs_harness_sep {
                command.arg("--");
                if explicit_node_filters.len() > 1 {
                    for f in &explicit_node_filters {
                        command.arg(f);
                    }
                }
                if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
                    command.arg("--ignored");
                }
                for arg in harness_extra_args {
                    command.arg(arg);
                }
            }
        }

        let seq_timeout = if precompiled_duration_ms.contains_key(&crate_dir) {
            timeout
        } else {
            compile_timeout
        };
        const CARGO_SEQ_MAX_BYTES: usize = 2 * 1024 * 1024;
        let output = run_command_with_timeout_tracked(
            &mut command,
            seq_timeout,
            CARGO_SEQ_MAX_BYTES,
            active_pgid,
            active_pgids,
        )?;
        if output.timed_out {
            return Err(invalid(format!(
                "cargo test execution timed out after {}s",
                seq_timeout.as_secs()
            ))
            .into());
        }
        if output.cancelled {
            return Err(
                invalid("TEST_CANCELLED: cargo test child process group was terminated").into(),
            );
        }

        let output_truncated = output.stdout.len() >= CARGO_SEQ_MAX_BYTES
            || output.stderr.len() >= CARGO_SEQ_MAX_BYTES;
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
        }
        let overall_success = output.status.success();
        let mut stats =
            parse_cargo_test_output(&combined_out, test_file, output.duration_ms, overall_success);
        // Never cache per-node entries on truncated output (a later failing duplicate-name test
        // may have been omitted), nor on any module file (root OR intermediate parent like `src/a.rs`)
        // that declares external child modules when running without exact node scoping, nor when
        // `#[path]` ownership is uncertain.
        if output_truncated
            || (seq_custom_mod_path && explicit_node_filters.is_empty() && file_mod_prefix.is_none())
            || (explicit_node_filters.is_empty()
                && exact_node_matches.is_none()
                && (!root_sibling_prefixes.is_empty() || !ambiguous_mod_prefixes.is_empty()))
        {
            stats.node_results.clear();
        }
        let zero_test_filtered = stats.passed == 0
            && stats.failed == 0
            && stats.skipped == 0
            && stats.errors == 0
            && (file_mod_prefix.is_some()
                || rust_module_filter_for_file(test_file).is_some()
                || !explicit_node_filters.is_empty()
                || k_expr.is_some_and(|k| !k.trim().is_empty()));
        let outcome = if stats.failed > 0 {
            TestOutcome::Failed
        } else if stats.errors > 0 {
            TestOutcome::Error
        } else if stats.passed > 0 && stats.skipped == 0 {
            TestOutcome::Passed
        } else if (stats.skipped > 0 || zero_test_filtered) && overall_success {
            // Unmatched path-derived module filter (e.g. `#[path = "real.rs"] mod renamed;`)
            // executed 0 tests; never report a false "passed" green result!
            TestOutcome::Skipped
        } else if overall_success {
            TestOutcome::Passed
        } else {
            TestOutcome::Failed
        };
        let now = now_unix_ms();
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
                snippet
            },
            updated_unix_ms: now,
            source: source_label.to_string(),
            environment_hash: environment_hash.to_string(),
            tctx_version: env!("CARGO_PKG_VERSION").to_string(),
            runner_mode: "cargo_test".to_string(),
            started_unix_ms: run_started_unix_ms,
            completed_unix_ms: now,
            dynamic_dependencies: Vec::new(),
            dynamic_dependencies_complete: !zero_test_filtered && !output_truncated,
            node_results: stats.node_results,
            external_module_stamps: BTreeMap::new(),
            exit_code: if outcome.is_green() || outcome == TestOutcome::Skipped {
                None
            } else {
                Some(output.status.code().filter(|&c| c != 0).unwrap_or(101))
            },
        });
    }

    Ok((records, all_outputs.join("\n")))
}


