use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use test_context::policy::MemoryPolicy;
use test_context::{Result, catalog, daemon, diagnostics, invalid, registration, shared_mcp};

#[derive(Parser)]
#[command(
    name = "tctx",
    version,
    about = "Shared local Rust Python AST dependency graph, speculative background pytest runner, and incremental test cache"
)]
struct Args {
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    scope: Vec<String>,
    #[arg(long)]
    glob: Vec<String>,
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long)]
    no_default_excludes: bool,
    #[arg(long)]
    pytest_cmd: Vec<String>,
    #[arg(long)]
    no_background: bool,
    #[arg(
        long,
        help = "Allow speculative background execution for unmarked tests; default requires @pytest.mark.background_safe"
    )]
    background_all: bool,
    #[arg(
        long,
        help = "Enable multi-file parallel delta execution for modules marked pytestmark=pytest.mark.parallel_safe"
    )]
    parallel_delta: bool,
    #[arg(long, default_value_t = 75)]
    background_max_load_ratio: usize,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Daemon,
    Ping,
    Register,
    Plan,
    Unregister,
    Workspaces,
    SharedMcp,
    Mcp,
    NativeBridge,
    Doctor,
    Status,
    Sync {
        #[arg(long)]
        wait: bool,
    },
    Invalidate {
        #[arg(long)]
        file: Vec<String>,
    },
    AuthorizeWorktrees {
        #[arg(long)]
        revoke: bool,
    },
    PruneWorktrees {
        #[arg(long)]
        apply: bool,
    },
    Graph {
        file: String,
    },
    Affected {
        #[arg(long)]
        file: Vec<String>,
        #[arg(long, default_value_t = true)]
        git_diff: bool,
    },
    Run {
        targets: Vec<String>,
        #[arg(short = 'k', long)]
        k: Option<String>,
        #[arg(short = 'm', long)]
        mark: Option<String>,
        #[arg(long)]
        no_cache: bool,
        #[arg(long)]
        no_rerun_failed: bool,
        #[arg(long)]
        ruff_fix: bool,
        #[arg(long)]
        report_format: Option<String>,
        #[arg(long)]
        report_out: Option<PathBuf>,
        #[arg(long)]
        pytest_arg: Vec<String>,
    },
    Ruff {
        files: Vec<String>,
        #[arg(long)]
        fix: bool,
    },
    Explain,
    CacheExport {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    CacheImport {
        #[arg(long, value_name = "FILE")]
        input: PathBuf,
    },
    HookInstall {
        #[arg(long)]
        pre_push: bool,
    },
    #[command(trailing_var_arg = true)]
    Pytest {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(trailing_var_arg = true)]
    CargoTest {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(trailing_var_arg = true)]
    Vitest {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(trailing_var_arg = true)]
    Jest {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    // Support invocation via `tctx-pytest`, `tctx-cargo-test`, `tctx-vitest`, or `tctx-jest` symlink as a drop-in replacement
    let argv0 = std::env::args()
        .next()
        .and_then(|p| {
            PathBuf::from(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    if argv0 == "tctx-pytest" {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        match run_pytest_dropin(PathBuf::from("."), &raw_args) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("tctx-pytest error: {err}");
                std::process::exit(2);
            }
        }
    }
    if argv0 == "tctx-cargo-test" {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        match run_cargo_test_dropin(PathBuf::from("."), &raw_args) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("tctx-cargo-test error: {err}");
                std::process::exit(2);
            }
        }
    }
    if argv0 == "tctx-vitest" || argv0 == "tctx-jest" {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        let req_runner = if argv0 == "tctx-jest" {
            "jest"
        } else {
            "vitest"
        };
        match run_frontend_dropin(PathBuf::from("."), &raw_args, req_runner) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("{argv0} error: {err}");
                std::process::exit(2);
            }
        }
    }

    match run() {
        Ok(exit_code) => {
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let args = Args::parse();
    match &args.command {
        Action::Daemon => {
            daemon::serve()?;
            Ok(0)
        }
        Action::SharedMcp | Action::Mcp => {
            shared_mcp::serve_stdio()?;
            Ok(0)
        }
        Action::NativeBridge => {
            daemon::native_bridge(io::stdin().lock(), io::stdout().lock())?;
            Ok(0)
        }
        Action::Doctor => {
            print_json(&diagnostics::doctor(&args.root)?)?;
            Ok(0)
        }
        Action::Ping => {
            let res = daemon::request(&daemon::Request {
                root: None,
                name: "ping".to_string(),
                arguments: json!({}),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Workspaces => {
            let res = daemon::request_or_local(&daemon::Request {
                root: None,
                name: "test_workspaces".to_string(),
                arguments: json!({}),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::AuthorizeWorktrees { revoke } => {
            print_json(&registration::authorize(&args.root, !revoke)?)?;
            Ok(0)
        }
        Action::PruneWorktrees { apply } => {
            print_json(&registration::prune(*apply)?)?;
            Ok(0)
        }
        Action::Register | Action::Plan => {
            let planning = matches!(&args.command, Action::Plan);
            let policy = MemoryPolicy {
                background_enabled: !args.no_background,
                background_safe_only: !args.background_all,
                parallel_delta_enabled: args.parallel_delta,
                background_max_load_ratio_percent: args.background_max_load_ratio,
                ..MemoryPolicy::default()
            };
            policy.validate().map_err(invalid)?;
            let spec = catalog::Spec {
                root: args.root.clone(),
                scopes: args.scope.clone(),
                globs: args.glob.clone(),
                excludes: args.exclude.clone(),
                default_excludes: !args.no_default_excludes,
                pytest_cmd: args.pytest_cmd.clone(),
                policy,
                allow_worktrees: false,
                managed_parent: None,
            };
            let plan = registration::plan(&spec)?;
            if planning {
                print_json(&plan)?;
                return Ok(0);
            }
            let registered = catalog::register(spec)?;
            // Notify running daemon if reachable so it starts watching immediately
            let _ = daemon::request(&daemon::Request {
                root: Some(registered.root),
                name: "test_status".to_string(),
                arguments: json!({"load": true}),
                compact: false,
            });
            print_json(&json!({ "registered": true, "plan": plan }))?;
            Ok(0)
        }
        Action::Unregister => {
            catalog::unregister(&args.root)?;
            print_json(&json!({ "unregistered": true }))?;
            Ok(0)
        }
        Action::Status => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_status".to_string(),
                arguments: json!({"load": true}),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Sync { wait } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_sync".to_string(),
                arguments: json!({ "wait": wait }),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Invalidate { file } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_invalidate".to_string(),
                arguments: json!({ "files": file }),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Graph { file } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_graph".to_string(),
                arguments: json!({ "file": file }),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Affected { file, git_diff } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_affected".to_string(),
                arguments: json!({ "files": file, "git_diff": git_diff }),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::Run {
            targets,
            k,
            mark,
            no_cache,
            no_rerun_failed,
            ruff_fix,
            report_format,
            report_out,
            pytest_arg,
        } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local_cli(&daemon::Request {
                root: Some(root),
                name: "test_run".to_string(),
                arguments: json!({
                    "targets": targets,
                    "k": k,
                    "mark": mark,
                    "no_cache": no_cache,
                    "rerun_failed": !no_rerun_failed,
                    "ruff_fix": ruff_fix,
                    "full_suite": true,
                    "pytest_args": pytest_arg,
                }),
                compact: false,
            })?;
            if let Some(fmt) = report_format.as_deref()
                && let Ok(parsed_resp) =
                    serde_json::from_value::<test_context::runner::TestRunResponse>(res.clone())
            {
                let rendered = match fmt {
                    "junit" => test_context::runner::format_junit_xml_report(&parsed_resp),
                    "sarif" => serde_json::to_string_pretty(
                        &test_context::runner::format_sarif_report(&parsed_resp),
                    )?,
                    _ => serde_json::to_string_pretty(&res)?,
                };
                if let Some(out_path) = report_out {
                    std::fs::write(out_path, rendered.as_bytes())?;
                } else {
                    println!("{rendered}");
                    let failed = res["status"].as_str() == Some("failed");
                    return Ok(if failed { 1 } else { 0 });
                }
            }
            print_json(&res)?;
            let failed = res["status"].as_str() == Some("failed");
            Ok(if failed { 1 } else { 0 })
        }
        Action::Ruff { files, fix } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_ruff".to_string(),
                arguments: json!({
                    "files": files,
                    "fix": fix,
                }),
                compact: false,
            })?;
            print_json(&res)?;
            let failed = res["status"].as_str() == Some("failed");
            Ok(if failed { 1 } else { 0 })
        }
        Action::Explain => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_explain".to_string(),
                arguments: json!({}),
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::CacheExport { out } => {
            let root = args.root.canonicalize()?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_cache_export".to_string(),
                arguments: json!({}),
                compact: false,
            })?;
            if let Some(out_path) = out {
                std::fs::write(out_path, serde_json::to_vec_pretty(&res)?)?;
            } else {
                print_json(&res)?;
            }
            Ok(0)
        }
        Action::CacheImport { input } => {
            let root = args.root.canonicalize()?;
            let bytes = std::fs::read(input)?;
            let bundle: Value = serde_json::from_slice(&bytes)?;
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_cache_import".to_string(),
                arguments: bundle,
                compact: false,
            })?;
            print_json(&res)?;
            Ok(0)
        }
        Action::HookInstall { pre_push } => {
            use std::os::unix::fs::PermissionsExt;
            let root = args.root.canonicalize()?;
            let hooks_dir = root.join(".git/hooks");
            if !hooks_dir.is_dir() {
                return Err(invalid("no .git/hooks directory found in workspace root").into());
            }
            let hook_name = if *pre_push { "pre-push" } else { "pre-commit" };
            let hook_path = hooks_dir.join(hook_name);
            let script = "#!/usr/bin/env bash\nset -euo pipefail\nrepo_root=\"$(git rev-parse --show-toplevel)\"\nexec tctx --root \"$repo_root\" run --ruff-fix\n";
            std::fs::write(&hook_path, script)?;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
            print_json(&json!({
                "installed": true,
                "hook": hook_name,
                "path": hook_path,
            }))?;
            Ok(0)
        }
        Action::Pytest { args: pytest_args } => run_pytest_dropin(args.root.clone(), pytest_args),
        Action::CargoTest { args: cargo_args } => {
            run_cargo_test_dropin(args.root.clone(), cargo_args)
        }
        Action::Vitest { args: fe_args } => {
            run_frontend_dropin(args.root.clone(), fe_args, "vitest")
        }
        Action::Jest { args: fe_args } => {
            run_frontend_dropin(args.root.clone(), fe_args, "jest")
        }
    }
}

