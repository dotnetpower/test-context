use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_pytest_parallel(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    preload_modules: &[String],
    environment_hash: &str,
    pytest_plugins: &[String],
    capture_dynamic_dependencies: bool,
    active_pgids: &Mutex<BTreeSet<i32>>,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 4);
    let mut all_records = Vec::new();
    let mut outputs = Vec::new();
    for wave in invocation_targets.chunks(parallelism) {
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .iter()
                .map(|target| {
                    scope.spawn(move || {
                        let file = target.split("::").next().unwrap_or(target);
                        let mut one_hash = BTreeMap::new();
                        if let Some(hash) = dep_hashes.get(file) {
                            one_hash.insert(file.to_string(), hash.clone());
                        }
                        let local_manager = Mutex::new(ZygoteManager::default());
                        execute_pytest_batch(
                            spec,
                            std::slice::from_ref(target),
                            &one_hash,
                            k_expr,
                            mark_expr,
                            extra_args,
                            "on_demand_parallel",
                            Some(&local_manager),
                            None,
                            Some(active_pgids),
                            preload_modules,
                            environment_hash,
                            pytest_plugins,
                            capture_dynamic_dependencies,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| invalid("parallel pytest worker panicked").into())
                        .and_then(|result| result)
                })
                .collect::<Vec<Result<(Vec<CachedTestRecord>, String)>>>()
        });
        for result in results {
            let (mut records, output) = result?;
            all_records.append(&mut records);
            if !output.trim().is_empty() {
                outputs.push(output);
            }
        }
    }
    Ok((all_records, outputs.join("\n")))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_pytest_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    zygote_slot: Option<&Mutex<ZygoteManager>>,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    preload_modules: &[String],
    environment_hash: &str,
    pytest_plugins: &[String],
    capture_dynamic_dependencies: bool,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let rs_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| split_target_file_and_node_on_disk(&spec.root, t).0.ends_with(".rs"))
        .cloned()
        .collect();
    let fe_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| {
            crate::ast_graph::is_frontend_source_path(
                split_target_file_and_node_on_disk(&spec.root, t).0,
            )
        })
        .cloned()
        .collect();
    let sanitized_extra_args: Vec<String> = extra_args
        .iter()
        .filter(|a| {
            !a.starts_with("__tctx_fe_runner__=")
                && !a.starts_with("__tctx_cargo_target__=")
                && !a.starts_with("__tctx_cargo_cwd__=")
                && !a.starts_with("__tctx_orig_target__=")
                && a.as_str() != "__tctx_orig_empty__"
        })
        .cloned()
        .collect();
    let extra_args_for_non_rs = sanitized_extra_args.as_slice();
    let go_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| split_target_file_and_node_on_disk(&spec.root, t).0.ends_with("_test.go"))
        .cloned()
        .collect();
    let py_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| {
            let f = split_target_file_and_node_on_disk(&spec.root, t).0;
            !f.ends_with(".rs")
                && !f.ends_with("_test.go")
                && !crate::ast_graph::is_frontend_source_path(f)
        })
        .cloned()
        .collect();

    if !rs_targets.is_empty()
        && fe_targets.is_empty()
        && go_targets.is_empty()
        && py_targets.is_empty()
    {
        return execute_cargo_test_batch(
            spec,
            &rs_targets,
            dep_hashes,
            k_expr,
            mark_expr,
            extra_args,
            source_label,
            active_pgid,
            active_pgids,
            environment_hash,
        );
    }
    if !fe_targets.is_empty()
        && rs_targets.is_empty()
        && go_targets.is_empty()
        && py_targets.is_empty()
    {
        return execute_frontend_test_batch(
            spec,
            &fe_targets,
            dep_hashes,
            k_expr,
            mark_expr,
            extra_args,
            source_label,
            active_pgid,
            active_pgids,
            environment_hash,
        );
    }
    if !go_targets.is_empty()
        && rs_targets.is_empty()
        && fe_targets.is_empty()
        && py_targets.is_empty()
    {
        return execute_go_test_batch(
            spec,
            &go_targets,
            dep_hashes,
            k_expr,
            extra_args_for_non_rs,
            source_label,
            active_pgid,
            active_pgids,
            environment_hash,
        );
    }
    if !rs_targets.is_empty() || !fe_targets.is_empty() || !go_targets.is_empty() {
        let mut combined_records = Vec::new();
        let mut combined_outputs = Vec::new();
        let (rs_res, fe_res, go_res, py_res) = std::thread::scope(|scope| {
            let rs_handle = if !rs_targets.is_empty() {
                let rs_hashes: BTreeMap<String, String> = dep_hashes
                    .iter()
                    .filter(|(k, _)| k.ends_with(".rs"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let rs_extra_args = if !py_targets.is_empty() || !fe_targets.is_empty() || !go_targets.is_empty() {
                    filter_cargo_args_from_mixed(extra_args)
                } else {
                    extra_args.to_vec()
                };
                Some(scope.spawn(move || {
                    execute_cargo_test_batch(
                        spec,
                        &rs_targets,
                        &rs_hashes,
                        k_expr,
                        mark_expr,
                        &rs_extra_args,
                        source_label,
                        None,
                        active_pgids,
                        environment_hash,
                    )
                }))
            } else {
                None
            };
            let fe_handle = if !fe_targets.is_empty() {
                let fe_hashes: BTreeMap<String, String> = dep_hashes
                    .iter()
                    .filter(|(k, _)| crate::ast_graph::is_frontend_source_path(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(scope.spawn(move || {
                    execute_frontend_test_batch(
                        spec,
                        &fe_targets,
                        &fe_hashes,
                        k_expr,
                        mark_expr,
                        extra_args,
                        source_label,
                        None,
                        active_pgids,
                        environment_hash,
                    )
                }))
            } else {
                None
            };
            let go_handle = if !go_targets.is_empty() {
                let go_hashes: BTreeMap<String, String> = dep_hashes
                    .iter()
                    .filter(|(k, _)| k.ends_with("_test.go"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(scope.spawn(move || {
                    execute_go_test_batch(
                        spec,
                        &go_targets,
                        &go_hashes,
                        k_expr,
                        extra_args_for_non_rs,
                        source_label,
                        active_pgid,
                        active_pgids,
                        environment_hash,
                    )
                }))
            } else {
                None
            };
            let py_res = if !py_targets.is_empty() {
                let py_hashes: BTreeMap<String, String> = dep_hashes
                    .iter()
                    .filter(|(k, _)| {
                        !k.ends_with(".rs")
                            && !k.ends_with("_test.go")
                            && !crate::ast_graph::is_frontend_source_path(k)
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(execute_pytest_batch(
                    spec,
                    &py_targets,
                    &py_hashes,
                    k_expr,
                    mark_expr,
                    extra_args_for_non_rs,
                    source_label,
                    zygote_slot,
                    active_pgid,
                    active_pgids,
                    preload_modules,
                    environment_hash,
                    pytest_plugins,
                    capture_dynamic_dependencies,
                ))
            } else {
                None
            };
            (
                rs_handle.map(|h| h.join().unwrap_or_else(|_| Err(invalid("rs panic").into()))),
                fe_handle.map(|h| h.join().unwrap_or_else(|_| Err(invalid("fe panic").into()))),
                go_handle.map(|h| h.join().unwrap_or_else(|_| Err(invalid("go panic").into()))),
                py_res,
            )
        });
        for res in [rs_res, fe_res, go_res, py_res].into_iter().flatten() {
            let (mut recs, out) = res?;
            combined_records.append(&mut recs);
            if !out.trim().is_empty() {
                combined_outputs.push(out);
            }
        }
        return Ok((combined_records, combined_outputs.join("\n")));
    }

    let extra_args = extra_args_for_non_rs;
    let run_started_unix_ms = now_unix_ms();
    let pytest_cmd = spec.effective_pytest_cmd();
    let Some((program, base_args)) = pytest_cmd.split_first() else {
        return Err(invalid("empty pytest_cmd").into());
    };

    let tmp_dir = tempfile::tempdir()?;
    let junit_path = tmp_dir.path().join("junit.xml");
    let pythonpath = build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH"));

    let (mut zygote_cmd_compatible, configured_pytest_args) =
        extract_zygote_pytest_args(program, base_args);
    let mut common_pytest_args: Vec<String> = vec![
        "-q".to_string(),
        "--tb=short".to_string(),
        "--no-header".to_string(),
        "-p".to_string(),
        "no:cacheprovider".to_string(),
    ];
    if let Some(k) = k_expr
        && !k.trim().is_empty()
    {
        common_pytest_args.push("-k".to_string());
        common_pytest_args.push(k.to_string());
    }
    if let Some(m) = mark_expr
        && !m.trim().is_empty()
    {
        common_pytest_args.push("-m".to_string());
        common_pytest_args.push(m.to_string());
    }
    for arg in extra_args {
        common_pytest_args.push(arg.clone());
    }
    for target in invocation_targets {
        common_pytest_args.push(target.clone());
    }
    let mut zygote_pytest_args = configured_pytest_args;
    zygote_pytest_args.extend(common_pytest_args.iter().cloned());

    // Bypass zygote (falling back to native subprocess pytest) whenever `--disable-plugin-autoload`
    // or `-p no:<plugin>` (other than our own `no:cacheprovider`) is active via CLI args,
    // `PYTEST_ADDOPTS`, or the effective pytest config (including `-c <custom.ini>`), OR when
    // plugin discovery failed (`PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL`), so disabled plugins are
    // respected and failed plugin discovery never runs tests with zero plugins.
    if zygote_cmd_compatible
        && (pytest_run_has_plugin_overrides(&spec.root, &zygote_pytest_args)
            || pytest_plugins
                .iter()
                .any(|p| p == PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL)
            || crate::zygote::script_shebang_has_interpreter_flags(&spec.root, program))
    {
        zygote_cmd_compatible = false;
    }

    let timeout_secs = spec.policy.pytest_timeout_secs;
    let mut output_opt = None;
    if zygote_cmd_compatible
        && let Some(slot) = zygote_slot
        && let Ok(mut manager) = slot.lock()
        && manager
            .ensure_with_env(
                spec,
                pythonpath.clone(),
                preload_modules,
                pytest_plugins,
                environment_hash,
            )
            .is_ok()
    {
        match manager.run(
            &zygote_pytest_args,
            timeout_secs,
            capture_dynamic_dependencies,
            active_pgids,
        ) {
            Ok(out) => {
                output_opt = Some(out);
            }
            Err(err) => {
                // Only fall back to subprocess pytest if the zygote failed BEFORE starting the
                // test child; never re-execute tests a second time after execution began.
                if !err.to_string().contains("ZYGOTE_PRE_START:") {
                    return Err(err);
                }
            }
        }
    }

    let used_zygote = output_opt.is_some();
    let output = if let Some(out) = output_opt {
        out
    } else {
        let mut command = Command::new(program);
        command.current_dir(&spec.root);
        command.env("PYTHONPATH", &pythonpath);
        command.args(base_args);
        command.arg("-o");
        command.arg("junit_family=xunit1");
        command.arg(format!("--junit-xml={}", junit_path.display()));
        // Append only `common_pytest_args` (NOT `configured_pytest_args`, which are already in `base_args`).
        command.args(&common_pytest_args);
        let timeout = Duration::from_secs(timeout_secs);
        run_command_with_timeout_tracked(
            &mut command,
            timeout,
            2 * 1024 * 1024,
            active_pgid,
            active_pgids,
        )?
    };
    if output.timed_out {
        return Err(invalid(format!(
            "pytest execution timed out after {}s",
            spec.policy.pytest_timeout_secs
        ))
        .into());
    }
    if output.cancelled {
        return Err(invalid("TEST_CANCELLED: pytest child process group was terminated").into());
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    let combined_out = if stderr_str.trim().is_empty() {
        stdout_str.to_string()
    } else {
        format!("{}\n{}", stdout_str.trim(), stderr_str.trim())
    };
    let snippet = truncate_str(&combined_out, 8192);

    let (
        per_file,
        dynamic_dependencies,
        shared_dynamic_dependencies,
        module_dynamic_dependencies,
        dynamic_deps_incomplete,
    ) = if let Some(structured) = output.structured.as_deref() {
        parse_structured_reports(structured, dep_hashes)
    } else {
        let xml_content = std::fs::read_to_string(&junit_path).unwrap_or_default();
        (
            parse_junit_xml_per_file(&xml_content, dep_hashes),
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            true,
        )
    };
    // Parse collected item counts per file from zygote output so partially-executed files
    // (e.g. interrupted by `--maxfail=1` or `-x`) are never cached as complete file passes.
    // Verify and store pre-read stamps for every imported third-party `site-packages`/
    // `dist-packages` module file (both lexical & resolved targets for symlinks) and parent
    // package directories (catching nested shadowing like `pkg/helper/__init__.py`). If ANY
    // entry changed between child open and post-run receipt, mark capture incomplete!
    let mut external_module_stamps: BTreeMap<String, crate::cache::ExternalModuleStamp> =
        BTreeMap::new();
    let mut external_module_stamp_failed = false;
    if let Some(ext_entries) = output
        .structured
        .as_deref()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(raw).ok())
        .and_then(|v| {
            v.get("external_module_entries")
                .and_then(|a| a.as_array())
                .cloned()
        })
    {
        for item in ext_entries {
            let Some(arr) = item.as_array() else {
                external_module_stamp_failed = true;
                break;
            };
            if arr.len() != 9 {
                external_module_stamp_failed = true;
                break;
            }
            let (
                Some(lex_s),
                Some(real_s),
                Some(is_dir),
                Some(mtime_sec),
                Some(mtime_nsec),
                Some(size),
                Some(ino),
                Some(ctime_sec),
                Some(ctime_nsec),
            ) = (
                arr[0].as_str(),
                arr[1].as_str(),
                arr[2].as_bool(),
                arr[3].as_i64(),
                arr[4].as_i64(),
                arr[5].as_u64(),
                arr[6].as_u64(),
                arr[7].as_i64(),
                arr[8].as_i64(),
            )
            else {
                external_module_stamp_failed = true;
                break;
            };
            let recorded_stamp = crate::ast_graph::FileStamp {
                mtime_sec,
                mtime_nsec,
                size,
                ino,
                ctime_sec,
                ctime_nsec,
            };
            let Ok(live_canon) = std::fs::canonicalize(Path::new(lex_s)) else {
                external_module_stamp_failed = true;
                break;
            };
            if live_canon != Path::new(real_s) {
                external_module_stamp_failed = true;
                break;
            }
            let live_stamp = if is_dir {
                dir_stamp(&live_canon)
            } else {
                crate::ast_graph::file_stamp(&live_canon)
            };
            if live_stamp != Some(recorded_stamp) {
                external_module_stamp_failed = true;
                break;
            }
            external_module_stamps.insert(
                lex_s.to_string(),
                crate::cache::ExternalModuleStamp {
                    resolved_path: real_s.to_string(),
                    stamp: recorded_stamp,
                    is_dir,
                },
            );
        }
    }
    let dynamic_deps_incomplete = dynamic_deps_incomplete || external_module_stamp_failed;
    let collected_counts: BTreeMap<String, usize> = output
        .structured
        .as_deref()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(raw).ok())
        .and_then(|v| v.get("collected_counts").and_then(|m| m.as_object()).cloned())
        .map(|obj| {
            obj.into_iter()
                .filter_map(|(k, v)| {
                    v.as_u64()
                        .map(|n| (k.trim_start_matches("./").replace('\\', "/"), n as usize))
                })
                .collect()
        })
        .unwrap_or_default();
    let batch_may_exit_early = !output.status.success()
        && (collected_counts.is_empty()
            || zygote_pytest_args.iter().any(|a| {
                a == "-x"
                    || a == "--exitfirst"
                    || a == "--maxfail"
                    || a.starts_with("--maxfail=")
                    || a.starts_with("-x")
            })
            || std::env::var("PYTEST_ADDOPTS").is_ok_and(|v| {
                v.contains("-x") || v.contains("--exitfirst") || v.contains("--maxfail")
            }));
    // Propagate module-attributed runtime reads to every file in the batch that also depends on
    // that module (handled at apply time via `module_dep:<mod>::<data>` sentinel entries, OR wait:
    // encode module-attributed entries cleanly so runner attaches them to matching batch files).
    let module_dep_tokens: Vec<String> = module_dynamic_dependencies
        .into_iter()
        .flat_map(|(mod_rel, data_files)| {
            data_files
                .into_iter()
                .map(move |data_rel| format!("__tctx_mod_dep__:{mod_rel}::{data_rel}"))
        })
        .collect();

    let now = now_unix_ms();
    let mut records = Vec::new();
    let overall_success = output.status.success();
    // If pytest exited nonzero with zero per-test failures/errors recorded, OR exited with
    // exit code >= 2 (interrupted / internal error / usage error), OR emitted session-level
    // teardown/internal error markers alongside a failing test, never cache sibling passes!
    let pytest_exit_code = output.status.code().unwrap_or(1);
    let has_session_error_marker = !overall_success
        && (pytest_exit_code >= 2
            || snippet.contains("INTERNALERROR>")
            || snippet.contains("ERROR at teardown of ")
            || snippet.contains("pytest_sessionfinish"));
    let unexplained_session_failure = !overall_success
        && (has_session_error_marker
            || (!per_file.is_empty()
                && per_file
                    .values()
                    .all(|s| s.failed == 0 && s.errors == 0)));
    let xml_total_ms: u64 = per_file.values().map(|s| s.duration_ms).sum();
    let overhead_per_file =
        output.duration_ms.saturating_sub(xml_total_ms) / (dep_hashes.len().max(1) as u64);

    for (test_file, dep_hash) in dep_hashes {
        if let Some(stats) = per_file.get(test_file) {
            let outcome = if stats.failed > 0 {
                TestOutcome::Failed
            } else if stats.errors > 0 || unexplained_session_failure {
                TestOutcome::Error
            } else if stats.passed > 0 {
                TestOutcome::Passed
            } else if stats.skipped > 0 && overall_success {
                TestOutcome::Skipped
            } else if overall_success {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };
            let eff_errors = if unexplained_session_failure {
                stats.errors.max(1)
            } else {
                stats.errors
            };
            let reported_count = stats.passed + stats.failed + stats.skipped + stats.errors;
            let file_interrupted = collected_counts
                .get(test_file)
                .is_some_and(|&expected| expected > 0 && reported_count < expected)
                || (batch_may_exit_early && outcome.is_green());
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: stats.passed,
                failed: stats.failed,
                skipped: stats.skipped,
                errors: eff_errors,
                duration_ms: stats.duration_ms.saturating_add(overhead_per_file).max(1),
                failed_nodeids: stats.failed_nodeids.clone(),
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    snippet.clone()
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: if used_zygote {
                    "zygote".to_string()
                } else {
                    "subprocess".to_string()
                },
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                // Union per-node reads with shared (collection-time & fixture setup/teardown) reads
                // so session fixtures propagate to every file in a batch without coupling unrelated
                // files to another test's private `call`-phase reads.
                dynamic_dependencies: {
                    let mut merged: BTreeSet<String> = stats.dynamic_dependencies.clone();
                    // Always merge shared (parent-loaded & session-fixture) dependencies for BOTH
                    // singleton and multi-file batches so parent-loaded local files are never lost.
                    merged.extend(shared_dynamic_dependencies.iter().cloned());
                    if dep_hashes.len() == 1 {
                        merged.extend(dynamic_dependencies.iter().cloned());
                    } else {
                        merged.extend(module_dep_tokens.iter().cloned());
                    }
                    merged.into_iter().collect()
                },
                dynamic_dependencies_complete: used_zygote
                    && capture_dynamic_dependencies
                    && !dynamic_deps_incomplete
                    && !unexplained_session_failure
                    && !file_interrupted,
                node_results: stats.node_results.clone(),
                external_module_stamps: external_module_stamps.clone(),
                exit_code: if overall_success {
                    None
                } else {
                    Some(pytest_exit_code)
                },
            });
        } else {
            // Fallback when JUnit XML had no testcase entry for this file (e.g., collection error or single file)
            // If this was a multi-file batch that failed due to a collection error in one file,
            // only record a failure for files explicitly mentioned in the error output; leave unmentioned sibling files uncached rather than poisoning them.
            if !overall_success
                && dep_hashes.len() > 1
                && !combined_out.contains(test_file.as_str())
            {
                continue;
            }
            let per_file_dur = (output.duration_ms / (dep_hashes.len().max(1) as u64)).max(1);
            // Without a per-file report a clean exit is not evidence that this file passed (all of its
            // tests may have been skipped), so it is reported but never cached as a pass.
            let outcome = if overall_success {
                TestOutcome::Skipped
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: 0,
                failed: usize::from(!overall_success),
                skipped: 0,
                errors: 0,
                duration_ms: per_file_dur,
                failed_nodeids: if overall_success {
                    Vec::new()
                } else {
                    vec![test_file.clone()]
                },
                output_snippet: if overall_success {
                    String::new()
                } else {
                    snippet.clone()
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: if used_zygote {
                    "zygote".to_string()
                } else {
                    "subprocess".to_string()
                },
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: if dep_hashes.len() == 1 {
                    dynamic_dependencies.clone()
                } else {
                    Vec::new()
                },
                dynamic_dependencies_complete: used_zygote
                    && capture_dynamic_dependencies
                    && !dynamic_deps_incomplete
                    && !unexplained_session_failure,
                node_results: BTreeMap::new(),
                external_module_stamps: external_module_stamps.clone(),
                exit_code: if overall_success {
                    None
                } else {
                    Some(pytest_exit_code)
                },
            });
        }
    }

    // If pytest exited nonzero and ZERO failure/error records were attributed to any file (e.g.
    // a shared `conftest.py` import error that never mentioned individual test file paths),
    // synthesize an explicit uncacheable Error record so the batch NEVER aggregates as "passed".
    if !overall_success && !records.iter().any(|r| !r.outcome.is_green()) {
        for r in &mut records {
            r.outcome = TestOutcome::Error;
            r.errors = r.errors.max(1);
            r.dynamic_dependencies_complete = false;
            if r.output_snippet.is_empty() {
                r.output_snippet.clone_from(&snippet);
            }
        }
        let existing_files: BTreeSet<String> =
            records.iter().map(|r| r.test_file.clone()).collect();
        let per_file_dur = (output.duration_ms / (dep_hashes.len().max(1) as u64)).max(1);
        for (test_file, dep_hash) in dep_hashes {
            if !existing_files.contains(test_file) {
                records.push(CachedTestRecord {
                    test_file: test_file.clone(),
                    dep_hash: dep_hash.clone(),
                    outcome: TestOutcome::Error,
                    passed: 0,
                    failed: 0,
                    skipped: 0,
                    errors: 1,
                    duration_ms: per_file_dur,
                    failed_nodeids: vec![test_file.clone()],
                    output_snippet: snippet.clone(),
                    updated_unix_ms: now,
                    source: source_label.to_string(),
                    environment_hash: environment_hash.to_string(),
                    tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                    runner_mode: if used_zygote {
                        "zygote".to_string()
                    } else {
                        "subprocess".to_string()
                    },
                    started_unix_ms: run_started_unix_ms,
                    completed_unix_ms: now,
                    dynamic_dependencies: Vec::new(),
                    dynamic_dependencies_complete: false,
                    node_results: BTreeMap::new(),
                    external_module_stamps: BTreeMap::new(),
                    exit_code: Some(pytest_exit_code),
                });
            }
        }
    }

    Ok((records, snippet))
}

/// Extracts pytest CLI flags from a configured `pytest_cmd` for zygote mode.
/// Returns `(true, extra_pytest_args)` ONLY for verified Python (`python`, `python3`, `python3.X`,
/// `pypy3`) `-m pytest [args...]` or direct `pytest [args...]` commands; custom wrapper scripts
/// (e.g. `./run_pytest.sh -m pytest`) return `(false, [])` so subprocess execution honors them.
pub(super) fn extract_zygote_pytest_args(program: &str, base_args: &[String]) -> (bool, Vec<String>) {
    let prog_name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    let stem = prog_name.strip_suffix(".exe").unwrap_or(prog_name);
    if stem == "pytest" || stem.starts_with("pytest-") || stem.starts_with("pytest3") {
        return (true, base_args.to_vec());
    }
    let is_python_interp = stem == "python"
        || stem == "python3"
        || stem == "pypy"
        || stem == "pypy3"
        || stem
            .strip_prefix("python3.")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
    if is_python_interp && base_args.len() >= 2 && base_args[0] == "-m" && base_args[1] == "pytest" {
        return (true, base_args[2..].to_vec());
    }
    (false, Vec::new())
}

pub(super) fn tokens_disable_pytest_plugins(tokens: &[&str]) -> bool {
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok == "--disable-plugin-autoload" || tok.contains("--disable-plugin-autoload") {
            return true;
        }
        if let Some(rest) = tok.strip_prefix("-p") {
            let val = if rest.is_empty() {
                tokens.get(i + 1).copied().unwrap_or("")
            } else {
                rest.trim_start_matches('=')
            };
            if let Some(disabled) = val.strip_prefix("no:")
                && disabled != "cacheprovider"
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Checks whether CLI args, `PYTEST_ADDOPTS`, or the effective pytest config file (including
/// `-c <file>` / `--config-file=<file>`) disables plugin autoloading or blocks any plugin via `-p no:<name>`.
pub(super) fn pytest_run_has_plugin_overrides(root: &Path, args: &[String]) -> bool {
    if std::env::var_os("PYTEST_DISABLE_PLUGIN_AUTOLOAD").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if tokens_disable_pytest_plugins(&arg_refs) {
        return true;
    }
    if let Ok(addopts) = std::env::var("PYTEST_ADDOPTS") {
        let env_toks: Vec<&str> = addopts.split_whitespace().collect();
        if tokens_disable_pytest_plugins(&env_toks) {
            return true;
        }
    }
    let mut cfg_paths: Vec<PathBuf> = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let a = args[idx].as_str();
        // Check `-o addopts=...` / `--override-ini=addopts=...` overrides for plugin disables.
        if (a == "-o" || a == "--override-ini")
            && let Some(val) = args.get(idx + 1)
        {
            if val.contains("--disable-plugin-autoload")
                || val.contains("no:")
                || val.trim_start().starts_with("addopts")
            {
                let toks: Vec<&str> = val
                    .split(|c: char| c.is_whitespace() || c == '=' || c == '"' || c == '\'' || c == ',')
                    .filter(|s| !s.is_empty())
                    .collect();
                if tokens_disable_pytest_plugins(&toks) {
                    return true;
                }
            }
            idx += 2;
            continue;
        }
        if let Some(val) = a
            .strip_prefix("-o=")
            .or_else(|| a.strip_prefix("--override-ini="))
            .or_else(|| if a.starts_with("-o") && a.len() > 2 { Some(&a[2..]) } else { None })
        {
            let toks: Vec<&str> = val
                .split(|c: char| c.is_whitespace() || c == '=' || c == '"' || c == '\'' || c == ',')
                .filter(|s| !s.is_empty())
                .collect();
            if tokens_disable_pytest_plugins(&toks) {
                return true;
            }
        }
        if (a == "-c" || a == "--config-file")
            && let Some(p) = args.get(idx + 1)
        {
            let pb = PathBuf::from(p);
            cfg_paths.push(if pb.is_absolute() { pb } else { root.join(pb) });
            idx += 2;
            continue;
        }
        if let Some(p) = a
            .strip_prefix("-c=")
            .or_else(|| a.strip_prefix("--config-file="))
            .or_else(|| if a.starts_with("-c") && a.len() > 2 { Some(&a[2..]) } else { None })
        {
            let pb = PathBuf::from(p);
            cfg_paths.push(if pb.is_absolute() { pb } else { root.join(pb) });
        }
        idx += 1;
    }
    if cfg_paths.is_empty() {
        for name in [
            "pytest.toml",
            ".pytest.toml",
            "pytest.ini",
            ".pytest.ini",
            "pyproject.toml",
            "setup.cfg",
            "tox.ini",
        ] {
            let p = root.join(name);
            if p.is_file() {
                cfg_paths.push(p);
            }
        }
    }
    for cfg in cfg_paths {
        if let Ok(bytes) = std::fs::read(&cfg) {
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(128 * 1024)]);
            if text.contains("--disable-plugin-autoload") {
                return true;
            }
            for line in text.lines() {
                let toks: Vec<&str> = line
                    .split(|c: char| c.is_whitespace() || c == ',' || c == '"' || c == '\'' || c == '[' || c == ']')
                    .filter(|s| !s.is_empty())
                    .collect();
                if tokens_disable_pytest_plugins(&toks) {
                    return true;
                }
            }
        }
    }
    false
}


