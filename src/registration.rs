use crate::catalog::{self, Spec};
use crate::workspace::capture_with_timeout;
use crate::{Result, invalid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
        "frontend_test_cmd": spec.effective_frontend_test_cmd(),
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
    let mut specs = catalog::load()?;
    let missing = catalog::missing_managed_roots(&specs)?;
    if apply && !missing.is_empty() {
        catalog::mutate(|entries| {
            entries.retain(|spec| !missing.contains(&spec.root));
            Ok(())
        })?;
        specs.retain(|spec| !missing.contains(&spec.root));
    } else if !apply {
        specs.retain(|spec| !missing.contains(&spec.root));
    }
    let active_roots: Vec<PathBuf> = specs.into_iter().map(|s| s.root).collect();
    let cache_gc = crate::cache::gc_cache_dir(&active_roots, Duration::from_secs(300), apply)?;
    Ok(json!({
        "applied": apply,
        "pruned_roots": missing,
        "cache_gc": cache_gc,
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

fn cached_worktrees(parent: &Path, force_refresh: bool) -> Result<Vec<PathBuf>> {
    type CachedOutcome = std::result::Result<Vec<PathBuf>, String>;
    // Store (probe_started, probe_finished, outcome) so forced refreshes only reuse probes
    // that STARTED after the caller began waiting.
    type WorktreeMap = HashMap<PathBuf, (Instant, Instant, CachedOutcome)>;
    type WorktreeGateMap = HashMap<PathBuf, std::sync::Arc<Mutex<()>>>;
    static WORKTREE_CACHE: OnceLock<Mutex<WorktreeMap>> = OnceLock::new();
    static WORKTREE_GATES: OnceLock<Mutex<WorktreeGateMap>> = OnceLock::new();
    let cache = WORKTREE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((_, finished, cached)) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(parent)
    {
        // Always respect short negative cache (2s) so failed 10s probes are never hammered;
        // on force_refresh (after a confirmed miss), always refresh positive entries immediately.
        let hit = match cached {
            Ok(_) => !force_refresh && finished.elapsed() < Duration::from_secs(5),
            Err(_) => finished.elapsed() < Duration::from_secs(2),
        };
        if hit {
            return cached.clone().map_err(|msg| invalid(msg).into());
        }
    }
    let request_started = Instant::now();
    // Singleflight per parent so concurrent cold requests share both successes and failures
    let gate = {
        let gates = WORKTREE_GATES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = gates.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() > 128 {
            guard.retain(|_, g| std::sync::Arc::strong_count(g) > 1);
        }
        std::sync::Arc::clone(
            guard
                .entry(parent.to_path_buf())
                .or_insert_with(|| std::sync::Arc::new(Mutex::new(()))),
        )
    };
    let _singleflight = gate.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((probe_started, finished, cached)) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(parent)
    {
        // Reuse any result whose Git probe STARTED after this caller began waiting,
        // guaranteeing a newly created worktree before this request can never be hidden.
        let hit = match cached {
            Ok(_) => {
                (!force_refresh && finished.elapsed() < Duration::from_secs(5))
                    || *probe_started >= request_started
            }
            Err(_) => finished.elapsed() < Duration::from_secs(2),
        };
        if hit {
            return cached.clone().map_err(|msg| invalid(msg).into());
        }
    }
    let probe_started = Instant::now();
    let outcome: CachedOutcome = worktrees(parent).map_err(|e| e.to_string());
    let probe_finished = Instant::now();
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if guard.len() > 128 {
        guard.retain(|_, (_, ts, _)| ts.elapsed() < Duration::from_secs(30));
    }
    guard.insert(
        parent.to_path_buf(),
        (probe_started, probe_finished, outcome.clone()),
    );
    outcome.map_err(|msg| invalid(msg).into())
}

fn find_matching_worktree<'a>(
    parents: &[&'a Spec],
    requested: &Path,
    force_refresh: bool,
) -> Option<(usize, &'a Spec, PathBuf)> {
    if parents.len() > 1 {
        const MAX_WORKTREE_PROBE_WORKERS: usize = 4;
        let workers = parents.len().min(MAX_WORKTREE_PROBE_WORKERS);
        let next_idx = std::sync::atomic::AtomicUsize::new(0);
        // Track the lowest matching parent index for deterministic policy inheritance
        // when multiple authorized roots belong to the same Git repository.
        let best_match = Mutex::new(None::<(usize, PathBuf)>);
        let next_ref = &next_idx;
        let best_ref = &best_match;
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(move || {
                    use std::sync::atomic::Ordering;
                    loop {
                        let idx = next_ref.fetch_add(1, Ordering::AcqRel);
                        let Some(parent) = parents.get(idx) else {
                            break;
                        };
                        if best_ref
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .as_ref()
                            .is_some_and(|(best_idx, _)| idx >= *best_idx)
                        {
                            break;
                        }
                        if let Ok(trees) = cached_worktrees(&parent.root, force_refresh)
                            && let Some(tree) = trees.into_iter().find(|t| requested.starts_with(t))
                        {
                            let mut guard = best_ref.lock().unwrap_or_else(|p| p.into_inner());
                            if guard
                                .as_ref()
                                .is_none_or(|(best_idx, _)| idx < *best_idx)
                            {
                                *guard = Some((idx, tree));
                            }
                        }
                    }
                });
            }
        });
        best_match
            .into_inner()
            .unwrap_or_else(|p| p.into_inner())
            .map(|(idx, tree)| (idx, parents[idx], tree))
    } else if let Some(&parent) = parents.first() {
        cached_worktrees(&parent.root, force_refresh)
            .ok()
            .and_then(|trees| trees.into_iter().find(|t| requested.starts_with(t)))
            .map(|tree| (0, parent, tree))
    } else {
        None
    }
}

pub fn prepare_or_resolve_spec_canonical(requested: &Path) -> Result<Spec> {
    let specs = catalog::load_arc()?;
    if let Some(existing) = specs
        .iter()
        .filter(|s| requested.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
    {
        return Ok(existing.clone());
    }
    let parents: Vec<&Spec> = specs
        .iter()
        .filter(|s| s.allow_worktrees && s.managed_parent.is_none())
        .collect();
    // Try fast cached list first. On a cached hit at idx > 0, refresh lower-index parents to guarantee
    // deterministic lowest-index precedence; on a complete miss, force-refresh all parents once.
    let matched = match find_matching_worktree(&parents, requested, false) {
        Some((0, parent, tree)) => Some((parent, tree)),
        Some((idx, parent, tree)) => {
            if let Some((_, earlier_parent, earlier_tree)) =
                find_matching_worktree(&parents[..idx], requested, true)
            {
                Some((earlier_parent, earlier_tree))
            } else {
                Some((parent, tree))
            }
        }
        None => find_matching_worktree(&parents, requested, true)
            .map(|(_, parent, tree)| (parent, tree)),
    };
    if let Some((parent, tree)) = matched {
        let inherited = Spec {
            root: tree,
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
    Err(invalid(format!(
        "workspace is not registered with test-context: {} (run `tctx --root {} register`)",
        requested.display(),
        requested.display()
    ))
    .into())
}

pub fn prepare_or_resolve_spec(requested: &Path) -> Result<Spec> {
    let requested = requested.canonicalize()?;
    prepare_or_resolve_spec_canonical(&requested)
}
