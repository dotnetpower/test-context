use crate::catalog::{self, Spec};
use crate::workspace::capture_with_timeout;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn plan(spec: &Spec) -> Result<Value> {
    let workspace = spec.workspace()?;
    let inventory = workspace.files_limited(&spec.policy)?;
    let watches = workspace
        .watch_directories_for(inventory.files.iter().map(PathBuf::as_path), &spec.policy)?;
    Ok(json!({
        "root": workspace.root,
        "scopes": workspace.scopes,
        "globs": workspace.globs,
        "excludes": workspace.excludes,
        "default_excludes": workspace.default_excludes,
        "pytest_cmd": spec.effective_pytest_cmd(),
        "cargo_cmd": spec.effective_cargo_cmd(),
        "ruff_cmd": spec.effective_ruff_cmd(),
        "policy": spec.policy,
        "allow_worktrees": spec.allow_worktrees,
        "managed_parent": spec.managed_parent,
        "files": inventory.files.len(),
        "metadata_source_bytes": inventory.metadata_source_bytes,
        "path_bytes": inventory.total_path_bytes,
        "watch_directories": watches.len(),
    }))
}

pub fn authorize(root: &Path, allow: bool) -> Result<Value> {
    let root = root.canonicalize()?;
    if allow {
        worktrees(&root)?;
    }
    catalog::mutate(|specs| {
        let spec = specs
            .iter_mut()
            .find(|s| s.root == root)
            .ok_or_else(|| invalid("authorize an exact registered repository first"))?;
        if spec.managed_parent.is_some() {
            return Err(invalid("managed worktrees cannot authorize other roots").into());
        }
        spec.allow_worktrees = allow;
        Ok(())
    })?;
    Ok(json!({"root": root, "allow_worktrees": allow}))
}

pub fn prune(apply: bool) -> Result<Value> {
    let specs = catalog::load()?;
    let missing = catalog::missing_managed_roots(&specs)?;
    if apply && !missing.is_empty() {
        catalog::mutate(|entries| {
            entries.retain(|spec| !missing.contains(&spec.root));
            Ok(())
        })?;
    }
    Ok(json!({
        "applied": apply,
        "pruned_roots": missing,
    }))
}

fn worktrees(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotePath=false", "-C"])
        .arg(parent)
        .args(["worktree", "list", "--porcelain"]);
    let bytes = capture_with_timeout(&mut command, Duration::from_secs(10))?;
    let output = std::str::from_utf8(&bytes)?;
    let mut roots = Vec::new();
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path);
            if path.is_absolute()
                && let Ok(canon) = path.canonicalize()
            {
                roots.push(canon);
            }
        }
    }
    Ok(roots)
}

pub fn prepare_or_resolve_spec(requested: &Path) -> Result<Spec> {
    let requested = requested.canonicalize()?;
    if let Ok(spec) = catalog::resolve_spec(&requested) {
        return Ok(spec);
    }
    let specs = catalog::load()?;
    for parent in specs
        .iter()
        .filter(|s| s.allow_worktrees && s.managed_parent.is_none())
    {
        if let Ok(trees) = worktrees(&parent.root) {
            for tree in trees {
                if requested.starts_with(&tree) {
                    let inherited = Spec {
                        root: tree.clone(),
                        scopes: parent.scopes.clone(),
                        globs: parent.globs.clone(),
                        excludes: parent.excludes.clone(),
                        default_excludes: parent.default_excludes,
                        pytest_cmd: parent.pytest_cmd.clone(),
                        policy: parent.policy.clone(),
                        allow_worktrees: false,
                        managed_parent: Some(parent.root.clone()),
                    };
                    return catalog::register(inherited);
                }
            }
        }
    }
    Err(invalid(format!(
        "workspace is not registered with test-context: {} (run `tctx --root {} register`)",
        requested.display(),
        requested.display()
    ))
    .into())
}