fn run_frontend_dropin(root: PathBuf, raw_args: &[String], requested_runner: &str) -> Result<i32> {
    let root = root.canonicalize()?;
    let mut targets = Vec::new();
    let mut k: Option<String> = None;
    let mut no_cache = false;
    let mut extra_args = Vec::new();

    let mut i = 0usize;
    if matches!(raw_args.first().map(String::as_str), Some("run" | "watch")) {
        i = 1;
    }
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "-t" | "--testNamePattern" | "-k" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("error: option {arg} requires a value");
                return Ok(1);
            }
            "--no-cache" | "--clearCache" => {
                no_cache = true;
            }
            "--runTestsByPath" | "--watch=false" => {}
            // Two-token Vitest/Jest CLI options whose value must NOT be mistaken for a test target!
            "-c" | "--config" | "--root" | "--dir" | "--maxWorkers" | "-w"
            | "--maxConcurrency" | "--pool" | "--reporter" | "--reporters"
            | "--testPathPattern" | "--testPathIgnorePatterns" | "--setupFilesAfterEnv"
            | "--environment" | "--testEnvironment" | "--project" | "--shard"
            | "--outputFile" | "--coverageDirectory" | "--mode" | "--bail" | "-b"
            | "--testTimeout" | "--hookTimeout" | "--retry" => {
                extra_args.push(arg.clone());
                if i + 1 < raw_args.len() {
                    extra_args.push(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            _ if arg.starts_with('-') => {
                extra_args.push(arg.clone());
            }
            _ => {
                // Vitest and Jest treat positional arguments like `Button` as filename substring
                // patterns rather than exact paths. Only route existing paths or `file::node`
                // selectors into `targets`; forward filename patterns directly to the runner!
                let file_part = arg.split("::").next().unwrap_or(arg);
                let exists_in_root = root.join(file_part).exists()
                    || Path::new(file_part).exists()
                    || arg.contains("::");
                if exists_in_root {
                    targets.push(arg.clone());
                } else {
                    extra_args.push(arg.clone());
                }
            }
        }
        i += 1;
    }

    let res = daemon::request_or_local_cli(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": targets,
            "k": k,
            "no_cache": no_cache,
            "rerun_failed": true,
            "full_suite": true,
            "language_filter": requested_runner,
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    match res["status"].as_str() {
        Some("failed" | "no_tests") => Ok(1),
        _ => Ok(0),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoTargetRun {
    cargo_target: Option<String>,
    targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoFallbackRun {
    base: String,
    target_args: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CargoDropinParsed {
    targets: Vec<String>,
    k: Option<String>,
    mark: Option<String>,
    no_cache: bool,
    cargo_target: Option<String>,
    /// Per-target-identity invocations preserved when multiple Cargo target selectors are combined
    /// (e.g. `--lib --bin app` or `--bin a --bin b`) so shared source paths run under each identity.
    target_runs: Vec<CargoTargetRun>,
    /// Direct Cargo invocations (executed with bounded timeout & process-group cancellation) for
    /// targets with 0 statically discovered tests or module-level macro-generated tests.
    fallback_cargo_runs: Vec<CargoFallbackRun>,
    /// When true, invoke `fallback_cargo_runs` from the caller's original working directory
    /// (preserving `.cargo/config.toml` and `rust-toolchain.toml` discovery, e.g. `--workspace`
    /// from a member directory or `--manifest-path`).
    preserve_caller_cwd_for_fallback: bool,
    /// Absolute path to an internal nested `--manifest-path nested/Cargo.toml` so direct Cargo
    /// fallbacks run from `caller_root` with `--manifest-path <abs>` instead of changing cwd!
    internal_manifest_abs: Option<PathBuf>,
    /// Effective package directories selected by `-p`, `--manifest-path`, `--workspace`, or
    /// `default-members`, retained for targetless `no_tests` fallback verification.
    effective_pkg_bases: Vec<String>,
    /// Set when `--workspace` / `--all` is invoked from inside a nested workspace member directory
    /// so sibling members are resolved and executed relative to Cargo's actual `workspace_root`.
    effective_root: Option<PathBuf>,
    /// True when all selected targets were completely handled by `fallback_cargo_runs` so
    /// `run_cargo_test_dropin` skips `test_run` and never runs those tests twice.
    skip_test_run: bool,
    unresolved_target_error: Option<String>,
    extra_args: Vec<String>,
}

fn resolve_package_dir_targets(root: &Path, pkg: &str) -> (Option<PathBuf>, Vec<String>) {
    // 1. Prefer authoritative Cargo metadata so `-p <pkg>` supports globs (`-p 'test-*'`),
    //    `name@version`, legacy `name:version`, full pkgids, AND sibling workspace packages when
    //    invoked from inside a member directory (returning Cargo's actual `workspace_root`)!
    if root.join("Cargo.toml").is_file()
        && let Some((ws_root, dirs)) =
            test_context::runner::resolve_cargo_package_dirs_via_metadata(root, pkg)
    {
        return (Some(ws_root), dirs);
    }
    let (spec_name, want_ver) = test_context::runner::parse_cargo_pkg_spec(pkg);
    if spec_name.is_empty() {
        return (None, Vec::new());
    }
    let glob_matcher = if spec_name.contains(['*', '?', '[']) {
        globset::GlobBuilder::new(spec_name)
            .literal_separator(false)
            .build()
            .ok()
            .map(|g| g.compile_matcher())
    } else {
        None
    };
    let matches_dir = |dir: &Path| -> bool {
        let Some((name, ver)) =
            test_context::runner::cargo_manifest_package_name_and_version(dir)
        else {
            return false;
        };
        let name_ok = if let Some(ref gm) = glob_matcher {
            gm.is_match(&name)
        } else {
            name == spec_name
        };
        if !name_ok {
            return false;
        }
        if let Some(wv) = want_ver {
            let Some(actual) = ver.as_deref() else {
                return false;
            };
            if !test_context::runner::cargo_version_spec_matches(actual, wv) {
                return false;
            }
        }
        true
    };
    // 2. Static fallback (e.g. synthetic test directories without a root Cargo.toml): match ONLY
    //    explicit `[package] name = "..."` (and version/glob when specified).
    let mut out = Vec::new();
    if matches_dir(root) {
        out.push(".".to_string());
    }
    if glob_matcher.is_none() {
        let alt = spec_name.replace('-', "_");
        let alt_dash = spec_name.replace('_', "-");
        for prefix in ["", "crates/", "packages/", "lib_rs/", "apps/", "services/", "tools/"] {
            for cand in [spec_name, alt.as_str(), alt_dash.as_str()] {
                let rel = if prefix.is_empty() {
                    cand.to_string()
                } else {
                    format!("{prefix}{cand}")
                };
                if !rel.is_empty() && matches_dir(&root.join(&rel)) && !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    (None, out)
}

fn parse_cargo_test_dropin_args(root: &Path, raw_args: &[String]) -> CargoDropinParsed {
    let mut parsed = CargoDropinParsed::default();
    let start_idx = usize::from(raw_args.first().map(String::as_str) == Some("test"));
    // Normalize `--flag=value` forms before `--` into separate tokens (`--test=smoke` ->
    // `["--test", "smoke"]`, `--bin=cli`, `--package=foo`, `-p=foo`, `--manifest-path=...`, etc.)
    // so Cargo equals-form selectors never bypass target resolution.
    let mut norm_args: Vec<String> = Vec::with_capacity(raw_args.len());
    let mut saw_sep = false;
    for arg in &raw_args[start_idx..] {
        if saw_sep {
            norm_args.push(arg.clone());
            continue;
        }
        if arg == "--" {
            saw_sep = true;
            norm_args.push(arg.clone());
            continue;
        }
        if let Some((flag, val)) = arg.split_once('=')
            && matches!(
                flag,
                "--test"
                    | "--bin"
                    | "-p"
                    | "--package"
                    | "--exclude"
                    | "-m"
                    | "--manifest-path"
                    | "-F"
                    | "--features"
                    | "--config"
                    | "--message-format"
                    | "-Z"
                    | "--target"
                    | "--target-dir"
                    | "--profile"
                    | "-j"
                    | "--jobs"
                    | "--color"
                    | "--bench"
                    | "--example"
            )
        {
            norm_args.push(flag.to_string());
            norm_args.push(val.to_string());
        } else if let Some((flag, val)) = ["-p", "-m", "-F", "-j", "-Z"]
            .iter()
            .find_map(|f| {
                arg.strip_prefix(f)
                    .filter(|v| !v.is_empty())
                    .map(|v| (*f, v))
            })
        {
            // Normalize attached short Cargo options (`-pfoo`, `-mnested/Cargo.toml`, `-Ffeat`,
            // `-j4`, `-Z...`) into two tokens using UTF-8-safe `strip_prefix` (never byte slicing!).
            norm_args.push(flag.to_string());
            norm_args.push(val.to_string());
        } else {
            norm_args.push(arg.clone());
        }
    }

    let mut i = 0usize;
    let mut after_double_dash = false;
    let mut selected_pkg_dirs: Vec<String> = Vec::new();
    let mut unresolved_pkg_specs: Vec<String> = Vec::new();
    let mut excluded_pkg_specs: Vec<String> = Vec::new();
    let mut selected_tests: Vec<String> = Vec::new();
    let mut selected_bins: Vec<String> = Vec::new();
    let mut selected_examples: Vec<String> = Vec::new();
    let mut selected_benches: Vec<String> = Vec::new();
    let mut selected_lib = false;
    let mut selected_all_bins = false;
    let mut selected_all_tests = false;
    let mut selected_all_examples = false;
    let mut selected_all_benches = false;
    let mut selected_all_targets = false;
    let mut selected_doc = false;
    let mut selected_workspace = false;
    let mut manifest_base_dir: Option<String> = None;
    let mut internal_manifest_abs: Option<PathBuf> = None;
    let mut external_manifest_path: Option<PathBuf> = None;
    let mut saw_manifest_path = false;
    let mut positional_args: Vec<String> = Vec::new();
    while i < norm_args.len() {
        let arg = &norm_args[i];
        if after_double_dash {
            match arg.as_str() {
                "--ignored" => {
                    parsed.mark = Some("ignore".to_string());
                }
                "-q" | "--quiet" => {}
                "--skip" | "--test-threads" | "--logfile" | "--format" | "--color" => {
                    parsed.extra_args.push(arg.clone());
                    if i + 1 < norm_args.len() {
                        parsed.extra_args.push(norm_args[i + 1].clone());
                        i += 2;
                        continue;
                    }
                }
                _ if arg.starts_with('-') => {
                    parsed.extra_args.push(arg.clone());
                }
                _ => {
                    if parsed.k.is_none() {
                        parsed.k = Some(arg.clone());
                    } else {
                        parsed.extra_args.push(arg.clone());
                    }
                }
            }
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                after_double_dash = true;
                parsed.extra_args.push("--".to_string());
            }
            "--test" => {
                if i + 1 < norm_args.len() {
                    selected_tests.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <NAME>' but none was supplied"
                ));
                return parsed;
            }
            "--bin" => {
                if i + 1 < norm_args.len() {
                    selected_bins.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <NAME>' but none was supplied"
                ));
                return parsed;
            }
            "-p" | "--package" => {
                if i + 1 < norm_args.len() {
                    let pkg = &norm_args[i + 1];
                    unresolved_pkg_specs.push(pkg.clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <SPEC>' but none was supplied"
                ));
                return parsed;
            }
            "-m" | "--manifest-path" => {
                if saw_manifest_path {
                    parsed.unresolved_target_error = Some(format!(
                        "error: the argument '{arg} <PATH>' cannot be used multiple times"
                    ));
                    return parsed;
                }
                saw_manifest_path = true;
                if i + 1 < norm_args.len() {
                    let mp = &norm_args[i + 1];
                    let mp_path = Path::new(mp);
                    let mp_abs = if mp_path.is_absolute() {
                        mp_path.to_path_buf()
                    } else {
                        root.join(mp_path)
                    };
                    // Validate the actual manifest path before consuming it so invalid paths like
                    // `--manifest-path ./missing.toml` report Cargo's error (exit 101) instead of
                    // silently testing the parent directory's `Cargo.toml`!
                    if !mp_abs.is_file() {
                        parsed.unresolved_target_error =
                            Some(format!("error: manifest path `{mp}` does not exist"));
                        return parsed;
                    }
                    if mp_abs.file_name().and_then(|n| n.to_str()) != Some("Cargo.toml") {
                        parsed.unresolved_target_error = Some(format!(
                            "error: the manifest-path must be a path to a Cargo.toml file (`{mp}`)"
                        ));
                        return parsed;
                    }
                    let canon_parent = mp_abs
                        .parent()
                        .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
                    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                    if let Some(cp) = canon_parent {
                        if let Ok(rel) = cp.strip_prefix(&canon_root) {
                            let s = rel.to_string_lossy().replace('\\', "/");
                            manifest_base_dir =
                                Some(if s.is_empty() { ".".to_string() } else { s });
                            internal_manifest_abs =
                                Some(mp_abs.canonicalize().unwrap_or(mp_abs));
                            external_manifest_path = None;
                        } else {
                            // External `--manifest-path /other/repo/Cargo.toml`: preserve the
                            // caller's working directory (so `.cargo/config.toml` and
                            // `rust-toolchain.toml` discovery match Cargo!) and forward the exact
                            // absolute `--manifest-path` to direct tracked Cargo execution.
                            external_manifest_path =
                                Some(mp_abs.canonicalize().unwrap_or(mp_abs));
                            manifest_base_dir = None;
                            internal_manifest_abs = None;
                        }
                    }
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <PATH>' but none was supplied"
                ));
                return parsed;
            }
            "--workspace" | "--all" => {
                if selected_workspace {
                    parsed.unresolved_target_error = Some(format!(
                        "error: the argument '{arg}' cannot be used multiple times"
                    ));
                    return parsed;
                }
                selected_workspace = true;
            }
            "--exclude" => {
                if i + 1 < norm_args.len() {
                    excluded_pkg_specs.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <SPEC>' but none was supplied"
                ));
                return parsed;
            }
            "-F" | "--features" | "--config" | "--message-format" | "-Z" | "--target"
            | "--target-dir" | "--profile" | "-j" | "--jobs" | "--color" => {
                if i + 1 < norm_args.len() {
                    parsed.extra_args.push(arg.clone());
                    parsed.extra_args.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <VALUE>' but none was supplied"
                ));
                return parsed;
            }
            "--example" => {
                if i + 1 < norm_args.len() {
                    selected_examples.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <NAME>' but none was supplied"
                ));
                return parsed;
            }
            "--bench" => {
                if i + 1 < norm_args.len() {
                    selected_benches.push(norm_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                parsed.unresolved_target_error = Some(format!(
                    "error: a value is required for '{arg} <NAME>' but none was supplied"
                ));
                return parsed;
            }
            "--lib" => {
                if selected_lib {
                    parsed.unresolved_target_error =
                        Some("error: the argument '--lib' cannot be used multiple times".to_string());
                    return parsed;
                }
                selected_lib = true;
            }
            "--bins" => {
                if selected_all_bins {
                    parsed.unresolved_target_error =
                        Some("error: the argument '--bins' cannot be used multiple times".to_string());
                    return parsed;
                }
                selected_all_bins = true;
            }
            "--tests" => {
                if selected_all_tests {
                    parsed.unresolved_target_error =
                        Some("error: the argument '--tests' cannot be used multiple times".to_string());
                    return parsed;
                }
                selected_all_tests = true;
            }
            "--examples" => {
                if selected_all_examples {
                    parsed.unresolved_target_error = Some(
                        "error: the argument '--examples' cannot be used multiple times".to_string(),
                    );
                    return parsed;
                }
                selected_all_examples = true;
            }
            "--benches" => {
                if selected_all_benches {
                    parsed.unresolved_target_error = Some(
                        "error: the argument '--benches' cannot be used multiple times".to_string(),
                    );
                    return parsed;
                }
                selected_all_benches = true;
            }
            "--all-targets" => {
                if selected_all_targets {
                    parsed.unresolved_target_error = Some(
                        "error: the argument '--all-targets' cannot be used multiple times"
                            .to_string(),
                    );
                    return parsed;
                }
                selected_all_targets = true;
            }
            "--doc" => {
                if selected_doc {
                    parsed.unresolved_target_error =
                        Some("error: the argument '--doc' cannot be used multiple times".to_string());
                    return parsed;
                }
                selected_doc = true;
            }
            "--no-cache" => {
                parsed.no_cache = true;
            }
            "--ignored" => {
                parsed.mark = Some("ignore".to_string());
            }
            "-q" | "--quiet" => {}
            _ if arg.starts_with('-') => {
                parsed.extra_args.push(arg.clone());
            }
            _ => {
                positional_args.push(arg.clone());
                let looks_like_file = (arg.ends_with(".rs") || arg.contains('/'))
                    && root.join(arg.split("::").next().unwrap_or(arg)).exists();
                if looks_like_file {
                    parsed.targets.push(arg.clone());
                } else if parsed.k.is_none() {
                    parsed.k = Some(arg.clone());
                } else {
                    parsed.extra_args.push(arg.clone());
                }
            }
        }
        i += 1;
    }

    // Nested `--manifest-path nested/Cargo.toml` (not at `.`) without explicit `.rs` file targets:
    // delegate directly from `caller_root` with `--manifest-path <abs>` so Cargo discovers
    // `.cargo/config.toml` and `rust-toolchain.toml` from `caller_root` (not `nested/`) identically
    // to native `cargo test --manifest-path nested/Cargo.toml`!
    if let Some(ref imp) = internal_manifest_abs
        && manifest_base_dir.as_deref() != Some(".")
        && parsed.targets.is_empty()
    {
        let mut nested_args: Vec<String> = vec![
            "--manifest-path".to_string(),
            imp.to_string_lossy().into_owned(),
        ];
        if selected_workspace {
            nested_args.push("--workspace".to_string());
        }
        for p in &unresolved_pkg_specs {
            nested_args.push("-p".to_string());
            nested_args.push(p.clone());
        }
        for ex in &excluded_pkg_specs {
            nested_args.push("--exclude".to_string());
            nested_args.push(ex.clone());
        }
        if selected_lib {
            nested_args.push("--lib".to_string());
        }
        if selected_all_bins {
            nested_args.push("--bins".to_string());
        }
        if selected_all_tests {
            nested_args.push("--tests".to_string());
        }
        if selected_all_examples {
            nested_args.push("--examples".to_string());
        }
        if selected_all_benches {
            nested_args.push("--benches".to_string());
        }
        if selected_all_targets {
            nested_args.push("--all-targets".to_string());
        }
        if selected_doc {
            nested_args.push("--doc".to_string());
        }
        for b in &selected_bins {
            nested_args.push("--bin".to_string());
            nested_args.push(b.clone());
        }
        for t in &selected_tests {
            nested_args.push("--test".to_string());
            nested_args.push(t.clone());
        }
        for ex in &selected_examples {
            nested_args.push("--example".to_string());
            nested_args.push(ex.clone());
        }
        for bn in &selected_benches {
            nested_args.push("--bench".to_string());
            nested_args.push(bn.clone());
        }
        parsed.fallback_cargo_runs.push(CargoFallbackRun {
            base: ".".to_string(),
            target_args: nested_args,
        });
        parsed.preserve_caller_cwd_for_fallback = true;
        parsed.skip_test_run = true;
        if parsed.extra_args.last().map(String::as_str) == Some("--") {
            parsed.extra_args.pop();
        }
        parsed.extra_args =
            test_context::runner::normalize_cargo_config_args(root, &parsed.extra_args);
        return parsed;
    }
    // Validate `--exclude` requires `--workspace` / `--all` (matching Cargo's exact CLI rule!).
    if !excluded_pkg_specs.is_empty() && !selected_workspace {
        parsed.unresolved_target_error =
            Some("error: --exclude can only be used together with --workspace".to_string());
        return parsed;
    }
    // External `--manifest-path /other/repo/Cargo.toml`: delegate directly to tracked `cargo test`
    // from the original invocation directory `root` with `--manifest-path <abs_path>` and all
    // user selectors preserved, so Cargo's `.cargo/config.toml`, toolchain, and package/target
    // resolution match native `cargo test` 100% without touching the caller's workspace cache!
    if let Some(ref ext_mp) = external_manifest_path {
        let ext_dir = ext_mp.parent().unwrap_or(root);
        let looks_like_external_file = |s: &str| -> bool {
            let base = s.split("::").next().unwrap_or(s);
            base.ends_with(".rs")
                || (base.contains('/') && (root.join(base).exists() || ext_dir.join(base).exists()))
        };
        if let Some(bad_file) = positional_args
            .iter()
            .find(|s| looks_like_external_file(s))
        {
            parsed.unresolved_target_error = Some(format!(
                "error: file-path target `{bad_file}` cannot be combined with external --manifest-path `{}`",
                ext_mp.display()
            ));
            return parsed;
        }
        let mut ext_args: Vec<String> = vec![
            "--manifest-path".to_string(),
            ext_mp.to_string_lossy().into_owned(),
        ];
        if selected_workspace {
            ext_args.push("--workspace".to_string());
        }
        for p in &unresolved_pkg_specs {
            ext_args.push("-p".to_string());
            ext_args.push(p.clone());
        }
        for ex in &excluded_pkg_specs {
            ext_args.push("--exclude".to_string());
            ext_args.push(ex.clone());
        }
        if selected_lib {
            ext_args.push("--lib".to_string());
        }
        if selected_all_bins {
            ext_args.push("--bins".to_string());
        }
        if selected_all_tests {
            ext_args.push("--tests".to_string());
        }
        if selected_all_examples {
            ext_args.push("--examples".to_string());
        }
        if selected_all_benches {
            ext_args.push("--benches".to_string());
        }
        if selected_all_targets {
            ext_args.push("--all-targets".to_string());
        }
        if selected_doc {
            ext_args.push("--doc".to_string());
        }
        for b in &selected_bins {
            ext_args.push("--bin".to_string());
            ext_args.push(b.clone());
        }
        for t in &selected_tests {
            ext_args.push("--test".to_string());
            ext_args.push(t.clone());
        }
        for ex in &selected_examples {
            ext_args.push("--example".to_string());
            ext_args.push(ex.clone());
        }
        for bn in &selected_benches {
            ext_args.push("--bench".to_string());
            ext_args.push(bn.clone());
        }
        parsed.fallback_cargo_runs.push(CargoFallbackRun {
            base: ".".to_string(),
            target_args: ext_args,
        });
        parsed.preserve_caller_cwd_for_fallback = true;
        parsed.skip_test_run = true;
        if parsed.extra_args.last().map(String::as_str) == Some("--") {
            parsed.extra_args.pop();
        }
        parsed.extra_args =
            test_context::runner::normalize_cargo_config_args(root, &parsed.extra_args);
        return parsed;
    }
    let mut active_root = root.to_path_buf();
    let canon_root = active_root
        .canonicalize()
        .unwrap_or_else(|_| active_root.clone());
    // Resolve all `-p` / `--package` specs (exact names, globs `'test-*'`, `name@ver`,
    // `name:ver`, full pkgids, AND sibling workspace packages from inside a member directory)
    // against `manifest_base_dir` (if specified) or `active_root`.
    if !unresolved_pkg_specs.is_empty() {
        let lookup_root = manifest_base_dir
            .as_deref()
            .map(|mb| {
                if mb == "." {
                    active_root.clone()
                } else {
                    active_root.join(mb)
                }
            })
            .unwrap_or_else(|| active_root.clone());
        let mut still_unresolved = Vec::new();
        for pkg in unresolved_pkg_specs {
            let (ws_root_opt, dirs) = resolve_package_dir_targets(&lookup_root, &pkg);
            if !dirs.is_empty() {
                if let Some(ref ws_root) = ws_root_opt
                    && *ws_root != canon_root
                {
                    active_root = ws_root.clone();
                    parsed.effective_root = Some(ws_root.clone());
                    for d in dirs {
                        if !selected_pkg_dirs.contains(&d) {
                            selected_pkg_dirs.push(d);
                        }
                    }
                } else {
                    for pkg_dir in dirs {
                        let joined = match manifest_base_dir.as_deref() {
                            Some(mb)
                                if ws_root_opt.is_none() && mb != "." && !mb.is_empty() =>
                            {
                                if pkg_dir == "." {
                                    mb.to_string()
                                } else {
                                    format!("{mb}/{pkg_dir}")
                                }
                            }
                            _ => pkg_dir,
                        };
                        if !selected_pkg_dirs.contains(&joined) {
                            selected_pkg_dirs.push(joined);
                        }
                    }
                }
            } else {
                still_unresolved.push(pkg);
            }
        }
        if let Some(bad_pkg) = still_unresolved.first() {
            if lookup_root.join("Cargo.toml").is_file() {
                parsed.unresolved_target_error = Some(format!(
                    "error: package ID specification `{bad_pkg}` did not match any packages"
                ));
                return parsed;
            }
            for pkg in still_unresolved {
                parsed.extra_args.push("-p".to_string());
                parsed.extra_args.push(pkg);
            }
        }
    }
    // Deduplicate repeated named target selectors (`--test foo --test foo`, `--bin a --bin a`)
    // preserving first-seen order so Cargo-accepted duplicate named selectors never execute twice!
    let dedup_preserve = |vec: &mut Vec<String>| {
        let mut seen = std::collections::BTreeSet::new();
        vec.retain(|s| seen.insert(s.clone()));
    };
    dedup_preserve(&mut selected_tests);
    dedup_preserve(&mut selected_bins);
    dedup_preserve(&mut selected_examples);
    dedup_preserve(&mut selected_benches);
    // Reject conflicting `--doc` + other target selector combinations (matching Cargo's exact
    // CLI error `Can't mix --doc with other target selecting options`!).
    let has_non_doc_target_selector = !selected_tests.is_empty()
        || !selected_bins.is_empty()
        || !selected_examples.is_empty()
        || !selected_benches.is_empty()
        || selected_lib
        || selected_all_bins
        || selected_all_tests
        || selected_all_examples
        || selected_all_benches
        || selected_all_targets;
    if selected_doc && has_non_doc_target_selector {
        parsed.unresolved_target_error = Some(
            "error: Can't mix --doc with other target selecting options".to_string(),
        );
        return parsed;
    }
    parsed.internal_manifest_abs = internal_manifest_abs.clone();
    // Honor Cargo's `--workspace` precedence FIRST: `--workspace -p foo` selects the ENTIRE
    // workspace (NOT just `-p foo`)!
    let mut effective_pkg_bases: Vec<String> = if selected_workspace {
        let ws_lookup = manifest_base_dir
            .as_deref()
            .map(|mb| {
                if mb == "." {
                    active_root.clone()
                } else {
                    active_root.join(mb)
                }
            })
            .unwrap_or_else(|| active_root.clone());
        if let Some((ws_root, ws_members)) =
            test_context::runner::discover_cargo_workspace_root_and_members(&ws_lookup, false)
        {
            if ws_root != canon_root {
                active_root = ws_root.clone();
                parsed.effective_root = Some(ws_root);
            }
            ws_members
        } else {
            let members =
                test_context::runner::discover_cargo_workspace_member_dirs(&ws_lookup, false);
            if members.is_empty() {
                vec![manifest_base_dir.clone().unwrap_or_else(|| ".".to_string())]
            } else {
                match manifest_base_dir.as_deref() {
                    Some(mb) if mb != "." && !mb.is_empty() => members
                        .into_iter()
                        .map(|m| {
                            if m == "." || m.is_empty() {
                                mb.to_string()
                            } else {
                                format!("{mb}/{m}")
                            }
                        })
                        .collect(),
                    _ => members,
                }
            }
        }
    } else if !selected_pkg_dirs.is_empty() {
        selected_pkg_dirs.clone()
    } else if let Some(ref mb) = manifest_base_dir {
        let mb_abs = if mb == "." {
            active_root.clone()
        } else {
            active_root.join(mb)
        };
        if !test_context::runner::cargo_manifest_has_package(&mb_abs)
            || test_context::runner::cargo_manifest_has_workspace(&mb_abs)
        {
            let members = test_context::runner::discover_cargo_workspace_member_dirs(&mb_abs, true);
            if members.is_empty() {
                vec![mb.clone()]
            } else {
                members
                    .into_iter()
                    .map(|m| {
                        if mb == "." || mb.is_empty() {
                            m
                        } else if m == "." || m.is_empty() {
                            mb.clone()
                        } else {
                            format!("{mb}/{m}")
                        }
                    })
                    .collect()
            }
        } else {
            vec![mb.clone()]
        }
    } else if !test_context::runner::cargo_manifest_has_package(&active_root)
        || test_context::runner::cargo_manifest_has_workspace(&active_root)
    {
        let members =
            test_context::runner::discover_cargo_workspace_member_dirs(&active_root, true);
        if members.is_empty() {
            vec![".".to_string()]
        } else {
            members
        }
    } else {
        vec![".".to_string()]
    };
    if !excluded_pkg_specs.is_empty() {
        effective_pkg_bases.retain(|base| {
            let base_abs = if base == "." {
                active_root.clone()
            } else {
                active_root.join(base)
            };
            let Some((pkg_name, pkg_ver)) =
                test_context::runner::cargo_manifest_package_name_and_version(&base_abs)
            else {
                return true;
            };
            // Follow Cargo's `--exclude <SPEC>` semantics strictly: match `[package].name` (or
            // glob pattern) AND validate `@version` whenever a version spec is provided so
            // `--exclude pkg@0.0.0` never excludes `pkg@0.1.4`!
            !excluded_pkg_specs.iter().any(|spec| {
                let s = spec.trim();
                // For URL-qualified exclusions (e.g. `--exclude file:///does-not-exist#package`),
                // require the source URI to actually match this package's manifest directory!
                if s.contains("://") {
                    let src_part = s.split_once('#').map_or(s, |(src, _)| src);
                    let canon_base = base_abs
                        .canonicalize()
                        .unwrap_or_else(|_| base_abs.clone());
                    let src_matches = if let Some(parsed_path) =
                        test_context::runner::cargo_file_url_to_path(src_part)
                    {
                        let spec_dir = parsed_path
                            .canonicalize()
                            .unwrap_or(parsed_path);
                        spec_dir == canon_base
                    } else {
                        false
                    };
                    if !src_matches {
                        return false;
                    }
                }
                let (name_spec, want_ver) = test_context::runner::parse_cargo_pkg_spec(s);
                let name_matches = if name_spec == pkg_name {
                    true
                } else if name_spec.contains(['*', '?', '['])
                    && let Ok(glob) = globset::GlobBuilder::new(name_spec)
                        .literal_separator(false)
                        .build()
                {
                    glob.compile_matcher().is_match(&pkg_name)
                } else {
                    false
                };
                if !name_matches {
                    return false;
                }
                if let Some(wv) = want_ver {
                    let Some(actual) = pkg_ver.as_deref() else {
                        return false;
                    };
                    return test_context::runner::cargo_version_spec_matches(actual, wv);
                }
                true
            })
        });
        if effective_pkg_bases.is_empty() {
            parsed.unresolved_target_error =
                Some("error: no packages matched after applying --exclude".to_string());
            return parsed;
        }
    }
    parsed.effective_pkg_bases = effective_pkg_bases.clone();
    let has_specific_target_flag = !selected_tests.is_empty()
        || !selected_bins.is_empty()
        || !selected_examples.is_empty()
        || !selected_benches.is_empty()
        || selected_lib
        || selected_all_bins
        || selected_all_tests
        || selected_all_examples
        || selected_all_benches
        || selected_all_targets
        || selected_doc;
    // Record the explicit Cargo target identity when a single target selector mode was passed so
    // runner ownership resolution resolves shared modules cleanly and never reverse-expands into
    // unrelated target kinds. When multiple selectors are combined (e.g. `--lib --bin NAME`),
    // each target identity is preserved in `parsed.target_runs` and executed separately!
    let target_mode_count = usize::from(selected_lib)
        + usize::from(selected_all_bins)
        + usize::from(selected_all_tests)
        + usize::from(selected_all_examples)
        + usize::from(selected_all_benches)
        + usize::from(selected_all_targets)
        + usize::from(selected_doc)
        + selected_bins.len()
        + selected_tests.len()
        + selected_examples.len()
        + selected_benches.len();
    if target_mode_count == 1 {
        if selected_lib {
            parsed.cargo_target = Some("lib".to_string());
        } else if selected_all_bins {
            parsed.cargo_target = Some("bins".to_string());
        } else if selected_doc {
            parsed.cargo_target = Some("doc".to_string());
        } else if let Some(b) = selected_bins.first() {
            parsed.cargo_target = Some(format!("bin:{b}"));
        } else if let Some(t) = selected_tests.first() {
            parsed.cargo_target = Some(format!("test:{t}"));
        }
    }
    // Check whether any selected workspace member has a different directory than `caller_root`
    // (`*root`) AND either that member OR `caller_root` (or any intermediate directory under
    // `active_root`) has a local `.cargo/config[.toml]` or `rust-toolchain[.toml]`. Native
    // `cargo test -p <pkg>` ALWAYS discovers Cargo config/toolchain from `caller_root` (e.g.
    // running `cargo test -p b` from member `a` loads `a/.cargo/config.toml`, NOT `b`'s!).
    let has_local_cfg_under_ws = |dir_abs: &Path| -> bool {
        let Ok(rel_from_ws) = dir_abs.strip_prefix(&active_root) else {
            return false;
        };
        if rel_from_ws.as_os_str().is_empty() {
            return false;
        }
        let mut rel = rel_from_ws;
        loop {
            let d = active_root.join(rel);
            if d.join(".cargo/config.toml").exists()
                || d.join(".cargo/config").exists()
                || d.join("rust-toolchain.toml").exists()
                || d.join("rust-toolchain").exists()
            {
                return true;
            }
            match rel.parent() {
                Some(p) if !p.as_os_str().is_empty() => rel = p,
                _ => break,
            }
        }
        false
    };
    let rustup_override_under_ws = || -> bool {
        let rustup_settings = std::env::var_os("RUSTUP_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".rustup"))
            })
            .map(|d| d.join("settings.toml"));
        let Some(settings_path) = rustup_settings else {
            return false;
        };
        let Ok(txt) = std::fs::read_to_string(settings_path) else {
            return false;
        };
        let active_canon = active_root
            .canonicalize()
            .unwrap_or_else(|_| active_root.clone());
        for dir_str in test_context::runner::parse_rustup_override_dirs(&txt) {
            let ov_path = Path::new(&dir_str)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&dir_str));
            if ov_path.starts_with(&active_canon) && ov_path != active_canon {
                return true;
            }
        }
        false
    };
    let caller_has_local_cfg = has_local_cfg_under_ws(root);
    let any_rustup_override_in_ws = rustup_override_under_ws();
    let has_member_local_cargo_cfg =
        test_context::runner::cargo_manifest_has_workspace(&active_root)
            && effective_pkg_bases.iter().any(|b| {
                let base_abs = if b == "." {
                    active_root.clone()
                } else {
                    active_root.join(b)
                };
                let base_canon = base_abs.canonicalize().unwrap_or(base_abs);
                base_canon != *root
                    && (caller_has_local_cfg
                        || any_rustup_override_in_ws
                        || has_local_cfg_under_ws(&base_canon))
            });
    if has_member_local_cargo_cfg && !parsed.targets.is_empty() {
        // File-scoped `tctx cargo test -p b b/src/lib.rs` invoked from a different directory with
        // local Cargo config: pass caller cwd so `execute_cargo_test_batch` preserves caller cwd
        // and selects the package via `-p`!
        parsed
            .extra_args
            .push(format!("__tctx_cargo_cwd__={}", root.display()));
    }
    // When multiple workspace packages are selected AND cross-package dependencies enable extra
    // features on another selected package (Cargo workspace feature unification), OR when caller
    // cwd differs from a selected member with local `.cargo/config[.toml]` / `rust-toolchain[.toml]`,
    // delegate directly from `caller_root` with `-p <pkg>` so Cargo's exact cwd config & unified
    // feature set are honored!
    if parsed.targets.is_empty()
        && (has_member_local_cargo_cfg
            || test_context::runner::workspace_needs_unified_cargo_features(
                &active_root,
                &effective_pkg_bases,
                &parsed.extra_args,
            ))
    {
        let mut unified_args: Vec<String> = Vec::new();
        if let Some(ref imp) = internal_manifest_abs {
            unified_args.push("--manifest-path".to_string());
            unified_args.push(imp.to_string_lossy().into_owned());
        }
        for base in &effective_pkg_bases {
            let base_abs = if base == "." {
                active_root.clone()
            } else {
                active_root.join(base)
            };
            if let Some(pkg_name) = test_context::runner::cargo_manifest_package_name(&base_abs) {
                unified_args.push("-p".to_string());
                unified_args.push(pkg_name);
            }
        }
        if selected_lib {
            unified_args.push("--lib".to_string());
        }
        if selected_all_bins {
            unified_args.push("--bins".to_string());
        }
        if selected_all_tests {
            unified_args.push("--tests".to_string());
        }
        if selected_all_examples {
            unified_args.push("--examples".to_string());
        }
        if selected_all_benches {
            unified_args.push("--benches".to_string());
        }
        if selected_all_targets {
            unified_args.push("--all-targets".to_string());
        }
        if selected_doc {
            unified_args.push("--doc".to_string());
        }
        for b in &selected_bins {
            unified_args.push("--bin".to_string());
            unified_args.push(b.clone());
        }
        for t in &selected_tests {
            unified_args.push("--test".to_string());
            unified_args.push(t.clone());
        }
        for ex in &selected_examples {
            unified_args.push("--example".to_string());
            unified_args.push(ex.clone());
        }
        for bn in &selected_benches {
            unified_args.push("--bench".to_string());
            unified_args.push(bn.clone());
        }
        parsed.fallback_cargo_runs.push(CargoFallbackRun {
            base: ".".to_string(),
            target_args: unified_args,
        });
        parsed.preserve_caller_cwd_for_fallback = true;
        parsed.skip_test_run = true;
        if parsed.extra_args.last().map(String::as_str) == Some("--") {
            parsed.extra_args.pop();
        }
        parsed.extra_args =
            test_context::runner::normalize_cargo_config_args(root, &parsed.extra_args);
        return parsed;
    }
    // Rebase explicit caller-relative file/node targets (e.g. from member `a`, `-p b ../b/src/lib.rs`)
    // against `caller_root` (`root`) onto `active_root` BEFORE enforcing package/exclusion scope!
    if !parsed.targets.is_empty() && active_root != *root {
        let active_canon = active_root
            .canonicalize()
            .unwrap_or_else(|_| active_root.clone());
        for t in &mut parsed.targets {
            let (raw_file, node_suffix) = match t.split_once("::") {
                Some((f, n)) => (f.trim(), Some(n)),
                None => (t.trim(), None),
            };
            let abs_cand = if Path::new(raw_file).is_absolute() {
                PathBuf::from(raw_file)
            } else {
                root.join(raw_file)
            };
            if let Ok(canon_f) = abs_cand.canonicalize()
                && let Ok(rel_to_active) = canon_f.strip_prefix(&active_canon)
            {
                let rel_str = rel_to_active.to_string_lossy().replace('\\', "/");
                *t = match node_suffix {
                    Some(n) => format!("{rel_str}::{n}"),
                    None => rel_str,
                };
            }
        }
    }
    // Enforce package/exclusion scope on explicit positional `.rs` file targets BEFORE
    // overlapping-selector or target-flag expansion so commands like
    // `tctx cargo-test --workspace --exclude b --tests --lib b/src/lib.rs` NEVER bypass exclusion!
    let explicit_file_targets: Vec<String> = parsed.targets.clone();
    if !explicit_file_targets.is_empty()
        && (!selected_pkg_dirs.is_empty()
            || !excluded_pkg_specs.is_empty()
            || manifest_base_dir.is_some())
    {
        for eft in &explicit_file_targets {
            let clean = eft
                .split("::")
                .next()
                .unwrap_or(eft)
                .trim()
                .trim_start_matches("./");
            let file_crate = test_context::runner::rust_crate_dir_on_disk(&active_root, clean);
            let file_base = if file_crate.is_empty() {
                "."
            } else {
                file_crate.as_str()
            };
            if !effective_pkg_bases.iter().any(|b| b == file_base) {
                parsed.unresolved_target_error = Some(format!(
                    "error: explicit file target `{eft}` is outside the selected/non-excluded packages"
                ));
                return parsed;
            }
        }
    }
    // When overlapping broad + specific Cargo target selectors are combined (e.g. `--tests --lib`,
    // `--all-targets --lib`, `--bins --bin foo`, `--examples --example bar`, `--benches --bench b`)
    // WITHOUT explicit positional `.rs` file targets, delegate the exact combined flags to ONE
    // tracked Cargo invocation per package so Cargo deduplicates target identities cleanly!
    let has_overlapping_target_selectors = explicit_file_targets.is_empty()
        && ((selected_all_targets && target_mode_count > 1)
            || (selected_all_tests
                && (selected_lib
                    || selected_all_bins
                    || !selected_bins.is_empty()
                    || !selected_tests.is_empty()))
            || (selected_all_bins && !selected_bins.is_empty())
            || (selected_all_examples && !selected_examples.is_empty())
            || (selected_all_benches && !selected_benches.is_empty()));
    if has_overlapping_target_selectors {
        let mut combined_flags: Vec<String> = Vec::new();
        if selected_all_targets {
            combined_flags.push("--all-targets".to_string());
        }
        if selected_lib {
            combined_flags.push("--lib".to_string());
        }
        if selected_all_bins {
            combined_flags.push("--bins".to_string());
        }
        if selected_all_tests {
            combined_flags.push("--tests".to_string());
        }
        if selected_all_examples {
            combined_flags.push("--examples".to_string());
        }
        if selected_all_benches {
            combined_flags.push("--benches".to_string());
        }
        for b in &selected_bins {
            combined_flags.push("--bin".to_string());
            combined_flags.push(b.clone());
        }
        for t in &selected_tests {
            combined_flags.push("--test".to_string());
            combined_flags.push(t.clone());
        }
        for ex in &selected_examples {
            combined_flags.push("--example".to_string());
            combined_flags.push(ex.clone());
        }
        for bn in &selected_benches {
            combined_flags.push("--bench".to_string());
            combined_flags.push(bn.clone());
        }
        for base in &effective_pkg_bases {
            parsed.fallback_cargo_runs.push(CargoFallbackRun {
                base: base.clone(),
                target_args: combined_flags.clone(),
            });
        }
        parsed.skip_test_run = true;
        if parsed.extra_args.last().map(String::as_str) == Some("--") {
            parsed.extra_args.pop();
        }
        parsed.extra_args =
            test_context::runner::normalize_cargo_config_args(root, &parsed.extra_args);
        return parsed;
    }
    let join_pkg = |base: &str, rel: &str| -> String {
        if base == "." || base.is_empty() {
            rel.to_string()
        } else {
            format!("{base}/{rel}")
        }
    };
    if !has_specific_target_flag && !parsed.targets.is_empty() {
        // Explicit positional `.rs` file target(s) were supplied without `--lib`/`--test`/`--bin`:
        // keep the request strictly scoped to those files instead of widening to package-wide
        // doctests, examples, or sibling workspace packages!
    } else if !has_specific_target_flag {
        let is_single_non_workspace_root = effective_pkg_bases.len() == 1
            && effective_pkg_bases[0] == "."
            && selected_pkg_dirs.is_empty()
            && excluded_pkg_specs.is_empty()
            && manifest_base_dir.is_none()
            && !test_context::runner::cargo_manifest_has_workspace(&active_root);
        let mut any_partial_or_full_fallback = false;
        let mut remaining_targets: Vec<String> = Vec::new();
        for base in &effective_pkg_bases {
            let base_abs = if base == "." {
                active_root.clone()
            } else {
                active_root.join(base)
            };
            let pkg_all_files =
                test_context::runner::collect_rust_package_all_target_files_for_args(
                    &base_abs,
                    &parsed.extra_args,
                );
            if pkg_all_files.is_empty() {
                if test_context::runner::cargo_manifest_has_any_testable_target(&base_abs) {
                    // Package has 0 statically discovered tests across all targets (e.g. testless
                    // lib/bin, macro-only tests, or doctests-only): run full `cargo test -p <pkg>`
                    // ONCE so lib, bins, integration tests, and doctests are all compiled & checked!
                    any_partial_or_full_fallback = true;
                    parsed.fallback_cargo_runs.push(CargoFallbackRun {
                        base: base.clone(),
                        target_args: Vec::new(),
                    });
                } else if !is_single_non_workspace_root && base != "." {
                    remaining_targets.push(base.clone());
                }
                continue;
            }
            let lib_direct =
                test_context::runner::rust_lib_target_needs_direct_cargo_mode(&base_abs, true);
            let bins_direct = test_context::runner::rust_all_bins_need_direct_cargo_mode(
                &base_abs,
                true,
                &parsed.extra_args,
            );
            let doc_direct = test_context::runner::rust_lib_has_potential_doctests(&base_abs);
            let integ_targets: Vec<(String, String)> =
                test_context::runner::collect_rust_integration_test_roots_and_names(&base_abs)
                    .into_iter()
                    .filter(|(tname, _)| {
                        test_context::runner::cargo_test_target_features_satisfied(
                            &base_abs,
                            tname,
                            &parsed.extra_args,
                        )
                    })
                    .collect();
            let mut dynamic_test_names: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for (tname, troot) in &integ_targets {
                if test_context::runner::rust_target_tree_may_have_dynamic_tests(&base_abs, troot)
                {
                    dynamic_test_names.insert(tname.clone());
                }
            }
            if lib_direct {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--lib".to_string()],
                });
            }
            if bins_direct {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--bins".to_string()],
                });
            }
            for tname in &dynamic_test_names {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--test".to_string(), tname.clone()],
                });
            }
            if doc_direct {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--doc".to_string()],
                });
            }
            if test_context::runner::cargo_manifest_has_examples(&base_abs) {
                // Default `cargo test` executes `[[example]]` targets marked `test = true` ONLY
                // when their `required-features` are satisfied by the transitive feature closure
                // (e.g. `default = ["foo"]`, `foo = ["bar"]`), AND compiles all other eligible
                // examples via `--examples --no-run`.
                let (test_examples, _) =
                    test_context::runner::collect_rust_test_enabled_examples(&base_abs);
                for (ex, req_feats) in test_examples {
                    if test_context::runner::cargo_required_features_satisfied(
                        &base_abs,
                        &req_feats,
                        &parsed.extra_args,
                    ) {
                        parsed.fallback_cargo_runs.push(CargoFallbackRun {
                            base: base.clone(),
                            target_args: vec!["--example".to_string(), ex],
                        });
                    }
                }
                // Default `cargo test` builds ordinary examples WITHOUT `#[cfg(test)]` (identical
                // to `cargo build --examples`), NOT `cargo test --examples --no-run`!
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["__cargo_build__".to_string(), "--examples".to_string()],
                });
            }
            let has_code_target_fallback =
                lib_direct || bins_direct || !dynamic_test_names.is_empty();
            if has_code_target_fallback {
                any_partial_or_full_fallback = true;
                // Push ONLY non-fallback target files so directly executed targets never run twice!
                if !lib_direct {
                    for lf in test_context::runner::collect_rust_lib_target_files(&base_abs) {
                        remaining_targets.push(join_pkg(base, &lf));
                    }
                }
                if !bins_direct {
                    for bp in test_context::runner::collect_rust_all_bin_target_paths_for_args(
                        &base_abs,
                        &parsed.extra_args,
                    ) {
                        remaining_targets.push(join_pkg(base, &bp));
                    }
                }
                for (tname, _) in &integ_targets {
                    if !dynamic_test_names.contains(tname)
                        && let Some(tfiles) =
                            test_context::runner::resolve_rust_named_test_target_files(
                                &base_abs, tname,
                            )
                    {
                        for tf in tfiles {
                            remaining_targets.push(join_pkg(base, &tf));
                        }
                    }
                }
            } else if !is_single_non_workspace_root {
                if base != "." {
                    remaining_targets.push(base.clone());
                } else {
                    for rf in pkg_all_files {
                        remaining_targets.push(rf);
                    }
                }
            }
        }
        if any_partial_or_full_fallback {
            if remaining_targets.is_empty() && parsed.targets.is_empty() {
                parsed.skip_test_run = true;
            } else {
                for t in remaining_targets {
                    parsed.targets.push(t);
                }
            }
        } else if !is_single_non_workspace_root {
            if remaining_targets.is_empty()
                && parsed.targets.is_empty()
                && !excluded_pkg_specs.is_empty()
            {
                parsed.skip_test_run = true;
            } else {
                for t in remaining_targets {
                    parsed.targets.push(t);
                }
            }
        }
    } else {
        let mut any_lib_exists = false;
        let mut matched_test_names: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut matched_bin_names: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut lib_run_targets: Vec<String> = Vec::new();
        let mut all_bins_run_targets: Vec<String> = Vec::new();
        let mut named_bin_runs: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let mut named_test_runs: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for base in &effective_pkg_bases {
            let base_abs = if base == "." {
                active_root.clone()
            } else {
                active_root.join(base)
            };
            if selected_lib || selected_doc {
                let has_lib = test_context::runner::cargo_manifest_has_lib(&base_abs);
                if has_lib {
                    any_lib_exists = true;
                }
                if selected_lib {
                    if has_lib
                        && test_context::runner::rust_lib_target_needs_direct_cargo(&base_abs)
                    {
                        parsed.fallback_cargo_runs.push(CargoFallbackRun {
                            base: base.clone(),
                            target_args: vec!["--lib".to_string()],
                        });
                    } else {
                        for lf in test_context::runner::collect_rust_lib_target_files(&base_abs) {
                            let joined = join_pkg(base, &lf);
                            parsed.targets.push(joined.clone());
                            lib_run_targets.push(joined);
                        }
                    }
                }
                if selected_doc && has_lib {
                    parsed.fallback_cargo_runs.push(CargoFallbackRun {
                        base: base.clone(),
                        target_args: vec!["--doc".to_string()],
                    });
                }
            }
            if selected_all_targets {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--all-targets".to_string()],
                });
            }
            if selected_all_tests {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--tests".to_string()],
                });
            }
            if selected_all_examples {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--examples".to_string()],
                });
            }
            if selected_all_benches {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--benches".to_string()],
                });
            }
            for ex in &selected_examples {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--example".to_string(), ex.clone()],
                });
            }
            for bn in &selected_benches {
                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                    base: base.clone(),
                    target_args: vec!["--bench".to_string(), bn.clone()],
                });
            }
            if selected_all_bins {
                if test_context::runner::rust_all_bins_need_direct_cargo_mode(
                    &base_abs,
                    false,
                    &parsed.extra_args,
                ) {
                    parsed.fallback_cargo_runs.push(CargoFallbackRun {
                        base: base.clone(),
                        target_args: vec!["--bins".to_string()],
                    });
                } else {
                    for bp in test_context::runner::collect_rust_all_bin_target_paths_for_args(
                        &base_abs,
                        &parsed.extra_args,
                    ) {
                        let joined = join_pkg(base, &bp);
                        parsed.targets.push(joined.clone());
                        all_bins_run_targets.push(joined);
                    }
                }
            }
            for name in &selected_tests {
                // Expand Cargo-compatible globs (`--test 'integration*'`) as well as literal names.
                let resolved_names: Vec<(String, String)> = if name.contains(['*', '?', '[']) {
                    if let Ok(glob) = globset::GlobBuilder::new(name)
                        .literal_separator(false)
                        .build()
                    {
                        let matcher = glob.compile_matcher();
                        test_context::runner::collect_rust_integration_test_roots_and_names(
                            &base_abs,
                        )
                        .into_iter()
                        .filter(|(tname, _)| matcher.is_match(tname))
                        .collect()
                    } else {
                        Vec::new()
                    }
                } else if let Some(root_rel) =
                    test_context::runner::resolve_rust_named_test_target_path(&base_abs, name)
                {
                    vec![(name.clone(), root_rel)]
                } else {
                    Vec::new()
                };
                if !resolved_names.is_empty() {
                    matched_test_names.insert(name.clone());
                }
                for (tname, root_rel) in resolved_names {
                    if test_context::runner::rust_target_tree_may_have_dynamic_tests(
                        &base_abs, &root_rel,
                    ) {
                        parsed.fallback_cargo_runs.push(CargoFallbackRun {
                            base: base.clone(),
                            target_args: vec!["--test".to_string(), tname],
                        });
                        parsed.targets.push(join_pkg(base, &root_rel));
                    } else if let Some(files) =
                        test_context::runner::resolve_rust_named_test_target_files(
                            &base_abs, &tname,
                        )
                    {
                        let entry = named_test_runs.entry(tname).or_default();
                        for f in files {
                            let joined = join_pkg(base, &f);
                            parsed.targets.push(joined.clone());
                            entry.push(joined);
                        }
                    }
                }
            }
            for name in &selected_bins {
                let resolved_bins: Vec<(String, String)> = if name.contains(['*', '?', '[']) {
                    if let Ok(glob) = globset::GlobBuilder::new(name)
                        .literal_separator(false)
                        .build()
                    {
                        let matcher = glob.compile_matcher();
                        test_context::runner::collect_rust_bin_roots_and_names(&base_abs)
                            .into_iter()
                            .filter(|(bname, _)| matcher.is_match(bname))
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else if let Some(root_rel) =
                    test_context::runner::resolve_rust_named_bin_target_path(&base_abs, name)
                {
                    vec![(name.clone(), root_rel)]
                } else {
                    Vec::new()
                };
                if !resolved_bins.is_empty() {
                    matched_bin_names.insert(name.clone());
                }
                for (bname, root_rel) in resolved_bins {
                    if let Some(files) =
                        test_context::runner::resolve_rust_named_bin_target_files(&base_abs, &bname)
                    {
                        if test_context::runner::rust_named_bin_needs_direct_cargo(
                            &base_abs, &bname,
                        ) && test_context::runner::rust_target_tree_may_have_dynamic_tests(
                            &base_abs, &root_rel,
                        ) {
                            parsed.fallback_cargo_runs.push(CargoFallbackRun {
                                base: base.clone(),
                                target_args: vec!["--bin".to_string(), bname],
                            });
                            for f in files {
                                parsed.targets.push(join_pkg(base, &f));
                            }
                            continue;
                        }
                        let entry = named_bin_runs.entry(bname).or_default();
                        for f in files {
                            let joined = join_pkg(base, &f);
                            parsed.targets.push(joined.clone());
                            entry.push(joined);
                        }
                    }
                }
            }
        }
        if !lib_run_targets.is_empty() {
            parsed.target_runs.push(CargoTargetRun {
                cargo_target: Some("lib".to_string()),
                targets: lib_run_targets,
            });
        }
        if !all_bins_run_targets.is_empty() {
            parsed.target_runs.push(CargoTargetRun {
                cargo_target: Some("bins".to_string()),
                targets: all_bins_run_targets,
            });
        }
        for (name, targets) in named_bin_runs {
            if !targets.is_empty() {
                parsed.target_runs.push(CargoTargetRun {
                    cargo_target: Some(format!("bin:{name}")),
                    targets,
                });
            }
        }
        for (name, targets) in named_test_runs {
            if !targets.is_empty() {
                parsed.target_runs.push(CargoTargetRun {
                    cargo_target: Some(format!("test:{name}")),
                    targets,
                });
            }
        }
        if !explicit_file_targets.is_empty() {
            // Intersect explicit positional `.rs` file targets (preserving `::node` selectors!)
            // with the selected target flags AND validate against every fallback target's owned
            // files so `--example demo tests/foo.rs` is rejected instead of running an unrelated
            // fallback, and `--lib src/foo.rs::passing_test` preserves `::passing_test`!
            let mut explicit_by_file: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for eft in &explicit_file_targets {
                let clean = eft
                    .split("::")
                    .next()
                    .unwrap_or(eft)
                    .trim()
                    .trim_start_matches("./")
                    .to_string();
                explicit_by_file.entry(clean).or_default().push(eft.clone());
            }
            for tr in &mut parsed.target_runs {
                let mut narrowed: Vec<String> = Vec::new();
                for t in &tr.targets {
                    let clean = t.trim_start_matches("./");
                    if let Some(orig_list) = explicit_by_file.get(clean) {
                        for orig in orig_list {
                            if !narrowed.contains(orig) {
                                narrowed.push(orig.clone());
                            }
                        }
                    }
                }
                tr.targets = narrowed;
            }
            parsed.target_runs.retain(|tr| !tr.targets.is_empty());
            let matched_files: std::collections::BTreeSet<String> = parsed
                .target_runs
                .iter()
                .flat_map(|tr| {
                    tr.targets.iter().map(|t| {
                        t.split("::")
                            .next()
                            .unwrap_or(t)
                            .trim()
                            .trim_start_matches("./")
                            .to_string()
                    })
                })
                .collect();
            // Compute the complete module-tree files (including testless files and manifest-defined
            // custom `[[example]]`/`[[bench]]` paths!) owned by each scheduled fallback run,
            // resolving per-fallback `::node` selectors independently (never mutating global `k`!)
            // and expanding short node names (`examples/demo.rs::fails` -> `tests::fails`) via AST!
            let mut fallback_owned_files: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            let orig_fbs = std::mem::take(&mut parsed.fallback_cargo_runs);
            let mut next_fbs: Vec<CargoFallbackRun> = Vec::new();
            for fb in orig_fbs {
                let fb_abs = if fb.base == "." {
                    active_root.clone()
                } else {
                    active_root.join(&fb.base)
                };
                let sel = fb.target_args.first().map(String::as_str).unwrap_or("");
                let sel_name = fb.target_args.get(1).map(String::as_str);
                let target_trees: Vec<(Vec<String>, String, Vec<String>)> =
                    test_context::runner::resolve_rust_target_trees_for_selector(
                        &fb_abs,
                        sel,
                        sel_name,
                        &parsed.extra_args,
                    );
                let strip_pkg_prefix = |full_rel: &str| -> String {
                    if fb.base == "." {
                        full_rel.to_string()
                    } else {
                        full_rel
                            .strip_prefix(&format!("{}/", fb.base))
                            .unwrap_or(full_rel)
                            .to_string()
                    }
                };
                // Resolve EACH owning Cargo target separately (so a shared file mounted by TWO
                // examples under different module names runs under EVERY owning target, and a
                // submodule-only `--doc src/foo.rs` selection is rejected rather than widening!).
                for (per_target_args, troot, tfiles) in target_trees {
                    let owned: Vec<String> = tfiles.iter().map(|f| join_pkg(&fb.base, f)).collect();
                    let mut whole_file_requested: Vec<String> = Vec::new();
                    let mut fb_node_reqs: Vec<(String, String)> = Vec::new();
                    let mut target_matched = false;
                    for f in &owned {
                        let clean = f.trim_start_matches("./").to_string();
                        if let Some(orig_list) = explicit_by_file.get(&clean) {
                            fallback_owned_files.insert(clean.clone());
                            target_matched = true;
                            for orig in orig_list {
                                if let Some((_, node)) = orig.split_once("::") {
                                    fb_node_reqs.push((clean.clone(), node.to_string()));
                                } else {
                                    whole_file_requested.push(clean.clone());
                                }
                            }
                        }
                    }
                    if !target_matched {
                        continue;
                    }
                    let is_doc_fb = per_target_args.iter().any(|a| a == "--doc");
                    let is_no_run_req = parsed
                        .extra_args
                        .iter()
                        .take_while(|a| a.as_str() != "--")
                        .any(|a| a == "--no-run");
                    let whole_target_covered = !whole_file_requested.is_empty()
                        && (is_no_run_req
                            || owned.len() <= 1
                            || owned.iter().all(|f| {
                                whole_file_requested
                                    .contains(&f.trim_start_matches("./").to_string())
                            }));
                    if whole_target_covered {
                        next_fbs.push(CargoFallbackRun {
                            base: fb.base.clone(),
                            target_args: per_target_args,
                        });
                    } else {
                        if is_doc_fb || parsed.k.is_some() {
                            parsed.unresolved_target_error = Some(format!(
                                "error: scoped file/node selector `{}` cannot be combined with direct Cargo fallback target",
                                explicit_file_targets.join(",")
                            ));
                            break;
                        }
                        for wf in &whole_file_requested {
                            let rel_in_crate = strip_pkg_prefix(wf);
                            let (fq_names, has_dynamic) =
                                test_context::runner::collect_rust_fq_test_names_in_file(
                                    &fb_abs,
                                    &troot,
                                    &rel_in_crate,
                                );
                            if has_dynamic || fq_names.is_empty() {
                                parsed.unresolved_target_error = Some(format!(
                                    "error: submodule file target `{wf}` cannot be isolated on direct Cargo fallback target"
                                ));
                                break;
                            }
                            for fq in fq_names {
                                let mut targs = per_target_args.clone();
                                targs.push(format!("__node_exact__:{fq}"));
                                next_fbs.push(CargoFallbackRun {
                                    base: fb.base.clone(),
                                    target_args: targs,
                                });
                            }
                        }
                        if parsed.unresolved_target_error.is_some() {
                            break;
                        }
                        for (file_rel, node_sel) in fb_node_reqs {
                            let rel_in_crate = strip_pkg_prefix(&file_rel);
                            let (all_fq, has_dynamic) =
                                test_context::runner::collect_rust_fq_test_names_in_file(
                                    &fb_abs,
                                    &troot,
                                    &rel_in_crate,
                                );
                            let resolved_fq: Vec<String> = all_fq
                                .into_iter()
                                .filter(|sym| {
                                    sym == &node_sel || sym.ends_with(&format!("::{node_sel}"))
                                })
                                .collect();
                            if has_dynamic || resolved_fq.is_empty() {
                                parsed.unresolved_target_error = Some(format!(
                                    "error: could not safely resolve node selector `{file_rel}::{node_sel}` to a static libtest symbol for direct Cargo fallback"
                                ));
                                break;
                            }
                            for fq in resolved_fq {
                                let mut targs = per_target_args.clone();
                                targs.push(format!("__node_exact__:{fq}"));
                                next_fbs.push(CargoFallbackRun {
                                    base: fb.base.clone(),
                                    target_args: targs,
                                });
                            }
                        }
                        if parsed.unresolved_target_error.is_some() {
                            break;
                        }
                    }
                }
                if parsed.unresolved_target_error.is_some() {
                    break;
                }
            }
            parsed.fallback_cargo_runs = next_fbs;
            // For `--no-run`, ALSO resolve ownership against the COMPLETE target module trees of
            // selected `--lib`/`--bins`/`--bin`/`--test` selectors (not just test-bearing files!)
            // so `tctx cargo-test --lib src/helper.rs --no-run` compiles `--lib` cleanly even when
            // `helper.rs` contains no `#[test]` functions itself!
            let is_no_run_flag = parsed
                .extra_args
                .iter()
                .take_while(|a| a.as_str() != "--")
                .any(|a| a == "--no-run");
            if is_no_run_flag && parsed.unresolved_target_error.is_none() {
                let mut sel_specs: Vec<(String, Option<String>)> = Vec::new();
                if selected_lib {
                    sel_specs.push(("--lib".to_string(), None));
                }
                if selected_all_bins {
                    sel_specs.push(("--bins".to_string(), None));
                }
                for b in &selected_bins {
                    sel_specs.push(("--bin".to_string(), Some(b.clone())));
                }
                for t in &selected_tests {
                    sel_specs.push(("--test".to_string(), Some(t.clone())));
                }
                for base in &effective_pkg_bases {
                    let base_abs = if base == "." {
                        active_root.clone()
                    } else {
                        active_root.join(base)
                    };
                    // Expand Cargo glob selectors (`--test 'integ*'`, `--bin 'cli*'`) against
                    // actual package targets so testless helpers under glob-matched targets resolve!
                    let mut expanded_specs: Vec<(String, Option<String>)> = Vec::new();
                    for (sf, sn) in &sel_specs {
                        if let Some(pat) = sn
                            && pat.contains(['*', '?', '['])
                            && let Ok(glob) = globset::GlobBuilder::new(pat)
                                .literal_separator(false)
                                .build()
                        {
                            let matcher = glob.compile_matcher();
                            if sf == "--test" {
                                for (tname, _) in
                                    test_context::runner::collect_rust_integration_test_roots_and_names(
                                        &base_abs,
                                    )
                                {
                                    if matcher.is_match(&tname) {
                                        expanded_specs.push(("--test".to_string(), Some(tname)));
                                    }
                                }
                            } else if sf == "--bin" {
                                for (bname, _) in
                                    test_context::runner::collect_rust_bin_roots_and_names(&base_abs)
                                {
                                    if matcher.is_match(&bname) {
                                        expanded_specs.push(("--bin".to_string(), Some(bname)));
                                    }
                                }
                            }
                        } else {
                            expanded_specs.push((sf.clone(), sn.clone()));
                        }
                    }
                    for (sf, sn) in &expanded_specs {
                        for (per_targs, _, tfiles) in
                            test_context::runner::resolve_rust_target_trees_for_selector(
                                &base_abs,
                                sf,
                                sn.as_deref(),
                                &parsed.extra_args,
                            )
                        {
                            let mut any_match = false;
                            for f in tfiles {
                                let clean = join_pkg(base, &f).trim_start_matches("./").to_string();
                                if explicit_by_file.contains_key(&clean) {
                                    fallback_owned_files.insert(clean);
                                    any_match = true;
                                }
                            }
                            if any_match {
                                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                                    base: base.clone(),
                                    target_args: per_targs,
                                });
                            }
                        }
                    }
                }
            }
            if parsed.unresolved_target_error.is_none() {
                for eft in &explicit_file_targets {
                    let clean = eft
                        .split("::")
                        .next()
                        .unwrap_or(eft)
                        .trim()
                        .trim_start_matches("./");
                    if !matched_files.contains(clean) && !fallback_owned_files.contains(clean) {
                        parsed.unresolved_target_error = Some(format!(
                            "error: explicit file target `{eft}` does not match the selected Cargo target flags"
                        ));
                        break;
                    }
                }
            }
            if parsed.unresolved_target_error.is_none() {
                parsed.targets = explicit_file_targets.clone();
            }
        }
        if parsed.target_runs.is_empty() && !parsed.fallback_cargo_runs.is_empty() {
            parsed.skip_test_run = true;
        }
        // Validate explicit named selectors so `--test nonexistent`, `--bin nonexistent`, or
        // `--lib` / `--doc` on a crate without a library target return an explicit Cargo-compatible
        // error (exit 101) rather than silently succeeding with `no_tests`.
        for name in &selected_tests {
            if !matched_test_names.contains(name) {
                parsed.unresolved_target_error =
                    Some(format!("error: no test target named `{name}`"));
                break;
            }
        }
        if parsed.unresolved_target_error.is_none() {
            for name in &selected_bins {
                if !matched_bin_names.contains(name) {
                    parsed.unresolved_target_error =
                        Some(format!("error: no bin target named `{name}`"));
                    break;
                }
            }
        }
        if parsed.unresolved_target_error.is_none()
            && (selected_lib || selected_doc)
            && !any_lib_exists
            && active_root.join("Cargo.toml").is_file()
        {
            parsed.unresolved_target_error =
                Some("error: no library targets found in package".to_string());
        }
    }
    if parsed.extra_args.last().map(String::as_str) == Some("--") {
        parsed.extra_args.pop();
    }
    // Resolve relative file-valued `--config <path>` arguments against the invocation root before
    // any member-directory `current_dir` changes alter their meaning.
    parsed.extra_args = test_context::runner::normalize_cargo_config_args(root, &parsed.extra_args);
    // Honor compile-only `--no-run` cleanly via direct tracked Cargo compilation so `--no-run`
    // is never duplicated with precompile, test binaries are not executed, and compile-only runs
    // never poison the pass cache.
    if parsed
        .extra_args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--no-run")
    {
        if !parsed.target_runs.is_empty() {
            let drained: Vec<CargoTargetRun> = std::mem::take(&mut parsed.target_runs);
            for tr in drained {
                if let Some(ct) = tr.cargo_target {
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
                        // Restrict `--no-run` fallback packages to those actually owning the
                        // narrowed `tr.targets` so `--workspace --lib a/src/lib.rs --no-run` does
                        // NOT widen compilation to unrelated sibling package `b`!
                        let tr_bases: std::collections::BTreeSet<String> = tr
                            .targets
                            .iter()
                            .map(|t| {
                                let clean = t
                                    .split("::")
                                    .next()
                                    .unwrap_or(t)
                                    .trim()
                                    .trim_start_matches("./");
                                let d = test_context::runner::rust_crate_dir_on_disk(
                                    &active_root,
                                    clean,
                                );
                                if d.is_empty() { ".".to_string() } else { d }
                            })
                            .collect();
                        for base in &parsed.effective_pkg_bases {
                            if !tr_bases.is_empty() && !tr_bases.contains(base) {
                                continue;
                            }
                            let base_abs = if base == "." {
                                active_root.clone()
                            } else {
                                active_root.join(base)
                            };
                            let has_target = if ct == "lib" || ct == "doc" {
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
                            if has_target {
                                parsed.fallback_cargo_runs.push(CargoFallbackRun {
                                    base: base.clone(),
                                    target_args: targs.clone(),
                                });
                            }
                        }
                    }
                }
            }
        } else if !has_specific_target_flag {
            // For default-run `--no-run` (without explicit `--doc`):
            // - When explicit `.rs` file targets were supplied (`tctx cargo-test tests/ok.rs --no-run`),
            //   resolve each file to its exact Cargo target selector (`--test ok`, `--lib`, etc.)
            //   so unrelated broken integration targets do not fail the compile check!
            // - Otherwise replace auto-scheduled `--lib`/`--doc`/`--examples` fallbacks with
            //   compile-only `cargo test --no-run` per package (never passing `--doc --no-run`!).
            parsed.fallback_cargo_runs.clear();
            if !explicit_file_targets.is_empty() {
                for eft in &explicit_file_targets {
                    let clean = eft
                        .split("::")
                        .next()
                        .unwrap_or(eft)
                        .trim()
                        .trim_start_matches("./");
                    let file_crate = test_context::runner::rust_crate_dir_on_disk(&active_root, clean);
                    let base = if file_crate.is_empty() {
                        ".".to_string()
                    } else {
                        file_crate
                    };
                    let base_abs = if base == "." {
                        active_root.clone()
                    } else {
                        active_root.join(&base)
                    };
                    let rel_in_crate = if base == "." {
                        clean
                    } else {
                        clean.strip_prefix(&format!("{base}/")).unwrap_or(clean)
                    };
                    let resolved = test_context::runner::resolve_cargo_target_args_for_file(
                        &base_abs,
                        rel_in_crate,
                        &parsed.extra_args,
                    );
                    if resolved.is_empty() {
                        parsed.unresolved_target_error = Some(format!(
                            "error: could not resolve explicit file target `{eft}` to a Cargo target for `--no-run`"
                        ));
                        break;
                    }
                    for targs in resolved {
                        parsed.fallback_cargo_runs.push(CargoFallbackRun {
                            base: base.clone(),
                            target_args: targs,
                        });
                    }
                }
            } else {
                for base in &parsed.effective_pkg_bases {
                    parsed.fallback_cargo_runs.push(CargoFallbackRun {
                        base: base.clone(),
                        target_args: Vec::new(),
                    });
                }
            }
        }
        parsed.skip_test_run = true;
    }
    // Deduplicate identical `fallback_cargo_runs` (e.g. overlapping globs `--test 'foo*' --test foo`).
    let mut seen_fb: std::collections::BTreeSet<(String, Vec<String>)> =
        std::collections::BTreeSet::new();
    parsed
        .fallback_cargo_runs
        .retain(|fb| seen_fb.insert((fb.base.clone(), fb.target_args.clone())));
    parsed
}

