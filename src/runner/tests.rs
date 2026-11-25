use super::*;

fn watcher_fixture(max_watch_dirs: usize) -> (tempfile::TempDir, Arc<ManagedRoot>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join("pkg/nested")).unwrap();
    std::fs::write(root.join("pkg/nested/module.py"), "VALUE = 1\n").unwrap();
    let managed = ManagedRoot::load(Spec {
        root,
        scopes: Vec::new(),
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: crate::policy::MemoryPolicy {
            background_enabled: false,
            max_watch_dirs,
            ..Default::default()
        },
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();
    (temp, managed)
}

fn wait_for_watcher(managed: &ManagedRoot, predicate: impl Fn(&RootState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut guard = managed.lock_state();
        managed.drain_fs_events_locked(&mut guard);
        if predicate(&guard) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watch state did not converge: directories={:?}, files={:?}",
            guard.watched_dirs,
            guard.graph.files.keys().collect::<Vec<_>>()
        );
        drop(guard);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn watcher_removes_deleted_and_moved_directory_subtrees() {
    for move_outside in [false, true] {
        let (temp, managed) = watcher_fixture(3);
        let root = temp.path().join("repo");
        if move_outside {
            std::fs::rename(root.join("pkg"), temp.path().join("moved")).unwrap();
        } else {
            std::fs::remove_dir_all(root.join("pkg")).unwrap();
        }
        wait_for_watcher(&managed, |state| {
            state.watched_dirs == BTreeSet::from([root.clone()])
                && !state.graph.files.contains_key("pkg/nested/module.py")
        });
        assert_eq!(
            *managed.watcher_filter_dirs.lock().unwrap(),
            BTreeSet::from([root])
        );
        managed.shutdown();
    }
}

#[test]
fn watcher_releases_capacity_and_rewatches_recreated_directories() {
    let (temp, managed) = watcher_fixture(3);
    let root = temp.path().join("repo");
    for value in [2, 3] {
        std::fs::remove_dir_all(root.join("pkg")).unwrap();
        wait_for_watcher(&managed, |state| state.watched_dirs.len() == 1);
        std::fs::create_dir(root.join("pkg")).unwrap();
        wait_for_watcher(&managed, |state| state.watched_dirs.len() == 2);
        std::fs::create_dir(root.join("pkg/nested")).unwrap();
        wait_for_watcher(&managed, |state| state.watched_dirs.len() == 3);
        let source = format!("VALUE = {value}\n");
        std::fs::write(root.join("pkg/nested/module.py"), &source).unwrap();
        wait_for_watcher(&managed, |state| {
            state
                .graph
                .files
                .get("pkg/nested/module.py")
                .is_some_and(|node| node.sha256 == sha256_hex(source.as_bytes()))
        });
    }
    managed.shutdown();
}

#[test]
fn watcher_overflow_reconciles_missing_directories() {
    let (temp, managed) = watcher_fixture(3);
    std::fs::remove_dir_all(temp.path().join("repo/pkg")).unwrap();
    managed.fs_overflow.store(true, Ordering::Release);
    {
        let mut guard = managed.lock_state();
        managed.drain_fs_events_locked(&mut guard);
        assert_eq!(guard.watched_dirs.len(), 1);
        assert!(!guard.graph.files.contains_key("pkg/nested/module.py"));
    }
    managed.shutdown();
}

#[test]
fn watcher_file_deletion_keeps_parent_watches_for_recreation() {
    let (temp, managed) = watcher_fixture(3);
    let file = temp.path().join("repo/pkg/nested/module.py");
    std::fs::remove_file(&file).unwrap();
    wait_for_watcher(&managed, |state| {
        !state.graph.files.contains_key("pkg/nested/module.py")
    });
    assert_eq!(managed.lock_state().watched_dirs.len(), 3);
    std::fs::write(file, "VALUE = 2\n").unwrap();
    wait_for_watcher(&managed, |state| {
        state
            .graph
            .files
            .get("pkg/nested/module.py")
            .is_some_and(|node| node.sha256 == sha256_hex(b"VALUE = 2\n"))
    });
    managed.shutdown();
}

#[test]
fn watcher_moves_within_workspace_preserve_nested_watches() {
    let (temp, managed) = watcher_fixture(3);
    let root = temp.path().join("repo");
    std::fs::rename(root.join("pkg"), root.join("renamed")).unwrap();
    wait_for_watcher(&managed, |state| {
        state.watched_dirs
            == BTreeSet::from([
                root.clone(),
                root.join("renamed"),
                root.join("renamed/nested"),
            ])
            && state.graph.files.contains_key("renamed/nested/module.py")
            && !state.graph.files.contains_key("pkg/nested/module.py")
    });
    std::fs::write(root.join("renamed/nested/module.py"), "VALUE = 2\n").unwrap();
    wait_for_watcher(&managed, |state| {
        state
            .graph
            .files
            .get("renamed/nested/module.py")
            .is_some_and(|node| node.sha256 == sha256_hex(b"VALUE = 2\n"))
    });
    managed.shutdown();
}

#[test]
fn watcher_root_deletion_removes_all_watches() {
    let (temp, managed) = watcher_fixture(3);
    std::fs::remove_dir_all(temp.path().join("repo")).unwrap();
    wait_for_watcher(&managed, |state| {
        state.watched_dirs.is_empty() && state.graph.files.is_empty()
    });
    assert!(managed.watcher_filter_dirs.lock().unwrap().is_empty());
    managed.shutdown();
}

#[test]
fn watcher_safety_scan_and_sync_prune_missing_directories() {
    for explicit_sync in [false, true] {
        let (temp, managed) = watcher_fixture(3);
        std::fs::remove_dir_all(temp.path().join("repo/pkg")).unwrap();
        if explicit_sync {
            managed.sync(false).unwrap();
        } else {
            let mut guard = managed.lock_state();
            guard.last_safety_scan = Instant::now() - Duration::from_secs(301);
            managed.maybe_safety_reconcile_locked(&mut guard);
        }
        assert_eq!(managed.lock_state().watched_dirs.len(), 1);
        assert_eq!(managed.watcher_filter_dirs.lock().unwrap().len(), 1);
        managed.shutdown();
    }
}

#[test]
fn watcher_failed_registration_is_reported_without_consuming_capacity() {
    let (temp, managed) = watcher_fixture(4);
    let directory = temp.path().join("repo/future");
    {
        let mut guard = managed.lock_state();
        managed.add_watch_dirs_locked(&mut guard, BTreeSet::from([directory.clone()]));
        assert_eq!(guard.watched_dirs.len(), 3);
        assert!(
            guard
                .last_watcher_error
                .as_ref()
                .is_some_and(|error| error.contains("future"))
        );
        assert!(
            !managed
                .watcher_filter_dirs
                .lock()
                .unwrap()
                .contains(&directory)
        );
        std::fs::create_dir(&directory).unwrap();
        managed.add_watch_dirs_locked(&mut guard, BTreeSet::from([directory.clone()]));
        assert!(guard.watched_dirs.contains(&directory));
        assert!(
            managed
                .watcher_filter_dirs
                .lock()
                .unwrap()
                .contains(&directory)
        );
    }
    managed.shutdown();
}

#[test]
fn junit_xml_parser_handles_angle_brackets_inside_quoted_attributes() {
    let mut known = BTreeMap::new();
    known.insert("tests/test_cmp.py".to_string(), "hash1".to_string());
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="pytest" errors="0" failures="1" skipped="0" tests="2">
    <testcase classname="tests.test_cmp" name="test_gt[a &gt; b -> c > d]" file="tests/test_cmp.py" time="0.012" />
    <testcase classname="tests.test_cmp" name="test_fail[x > y]" file="tests/test_cmp.py" time="0.005">
      <failure message="assert 1 > 2">AssertionError</failure>
    </testcase>
  </testsuite>
</testsuites>"#;
    let parsed = parse_junit_xml_per_file(xml, &known);
    let st = parsed
        .get("tests/test_cmp.py")
        .expect("stats for test_cmp.py");
    assert_eq!(st.passed, 1);
    assert_eq!(st.failed, 1);
    assert_eq!(
        st.failed_nodeids,
        vec!["tests/test_cmp.py::test_fail[x > y]"]
    );
}

#[test]
fn background_policy_requires_module_opt_in_and_denies_live_markers() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path(), Vec::new(), Vec::new()).unwrap();
    let base_spec = Spec {
        root: root.path().to_path_buf(),
        scopes: Vec::new(),
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    };
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "test_safe.py",
        b"import pytest\npytestmark = pytest.mark.background_safe\ndef test_ok(): pass\n",
    );
    graph.insert_file_bytes(
        "test_live.py",
        b"import pytest\npytestmark = [pytest.mark.background_safe, pytest.mark.live]\ndef test_live(): pass\n",
    );
    graph.insert_file_bytes("test_plain.py", b"def test_plain(): pass\n");
    graph.recompute_edges();
    let _ = workspace;
    assert!(background_eligible(&base_spec, &graph, "test_safe.py"));
    assert!(!background_eligible(&base_spec, &graph, "test_live.py"));
    assert!(!background_eligible(&base_spec, &graph, "test_plain.py"));
    let mut cache = RootCache::new(root.path().to_path_buf());
    cache.record_invocation(&["test_plain.py".to_string()]);
    assert!(
        !background_eligible_from_parts(&base_spec, &graph, &cache, "test_plain.py"),
        "learned foreground invocations must not bypass background-safe opt-in"
    );
}

