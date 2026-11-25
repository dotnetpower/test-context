use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::PathBuf;
use test_context::policy::MemoryPolicy;
use test_context::{Result, catalog, daemon, diagnostics, invalid, registration, shared_mcp};

mod cli;
use cli::{run_cargo_test_dropin, run_frontend_dropin, run_pytest_dropin};
#[cfg(test)]
use cli::parse_cargo_test_dropin_args;

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
