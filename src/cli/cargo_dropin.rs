use super::*;

pub(super) fn run_tracked_direct_cargo_test(
    root: &Path,
    base: &str,
    target_args: &[String],
    parsed: &CargoDropinParsed,
) -> Result<i32> {
    let cargo_cmd = test_context::catalog::detect_cargo_cmd(root);
    let (prog, base_args) = cargo_cmd
        .split_first()
        .ok_or_else(|| "empty cargo_cmd".to_string())?;
    let workdir = if base == "." {
        root.to_path_buf()
    } else {
        root.join(base)
    };
    let is_build_subcmd = target_args.iter().any(|a| a == "__cargo_build__");
    let per_run_node_exact: Option<&str> = target_args
        .iter()
        .find_map(|a| a.strip_prefix("__node_exact__:"));
    let mut cmd = std::process::Command::new(prog);
    cmd.current_dir(&workdir);
    cmd.args(base_args);
    cmd.arg(if is_build_subcmd { "build" } else { "test" });
    // Preserve explicit package identity (`-p <pkg_name>`) so running from a workspace root
    // with `default-members = ["a"]` never tests `a` when `root` was explicitly selected!
    // Skip auto-adding `-p` when `target_args` already specifies `-p` or `--manifest-path`
    // (e.g. unified multi-package runs or external `--manifest-path` delegations).
    if !target_args
        .iter()
        .any(|a| a == "-p" || a == "--package" || a == "--manifest-path" || a == "-m")
        && !parsed
            .extra_args
            .iter()
            .any(|a| a == "-p" || a == "--package")
        && let Some(pkg_name) = test_context::runner::cargo_manifest_package_name(&workdir)
    {
        cmd.arg("-p").arg(pkg_name);
    }
    let target_has_no_run = target_args.iter().any(|a| a == "--no-run");
    for ta in target_args {
        if ta == "__cargo_build__" || ta.starts_with("__node_exact__:") {
            continue;
        }
        cmd.arg(ta);
    }
    if is_build_subcmd {
        for arg in parsed.extra_args.iter().take_while(|a| a.as_str() != "--") {
            if arg != "--no-run" && arg != "--no-fail-fast" {
                cmd.arg(arg);
            }
        }
    } else {
        if let Some(fq) = per_run_node_exact {
            cmd.arg(fq);
        } else if let Some(ref k) = parsed.k {
            cmd.arg(k);
        }
        let mut after_sep = false;
        let mut emitted_sep = false;
        let mut emitted_exact = false;
        for arg in &parsed.extra_args {
            if arg.starts_with("__tctx_cargo_cwd__=")
                || arg.starts_with("__tctx_cargo_target__=")
                || arg.starts_with("__tctx_orig_target__=")
                || arg == "__tctx_orig_empty__"
                || arg.starts_with("__tctx_fe_runner__=")
            {
                continue;
            }
            if arg == "--" {
                after_sep = true;
                emitted_sep = true;
            }
            if !after_sep && target_has_no_run && arg == "--no-run" {
                continue;
            }
            if after_sep && arg == "--exact" {
                emitted_exact = true;
            }
            cmd.arg(arg);
        }
        let wants_ignored = parsed
            .mark
            .as_deref()
            .is_some_and(|m| m == "ignore" || m == "ignored");
        let wants_exact = per_run_node_exact.is_some() && !emitted_exact;
        if wants_ignored || wants_exact {
            if !emitted_sep {
                cmd.arg("--");
            }
            if wants_exact {
                cmd.arg("--exact");
            }
            if wants_ignored {
                cmd.arg("--ignored");
            }
        }
    }
    let policy = test_context::catalog::resolve_spec(root)
        .map(|s| s.policy)
        .unwrap_or_default();
    let compile_timeout = test_context::runner::cargo_compile_timeout_for_policy(&policy);
    let exec_timeout = std::time::Duration::from_secs(policy.pytest_timeout_secs.max(1));
    let has_no_run_flag = target_has_no_run
        || parsed
            .extra_args
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "--no-run");
    let has_doc_flag = target_args.iter().any(|a| a == "--doc");
    // When running (not compile-only) and workspace execution timeout is shorter than compile
    // timeout (including `--doc` doctests and custom harnesses!), precompile first under
    // `compile_timeout` (`--lib --no-run` for `--doc`, `--no-run` otherwise) and then execute
    // under `exec_timeout`!
    let run_timeout = if is_build_subcmd || has_no_run_flag {
        compile_timeout
    } else {
        // Always precompile under `compile_timeout` and execute under `exec_timeout` so short
        // compile timeouts never truncate test execution and short execution timeouts are enforced!
        let mut pre_cmd = std::process::Command::new(prog);
        pre_cmd.current_dir(&workdir);
        pre_cmd.args(base_args);
        if has_doc_flag {
            // `cargo test --doc` builds the non-test library (`cargo build --lib`, NOT `#[cfg(test)]`
            // unit tests!) while preserving `--manifest-path`, `-m`, `-p`, `--package`, `--workspace`,
            // and `--exclude` from `target_args`!
            pre_cmd.arg("build").arg("--lib");
        } else {
            pre_cmd.arg("test").arg("--no-run");
        }
        if !target_args
            .iter()
            .any(|a| a == "-p" || a == "--package" || a == "--manifest-path" || a == "-m")
            && !parsed
                .extra_args
                .iter()
                .any(|a| a == "-p" || a == "--package")
            && let Some(pkg_name) = test_context::runner::cargo_manifest_package_name(&workdir)
        {
            pre_cmd.arg("-p").arg(pkg_name);
        }
        for ta in target_args {
            if ta == "__cargo_build__"
                || ta.starts_with("__node_exact__:")
                || ta == "--no-run"
                || (has_doc_flag && ta == "--doc")
            {
                continue;
            }
            pre_cmd.arg(ta);
        }
        for arg in parsed.extra_args.iter().take_while(|a| a.as_str() != "--") {
            if arg.starts_with("__tctx_cargo_cwd__=")
                || arg.starts_with("__tctx_cargo_target__=")
                || arg.starts_with("__tctx_orig_target__=")
                || arg == "__tctx_orig_empty__"
                || arg.starts_with("__tctx_fe_runner__=")
            {
                continue;
            }
            if arg != "--no-run" && (!has_doc_flag || arg != "--no-fail-fast") {
                pre_cmd.arg(arg);
            }
        }
        let pre_out = test_context::workspace::run_command_with_timeout(
            &mut pre_cmd,
            compile_timeout,
            8 * 1024 * 1024,
        )?;
        if pre_out.timed_out || pre_out.cancelled {
            eprintln!(
                "error: cargo test compilation timed out or was cancelled after {}s in `{base}`",
                compile_timeout.as_secs()
            );
            return Ok(124);
        }
        if !pre_out.status.success() {
            if !pre_out.stdout.is_empty() {
                print!("{}", String::from_utf8_lossy(&pre_out.stdout));
            }
            if !pre_out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&pre_out.stderr));
            }
            return Ok(pre_out.status.code().unwrap_or(101));
        }
        // `cargo test --doc` compiles each doctest binary + dev-dependencies AND runs them in one
        // process (Cargo has no `--doc --no-run` mode), so budget both phases (`compile_timeout +
        // exec_timeout`) for `--doc` while strictly enforcing `exec_timeout` for precompiled runs!
        if has_doc_flag {
            compile_timeout.saturating_add(exec_timeout)
        } else {
            exec_timeout
        }
    };
    let timeout_secs = run_timeout.as_secs();
    let out = test_context::workspace::run_command_with_timeout(
        &mut cmd,
        run_timeout,
        8 * 1024 * 1024,
    )?;
    if !out.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    if out.timed_out || out.cancelled {
        eprintln!(
            "error: cargo test timed out or was cancelled after {timeout_secs}s in `{base}`"
        );
        return Ok(124);
    }
    if !out.status.success() {
        return Ok(out.status.code().unwrap_or(101));
    }
    Ok(0)
}