#[test]
fn unavailable_python_identity_never_reuses_a_cache_partition() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let broken_python = root.path().join("python-broken");
    std::fs::write(&broken_python, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&broken_python, std::fs::Permissions::from_mode(0o700)).unwrap();
    let spec = Spec {
        root: root.path().to_path_buf(),
        scopes: Vec::new(),
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: vec![broken_python.to_string_lossy().into_owned()],
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    };
    let first = environment_fingerprint(&spec);
    let second = environment_fingerprint(&spec);
    assert_ne!(first, second);
}

#[test]
fn resource_keys_are_derived_from_module_markers() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "test_db.py",
        b"import pytest\npytestmark = [pytest.mark.background_safe, pytest.mark.resource_postgres, pytest.mark.serial]\ndef test_db(): pass\n",
    );
    graph.recompute_edges();
    let keys = test_resource_keys(&graph, "test_db.py");
    assert!(keys.contains("postgres"));
    assert!(keys.contains("serial"));
}

#[test]
fn parses_cargo_test_output_and_compile_errors_with_node_results() {
    let out = r#"
running 3 tests
test math::tests::test_add_unit ... ok
test math::tests::test_sub_unit ... FAILED
test math::tests::test_slow ... ignored, requires cluster

failures:
    math::tests::test_sub_unit

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s
"#;
    let stats = parse_cargo_test_output(out, "src/math.rs", 15, false);
    assert_eq!(stats.passed, 1);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.errors, 0);
    assert_eq!(
        stats.failed_nodeids,
        vec!["src/math.rs::test_sub_unit".to_string()]
    );
    assert_eq!(
        stats.node_results["src/math.rs::test_add_unit"].outcome,
        TestOutcome::Passed
    );
    assert_eq!(
        stats.node_results["src/math.rs::math::tests::test_add_unit"].outcome,
        TestOutcome::Passed
    );

    let compile_err = "error[E0425]: cannot find value `missing` in this scope\nerror: could not compile `sample`";
    let err_stats = parse_cargo_test_output(compile_err, "src/lib.rs", 20, false);
    assert_eq!(err_stats.errors, 1);
    assert_eq!(err_stats.failed_nodeids, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        rust_module_filter_for_file("src/engine/parser.rs"),
        Some("engine::parser::".to_string())
    );
    assert_eq!(rust_module_filter_for_file("src/lib.rs"), None);
}

