use super::*;

#[test]
fn active_connection_guard_releases_exactly_one_slot() {
    let count = Arc::new(AtomicUsize::new(1));
    {
        let _guard = ActiveConnection(Arc::clone(&count));
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
    assert_eq!(count.load(Ordering::Acquire), 0);
}

#[test]
fn shutdown_gate_rejects_queued_requests() {
    let hub = DaemonHub::new();
    hub.begin_shutdown();
    let error = hub
        .execute(Request {
            root: None,
            name: "ping".to_string(),
            arguments: json!({}),
            compact: false,
        })
        .unwrap_err();
    assert_eq!(error, "daemon is shutting down");
}

#[test]
fn allocator_trim_is_safe_without_loaded_roots() {
    let _ = trim_allocator();
}

#[test]
fn daemon_rss_probe_reports_current_process_memory() {
    assert!(daemon_rss_bytes().is_some_and(|rss| rss > 0));
}

#[test]
fn large_roots_use_shorter_idle_retention() {
    assert_eq!(root_idle_timeout(4_999), IDLE_ROOT_TIMEOUT);
    assert_eq!(root_idle_timeout(5_000), LARGE_ROOT_IDLE_TIMEOUT);
}

#[test]
fn connection_workers_are_bounded_for_local_daemon_load() {
    assert_eq!(connection_worker_count(1), 4);
    assert_eq!(connection_worker_count(8), 8);
    assert_eq!(connection_worker_count(64), MAX_CONNECTION_WORKERS);
}

#[test]
fn persistent_connections_expire_after_idle_deadline() {
    let recent = Instant::now();
    assert!(!persistent_connection_expired(recent));
    let stale = Instant::now() - PERSISTENT_CONNECTION_IDLE_TIMEOUT;
    assert!(persistent_connection_expired(stale));
}

#[test]
fn native_bridge_classifies_transport_disconnects() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::UnexpectedEof,
    ] {
        let error = std::io::Error::new(kind, "transport closed");
        assert!(is_transport_disconnect(&error));
    }
    let application_error = ApplicationError("test failed".to_string());
    assert!(!is_transport_disconnect(&application_error));
}

#[test]
fn status_loading_is_explicit() {
    assert!(
        !json!({})
            .get("load")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    assert!(
        json!({"load": true})
            .get("load")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
}

#[test]
fn daemon_hub_recovers_from_poisoned_load_gate_and_roots_mutex() {
    let hub = Arc::new(DaemonHub::new());
    let hub_for_panic = Arc::clone(&hub);
    let _ = std::thread::spawn(move || {
        let _gate = hub_for_panic.load_gate.lock().unwrap();
        let _roots = hub_for_panic.roots.lock().unwrap();
        panic!("simulated panic while holding load_gate and roots");
    })
    .join();

    assert!(hub.load_gate.is_poisoned());
    assert!(hub.roots.is_poisoned());

    let pong = hub
        .execute(Request {
            root: None,
            name: "ping".to_string(),
            arguments: json!({}),
            compact: false,
        })
        .expect("ping succeeds after poison");
    assert_eq!(pong["ok"], json!(true));

    let temp = tempfile::tempdir().unwrap();
    let err = hub
        .get_or_load_root(&temp.path().join("nonexistent-subdir"))
        .err()
        .expect("missing directory returns error, not load gate poisoned");
    assert!(
        !err.to_string().contains("poisoned"),
        "unexpected poisoned error: {err}"
    );
}