pub(crate) fn run_cargo_test_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
    let caller_root = root.canonicalize()?;
    let parsed = parse_cargo_test_dropin_args(&caller_root, raw_args);
    if let Some(err_msg) = parsed.unresolved_target_error {
        eprintln!("{err_msg}");
        return Ok(101);
    }
    let exec_root = parsed.effective_root.clone().unwrap_or_else(|| caller_root.clone());
    let no_fail_fast = parsed
        .extra_args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--no-fail-fast");
    let mut first_err_code: Option<i32> = None;
    // 1. Run any direct tracked Cargo fallback invocations (e.g. testless targets, doctests,
    //    unified workspace runs, external/nested manifests, or macro-test targets) with bounded timeout.
    for fb in &parsed.fallback_cargo_runs {
        if parsed.preserve_caller_cwd_for_fallback {
            let code =
                run_tracked_direct_cargo_test(&caller_root, ".", &fb.target_args, &parsed)?;
            if code != 0 {
                if no_fail_fast {
                    first_err_code.get_or_insert(code);
                } else {
                    return Ok(code);
                }
            }
            continue;
        }
        if let Some(ref imp) = parsed.internal_manifest_abs {
            // Preserve caller cwd for nested `--manifest-path nested/Cargo.toml` fallbacks too
            // (so Cargo discovers `.cargo/config.toml` and `rust-toolchain.toml` from `caller_root`)
            // while passing `--manifest-path <abs>` and `-p <pkg>` explicitly!
            let base_abs = if fb.base == "." {
                exec_root.clone()
            } else {
                exec_root.join(&fb.base)
            };
            let mut margs: Vec<String> = vec![
                "--manifest-path".to_string(),
                imp.to_string_lossy().into_owned(),
            ];
            if !fb.target_args.iter().any(|a| a == "-p" || a == "--package")
                && !parsed.extra_args.iter().any(|a| a == "-p" || a == "--package")
                && let Some(pkg_name) =
                    test_context::runner::cargo_manifest_package_name(&base_abs)
            {
                margs.push("-p".to_string());
                margs.push(pkg_name);
            }
            margs.extend(fb.target_args.iter().cloned());
            let code = run_tracked_direct_cargo_test(&caller_root, ".", &margs, &parsed)?;
            if code != 0 {
                if no_fail_fast {
                    first_err_code.get_or_insert(code);
                } else {
                    return Ok(code);
                }
            }
            continue;
        }
        // Whenever a workspace fallback targets a package whose directory differs from
        // `caller_root` (e.g. `cargo test -p a` from workspace root OR `cargo test -p b` from
        // member `a`), preserve `caller_root` as cwd and select the package with `-p <pkg>` so
        // Cargo loads `.cargo/config.toml` / `rust-toolchain.toml` from `caller_root`!
        let fb_base_abs = if fb.base == "." {
            exec_root.clone()
        } else {
            exec_root.join(&fb.base)
        };
        let fb_base_canon = fb_base_abs.canonicalize().unwrap_or(fb_base_abs);
        if fb_base_canon != caller_root
            && test_context::runner::cargo_manifest_has_workspace(&exec_root)
            && !fb.target_args.iter().any(|a| a == "-p" || a == "--package")
            && !parsed.extra_args.iter().any(|a| a == "-p" || a == "--package")
            && let Some(pkg_name) =
                test_context::runner::cargo_manifest_package_name(&fb_base_canon)
        {
            let mut pargs = vec!["-p".to_string(), pkg_name];
            pargs.extend(fb.target_args.iter().cloned());
            let code = run_tracked_direct_cargo_test(&caller_root, ".", &pargs, &parsed)?;
            if code != 0 {
                if no_fail_fast {
                    first_err_code.get_or_insert(code);
                } else {
                    return Ok(code);
                }
            }
            continue;
        }
        let code = run_tracked_direct_cargo_test(&exec_root, &fb.base, &fb.target_args, &parsed)?;
        if code != 0 {
            if no_fail_fast {
                first_err_code.get_or_insert(code);
            } else {
                return Ok(code);
            }
        }
    }
    // 2. When explicit target selectors produced per-identity runs (`--lib`, `--bins`, `--bin NAME`,
    //    `--test NAME`, or combinations like `--lib --bin NAME`), execute each target identity
    //    separately so shared source paths are compiled and tested under EVERY selected target!
    if !parsed.target_runs.is_empty() {
        for tr in &parsed.target_runs {
            let res = daemon::request_or_local_cli(&daemon::Request {
                root: Some(exec_root.clone()),
                name: "test_run".to_string(),
                arguments: json!({
                    "targets": tr.targets,
                    "k": parsed.k,
                    "mark": parsed.mark,
                    "no_cache": parsed.no_cache,
                    "rerun_failed": true,
                    "full_suite": true,
                    "language_filter": "rust",
                    "cargo_target": tr.cargo_target,
                    "pytest_args": parsed.extra_args,
                }),
                compact: false,
            })?;
            if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
                println!("{summary}");
            } else {
                print_json(&res)?;
            }
            match res["status"].as_str() {
                Some("failed") => {
                    let fail_code = res
                        .get("runner_exit_code")
                        .and_then(Value::as_i64)
                        .and_then(|c| i32::try_from(c).ok())
                        .filter(|&c| c != 0)
                        .unwrap_or(101);
                    if no_fail_fast {
                        first_err_code.get_or_insert(fail_code);
                    } else {
                        return Ok(fail_code);
                    }
                }
                Some("no_tests") => {
                    if let Some(ref ct) = tr.cargo_target {
                        let target_args: Vec<String> = if ct == "lib" {
                            vec!["--lib".to_string()]
                        } else if ct == "bins" {
                            vec!["--bins".to_string()]
                        } else if let Some(b) = ct.strip_prefix("bin:") {
                            vec!["--bin".to_string(), b.to_string()]
                        } else if let Some(t) = ct.strip_prefix("test:") {
                            vec!["--test".to_string(), t.to_string()]
                        } else {
                            Vec::new()
                        };
                        if !target_args.is_empty() {
                            for base in &parsed.effective_pkg_bases {
                                let base_abs = if base == "." {
                                    exec_root.clone()
                                } else {
                                    exec_root.join(base)
                                };
                                let has_target = if ct == "lib" {
                                    test_context::runner::cargo_manifest_has_lib(&base_abs)
                                } else if ct == "bins" {
                                    test_context::runner::cargo_manifest_has_any_testable_target(
                                        &base_abs,
                                    )
                                } else if let Some(b) = ct.strip_prefix("bin:") {
                                    test_context::runner::resolve_rust_named_bin_target_path(
                                        &base_abs, b,
                                    )
                                    .is_some()
                                } else if let Some(t) = ct.strip_prefix("test:") {
                                    test_context::runner::resolve_rust_named_test_target_path(
                                        &base_abs, t,
                                    )
                                    .is_some()
                                } else {
                                    false
                                };
                                if !has_target {
                                    continue;
                                }
                                let base_canon = base_abs.canonicalize().unwrap_or(base_abs.clone());
                                let code = if base_canon != caller_root
                                    && test_context::runner::cargo_manifest_has_workspace(&exec_root)
                                    && !target_args.iter().any(|a| a == "-p" || a == "--package")
                                    && !parsed.extra_args.iter().any(|a| a == "-p" || a == "--package")
                                    && let Some(pkg_name) =
                                        test_context::runner::cargo_manifest_package_name(&base_canon)
                                {
                                    let mut pargs = vec!["-p".to_string(), pkg_name];
                                    pargs.extend(target_args.iter().cloned());
                                    run_tracked_direct_cargo_test(
                                        &caller_root,
                                        ".",
                                        &pargs,
                                        &parsed,
                                    )?
                                } else {
                                    run_tracked_direct_cargo_test(
                                        &exec_root,
                                        base,
                                        &target_args,
                                        &parsed,
                                    )?
                                };
                                if code != 0 {
                                    if no_fail_fast {
                                        first_err_code.get_or_insert(code);
                                    } else {
                                        return Ok(code);
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    if no_fail_fast {
                        first_err_code.get_or_insert(101);
                    } else {
                        return Ok(101);
                    }
                }
                _ => {}
            }
        }
        return Ok(first_err_code.unwrap_or(0));
    }
    if parsed.skip_test_run {
        return Ok(first_err_code.unwrap_or(0));
    }
    let had_selection = !parsed.targets.is_empty() || parsed.k.is_some();

    let res = daemon::request_or_local_cli(&daemon::Request {
        root: Some(exec_root.clone()),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": parsed.targets,
            "k": parsed.k,
            "mark": parsed.mark,
            "no_cache": parsed.no_cache,
            "rerun_failed": true,
            "full_suite": true,
            "language_filter": "rust",
            "cargo_target": parsed.cargo_target,
            "pytest_args": parsed.extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    match res["status"].as_str() {
        Some("failed") => {
            let fail_code = first_err_code.unwrap_or_else(|| {
                res.get("runner_exit_code")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok())
                    .filter(|&c| c != 0)
                    .unwrap_or(101)
            });
            Ok(fail_code)
        }
        Some("no_tests") => {
            // When the user explicitly selected `.rs` file(s):
            // - If the file is owned by a declared Cargo target (e.g. a `harness = false` test at
            //   `checks/probe.rs` or a testless `[[bin]]`), execute ONLY its owning target(s)!
            // - If the file is an unreferenced orphan (`src/orphan.rs`), preserve scoped `no_tests`
            //   instead of widening to package-wide `cargo test`!
            let had_explicit_rs_file = !parsed.targets.is_empty()
                && parsed.targets.iter().all(|t| {
                    let f = t.split("::").next().unwrap_or(t).trim();
                    f.ends_with(".rs")
                });
            if had_explicit_rs_file {
                let mut seen_owned: std::collections::BTreeSet<(String, Vec<String>)> =
                    std::collections::BTreeSet::new();
                for eft in &parsed.targets {
                    if eft.contains("::") {
                        eprintln!(
                            "error: node selector `{eft}` cannot be isolated on testless or custom-harness target"
                        );
                        return Ok(101);
                    }
                    let clean = eft
                        .split("::")
                        .next()
                        .unwrap_or(eft)
                        .trim()
                        .trim_start_matches("./");
                    let file_crate =
                        test_context::runner::rust_crate_dir_on_disk(&exec_root, clean);
                    let base = if file_crate.is_empty() {
                        ".".to_string()
                    } else {
                        file_crate
                    };
                    let base_abs = if base == "." {
                        exec_root.clone()
                    } else {
                        exec_root.join(&base)
                    };
                    let rel_in_crate = if base == "." {
                        clean
                    } else {
                        clean.strip_prefix(&format!("{base}/")).unwrap_or(clean)
                    };
                    let owned_targets = test_context::runner::resolve_cargo_target_args_for_file(
                        &base_abs,
                        rel_in_crate,
                        &parsed.extra_args,
                    );
                    let base_canon = base_abs.canonicalize().unwrap_or(base_abs.clone());
                    for targs in owned_targets {
                        if seen_owned.insert((base.clone(), targs.clone())) {
                            let code = if base_canon != caller_root
                                && test_context::runner::cargo_manifest_has_workspace(&exec_root)
                                && !targs.iter().any(|a| a == "-p" || a == "--package")
                                && !parsed.extra_args.iter().any(|a| a == "-p" || a == "--package")
                                && let Some(pkg_name) =
                                    test_context::runner::cargo_manifest_package_name(&base_canon)
                            {
                                let mut pargs = vec!["-p".to_string(), pkg_name];
                                pargs.extend(targs.iter().cloned());
                                run_tracked_direct_cargo_test(&caller_root, ".", &pargs, &parsed)?
                            } else {
                                run_tracked_direct_cargo_test(&exec_root, &base, &targs, &parsed)?
                            };
                            if code != 0 {
                                if no_fail_fast {
                                    first_err_code.get_or_insert(code);
                                } else {
                                    return Ok(code);
                                }
                            }
                        }
                    }
                }
                return Ok(first_err_code.unwrap_or(0));
            }
            // Track coverage PER TARGET (only a full `target_args.is_empty()` fallback covers an
            // entire package — `--lib` alone never masks an uncompiled binary or doctest!).
            let fully_covered_bases: std::collections::BTreeSet<&str> = parsed
                .fallback_cargo_runs
                .iter()
                .filter(|r| r.target_args.is_empty())
                .map(|r| r.base.as_str())
                .collect();
            let mut ran_any_direct = !parsed.fallback_cargo_runs.is_empty();
            for base in &parsed.effective_pkg_bases {
                if fully_covered_bases.contains(base.as_str()) {
                    continue;
                }
                let base_abs = if base == "." {
                    exec_root.clone()
                } else {
                    exec_root.join(base)
                };
                if test_context::runner::cargo_manifest_has_any_testable_target(&base_abs) {
                    ran_any_direct = true;
                    let base_canon = base_abs.canonicalize().unwrap_or(base_abs.clone());
                    let code = if base_canon != caller_root
                        && test_context::runner::cargo_manifest_has_workspace(&exec_root)
                        && !parsed.extra_args.iter().any(|a| a == "-p" || a == "--package")
                        && let Some(pkg_name) =
                            test_context::runner::cargo_manifest_package_name(&base_canon)
                    {
                        run_tracked_direct_cargo_test(
                            &caller_root,
                            ".",
                            &["-p".to_string(), pkg_name],
                            &parsed,
                        )?
                    } else {
                        run_tracked_direct_cargo_test(&exec_root, base, &[], &parsed)?
                    };
                    if code != 0 {
                        if no_fail_fast {
                            first_err_code.get_or_insert(code);
                        } else {
                            return Ok(code);
                        }
                    }
                }
            }
            if had_selection && !ran_any_direct {
                Ok(first_err_code.unwrap_or(101))
            } else {
                Ok(first_err_code.unwrap_or(0))
            }
        }
        _ => Ok(first_err_code.unwrap_or(0)),
    }
}