#[test]
fn fail_first_scheduler_orders_previously_failed_tests_before_passing_and_slow_tests() {
    let root = tempfile::tempdir().unwrap();
    let mut cache = RootCache::new(root.path().to_path_buf());
    let policy = crate::policy::MemoryPolicy::default();
    cache.insert(
        CachedTestRecord {
            test_file: "tests/test_slow_pass.py".to_string(),
            dep_hash: "h1".to_string(),
            outcome: TestOutcome::Passed,
            passed: 1,
            failed: 0,
            skipped: 0,
            errors: 0,
            duration_ms: 800,
            failed_nodeids: Vec::new(),
            output_snippet: String::new(),
            updated_unix_ms: 100,
            source: "on_demand".to_string(),
            environment_hash: String::new(),
            tctx_version: String::new(),
            runner_mode: String::new(),
            started_unix_ms: 0,
            completed_unix_ms: 100,
            dynamic_dependencies: Vec::new(),
            dynamic_dependencies_complete: true,
            node_results: BTreeMap::new(),
            external_module_stamps: BTreeMap::new(),
            exit_code: None,
        },
        &policy,
    );
    cache.insert(
        CachedTestRecord {
            test_file: "tests/test_prev_failed.rs".to_string(),
            dep_hash: "h2".to_string(),
            outcome: TestOutcome::Failed,
            passed: 0,
            failed: 1,
            skipped: 0,
            errors: 0,
            duration_ms: 500,
            failed_nodeids: vec!["tests/test_prev_failed.rs::test_bug".to_string()],
            output_snippet: "assertion failed".to_string(),
            updated_unix_ms: 200,
            source: "on_demand".to_string(),
            environment_hash: String::new(),
            tctx_version: String::new(),
            runner_mode: String::new(),
            started_unix_ms: 100,
            completed_unix_ms: 200,
            dynamic_dependencies: Vec::new(),
            dynamic_dependencies_complete: true,
            node_results: BTreeMap::new(),
            external_module_stamps: BTreeMap::new(),
            exit_code: None,
        },
        &policy,
    );
    let mut targets = vec![
        "tests/test_slow_pass.py".to_string(),
        "tests/test_unknown.py".to_string(),
        "tests/test_prev_failed.rs".to_string(),
    ];
    sort_targets_fail_first(&mut targets, &cache);
    assert_eq!(
        targets,
        vec![
            "tests/test_prev_failed.rs".to_string(),
            "tests/test_slow_pass.py".to_string(),
            "tests/test_unknown.py".to_string(),
        ]
    );
}

