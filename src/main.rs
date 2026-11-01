use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::PathBuf;
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
        pytest_arg: Vec<String>,
    },
    Ruff {
        files: Vec<String>,
        #[arg(long)]
        fix: bool,
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

fn run_cargo_test_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
    let root = root.canonicalize()?;
    let mut targets = Vec::new();
    let mut k: Option<String> = None;
    let mut mark: Option<String> = None;
    let mut no_cache = false;
    let mut extra_args = Vec::new();

    let mut i = 0usize;
    if raw_args.first().map(String::as_str) == Some("test") {
        i = 1;
    }
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "--test" => {
                if i + 1 < raw_args.len() {
                    let name = &raw_args[i + 1];
                    if name.ends_with(".rs") {
                        targets.push(name.clone());
                    } else {
                        targets.push(format!("tests/{name}.rs"));
                    }
                    i += 2;
                    continue;
                }
            }
            "--lib" => {
                targets.push("src".to_string());
            }
            "--no-cache" => {
                no_cache = true;
            }
            "--ignored" => {
                mark = Some("ignore".to_string());
            }
            "-q" | "--quiet" | "--" => {}
            _ if arg.starts_with('-') => {
                extra_args.push(arg.clone());
            }
            _ => {
                if arg.ends_with(".rs") || arg.contains('/') || arg.contains("::") {
                    targets.push(arg.clone());
                } else {
                    targets.push(arg.clone());
                    if k.is_none() {
                        k = Some(arg.clone());
                    }
                }
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
    let mut out = io::stdout().lock();
    let bytes = serde_json::to_vec_pretty(value)?;
    if let Err(err) = out
        .write_all(&bytes)
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
