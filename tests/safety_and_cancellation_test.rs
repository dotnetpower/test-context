use std::path::Path;
use std::time::{Duration, Instant};
use test_context::catalog::{self, Spec};
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
fn concurrent_edits_cancellation_large_stdout_and_skipped_safety() {
    let temp = tempfile::tempdir().unwrap();
    let state_home = temp.path().join("state");
    unsafe {
        std::env::set_var("TCTX_HOME", &state_home);
    }

    let repo = temp.path().join("safety_repo");
    std::fs::create_dir_all(&repo).unwrap();
    write_file(
        &repo,
        "pyproject.toml",
        "[project]\nname = \"safety_sample\"\nversion = \"0.1.0\"\n",
    );
    write_file(&repo, "src/pkg/__init__.py", "");
    write_file(
        &repo,
        "src/pkg/slow.py",
        "def value() -> int:\n    return 1\n",
    );
    write_file(
        &repo,
        "tests/test_slow.py",
        "import time\nfrom pkg.slow import value\n\ndef test_slow():\n    time.sleep(0.5)\n    assert value() in (1, 2)\n",
    );

    let spec = catalog::register(Spec {
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
    let unstable_root = ManagedRoot::load(spec).unwrap();

    // 1. A source edit during pytest execution is absorbed by automatic stability retry.
    let running = std::sync::Arc::clone(&unstable_root);
    let handle = std::thread::spawn(move || {
        running.run_tests(&TestRunRequest {
            targets: vec!["tests/test_slow.py".to_string()],
            no_cache: true,
            ..TestRunRequest::default()
        })
    });
    std::thread::sleep(Duration::from_millis(80));
    write_file(
        &repo,
        "src/pkg/slow.py",
        "def value() -> int:\n    return 2\n",
    );
    let retried = handle.join().unwrap().unwrap();
    assert_eq!(retried.status, "passed");

    // 2. Continuous edits defeat every retry; the discarded result is reported instead of cached.
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
            std::thread::sleep(Duration::from_millis(50));
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

    // 3. Client cancellation terminates the active child process group and never caches a failure.
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

    // 4. Multi-megabyte stdout is drained without deadlock and bounded in the response.
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

    // 5. Skipped tests are reported as skipped and never cached as a pass.
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

    // 6. Parametrized suite & user --junit-xml report preservation.
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
}