#[test]
fn demux_cargo_lib_output_splits_single_lib_run_across_source_files() {
    let out = r#"
running 4 tests
test calc::tests::test_add ... ok
test calc::sub::tests::test_sub ... ok
test engine::tests::test_run ... FAILED
test tests::test_root_lib ... ok

failures:
    engine::tests::test_run

test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
"#;
    let files = vec![
        "src/lib.rs".to_string(),
        "src/calc.rs".to_string(),
        "src/calc/sub.rs".to_string(),
        "src/engine.rs".to_string(),
    ];
    let demuxed = demux_cargo_lib_output(out, &files, 40, false);
    assert_eq!(demuxed["src/calc.rs"].passed, 1);
    assert_eq!(demuxed["src/calc.rs"].failed, 0);
    assert_eq!(demuxed["src/calc/sub.rs"].passed, 1);
    assert_eq!(demuxed["src/engine.rs"].passed, 0);
    assert_eq!(demuxed["src/engine.rs"].failed, 1);
    assert_eq!(
        demuxed["src/engine.rs"].failed_nodeids,
        vec!["src/engine.rs::test_run".to_string()]
    );
    assert_eq!(demuxed["src/lib.rs"].passed, 1);
}

#[test]
fn parses_ruff_json_diagnostics_and_fixability() {
    let json_bytes = br#"[
  {
"code": "F401",
"filename": "/repo/src/pkg/service.py",
"fix": {
  "applicability": "safe",
  "message": "Remove unused import: `os`"
},
"location": { "row": 3, "column": 8 },
"message": "`os` imported but unused"
  },
  {
"code": "F821",
"filename": "/repo/src/pkg/service.py",
"fix": null,
"location": { "row": 10, "column": 12 },
"message": "Undefined name `missing_symbol`"
  }
]"#;
    let known = vec!["src/pkg/service.py".to_string()];
    let by_file = parse_ruff_json_output(json_bytes, Path::new("/repo"), &known);
    let diags = &by_file["src/pkg/service.py"];
    assert_eq!(diags.len(), 2);
    assert_eq!(diags[0].code, "F401");
    assert_eq!(diags[0].row, 3);
    assert_eq!(diags[0].column, 8);
    assert!(diags[0].fixable);
    assert_eq!(
        diags[0].fix_message.as_deref(),
        Some("Remove unused import: `os`")
    );
    assert_eq!(diags[1].code, "F821");
    assert!(!diags[1].fixable);
}

