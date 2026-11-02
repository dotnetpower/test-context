use crate::Result;
use crate::catalog;
use crate::daemon::{self, Request};
use serde_json::{Value, json};
use std::path::Path;

pub fn doctor(root: &Path) -> Result<Value> {
    let state_dir = catalog::state_dir()?;
    let sock_path = catalog::socket_path()?;
    let specs = catalog::load()?;
    let cargo_bin = catalog::detect_cargo_cmd(root)
        .into_iter()
        .next()
        .unwrap_or_else(|| "cargo".to_string());
    let (ping, cargo_available, node_available) = std::thread::scope(|scope| {
        let ping_h = scope.spawn(|| {
            daemon::request(&Request {
                root: None,
                name: "ping".to_string(),
                arguments: json!({}),
                compact: false,
            })
            .ok()
        });
        let cargo_h = scope.spawn(move || {
            std::process::Command::new(cargo_bin)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        });
        let node_h = scope.spawn(|| {
            std::process::Command::new("node")
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        });
        (
            ping_h.join().ok().flatten(),
            cargo_h.join().unwrap_or(false),
            node_h.join().unwrap_or(false),
        )
    });

    let resolved = root
        .canonicalize()
        .ok()
        .and_then(|canon| catalog::resolve_spec(&canon).ok());
    let ruff_available = catalog::detect_ruff_cmd(root, None).is_some();
    let frontend_test_cmd = catalog::detect_frontend_test_cmd(root, None);

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "state_dir": state_dir,
        "socket_path": sock_path,
        "socket_exists": sock_path.exists(),
        "daemon_reachable": ping.is_some(),
        "daemon_ping": ping,
        "cargo_available": cargo_available,
        "node_available": node_available,
        "ruff_available": ruff_available,
        "frontend_test_cmd": frontend_test_cmd,
        "registered_workspaces": specs.len(),
        "current_root_registered": resolved.is_some(),
        "current_root_spec": resolved,
    }))
}
