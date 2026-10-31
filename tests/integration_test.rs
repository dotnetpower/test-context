use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use test_context::catalog::{self, Spec};
use test_context::daemon::{self, Request};
use test_context::policy::MemoryPolicy;
use test_context::runner::{ManagedRoot, TestRunRequest};
use test_context::shared_mcp;

fn write_file(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

#[test]
fn end_to_end_ast_graph_background_cache_and_delta_pytest() {
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state");
    unsafe {
        std::env::set_var("TCTX_HOME", &state_home);
    }

    let repo = temp.path().join("sample_repo");
    std::fs::create_dir_all(&repo).unwrap();

    write_file(
        &repo,
        "pyproject.toml",
        "[project]\nname = \"sample\"\nversion = \"0.1.0\"\n",
    );
    write_file(&repo, "src/pkg/__init__.py", "");
    write_file(
        &repo,
        "src/pkg/math_ops.py",
        "def add(a: int, b: int) -> int:\n    return a + b\n",
    );
    write_file(
        &repo,
        "src/pkg/service.py",
        "from pkg.math_ops import add\n\ndef compute_double(x: int) -> int:\n    return add(x, x)\n",
    );
    write_file(&repo, "config/settings.json", "{\"greeting\": \"hello\"}\n");
    write_file(
        &repo,
        "src/pkg/isolated.py",
        "import json\nfrom pathlib import Path\n\ndef greet(name: str) -> str:\n    cfg = json.loads(Path('config/settings.json').read_text())\n    return f\"{cfg['greeting']} {name}\"\n",
    );
    write_file(
        &repo,
        "tests/conftest.py",
        "import pytest\n\n@pytest.fixture\ndef base_number() -> int:\n    return 21\n",
    );
    write_file(
        &repo,
        "tests/test_math.py",
        "import pytest\nfrom pkg.math_ops import add\n\npytestmark = pytest.mark.node_cache_safe\n\ndef test_add():\n    assert add(2, 3) == 5\n",
    );
    write_file(
        &repo,
        "tests/test_service.py",
        "from pkg.service import compute_double\n\ndef test_service(base_number):\n    assert compute_double(base_number) == 42\n",
    );
    write_file(
        &repo,
        "tests/test_isolated.py",
        "from pkg.isolated import greet\n\ndef test_greet():\n    assert greet('world') == 'hello world'\n",
    );

    let spec = catalog::register(Spec {
        root: repo.clone(),
        scopes: vec![".".to_string()],
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: MemoryPolicy {
            background_enabled: true,
            background_safe_only: false,
            background_max_load_ratio_percent: 200,
            background_batch_size: 8,
            ..MemoryPolicy::default()
        },
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let lightweight_status = daemon::request_or_local(&Request {
        root: Some(repo.clone()),
        name: "test_status".to_string(),
        arguments: json!({"load": false}),
        compact: false,
    })
    .unwrap();
    assert_eq!(
        lightweight_status["workspace"]["state"].as_str(),
        Some("registered_unloaded")
    );

    let managed = ManagedRoot::load(spec).unwrap();
    managed.invalidate(&[]).unwrap();

    // 1. Check AST graph & affected analysis for `src/pkg/math_ops.py`
    let affected = managed
        .affected(&["src/pkg/math_ops.py".to_string()], false)
        .unwrap();
    let affected_files: Vec<String> = affected["affected_tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["test_file"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        affected_files,
        vec![
            "tests/test_math.py".to_string(),
            "tests/test_service.py".to_string()
        ]
    );

    // 2. Wait for the speculative background worker to finish warming the cache
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let st = managed.status();
        if st["cached_passed_files"].as_u64() == Some(3) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background worker did not complete in time: {st}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // 3. Running all tests now should be an instant 100% cache hit (0 executed files!)
    let run1 = managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(run1.status, "passed");
    assert_eq!(run1.mode, "instant_cache_hit");
    assert_eq!(run1.cached_files_count, 3);
    assert_eq!(run1.executed_files_count, 0);
    assert_eq!(run1.passed, 3);
    assert_eq!(run1.failed, 0);
    assert_eq!(
        managed.status()["cache_storage"]["invocation_patterns"].as_u64(),
        Some(1)
    );

    // 4. Modify `src/pkg/math_ops.py` and immediately invoke `run_tests` (before background finishes or with hybrid delta)
    write_file(
        &repo,
        "src/pkg/math_ops.py",
        "def add(a: int, b: int) -> int:\n    return (a + b) + 0\n",
    );
    let run2 = managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(run2.status, "passed");
    // `tests/test_isolated.py` must be reused from cache!
    assert!(run2.cached_files_count >= 1);
    assert!(
        run2.cached_files
            .iter()
            .any(|f| f.test_file == "tests/test_isolated.py")
    );
    assert_eq!(run2.passed, 3);

    // 5. Revert `src/pkg/math_ops.py` back to its original content -> Merkle hash matches historical cache!
    write_file(
        &repo,
        "src/pkg/math_ops.py",
        "def add(a: int, b: int) -> int:\n    return a + b\n",
    );
    let run3 = managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(run3.status, "passed");
    assert_eq!(run3.mode, "instant_cache_hit");
    assert_eq!(run3.cached_files_count, 3);
    assert_eq!(run3.executed_files_count, 0);

    // 5b. Safe node-level cache reuses the exact node result without returning whole-file counts.
    let node_run = managed
        .run_tests(&TestRunRequest {
            targets: vec!["tests/test_math.py::test_add".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(node_run.mode, "instant_cache_hit");
    assert_eq!(node_run.passed, 1);
    assert_eq!(node_run.cached_files_count, 1);
    assert_eq!(node_run.executed_files_count, 0);

    let no_match = managed
        .run_tests(&TestRunRequest {
            targets: vec!["tests/typo_does_not_exist.py".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap_err()
        .to_string();
    assert!(no_match.contains("NO_TESTS_MATCHED"), "{no_match}");
    assert_eq!(
        managed.status()["cache_storage"]["invocation_patterns"].as_u64(),
        Some(1),
        "nodeid and failed target requests must not be learned"
    );

    // 6. Modify referenced JSON data file `config/settings.json` -> must invalidate `tests/test_isolated.py`!
    write_file(
        &repo,
        "config/settings.json",
        "{\"greeting\": \"hello\"}\n\n",
    );
    let affected_by_json = managed
        .affected(&["config/settings.json".to_string()], false)
        .unwrap();
    let json_affected_files: Vec<String> = affected_by_json["affected_tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["test_file"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        json_affected_files,
        vec!["tests/test_isolated.py".to_string()]
    );

    // 7. Verify MCP JSON-RPC dispatch
    let mcp_tools = shared_mcp::dispatch_with_root(
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }),
        Some(&repo),
    )
    .unwrap();
    assert_eq!(mcp_tools["result"]["tools"].as_array().unwrap().len(), 7);

    managed.shutdown();

    // 8. WAL-backed cache survives a full ManagedRoot restart.
    let reloaded_spec = catalog::resolve_spec(&repo).unwrap();
    let reloaded = ManagedRoot::load(reloaded_spec).unwrap();
    let persisted = reloaded
        .run_tests(&TestRunRequest {
            targets: vec![
                "tests/test_math.py".to_string(),
                "tests/test_service.py".to_string(),
            ],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(persisted.mode, "instant_cache_hit");
    assert_eq!(persisted.executed_files_count, 0);
    reloaded.shutdown();

    // 9. A source edit during pytest execution must discard the result rather than cache a false pass.
    write_file(
        &repo,
        "src/pkg/slow.py",
        "def value() -> int:\n    return 1\n",
    );
    write_file(
        &repo,
        "tests/test_slow.py",
        "import time\nfrom pkg.slow import value\n\ndef test_slow():\n    time.sleep(1.0)\n    assert value() in (1, 2)\n",
    );
    let unstable_root = ManagedRoot::load(Spec {
        root: repo.clone(),
        scopes: vec![".".to_string()],
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: MemoryPolicy {
            background_enabled: false,
            ..MemoryPolicy::default()
        },
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    let running = std::sync::Arc::clone(&unstable_root);
    let handle = std::thread::spawn(move || {
        running.run_tests(&TestRunRequest {
            targets: vec!["tests/test_slow.py".to_string()],
            no_cache: true,
            ..TestRunRequest::default()
        })
    });
    std::thread::sleep(Duration::from_millis(100));
    write_file(
        &repo,
        "src/pkg/slow.py",
        "def value() -> int:\n    return 2\n",
    );
    // A single mid-run edit is absorbed by the automatic stability retry and yields a current result.
    let retried = handle.join().unwrap().unwrap();
    assert_eq!(retried.status, "passed");

    // Continuous edits defeat every retry; the discarded result is reported instead of cached.
    let churn_root = std::sync::Arc::clone(&unstable_root);
    let churn_repo = repo.clone();
    let stop_churn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn_flag = std::sync::Arc::clone(&stop_churn);
    let churn = std::thread::spawn(move || {
        let mut value = 10;
        while !churn_flag.load(std::sync::atomic::Ordering::Acquire) {
            write_file(
                &churn_repo,
                "src/pkg/slow.py",
                &format!("def value() -> int:\n    return {}\n", value),
            );
            value += 1;
            std::thread::sleep(Duration::from_millis(60));
        }
    });
    let error = churn_root
        .run_tests(&TestRunRequest {
            targets: vec!["tests/test_slow.py".to_string()],
            no_cache: true,
            ..TestRunRequest::default()
        })
        .unwrap_err()
        .to_string();
    stop_churn.store(true, std::sync::atomic::Ordering::Release);
    churn.join().unwrap();
    assert!(error.contains("SOURCE_CHANGED_DURING_TEST"), "{error}");
    assert!(error.contains("retry once edits settle"), "{error}");

    // 10. A client cancellation terminates the active child process group and never caches a failure.
    let cancelling_root = std::sync::Arc::clone(&unstable_root);
    let cancel_handle = std::thread::spawn(move || {
        cancelling_root.run_tests(&TestRunRequest {
            targets: vec!["tests/test_slow.py".to_string()],
            no_cache: true,
            ..TestRunRequest::default()
        })
    });
    let cancel_deadline = Instant::now() + Duration::from_secs(2);
    while !unstable_root.cancel_active() {
        assert!(
            Instant::now() < cancel_deadline,
            "zygote child never became cancellable"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let cancelled = cancel_handle.join().unwrap().unwrap_err().to_string();
    assert!(cancelled.contains("TEST_CANCELLED"), "{cancelled}");

    // 11. Multi-megabyte stdout is drained without deadlock and bounded in the response.
    write_file(
        &repo,
        "tests/test_noisy.py",
        "def test_noisy():\n    print('x' * (3 * 1024 * 1024))\n    assert True\n",
    );
    let noisy = unstable_root
        .run_tests(&TestRunRequest {
            targets: vec!["tests/test_noisy.py".to_string()],
            no_cache: true,
            pytest_args: vec!["-s".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(noisy.status, "passed");
    assert!(noisy.stdout_summary.len() < 12_000);

    // 12. A test skipped during fixture setup (for example a missing service variable) is reported as
    // skipped and never cached as a pass, so it cannot mask a run in a fully provisioned environment.
    write_file(
        &repo,
        "tests/test_needs_service.py",
        "import pytest\n\n@pytest.fixture\ndef service():\n    pytest.skip('SERVICE_URL is not set')\n\ndef test_service(service):\n    assert service\n",
    );
    for _ in 0..2 {
        let skipped = unstable_root
            .run_tests(&TestRunRequest {
                targets: vec!["tests/test_needs_service.py".to_string()],
                ..TestRunRequest::default()
            })
            .unwrap();
        assert_eq!(skipped.mode, "full_run", "{skipped:?}");
        assert_eq!(skipped.executed_files_count, 1, "{skipped:?}");
        assert_eq!((skipped.passed, skipped.skipped), (0, 1), "{skipped:?}");
        assert_eq!(skipped.executed_files[0].outcome.as_str(), "skipped");
        assert!(
            skipped
                .stdout_summary
                .contains("skipped tests are never cached")
        );
    }

    write_file(
        &repo,
        "tests/test_many.py",
        "import pytest\n@pytest.mark.parametrize('i', range(240))\ndef test_many(i):\n    assert i >= 0\n",
    );
    for _ in 0..2 {
        let many = unstable_root
            .run_tests(&TestRunRequest {
                targets: vec!["tests/test_many.py".to_string()],
                no_cache: true,
                ..TestRunRequest::default()
            })
            .unwrap();
        assert_eq!(many.status, "passed");
        assert_eq!((many.passed, many.failed, many.skipped), (240, 0, 0));
        assert_eq!(many.executed_files_count, 1);
        assert!(!many.stdout_summary.contains("generated xml file"));
    }
    let cached_many = unstable_root
        .run_tests(&TestRunRequest {
            targets: vec!["tests/test_many.py".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(cached_many.mode, "instant_cache_hit");
    assert_eq!(cached_many.passed, 240);
    let user_junit = unstable_root
        .run_tests(&TestRunRequest {
            targets: vec!["tests/test_many.py".to_string()],
            no_cache: true,
            pytest_args: vec!["--junit-xml=report.xml".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(user_junit.passed, 240);
    let xml = std::fs::read_to_string(repo.join("report.xml")).unwrap();
    assert_eq!(xml.matches("<testcase ").count(), 240);
    unstable_root.shutdown();

    // 13. End-to-end Rust (`cargo test`) + Python (`pytest`) hybrid workspace execution, node cache, and delta run.
    let rust_repo = temp.path().join("rust_hybrid_repo");
    std::fs::create_dir_all(&rust_repo).unwrap();
    write_file(
        &rust_repo,
        "Cargo.toml",
        "[package]\nname = \"mini_calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&rust_repo, "src/lib.rs", "pub mod calc;\n");
    write_file(
        &rust_repo,
        "src/calc.rs",
        "pub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_mul_unit() {\n        assert_eq!(mul(3, 4), 12);\n    }\n}\n",
    );
    write_file(
        &rust_repo,
        "tests/calc_integration.rs",
        "use mini_calc::calc::mul;\n\n#[test]\nfn test_mul_integration() {\n    assert_eq!(mul(6, 7), 42);\n}\n",
    );
    write_file(
        &rust_repo,
        "tests/test_py_side.py",
        "def test_py_ok():\n    assert 10 + 10 == 20\n",
    );

    let rust_spec = catalog::register(Spec {
        root: rust_repo.clone(),
        scopes: vec![".".to_string()],
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: MemoryPolicy {
            background_enabled: false,
            ..MemoryPolicy::default()
        },
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    let rust_managed = ManagedRoot::load(rust_spec).unwrap();
    let st = rust_managed.status();
    assert_eq!(st["indexed_rust_files"].as_u64(), Some(3));
    assert_eq!(st["total_rust_test_files"].as_u64(), Some(2));
    assert_eq!(st["total_python_test_files"].as_u64(), Some(1));

    let calc_graph = rust_managed.graph_info("src/calc.rs").unwrap();
    assert_eq!(calc_graph.language, "rust");
    assert_eq!(calc_graph.test_symbols, vec!["test_mul_unit".to_string()]);

    let first_hybrid = rust_managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(first_hybrid.status, "passed");
    assert_eq!(first_hybrid.mode, "full_run");
    assert_eq!(first_hybrid.executed_files_count, 3);
    assert_eq!(first_hybrid.passed, 3);

    let second_hybrid = rust_managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(second_hybrid.status, "passed");
    assert_eq!(second_hybrid.mode, "instant_cache_hit");
    assert_eq!(second_hybrid.cached_files_count, 3);
    assert_eq!(second_hybrid.executed_files_count, 0);

    // Rust node-level cache hit (`src/calc.rs::test_mul_unit`) returns immediately from cache
    let rust_node_hit = rust_managed
        .run_tests(&TestRunRequest {
            targets: vec!["src/calc.rs::test_mul_unit".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(rust_node_hit.mode, "instant_cache_hit");
    assert_eq!(rust_node_hit.passed, 1);
    assert_eq!(rust_node_hit.executed_files_count, 0);

    // Editing `src/calc.rs` invalidates only the 2 Rust test files while keeping `tests/test_py_side.py` cached!
    write_file(
        &rust_repo,
        "src/calc.rs",
        "pub fn mul(a: i32, b: i32) -> i32 {\n    (a * b) + 0\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_mul_unit() {\n        assert_eq!(mul(3, 4), 12);\n    }\n}\n",
    );
    let delta_hybrid = rust_managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(delta_hybrid.status, "passed");
    assert_eq!(delta_hybrid.mode, "hybrid_delta");
    assert_eq!(delta_hybrid.cached_files_count, 1);
    assert_eq!(delta_hybrid.executed_files_count, 2);
    assert_eq!(
        delta_hybrid.cached_files[0].test_file,
        "tests/test_py_side.py"
    );

    // Fuzzy did_you_mean suggestion on typo target
    let typo_err = rust_managed
        .run_tests(&TestRunRequest {
            targets: vec!["tests/calc_integraton.rs".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap_err()
        .to_string();
    assert!(typo_err.contains("did_you_mean="), "{typo_err}");
    assert!(
        typo_err.contains("tests/calc_integration.rs"),
        "{typo_err}"
    );

    // 14. Pre-flight Ruff check in `affected()` / `check_ruff()` and automatic remediation via `ruff_fix: true` in `run_tests()`.
    if test_context::catalog::detect_ruff_cmd(&rust_repo, None).is_some() {
        write_file(
            &rust_repo,
            "tests/test_py_side.py",
            "import os\n\ndef test_py_ok():\n    assert 10 + 10 == 20\n",
        );
        let aff_with_ruff = rust_managed
            .affected(&["tests/test_py_side.py".to_string()], false)
            .unwrap();
        assert!(
            aff_with_ruff["ruff_errors_count"].as_u64().unwrap_or(0) >= 1,
            "expected pre-flight F401 ruff error: {aff_with_ruff}"
        );
        assert!(
            aff_with_ruff["ruff_diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"].as_str() == Some("F401") && d["fixable"].as_bool() == Some(true)),
            "{aff_with_ruff}"
        );

        let fixed_run = rust_managed
            .run_tests(&TestRunRequest {
                targets: vec!["tests/test_py_side.py".to_string()],
                ruff_fix: true,
                ..TestRunRequest::default()
            })
            .unwrap();
        assert_eq!(fixed_run.status, "passed");
        assert!(
            fixed_run.ruff_diagnostics.is_empty(),
            "ruff_fix=true must auto-fix F401 before running pytest: {:?}",
            fixed_run.ruff_diagnostics
        );
        let cleaned_source =
            std::fs::read_to_string(rust_repo.join("tests/test_py_side.py")).unwrap();
        assert!(
            !cleaned_source.contains("import os"),
            "unused import was not removed by ruff_fix: {cleaned_source}"
        );
    }

    rust_managed.shutdown();
}

#[test]
fn daemon_graceful_shutdown_cleans_socket_and_rejects_unsafe_stale_path() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = env!("CARGO_BIN_EXE_tctx");

    let mut daemon = Command::new(executable)
        .arg("daemon")
        .env("TCTX_HOME", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let socket = state.join("daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "daemon socket did not appear");
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        assert_eq!(
            libc::kill(i32::try_from(daemon.id()).unwrap(), libc::SIGTERM),
            0
        );
    }
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon did not stop cleanly: {status}");
    assert!(
        !socket.exists(),
        "daemon socket remained after graceful stop"
    );

    std::fs::write(&socket, b"unsafe-stale-regular-file").unwrap();
    let output = Command::new(executable)
        .arg("daemon")
        .env("TCTX_HOME", &state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to replace non-socket"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