#[test]
fn parses_vitest_jest_json_and_node_tap_test_outputs() {
    let mut known = BTreeMap::new();
    known.insert(
        "src/__tests__/Button.test.tsx".to_string(),
        "hash_btn".to_string(),
    );
    let vitest_json = br#"{
  "numTotalTests": 3,
  "testResults": [
{
  "name": "/repo/src/__tests__/Button.test.tsx",
  "status": "failed",
  "startTime": 1000,
  "endTime": 1045,
  "assertionResults": [
    {
      "title": "renders primary button",
      "fullName": "Button suite renders primary button",
      "status": "passed",
      "duration": 12.4,
      "failureMessages": []
    },
    {
      "title": "fires onClick handler",
      "fullName": "Button suite fires onClick handler",
      "status": "failed",
      "duration": 8.1,
      "failureMessages": ["AssertionError: expected 0 to be 1"]
    },
    {
      "title": "supports dark theme",
      "fullName": "Button suite supports dark theme",
      "status": "todo",
      "duration": 0.0,
      "failureMessages": []
    }
  ]
}
  ]
}"#;
    let parsed = parse_vitest_jest_json_output(vitest_json, &known);
    let st = &parsed["src/__tests__/Button.test.tsx"];
    assert_eq!(st.passed, 1);
    assert_eq!(st.failed, 1);
    assert_eq!(st.skipped, 1);
    assert_eq!(
        st.failed_nodeids,
        vec!["src/__tests__/Button.test.tsx::fires onClick handler".to_string()]
    );
    assert_eq!(
        st.node_results["src/__tests__/Button.test.tsx::renders primary button"].outcome,
        TestOutcome::Passed
    );
    assert_eq!(
        st.node_results
            ["src/__tests__/Button.test.tsx::Button suite renders primary button"]
            .outcome,
        TestOutcome::Passed
    );

    let tap_out = r#"
TAP version 13
# Subtest: formats instant_cache_hit output
ok 1 - formats instant_cache_hit output
# Subtest: handles missing root
not ok 2 - handles missing root
# Subtest: skipped browser check
ok 3 - skipped browser check # SKIP no display
1..3
"#;
    let tap_stats =
        parse_node_test_output(tap_out, "vscode-extension/test/nativeTools.test.ts", 30, false);
    assert_eq!(tap_stats.passed, 1);
    assert_eq!(tap_stats.failed, 1);
    assert_eq!(tap_stats.skipped, 1);
    assert_eq!(
        tap_stats.failed_nodeids,
        vec!["vscode-extension/test/nativeTools.test.ts::handles missing root".to_string()]
    );
}

#[test]
fn parses_eslint_json_and_node_jsonl_batch_outputs() {
    let eslint_json = br#"[
  {
"filePath": "/repo/vscode-extension/src/resultFormatter.ts",
"messages": [
  {
    "ruleId": "@typescript-eslint/no-unused-vars",
    "severity": 2,
    "message": "'unusedVal' is assigned a value but never used.",
    "line": 14,
    "column": 9,
    "fix": { "range": [100, 115], "text": "" }
  }
]
  }
]"#;
    let known_fe = vec!["vscode-extension/src/resultFormatter.ts".to_string()];
    let eslint_map =
        parse_eslint_json_output(eslint_json, Path::new("/repo"), &known_fe);
    let diags = &eslint_map["vscode-extension/src/resultFormatter.ts"];
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, "@typescript-eslint/no-unused-vars");
    assert_eq!(diags[0].row, 14);
    assert_eq!(diags[0].column, 9);
    assert!(diags[0].fixable);

    let jsonl = r#"{"t":"test:pass","f":"/repo/vscode-extension/out/test/manifest.test.js","n":"manifest metadata valid","d":4.2}
{"t":"test:fail","f":"/repo/vscode-extension/out/test/nativeTools.test.js","n":"formats status","d":1.8}
{"t":"test:pass","f":"/repo/vscode-extension/out/test/nativeTools.test.js","n":"skipped check","d":0.1,"s":true}
"#;
    let mut runnable_to_src = BTreeMap::new();
    runnable_to_src.insert(
        "vscode-extension/out/test/manifest.test.js".to_string(),
        "vscode-extension/test/manifest.test.ts".to_string(),
    );
    runnable_to_src.insert(
        "vscode-extension/out/test/nativeTools.test.js".to_string(),
        "vscode-extension/test/nativeTools.test.ts".to_string(),
    );
    let mut known = BTreeMap::new();
    known.insert(
        "vscode-extension/test/manifest.test.ts".to_string(),
        "h1".to_string(),
    );
    known.insert(
        "vscode-extension/test/nativeTools.test.ts".to_string(),
        "h2".to_string(),
    );
    let batch_stats = parse_node_jsonl_batch_output(jsonl, &runnable_to_src, &known);
    assert_eq!(
        batch_stats["vscode-extension/test/manifest.test.ts"].passed,
        1
    );
    assert_eq!(
        batch_stats["vscode-extension/test/nativeTools.test.ts"].failed,
        1
    );
    assert_eq!(
        batch_stats["vscode-extension/test/nativeTools.test.ts"].skipped,
        1
    );
    assert_eq!(
        batch_stats["vscode-extension/test/nativeTools.test.ts"].failed_nodeids,
        vec!["vscode-extension/test/nativeTools.test.ts::formats status".to_string()]
    );
}

