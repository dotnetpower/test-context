use super::*;

#[test]
fn bounded_output_drains_excess_without_signalling_child() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "head -c 1048576 /dev/zero"]);
    let output = run_command_with_timeout(&mut command, Duration::from_secs(5), 1024).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 1024);
}

#[test]
fn tracked_process_group_kill_is_reported_as_cancellation() {
    let active_pgid = std::sync::Arc::new(AtomicI32::new(0));
    let worker_pgid = std::sync::Arc::clone(&active_pgid);
    let handle = std::thread::spawn(move || {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        run_command_with_timeout_tracked(
            &mut command,
            Duration::from_secs(35),
            1024,
            Some(&worker_pgid),
            None,
        )
        .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while active_pgid.load(Ordering::Acquire) <= 0 {
        assert!(
            Instant::now() < deadline,
            "child process group was not published"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pgid = active_pgid.load(Ordering::Acquire);
    assert_eq!(unsafe { libc::kill(-pgid, libc::SIGKILL) }, 0);
    let output = handle.join().unwrap();
    assert!(output.cancelled);
    assert!(!output.timed_out);
}

#[test]
fn include_globs_limit_python_inventory_but_keep_pytest_config() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(
        root.path(),
        vec![".".to_string()],
        vec!["tests/**/*.py".to_string()],
    )
    .unwrap();
    assert!(workspace.is_permitted_rel(Path::new("tests/unit/test_api.py")));
    assert!(!workspace.is_permitted_rel(Path::new("src/pkg/api.py")));
    assert!(workspace.is_permitted_rel(Path::new("pyproject.toml")));
    assert!(workspace.is_permitted_rel(Path::new("Cargo.toml")));
}

#[test]
fn default_globs_include_rust_sources_configs_and_snapshot_data_files() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path(), Vec::new(), Vec::new()).unwrap();
    assert!(workspace.is_permitted_rel(Path::new("src/lib.rs")));
    assert!(workspace.is_permitted_rel(Path::new("tests/integration.rs")));
    assert!(workspace.is_permitted_rel(Path::new("Cargo.toml")));
    assert!(workspace.is_permitted_rel(Path::new("rust-toolchain.toml")));
    assert!(workspace.is_permitted_data_rel(Path::new("snapshots/output.snap")));
    assert!(workspace.is_permitted_data_rel(Path::new("testdata/expected.golden")));
}

#[test]
fn root_gitignore_excludes_mutable_runtime_dirs_and_honors_negation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".gitignore"),
        "workspaces/\ndata/eval/*\n!data/eval/golden_fixture.json\ndata/autonomy/x_social_state.json\n",
    )
    .unwrap();
    let workspace = Workspace::new(root.path(), Vec::new(), Vec::new()).unwrap();
    assert!(!workspace.is_permitted_rel(Path::new("workspaces/wt1/tests/test_a.py")));
    assert!(!workspace.is_permitted_data_rel(Path::new("data/eval/live_state.json")));
    assert!(!workspace.is_permitted_data_rel(Path::new("data/autonomy/x_social_state.json")));
    assert!(workspace.is_permitted_data_rel(Path::new("data/eval/golden_fixture.json")));
    assert!(workspace.is_permitted_data_rel(Path::new("config/schema.json")));
    assert!(!workspace.is_gitignored_entry(Path::new("workspaces"), false));
    assert!(workspace.is_gitignored_entry(Path::new("workspaces"), true));
    assert!(workspace.is_gitignored_entry(Path::new("workspaces/wt1/a.py"), false));

    // Verify deep negation (`/*` + `!/src/nested/keep.py`) is not prematurely pruned at `src/`.
    let root2 = tempfile::tempdir().unwrap();
    std::fs::write(
        root2.path().join(".gitignore"),
        "/*\n!/src/nested/keep.py\n",
    )
    .unwrap();
    std::fs::create_dir_all(root2.path().join("src/nested")).unwrap();
    std::fs::write(root2.path().join("src/nested/keep.py"), "def ok(): pass\n").unwrap();
    std::fs::write(root2.path().join("src/ignored.py"), "def no(): pass\n").unwrap();
    let ws2 = Workspace::new(root2.path(), Vec::new(), Vec::new()).unwrap();
    let inv2 = ws2.files_limited(&MemoryPolicy::default()).unwrap();
    assert_eq!(inv2.files, vec![PathBuf::from("src/nested/keep.py")]);
}

#[cfg(unix)]
#[test]
fn read_known_with_limit_rejects_intermediate_symlink_escape() {
    let ws_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.py"), "SECRET = 1\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws_dir.path().join("linked")).unwrap();
    std::fs::write(ws_dir.path().join("ok.py"), "OK = 1\n").unwrap();

    let ws = Workspace::new(ws_dir.path(), Vec::new(), Vec::new()).unwrap();
    assert!(ws.read_known_with_limit(Path::new("ok.py"), 1024).is_ok());
    assert!(
        ws.read_known_with_limit(Path::new("linked/secret.py"), 1024)
            .is_err()
    );
}
