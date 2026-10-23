use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use test_context::catalog::{self, Spec};
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
    let error = handle.join().unwrap().unwrap_err().to_string();
    assert!(error.contains("SOURCE_CHANGED_DURING_TEST"), "{error}");

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
    unstable_root.shutdown();
}