#[test]
fn parses_clippy_go_test_structured_failures_and_junit_sarif_reports() {
    let clippy_ndjson = br#"{"reason":"compiler-message","message":{"code":{"code":"clippy::needless_return"},"level":"warning","message":"unneeded `return` statement","spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":12,"column_start":5}],"children":[{"spans":[{"suggested_replacement":"x + 1"}]}]}}
"#;
    let known_rs = vec!["src/lib.rs".to_string()];
    let clippy_map = parse_clippy_json_output(clippy_ndjson, Path::new("/repo"), &known_rs);
    let rs_diags = &clippy_map["src/lib.rs"];
    assert_eq!(rs_diags.len(), 1);
    assert_eq!(rs_diags[0].code, "clippy::needless_return");
    assert_eq!(rs_diags[0].row, 12);
    assert!(rs_diags[0].fixable);

    let go_ndjson = r#"{"Action":"run","Package":"github.com/acme/demo/pkg/calc","Test":"TestAdd"}
{"Action":"pass","Package":"github.com/acme/demo/pkg/calc","Test":"TestAdd","Elapsed":0.012}
{"Action":"fail","Package":"github.com/acme/demo/pkg/calc","Test":"TestDivZero","Elapsed":0.004}
"#;
    let mut go_known = BTreeMap::new();
    go_known.insert("pkg/calc/math_test.go".to_string(), "gh1".to_string());
    let go_stats = parse_go_test_json_output(go_ndjson, &go_known, Path::new("."));
    assert_eq!(go_stats["pkg/calc/math_test.go"].passed, 1);
    assert_eq!(go_stats["pkg/calc/math_test.go"].failed, 1);
    assert_eq!(
        go_stats["pkg/calc/math_test.go"].failed_nodeids,
        vec!["pkg/calc/math_test.go::TestDivZero".to_string()]
    );

    let raw_tb = "node_modules/vitest/dist/index.js:100:1\nsrc/components/Card.tsx:27:5\nE   AssertionError: expected 41 to equal 42\n";
    let structured = extract_structured_failures(
        "tests/test_card.py",
        &["tests/test_card.py::test_val".to_string()],
        raw_tb,
    );
    assert_eq!(structured.len(), 1);
    assert_eq!(structured[0].file, "src/components/Card.tsx");
    assert_eq!(structured[0].line, 27);
    assert!(structured[0].message.contains("AssertionError"));
    assert_eq!(
        structured[0].user_frames,
        vec!["src/components/Card.tsx:27:5".to_string()]
    );

    let dummy_resp = TestRunResponse {
        root: PathBuf::from("/repo"),
        status: "failed".to_string(),
        mode: "hybrid_delta".to_string(),
        total_test_files: 1,
        cached_files_count: 0,
        joined_background_files_count: 0,
        executed_files_count: 1,
        passed: 0,
        failed: 1,
        skipped: 0,
        errors: 0,
        duration_ms: 25,
        saved_ms: 0,
        cached_files: Vec::new(),
        executed_files: vec![FileRunSummary {
            test_file: "tests/test_card.py".to_string(),
            dep_hash: "h1".to_string(),
            outcome: TestOutcome::Failed,
            source: "executed".to_string(),
            passed: 0,
            failed: 1,
            skipped: 0,
            errors: 0,
            duration_ms: 25,
            failed_nodeids: vec!["tests/test_card.py::test_val".to_string()],
        }],
        failed_nodeids: vec!["tests/test_card.py::test_val".to_string()],
        ruff_diagnostics: rs_diags.clone(),
        structured_failures: structured,
        suspected_culprit_files: vec!["src/components/Card.tsx".to_string()],
        untested_modified_files: Vec::new(),
        flaky_tests: Vec::new(),
        missing_prerequisites: Vec::new(),
        warnings_summary: Vec::new(),
        runner_exit_code: Some(1),
        stdout_summary: "=== FAILED ===".to_string(),
    };
    let junit = format_junit_xml_report(&dummy_resp);
    assert!(junit.contains("<testsuites"));
    assert!(junit.contains("tests/test_card.py::test_val"));
    let sarif = format_sarif_report(&dummy_resp);
    assert_eq!(sarif["version"].as_str(), Some("2.1.0"));
    assert_eq!(sarif["runs"][0]["results"].as_array().unwrap().len(), 2);
}

