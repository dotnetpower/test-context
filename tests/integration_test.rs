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
    write_file(
        &repo,
        "src/pkg/isolated.py",
        "def greet(name: str) -> str:\n    return f'hello {name}'\n",
    );
    write_file(
        &repo,
        "tests/conftest.py",
        "import pytest\n\n@pytest.fixture\ndef base_number() -> int:\n    return 21\n",
    );
    write_file(
        &repo,
        "tests/test_math.py",
        "from pkg.math_ops import add\n\ndef test_add():\n    assert add(2, 3) == 5\n",
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
            background_batch_size: 8,
            ..MemoryPolicy::default()
        },
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let managed = ManagedRoot::load(spec).unwrap();

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

    // 6. Verify MCP JSON-RPC dispatch
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
    assert_eq!(
        mcp_tools["result"]["tools"].as_array().unwrap().len(),
        7
    );

    managed.shutdown();
}
