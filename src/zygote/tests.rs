use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;

#[test]
fn readable_wait_has_a_bounded_timeout() {
    let (reader, _writer) = UnixStream::pair().unwrap();
    let started = Instant::now();
    let error = wait_readable(
        reader.as_raw_fd(),
        Duration::from_millis(20),
        "test readiness",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("test readiness timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn readable_wait_returns_when_data_arrives() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(b"x").unwrap();
    wait_readable(reader.as_raw_fd(), Duration::from_secs(1), "test response").unwrap();
    let mut byte = [0_u8; 1];
    reader.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'x']);
}

fn test_spec(root: &Path) -> Spec {
    Spec {
        root: root.to_path_buf(),
        scopes: vec![".".to_string()],
        globs: Vec::new(),
        excludes: Vec::new(),
        default_excludes: true,
        pytest_cmd: Vec::new(),
        policy: crate::policy::MemoryPolicy::default(),
        allow_worktrees: false,
        managed_parent: None,
    }
}

#[test]
fn compatible_preload_subset_reuses_parent_but_config_changes_do_not() {
    let root = tempfile::tempdir().unwrap();
    let spec = test_spec(root.path());
    let pythonpath = std::ffi::OsString::from(root.path());
    let modules = vec!["json".to_string(), "math".to_string()];
    let mut manager = ZygoteManager::default();
    manager
        .ensure(&spec, pythonpath.clone(), &modules, &[])
        .unwrap();
    let pid = manager.status()["pid"].clone();
    manager
        .ensure(&spec, pythonpath.clone(), &modules[..1], &[])
        .unwrap();
    assert_eq!(manager.status()["pid"], pid);
    let parent = manager.zygote.as_mut().unwrap();
    assert!(!parent.matches_config(&spec, &pythonpath, &["decimal".to_string()], &[]));
    assert!(!parent.matches_config(&spec, &pythonpath, &modules, &["pytest".to_string()]));
    assert!(!parent.matches_config(&spec, std::ffi::OsStr::new("other"), &modules, &[]));
    let mut other_root = spec.clone();
    other_root.root = root.path().join("other");
    assert!(!parent.matches_config(&other_root, &pythonpath, &modules, &[]));
    parent.runs = 200;
    assert!(!parent.matches_config(&spec, &pythonpath, &modules, &[]));
}

#[test]
fn structured_reports_drain_multiple_pipe_chunks_with_and_without_dependency_capture() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("test_many.py"),
        "import pytest\n@pytest.mark.parametrize('i', range(1200))\ndef test_many(i):\n    assert i >= 0\n",
    )
    .unwrap();
    let spec = test_spec(root.path());
    let mut manager = ZygoteManager::default();
    manager.ensure(&spec, root.path().into(), &[], &[]).unwrap();
    let args = ["-q", "-p", "no:cacheprovider", "test_many.py"].map(str::to_string);
    for capture in [true, false] {
        let started = Instant::now();
        let output = manager.run(&args, 3, capture, None).unwrap();
        assert!(!output.timed_out);
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(5));
        let structured = output.structured.unwrap();
        assert!(structured.len() > 65536);
        let payload: serde_json::Value = serde_json::from_slice(&structured).unwrap();
        assert_eq!(payload["reports"].as_array().unwrap().len(), 1200);
    }
    assert!(!root.path().join("junit.xml").exists());
}

#[test]
fn direct_reporter_preserves_failures_skips_and_fixture_errors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("test_outcomes.py"),
        r#"import pytest
@pytest.fixture
def skipped_service():
    pytest.skip("service unavailable")
@pytest.fixture
def broken_setup():
    raise RuntimeError("setup failed")
@pytest.fixture
def broken_teardown():
    yield
    raise RuntimeError("teardown failed")
def test_pass():
    pass
def test_fail():
    assert False
def test_skip():
    pytest.skip("call skip")
def test_setup_skip(skipped_service):
    pass
def test_setup_error(broken_setup):
    pass
def test_teardown_error(broken_teardown):
    pass
"#,
    )
    .unwrap();
    let spec = test_spec(root.path());
    let mut manager = ZygoteManager::default();
    manager.ensure(&spec, root.path().into(), &[], &[]).unwrap();
    let args = ["-q", "-p", "no:cacheprovider", "test_outcomes.py"].map(str::to_string);
    for capture in [true, false] {
        let output = manager.run(&args, 3, capture, None).unwrap();
        assert!(!output.status.success());
        let payload: serde_json::Value =
            serde_json::from_slice(output.structured.as_ref().unwrap()).unwrap();
        let reports = payload["reports"].as_array().unwrap();
        assert_eq!(reports.len(), 7);
        assert_eq!(
            reports.iter().filter(|r| r["outcome"] == "skipped").count(),
            2
        );
        assert_eq!(
            reports.iter().filter(|r| r["outcome"] == "failed").count(),
            3
        );
        assert!(
            reports
                .iter()
                .any(|r| r["when"] == "setup" && r["outcome"] == "failed")
        );
        assert!(
            reports
                .iter()
                .any(|r| r["when"] == "teardown" && r["outcome"] == "failed")
        );
    }
}

#[test]
fn oversized_reports_fail_explicitly_and_allow_parent_recovery() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("test_oversized.py"),
        "import pytest\n@pytest.mark.parametrize('i', [0], ids=['x' * (4 * 1024 * 1024)])\ndef test_large_id(i):\n    assert i == 0\n",
    )
    .unwrap();
    let spec = test_spec(root.path());
    let mut manager = ZygoteManager::default();
    manager.ensure(&spec, root.path().into(), &[], &[]).unwrap();
    let args = ["-q", "-p", "no:cacheprovider", "test_oversized.py"].map(str::to_string);
    let error = manager
        .run(&args, 3, true, None)
        .err()
        .expect("oversized reports must fail")
        .to_string();
    assert!(error.contains("zygote result frame too large"), "{error}");
    assert_eq!(manager.status()["ready"], false);
    manager.ensure(&spec, root.path().into(), &[], &[]).unwrap();
    assert_eq!(manager.status()["ready"], true);
}
