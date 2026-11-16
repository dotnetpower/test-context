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
        match run_frontend_dropin(PathBuf::from("."), &raw_args) {
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
            shared_mcp::serve(io::stdin().lock(), io::stdout().lock())?;
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
            let res = daemon::request_or_local(&daemon::Request {
                root: Some(root),
                name: "test_run".to_string(),
                arguments: json!({
                    "targets": targets,
                    "k": k,
                    "mark": mark,
                    "no_cache": no_cache,
                    "rerun_failed": !no_rerun_failed,
                    "ruff_fix": ruff_fix,
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
        Action::Vitest { args: fe_args } | Action::Jest { args: fe_args } => {
            run_frontend_dropin(args.root.clone(), fe_args)
        }
    }
}

fn run_frontend_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
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
                if i + 1 < raw_args.len() {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            "--no-cache" | "--clearCache" => {
                no_cache = true;
            }
            "--runTestsByPath" | "--watch=false" => {}
            _ if arg.starts_with('-') => {
                extra_args.push(arg.clone());
            }
            _ => {
                targets.push(arg.clone());
            }
        }
        i += 1;
    }

    let res = daemon::request_or_local(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": targets,
            "k": k,
            "no_cache": no_cache,
            "rerun_failed": true,
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    let failed = res["status"].as_str() == Some("failed");
    Ok(if failed { 1 } else { 0 })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CargoDropinParsed {
    targets: Vec<String>,
    k: Option<String>,
    mark: Option<String>,
    no_cache: bool,
    extra_args: Vec<String>,
}

fn resolve_package_dir_target(root: &Path, pkg: &str) -> Option<String> {
    let alt = pkg.replace('-', "_");
    let alt_dash = pkg.replace('_', "-");
    for prefix in ["", "crates/", "packages/", "lib_rs/", "apps/", "services/", "tools/"] {
        for cand in [pkg, alt.as_str(), alt_dash.as_str()] {
            let rel = if prefix.is_empty() {
                cand.to_string()
            } else {
                format!("{prefix}{cand}")
            };
            if !rel.is_empty() && root.join(&rel).join("Cargo.toml").is_file() {
                return Some(rel);
            }
        }
    }
    if root.join("Cargo.toml").is_file()
        && let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml"))
        && text.lines().any(|l| {
            let t = l.trim();
            t.starts_with("name")
                && t.split_once('=').is_some_and(|(_, v)| {
                    v.trim().trim_matches('"').trim_matches('\'') == pkg
                })
        })
    {
        return Some(".".to_string());
    }
    None
}

fn parse_cargo_test_dropin_args(root: &Path, raw_args: &[String]) -> CargoDropinParsed {
    let mut parsed = CargoDropinParsed::default();
    let mut i = 0usize;
    if raw_args.first().map(String::as_str) == Some("test") {
        i = 1;
    }
    let mut after_double_dash = false;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        if after_double_dash {
            match arg.as_str() {
                "--ignored" => {
                    parsed.mark = Some("ignore".to_string());
                }
                "-q" | "--quiet" => {}
                "--skip" | "--test-threads" | "--logfile" | "--format" | "--color" => {
                    parsed.extra_args.push(arg.clone());
                    if i + 1 < raw_args.len() {
                        parsed.extra_args.push(raw_args[i + 1].clone());
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
                if i + 1 < raw_args.len() {
                    let name = &raw_args[i + 1];
                    if name.ends_with(".rs") {
                        parsed.targets.push(name.clone());
                    } else {
                        parsed.targets.push(format!("tests/{name}.rs"));
                    }
                    i += 2;
                    continue;
                }
            }
            "--bin" => {
                if i + 1 < raw_args.len() {
                    let name = &raw_args[i + 1];
                    let cand_rs = format!("src/bin/{name}.rs");
                    let cand_dir = format!("src/bin/{name}/main.rs");
                    if root.join(&cand_rs).is_file() {
                        parsed.targets.push(cand_rs);
                    } else if root.join(&cand_dir).is_file() {
                        parsed.targets.push(cand_dir);
                    } else if root.join("src/main.rs").is_file() {
                        parsed.targets.push("src/main.rs".to_string());
                    } else {
                        parsed.extra_args.push(arg.clone());
                        parsed.extra_args.push(name.clone());
                    }
                    i += 2;
                    continue;
                }
            }
            "-p" | "--package" => {
                if i + 1 < raw_args.len() {
                    let pkg = &raw_args[i + 1];
                    if let Some(pkg_dir) = resolve_package_dir_target(root, pkg) {
                        if pkg_dir != "." {
                            parsed.targets.push(pkg_dir);
                        }
                    } else {
                        parsed.extra_args.push(arg.clone());
                        parsed.extra_args.push(pkg.clone());
                    }
                    i += 2;
                    continue;
                }
            }
            "--features" | "--manifest-path" | "--target" | "--target-dir" | "--profile"
            | "-j" | "--jobs" | "--color" | "--bench" | "--example" => {
                parsed.extra_args.push(arg.clone());
                if i + 1 < raw_args.len() {
                    parsed.extra_args.push(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            "--lib" => {
                parsed.targets.push("src".to_string());
            }
            "--bins" => {
                if root.join("src/main.rs").is_file() {
                    parsed.targets.push("src/main.rs".to_string());
                }
                if root.join("src/bin").is_dir() {
                    parsed.targets.push("src/bin".to_string());
                }
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
                if arg.ends_with(".rs") || arg.contains('/') || arg.contains("::") {
                    parsed.targets.push(arg.clone());
                } else {
                    parsed.targets.push(arg.clone());
                    if parsed.k.is_none() {
                        parsed.k = Some(arg.clone());
                    }
                }
            }
        }
        i += 1;
    }
    if parsed.extra_args.last().map(String::as_str) == Some("--") {
        parsed.extra_args.pop();
    }
    parsed
}

fn run_cargo_test_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
    let root = root.canonicalize()?;
    let parsed = parse_cargo_test_dropin_args(&root, raw_args);

    let res = daemon::request_or_local(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": parsed.targets,
            "k": parsed.k,
            "mark": parsed.mark,
            "no_cache": parsed.no_cache,
            "rerun_failed": true,
            "pytest_args": parsed.extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    let failed = res["status"].as_str() == Some("failed");
    Ok(if failed { 1 } else { 0 })
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
                if i + 1 < raw_args.len() {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            "-m" => {
                if i + 1 < raw_args.len() {
                    mark = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
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

    let res = daemon::request_or_local(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": targets,
            "k": k,
            "mark": mark,
            "no_cache": no_cache,
            "rerun_failed": true,
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    let failed = res["status"].as_str() == Some("failed");
    Ok(if failed { 1 } else { 0 })
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
