use super::*;
use std::sync::{Mutex, OnceLock};

fn record(outcome: TestOutcome) -> CachedTestRecord {
    CachedTestRecord {
        test_file: "tests/test_one.py".to_string(),
        dep_hash: "same-hash".to_string(),
        outcome,
        passed: usize::from(outcome == TestOutcome::Passed),
        failed: usize::from(outcome == TestOutcome::Failed),
        skipped: usize::from(outcome == TestOutcome::Skipped),
        errors: usize::from(outcome == TestOutcome::Error),
        duration_ms: 10,
        failed_nodeids: Vec::new(),
        output_snippet: String::new(),
        updated_unix_ms: now_unix_ms(),
        source: "test".to_string(),
        environment_hash: "env".to_string(),
        tctx_version: env!("CARGO_PKG_VERSION").to_string(),
        runner_mode: "zygote".to_string(),
        started_unix_ms: 1,
        completed_unix_ms: 2,
        dynamic_dependencies: Vec::new(),
        dynamic_dependencies_complete: true,
        node_results: BTreeMap::new(),
        external_module_stamps: BTreeMap::new(),
        exit_code: None,
    }
}

#[test]
fn quarantines_flaky_same_hash_and_never_caches_all_skipped_file() {
    let root = tempfile::tempdir().unwrap();
    let mut cache = RootCache::new(root.path().to_path_buf());
    let policy = MemoryPolicy::default();
    cache.insert(record(TestOutcome::Passed), &policy);
    assert!(
        cache
            .get_matching("tests/test_one.py", "same-hash")
            .is_some()
    );
    cache.insert(record(TestOutcome::Failed), &policy);
    assert!(
        cache
            .get_matching("tests/test_one.py", "same-hash")
            .is_none()
    );

    let mut skipped = record(TestOutcome::Skipped);
    skipped.dep_hash = "skip-hash".to_string();
    cache.insert(skipped, &policy);
    assert!(
        cache
            .get_matching("tests/test_one.py", "skip-hash")
            .is_none()
    );
}

#[test]
fn wal_recovers_valid_prefix_from_truncated_tail_and_compacts_atomically() {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        state.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let previous = std::env::var_os("TCTX_HOME");
    unsafe {
        std::env::set_var("TCTX_HOME", state.path());
    }
    let root = state.path().join("project");
    std::fs::create_dir_all(&root).unwrap();

    let mut cache = RootCache::new(root.clone());
    cache.insert(record(TestOutcome::Passed), &MemoryPolicy::default());
    cache.save().unwrap();
    let wal_path = RootCache::wal_path(&root).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .unwrap()
        .write_all(b"{\"sequence\":")
        .unwrap();

    let mut loaded = RootCache::load_or_new(&root);
    assert!(
        loaded
            .get_matching("tests/test_one.py", "same-hash")
            .is_some()
    );
    assert_eq!(
        loaded.persistence_error.as_deref(),
        Some("CACHE_WAL_TRUNCATED_TAIL")
    );

    let mut second = record(TestOutcome::Passed);
    second.test_file = "tests/test_two.py".to_string();
    second.dep_hash = "second-hash".to_string();
    loaded.insert(second, &MemoryPolicy::default());
    loaded.save().unwrap();
    let repaired = RootCache::load_or_new(&root);
    assert!(
        repaired
            .get_matching("tests/test_one.py", "same-hash")
            .is_some()
    );
    assert!(
        repaired
            .get_matching("tests/test_two.py", "second-hash")
            .is_some()
    );

    let mut concurrent_a = RootCache::load_or_new(&root);
    let mut concurrent_b = RootCache::load_or_new(&root);
    let mut third = record(TestOutcome::Passed);
    third.test_file = "tests/test_three.py".to_string();
    third.dep_hash = "third-hash".to_string();
    concurrent_a.insert(third, &MemoryPolicy::default());
    let mut fourth = record(TestOutcome::Passed);
    fourth.test_file = "tests/test_four.py".to_string();
    fourth.dep_hash = "fourth-hash".to_string();
    concurrent_b.insert(fourth, &MemoryPolicy::default());
    concurrent_a.save().unwrap();
    concurrent_b.save().unwrap();
    concurrent_a.record_invocation(&["tests/test_three.py".to_string()]);
    concurrent_a.save().unwrap();
    concurrent_a.compact_snapshot(&wal_path).unwrap();
    let mut merged = RootCache::load_or_new(&root);
    assert!(
        merged
            .get_matching("tests/test_three.py", "third-hash")
            .is_some()
    );
    assert!(
        merged
            .get_matching("tests/test_four.py", "fourth-hash")
            .is_some()
    );

    for index in 0..200 {
        merged.record_invocation(&[format!("tests/test_pattern_{index}.py")]);
    }
    merged.save().unwrap();
    let bounded = RootCache::load_or_new(&root);
    assert!(bounded.invocation_patterns.len() <= 128);

    merged.compact_snapshot(&wal_path).unwrap();
    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0);
    assert!(RootCache::storage_path(&root).unwrap().is_file());

    unsafe {
        if let Some(value) = previous {
            std::env::set_var("TCTX_HOME", value);
        } else {
            std::env::remove_var("TCTX_HOME");
        }
    }
}

#[test]
fn enforces_record_and_total_memory_bounds() {
    let root = tempfile::tempdir().unwrap();
    let mut cache = RootCache::new(root.path().to_path_buf());
    let policy = MemoryPolicy {
        max_cache_bytes: 32 * 1024,
        max_node_results_per_file: 4,
        ..MemoryPolicy::default()
    };
    for index in 0..20 {
        let mut item = record(TestOutcome::Failed);
        item.test_file = format!("tests/test_{index}.py");
        item.dep_hash = format!("hash-{index}");
        item.output_snippet = "실패".repeat(10_000);
        for node_index in 0..20 {
            let nodeid = format!("tests/test_{index}.py::test_{node_index}");
            item.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Failed,
                    duration_ms: 1,
                    output_snippet: "trace".repeat(2_000),
                },
            );
        }
        cache.insert(item, &policy);
    }
    assert!(cache.estimated_bytes() <= policy.max_cache_bytes);
    assert!(cache.latest_by_file.values().all(|record| {
        record.node_results.len() <= 4
            && record.output_snippet.len() <= 8 * 1024
            && record
                .node_results
                .values()
                .all(|node| node.output_snippet.len() <= 2 * 1024)
    }));
}

#[test]
fn learns_bounded_invocation_patterns_and_ranks_affected_tests() {
    let root = tempfile::tempdir().unwrap();
    let mut cache = RootCache::new(root.path().to_path_buf());
    cache.record_invocation(&["tests/test_recent.py".to_string()]);
    for _ in 0..3 {
        cache.record_invocation(&["tests/test_frequent.py".to_string()]);
    }
    let ranked = cache.predicted_affected_tests(&[
        "tests/test_other.py".to_string(),
        "tests/test_recent.py".to_string(),
        "tests/test_frequent.py".to_string(),
    ]);
    assert_eq!(
        ranked,
        vec![
            "tests/test_frequent.py".to_string(),
            "tests/test_recent.py".to_string()
        ]
    );
    assert!(cache.was_invoked("tests/test_frequent.py"));
    assert!(!cache.was_invoked("tests/test_other.py"));

    for index in 0..200 {
        cache.record_invocation(&[format!("tests/test_pattern_{index}.py")]);
    }
    assert!(cache.invocation_patterns.len() <= 128);
    assert!(
        cache
            .pending_events
            .iter()
            .filter(|event| matches!(event, CacheWalEvent::Invocation { .. }))
            .count()
            <= 128
    );
}