#[test]
fn normalizes_workspace_absolute_targets_and_compacts_passed_boilerplate() {
    let root = Path::new("/home/user/dev/test-context");
    assert_eq!(
        normalize_target_for_root(root, "/home/user/dev/test-context/src/daemon.rs"),
        "src/daemon.rs"
    );
    assert_eq!(
        normalize_target_for_root(
            root,
            "/home/user/dev/test-context/src/daemon.rs::daemon_hub_recovers"
        ),
        "src/daemon.rs::daemon_hub_recovers"
    );
    assert_eq!(
        normalize_target_for_root(root, "./src/ast_graph.rs"),
        "src/ast_graph.rs"
    );
    assert_eq!(
        normalize_target_for_root(
            root,
            "[daemon.rs](/home/user/dev/test-context/src/daemon.rs:42)"
        ),
        "src/daemon.rs"
    );
    assert_eq!(
        normalize_target_for_root(
            root,
            "`file:///home/user/dev/test-context/src/mcp.rs#L10-L25`"
        ),
        "src/mcp.rs"
    );
    assert_eq!(
        normalize_target_for_root(
            root,
            "\\\\wsl.localhost\\Ubuntu\\home\\user\\dev\\test-context\\tests\\integration_test.rs::test_case"
        ),
        "tests/integration_test.rs::test_case"
    );
    assert_eq!(
        normalize_target_for_root(root, "test-context/src/runner.rs:100:5"),
        "src/runner.rs"
    );

    let verbose_pass = concat!(
        "running 3 tests\n",
        "test a::test_one ... ok\n",
        "test a::test_two ... ok\n",
        "warning: DeprecationWarning: legacy call\n",
        "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s\n",
        "Compiling sample v0.1.0 (/repo)\n",
        "    Finished `test` profile [unoptimized] target(s) in 0.42s\n",
        "     Running unittests src/lib.rs (target/debug/deps/sample-123)\n",
    );
    let compacted = compact_passed_delta_stdout(verbose_pass);
    assert_eq!(compacted, "warning: DeprecationWarning: legacy call");
}

#[test]
fn rust_unit_tests_are_background_eligible_and_main_bin_is_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"test-context\"\nversion = \"0.1.3\"\n\n[[bin]]\nname = \"tctx\"\npath = \"src/main.rs\"\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let (pkg, main_bin) = parse_cargo_package_and_main_bin(tmp.path());
    assert_eq!(pkg.as_deref(), Some("test-context"));
    assert_eq!(main_bin.as_deref(), Some("tctx"));

    let spec = Spec {
        root: tmp.path().to_path_buf(),
        scopes: Vec::new(),
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: Default::default(),
        allow_worktrees: false,
        managed_parent: None,
    };
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "src/calc.rs",
        b"#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_ok() { assert_eq!(1, 1); }\n}\n",
    );
    graph.insert_file_bytes(
        "tests/int_test.rs",
        b"#[test]\nfn test_int() { assert_eq!(1, 1); }\n",
    );
    graph.recompute_edges();
    assert!(background_eligible(&spec, &graph, "src/calc.rs"));
    assert!(!background_eligible(&spec, &graph, "tests/int_test.rs"));
}

