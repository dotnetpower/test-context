use serde_json::json;
use std::path::Path;
use test_context::catalog::{self, Spec};
use test_context::daemon::{self, Request};
use test_context::policy::MemoryPolicy;
use test_context::runner::{ManagedRoot, TestRunRequest};

fn write_file(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

#[test]
fn end_to_end_polyglot_rust_python_ruff_frontend_and_worktree_gc() {
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state");
    unsafe {
        std::env::set_var("TCTX_HOME", &state_home);
    }

    // 1. End-to-end Rust (`cargo test`) + Python (`pytest`) hybrid workspace execution, node cache, and delta run.
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
    assert!(second_hybrid.saved_ms > 0);

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

    // 2. Pre-flight Ruff check in `affected()` / `check_ruff()` and automatic remediation via `ruff_fix: true` in `run_tests()`.
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

    // 3. Frontend (JS/TS/CSS/Barrel) end-to-end execution, node cache hit, and CSS delta invalidation.
    write_file(&rust_repo, "src/ui/theme.css", ".btn { color: green; }\n");
    write_file(
        &rust_repo,
        "src/ui/button.mjs",
        "export const STYLESHEET = 'src/ui/theme.css';\nexport function renderButton(label) {\n  return `<button data-css=\"${STYLESHEET}\">${label}</button>`;\n}\n",
    );
    write_file(
        &rust_repo,
        "src/ui/index.mjs",
        "export { renderButton } from './button.mjs';\n",
    );
    write_file(
        &rust_repo,
        "src/ui/button.test.mjs",
        "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport { renderButton } from './index.mjs';\n\ntest('renders button element', () => {\n  assert.match(renderButton('Save'), /<button.*>Save<\\/button>/);\n});\n",
    );
    rust_managed.sync(false).unwrap();

    let fe_run1 = rust_managed
        .run_tests(&TestRunRequest {
            targets: vec!["src/ui/button.mjs".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(fe_run1.status, "passed");
    assert_eq!(fe_run1.mode, "full_run");
    assert_eq!(fe_run1.executed_files_count, 1);
    assert_eq!(fe_run1.passed, 1);

    let fe_run2 = rust_managed
        .run_tests(&TestRunRequest {
            targets: vec!["src/ui/button.test.mjs".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(fe_run2.mode, "instant_cache_hit");
    assert_eq!(fe_run2.cached_files_count, 1);
    assert_eq!(fe_run2.executed_files_count, 0);

    let fe_node_hit = rust_managed
        .run_tests(&TestRunRequest {
            targets: vec!["src/ui/button.test.mjs::renders button element".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(fe_node_hit.mode, "instant_cache_hit");
    assert_eq!(fe_node_hit.passed, 1);
    assert_eq!(fe_node_hit.executed_files_count, 0);

    // Editing `src/ui/theme.css` invalidates `src/ui/button.test.mjs` while keeping Rust and Python tests cached!
    write_file(&rust_repo, "src/ui/theme.css", ".btn { color: purple; }\n");
    let polyglot_delta = rust_managed.run_tests(&TestRunRequest::default()).unwrap();
    assert_eq!(polyglot_delta.status, "passed");
    assert_eq!(polyglot_delta.mode, "hybrid_delta");
    assert_eq!(polyglot_delta.cached_files_count, 3);
    assert_eq!(polyglot_delta.executed_files_count, 1);
    assert_eq!(
        polyglot_delta.executed_files[0].test_file,
        "src/ui/button.test.mjs"
    );

    // 4. Verify portable CI cache export/import and failure explanation
    let exported = rust_managed.export_cache_bundle().unwrap();
    assert!(exported["records_count"].as_u64().unwrap_or(0) >= 4);
    let imported = rust_managed.import_cache_bundle(&exported).unwrap();
    assert!(imported["imported_records"].as_u64().unwrap_or(0) >= 4);
    let explained = rust_managed.explain_failures().unwrap();
    assert!(explained["failing_test_files"].is_array());

    // 5. Verify ephemeral `.worktrees/` pruning, legacy WAL GC, `test_status(load=true)`, and untested source handling.
    let wt_repo = temp.path().join("demo.worktrees").join("ephemeral-branch");
    std::fs::create_dir_all(&wt_repo).unwrap();
    write_file(&wt_repo, "test_wt.py", "def test_wt():\n    assert True\n");
    let _ = catalog::register(Spec {
        root: wt_repo.clone(),
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
    std::fs::remove_dir_all(&wt_repo).unwrap();

    let cache_dir = catalog::cache_dir().unwrap();
    let legacy_wal = cache_dir.join(format!("{}.v5.wal.jsonl", "a".repeat(64)));
    std::fs::write(&legacy_wal, b"{}\n").unwrap();

    let pruned = test_context::registration::prune(true).unwrap();
    assert_eq!(pruned["pruned_roots"].as_array().unwrap().len(), 1);
    assert!(!legacy_wal.exists());

    let status_val = daemon::request_or_local(&Request {
        root: Some(rust_repo.clone()),
        name: "test_status".to_string(),
        arguments: json!({ "load": true }),
        compact: false,
    })
    .unwrap();
    assert_eq!(
        status_val["daemon"]["loaded_roots"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        status_val["workspace"]["indexed_python_files"].as_u64(),
        Some(1)
    );

    write_file(&rust_repo, "src/untested_draft.py", "def draft():\n    return 1\n");
    let live_spec = catalog::resolve_spec(&rust_repo).unwrap();
    let live_managed = ManagedRoot::load(live_spec).unwrap();
    let untested_run = live_managed
        .run_tests(&TestRunRequest {
            targets: vec!["src/untested_draft.py".to_string()],
            ..TestRunRequest::default()
        })
        .unwrap();
    assert_eq!(untested_run.status, "no_tests");
    assert_eq!(
        untested_run.untested_modified_files,
        vec!["src/untested_draft.py".to_string()]
    );
    live_managed.shutdown();

    rust_managed.shutdown();
}
