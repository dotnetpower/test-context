use crate::Result;
use crate::catalog;
use crate::daemon::{self, Request};
use serde_json::{Value, json};
use std::path::Path;

pub fn doctor(root: &Path) -> Result<Value> {
    let state_dir = catalog::state_dir()?;
    let sock_path = catalog::socket_path()?;
    let specs = catalog::load()?;
    let ping = daemon::request(&Request {
        root: None,
        name: "ping".to_string(),
        arguments: json!({}),
        compact: false,
    })
    .ok();

    let resolved = root
        .canonicalize()
        .ok()
        .and_then(|canon| catalog::resolve_spec(&canon).ok());
    let cargo_available = std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    let ruff_available = catalog::detect_ruff_cmd(root, None).is_some();

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "state_dir": state_dir,
        "socket_path": sock_path,
        "socket_exists": sock_path.exists(),
        "daemon_reachable": ping.is_some(),
        "daemon_ping": ping,
        "cargo_available": cargo_available,
        "ruff_available": ruff_available,
        "registered_workspaces": specs.len(),
        "current_root_registered": resolved.is_some(),
        "current_root_spec": resolved,
    }))
}
