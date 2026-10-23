use crate::catalog;
use crate::daemon::{self, Request};
use crate::Result;
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

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "state_dir": state_dir,
        "socket_path": sock_path,
        "socket_exists": sock_path.exists(),
        "daemon_reachable": ping.is_some(),
        "daemon_ping": ping,
        "registered_workspaces": specs.len(),
        "current_root_registered": resolved.is_some(),
        "current_root_spec": resolved,
    }))
}