fn run_tracked_direct_cargo_test(
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

fn run_cargo_test_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
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

fn run_pytest_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
    let root = root.canonicalize()?;
    let mut targets = Vec::new();
    let mut k: Option<String> = None;
    let mut mark: Option<String> = None;
    let mut no_cache = false;
    let mut extra_args = Vec::new();

    let mut i = 0usize;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "-k" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("ERROR: argument -k: expected one argument");
                return Ok(4);
            }
            "-m" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    mark = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("ERROR: argument -m: expected one argument");
                return Ok(4);
            }
            "--cache-clear" | "--no-cache" => {
                no_cache = true;
            }
            "-q" | "--quiet" | "-v" | "--verbose" => {
                // Handled by tctx runner summary
            }
            "-c" | "-o" | "-p" | "-W" | "-n" | "--maxfail" | "--tb" | "--rootdir"
            | "--confcutdir" | "--import-mode" | "--basetemp" | "--durations"
            | "--durations-min" | "--cov" | "--cov-report" | "--cov-config" | "--log-level"
            | "--log-format" | "--log-cli-level" | "--color" | "--capture" => {
                extra_args.push(arg.clone());
                if i + 1 < raw_args.len() {
                    extra_args.push(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            _ if arg.starts_with('-') => {
                extra_args.push(arg.clone());
            }
            _ => {
                targets.push(arg.clone());
            }
        }
        i += 1;
    }

    let res = daemon::request_or_local_cli(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": targets,
            "k": k,
            "mark": mark,
            "no_cache": no_cache,
            "rerun_failed": true,
            "full_suite": true,
            "language_filter": "python",
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    if let Some(code) = res
        .get("runner_exit_code")
        .and_then(Value::as_i64)
        .and_then(|c| i32::try_from(c).ok())
        && code != 0
    {
        return Ok(code);
    }

    match res["status"].as_str() {
        Some("failed") => Ok(1),
        Some("no_tests") => Ok(5),
        _ => Ok(0),
    }
}

fn print_json(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    if let Err(err) = serde_json::to_writer_pretty(&mut out, value)
        .map_err(io::Error::other)
        .and_then(|()| writeln!(out))
        .and_then(|()| out.flush())
    {
        if err.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(err.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_dropin_handles_two_token_flags_without_polluting_targets_or_k() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("crates/alpha/src")).unwrap();
        std::fs::write(
            tmp.path().join("crates/alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("src/bin/helper.rs"), "fn main() {}\n").unwrap();

        let args: Vec<String> = [
            "test",
            "-p",
            "alpha",
            "--features",
            "local-coordinator",
            "-j",
            "8",
            "--",
            "--skip",
            "slow_case",
            "fast_filter",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let parsed = parse_cargo_test_dropin_args(tmp.path(), &args);
        assert_eq!(parsed.targets, vec!["crates/alpha".to_string()]);
        assert_eq!(parsed.k, Some("fast_filter".to_string()));
        assert_eq!(
            parsed.extra_args,
            vec![
                "--features",
                "local-coordinator",
                "-j",
                "8",
                "--",
                "--skip",
                "slow_case",
            ]
        );

        let bin_args: Vec<String> = ["test", "--bin", "helper"]
            .into_iter()
            .map(String::from)
            .collect();
        let parsed_bin = parse_cargo_test_dropin_args(tmp.path(), &bin_args);
        assert_eq!(parsed_bin.targets, vec!["src/bin/helper.rs".to_string()]);
        assert_eq!(parsed_bin.k, None);
    }
}