#[test]
fn go_run_split_and_re2_escapes_handle_posix_classes_and_control_chars() {
    assert_eq!(
        split_go_run_alternations("TestFoo/[[:alpha:]|0]"),
        vec![vec!["TestFoo", "[[:alpha:]|0]"]]
    );
    assert_eq!(
        split_go_run_alternations("TestFoo/[^]|0]|TestBar/ok"),
        vec![vec!["TestFoo", "[^]|0]"], vec!["TestBar", "ok"]]
    );
    assert_eq!(go_re2_segment_matches(r"\f", "f"), Ok(false));
    assert_eq!(go_re2_segment_matches(r"\f", "\x0c"), Ok(true));
    assert_eq!(go_re2_segment_matches(r"\a", "a"), Ok(false));
    assert_eq!(go_re2_segment_matches(r"\a", "\x07"), Ok(true));
    assert_eq!(go_re2_segment_matches(r"\v", "v"), Ok(false));
    assert_eq!(go_re2_segment_matches(r"\v", "\x0b"), Ok(true));
    assert!(go_re2_segment_matches(r"\q", "q").is_err());
    assert_eq!(go_re2_segment_matches("[[:alpha:]]+", "hello"), Ok(true));
    assert_eq!(go_re2_segment_matches("^[[:alpha:]]+$", "123"), Ok(false));
    assert_eq!(go_re2_segment_matches("^[[:^digit:]]+$", "abc"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i)hello", "HeLLo"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i:foo)bar", "FOObar"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i:foo)bar", "FOOBAR"), Ok(false));
    assert_eq!(go_re2_segment_matches("(?:(?i)foo|bar)", "BAR"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?:(?i)foo|bar)baz", "barBAZ"), Ok(false));
    assert_eq!(go_re2_segment_matches("(?i)Σ", "ς"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i)k", "\u{212A}"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i)\u{1E9E}", "\u{00DF}"), Ok(true));
    assert_eq!(go_re2_segment_matches("(?i)I", "\u{0131}"), Ok(false));
    assert_eq!(go_testing_rewrite("hello world"), "hello_world");
    assert_eq!(go_testing_rewrite("hello\u{2003}world"), "hello_world");
    assert_eq!(go_testing_rewrite("bell\x07zw\u{200B}"), r"bell\azw\u200b");
    assert_eq!(go_testing_rewrite("\u{2764}\u{FE0F}"), "\u{2764}\u{FE0F}");
    assert_eq!(go_testing_rewrite("x\u{0378}y"), r"x\u0378y");
    assert_eq!(
        normalize_toml_table_header("[ features ]").as_deref(),
        Some("[features]")
    );
    assert_eq!(
        normalize_toml_table_header("[[ \"test\" ]]").as_deref(),
        Some("[[test]]")
    );
    assert_eq!(
        normalize_toml_table_header("[ workspace . dependencies ]").as_deref(),
        Some("[workspace.dependencies]")
    );
    let expanded = expand_cargo_toml_root_syntaxes(
        r#"
package.name = "demo"
lib.path = "custom.rs"
features = { default = ["integration"], integration = [] }
dependencies = { b = { path = "../b", features = ["x"] } }
workspace = { members = ["a", "b"] }
[package]
description = """
has unmatched [ bracket
"""
[lib]
path = "custom2.rs"
"#,
    );
    assert!(expanded.contains("[package]\nname = \"demo\""));
    assert!(expanded.contains("[lib]\npath = \"custom.rs\""));
    assert!(expanded.contains("[features]\ndefault = [\"integration\"]"));
    assert!(expanded.contains("[dependencies]\nb = { path = \"../b\", features = [\"x\"] }"));
    assert!(expanded.contains("[workspace]\nmembers = [\"a\", \"b\"]"));
    assert!(expanded.contains("[lib]\npath = \"custom2.rs\""));
    assert_eq!(unquote_toml_scalar("\"\"\"demo\"\"\"").as_deref(), Some("demo"));
    assert_eq!(
        split_toml_dotted_key(r#"features."f\u006f\u006f""#),
        vec!["features".to_string(), "foo".to_string()]
    );
    assert_eq!(
        split_toml_dotted_key(r#""target" . 'cfg(unix)' . dependencies . b"#),
        vec![
            "target".to_string(),
            "cfg(unix)".to_string(),
            "dependencies".to_string(),
            "b".to_string()
        ]
    );
    let ml_expanded = expand_cargo_toml_root_syntaxes(
        "[[test]]\nrequired-features = [\"\"\"f\\\n  oo\"\"\"]\n",
    );
    let mut ml_feats = Vec::new();
    parse_toml_string_array_items(&ml_expanded, &mut ml_feats);
    assert_eq!(ml_feats, vec!["foo".to_string()]);
}
