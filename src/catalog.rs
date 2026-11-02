use crate::policy::MemoryPolicy;
use crate::workspace::Workspace;
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const MAX_REGISTERED_ROOTS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub root: PathBuf,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub globs: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default = "default_true")]
    pub default_excludes: bool,
    #[serde(default)]
    pub pytest_cmd: Vec<String>,
    #[serde(default)]
    pub policy: MemoryPolicy,
    #[serde(default)]
    pub allow_worktrees: bool,
    #[serde(default)]
    pub managed_parent: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

impl Spec {
    pub fn workspace(&self) -> Result<Workspace> {
        Workspace::new_with_excludes(
            &self.root,
            self.scopes.clone(),
            self.globs.clone(),
            self.excludes.clone(),
            self.default_excludes,
        )
    }

    pub fn effective_pytest_cmd(&self) -> Vec<String> {
        if !self.pytest_cmd.is_empty() {
            return self.pytest_cmd.clone();
        }
        detect_pytest_cmd(&self.root, self.managed_parent.as_deref())
    }

    pub fn effective_cargo_cmd(&self) -> Vec<String> {
        detect_cargo_cmd(&self.root)
    }

    pub fn effective_ruff_cmd(&self) -> Option<Vec<String>> {
        detect_ruff_cmd(&self.root, self.managed_parent.as_deref())
    }

    pub fn effective_frontend_test_cmd(&self) -> Vec<String> {
        detect_frontend_test_cmd(&self.root, self.managed_parent.as_deref())
    }
}

pub fn detect_frontend_test_cmd(root: &Path, managed_parent: Option<&Path>) -> Vec<String> {
    if let Some(env_bin) = std::env::var_os("TCTX_FRONTEND_TEST_BIN") {
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let bin = p.to_string_lossy().into_owned();
            if name.contains("vitest") {
                return vec![bin, "run".to_string()];
            }
            return vec![bin];
        }
    }
    let mut search_roots = vec![root.to_path_buf()];
    for sub in ["frontend", "web", "client", "app", "ui", "vscode-extension"] {
        let d = root.join(sub);
        if d.is_dir() {
            search_roots.push(d);
        }
    }
    for container in ["packages", "apps", "services"] {
        let dir = root.join(container);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    search_roots.push(sub);
                }
            }
        }
    }
    if let Some(parent) = managed_parent {
        search_roots.push(parent.to_path_buf());
    }
    for sr in &search_roots {
        let vitest = sr.join("node_modules/.bin/vitest");
        if vitest.is_file() {
            return vec![vitest.to_string_lossy().into_owned(), "run".to_string()];
        }
        let jest = sr.join("node_modules/.bin/jest");
        if jest.is_file() {
            return vec![jest.to_string_lossy().into_owned()];
        }
    }
    vec!["node".to_string(), "--test".to_string()]
}

pub fn detect_frontend_runner_for_dir(
    root: &Path,
    pkg_dir: &str,
    fallback_cmd: &[String],
) -> (Vec<String>, PathBuf) {
    if !pkg_dir.is_empty() {
        let sub_root = root.join(pkg_dir);
        let vitest = sub_root.join("node_modules/.bin/vitest");
        if vitest.is_file() {
            return (
                vec![vitest.to_string_lossy().into_owned(), "run".to_string()],
                sub_root,
            );
        }
        let jest = sub_root.join("node_modules/.bin/jest");
        if jest.is_file() {
            return (vec![jest.to_string_lossy().into_owned()], sub_root);
        }
    }
    (fallback_cmd.to_vec(), root.to_path_buf())
}

pub fn detect_eslint_cmd_for_dir(root: &Path, pkg_dir: &str) -> Option<(PathBuf, PathBuf)> {
    if let Some(env_bin) = std::env::var_os("TCTX_ESLINT_BIN") {
        if env_bin == "none" || env_bin == "0" {
            return None;
        }
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            let workdir = if pkg_dir.is_empty() {
                root.to_path_buf()
            } else {
                root.join(pkg_dir)
            };
            return Some((p, workdir));
        }
    }
    if !pkg_dir.is_empty() {
        let sub_root = root.join(pkg_dir);
        let sub_eslint = sub_root.join("node_modules/.bin/eslint");
        if sub_eslint.is_file() {
            return Some((sub_eslint, sub_root));
        }
    }
    let root_eslint = root.join("node_modules/.bin/eslint");
    if root_eslint.is_file() {
        return Some((root_eslint, root.to_path_buf()));
    }
    None
}

pub fn detect_biome_cmd_for_dir(root: &Path, pkg_dir: &str) -> Option<(PathBuf, PathBuf)> {
    if let Some(env_bin) = std::env::var_os("TCTX_BIOME_BIN") {
        if env_bin == "none" || env_bin == "0" {
            return None;
        }
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            let workdir = if pkg_dir.is_empty() {
                root.to_path_buf()
            } else {
                root.join(pkg_dir)
            };
            return Some((p, workdir));
        }
    }
    if !pkg_dir.is_empty() {
        let sub_root = root.join(pkg_dir);
        let sub_biome = sub_root.join("node_modules/.bin/biome");
        if sub_biome.is_file() {
            return Some((sub_biome, sub_root));
        }
    }
    let root_biome = root.join("node_modules/.bin/biome");
    if root_biome.is_file() {
        return Some((root_biome, root.to_path_buf()));
    }
    None
}

pub fn detect_tsc_cmd_for_dir(root: &Path, pkg_dir: &str) -> Option<(PathBuf, PathBuf)> {
    if let Some(env_bin) = std::env::var_os("TCTX_TSC_BIN") {
        if env_bin == "none" || env_bin == "0" {
            return None;
        }
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            let workdir = if pkg_dir.is_empty() {
                root.to_path_buf()
            } else {
                root.join(pkg_dir)
            };
            return Some((p, workdir));
        }
    }
    if !pkg_dir.is_empty() {
        let sub_root = root.join(pkg_dir);
        let sub_tsc = sub_root.join("node_modules/.bin/tsc");
        if sub_tsc.is_file() && sub_root.join("tsconfig.json").is_file() {
            return Some((sub_tsc, sub_root));
        }
    }
    let root_tsc = root.join("node_modules/.bin/tsc");
    if root_tsc.is_file() && root.join("tsconfig.json").is_file() {
        return Some((root_tsc, root.to_path_buf()));
    }
    None
}

pub fn detect_nextest_cmd(_root: &Path) -> Option<Vec<String>> {
    if let Some(env_bin) = std::env::var_os("TCTX_NEXTEST_BIN") {
        if env_bin == "none" || env_bin == "0" {
            return None;
        }
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return Some(vec![p.to_string_lossy().into_owned(), "nextest".to_string()]);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.join("cargo-nextest").is_file() {
                return Some(vec!["cargo".to_string(), "nextest".to_string()]);
            }
        }
    }
    None
}

pub fn detect_go_cmd(_root: &Path) -> Vec<String> {
    if let Some(env_bin) = std::env::var_os("TCTX_GO_BIN") {
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return vec![p.to_string_lossy().into_owned()];
        }
    }
    vec!["go".to_string()]
}

pub fn detect_cargo_cmd(_root: &Path) -> Vec<String> {
    if let Some(env_bin) = std::env::var_os("TCTX_CARGO_BIN") {
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return vec![p.to_string_lossy().into_owned()];
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("cargo");
            if cand.is_file() {
                return vec![cand.to_string_lossy().into_owned()];
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let cand = PathBuf::from(home).join(".cargo/bin/cargo");
        if cand.is_file() {
            return vec![cand.to_string_lossy().into_owned()];
        }
    }
    vec!["cargo".to_string()]
}

pub fn detect_ruff_cmd(root: &Path, managed_parent: Option<&Path>) -> Option<Vec<String>> {
    if let Some(env_bin) = std::env::var_os("TCTX_RUFF_BIN") {
        if env_bin == "none" || env_bin == "0" {
            return None;
        }
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return Some(vec![p.to_string_lossy().into_owned()]);
        }
    }
    let mut candidates = vec![
        root.join(".venv/bin/ruff"),
        root.join("venv/bin/ruff"),
        root.join(".env/bin/ruff"),
    ];
    for container in ["src", "services", "packages", "apps"] {
        let dir = root.join(container);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    candidates.push(sub.join(".venv/bin/ruff"));
                    candidates.push(sub.join("venv/bin/ruff"));
                }
            }
        }
    }
    if let Some(parent) = managed_parent {
        candidates.push(parent.join(".venv/bin/ruff"));
        candidates.push(parent.join("venv/bin/ruff"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin/ruff"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return Some(vec![candidate.to_string_lossy().into_owned()]);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("ruff");
            if cand.is_file() {
                return Some(vec![cand.to_string_lossy().into_owned()]);
            }
        }
    }
    None
}

pub fn detect_pytest_cmd(root: &Path, managed_parent: Option<&Path>) -> Vec<String> {
    if let Some(env_bin) = std::env::var_os("TCTX_PYTEST_BIN") {
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return vec![p.to_string_lossy().into_owned()];
        }
    }
    let mut candidates = vec![
        root.join(".venv/bin/pytest"),
        root.join("venv/bin/pytest"),
        root.join(".env/bin/pytest"),
    ];
    for container in ["src", "services", "packages", "apps"] {
        let dir = root.join(container);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    candidates.push(sub.join(".venv/bin/pytest"));
                    candidates.push(sub.join("venv/bin/pytest"));
                }
            }
        }
    }
    if let Some(parent) = managed_parent {
        candidates.push(parent.join(".venv/bin/pytest"));
        candidates.push(parent.join("venv/bin/pytest"));
    }
    if let Some(grand) = root.parent().and_then(|p| p.parent()) {
        // Common git worktree sibling layout: <repo>.worktrees/<branch> -> check sibling <repo>/.venv/bin/pytest
        if let Some(dir_name) = root
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            && let Some(base_repo) = dir_name.strip_suffix(".worktrees")
        {
            candidates.push(grand.join(base_repo).join(".venv/bin/pytest"));
        }
    }
    for candidate in candidates {
        if candidate.is_file() {
            return vec![candidate.to_string_lossy().into_owned()];
        }
    }
    vec![
        "python3".to_string(),
        "-m".to_string(),
        "pytest".to_string(),
    ]
}

pub fn private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(invalid(format!(
                    "directory must be owned, non-symlink and mode 0700: {}",
                    path.display()
                ))
                .into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn state_dir() -> Result<PathBuf> {
    let base = std::env::var_os("TCTX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/state/test-context")
        });
    if !base.is_absolute() {
        return Err(invalid("TCTX_HOME/HOME must resolve to an absolute path").into());
    }
    private_dir(&base)?;
    Ok(base)
}

pub fn cache_dir() -> Result<PathBuf> {
    let dir = state_dir()?.join("cache");
    private_dir(&dir)?;
    Ok(dir)
}

pub fn socket_path() -> Result<PathBuf> {
    let path = state_dir()?.join("daemon.sock");
    if path.as_os_str().len() >= 104 {
        return Err(invalid("Unix socket path too long; use a shorter TCTX_HOME").into());
    }
    Ok(path)
}

pub fn load() -> Result<Vec<Spec>> {
    type RegistryCacheEntry = (PathBuf, (u64, i64, i64), Vec<Spec>);
    static REGISTRY_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<RegistryCacheEntry>>> =
        std::sync::OnceLock::new();
    let path = state_dir()?.join("workspaces.json");
    let stamp = std::fs::metadata(&path)
        .ok()
        .map(|m| (m.len(), m.mtime(), m.mtime_nsec()));
    let cache_mutex = REGISTRY_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some(current_stamp) = stamp
        && let Some((cached_path, cached_stamp, cached_specs)) = cache_mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        && cached_path == &path
        && *cached_stamp == current_stamp
    {
        return Ok(cached_specs.clone());
    }
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err(invalid("registry exceeds 1 MiB").into());
            }
            let specs: Vec<Spec> = serde_json::from_slice(&bytes)?;
            if specs.len() > MAX_REGISTERED_ROOTS {
                return Err(invalid(format!(
                    "at most {MAX_REGISTERED_ROOTS} roots can be registered"
                ))
                .into());
            }
            if let Some(current_stamp) = stamp {
                *cache_mutex
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((path, current_stamp, specs.clone()));
            }
            Ok(specs)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn mutate<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Vec<Spec>) -> Result<()>,
{
    let dir = state_dir()?;
    let lock_path = dir.join("registry.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let mut specs = load()?;
    f(&mut specs)?;
    let bytes = serde_json::to_vec_pretty(&specs)?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("registry exceeds 1 MiB").into());
    }
    let tmp_path = dir.join(format!("workspaces.json.tmp.{}", std::process::id()));
    let write_res: Result<()> = (|| {
        let mut tmp = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        tmp.write_all(&bytes)?;
        tmp.sync_all()?;
        std::fs::rename(&tmp_path, dir.join("workspaces.json"))?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

pub fn register(spec: Spec) -> Result<Spec> {
    let workspace = spec.workspace()?;
    let normalized = Spec {
        root: workspace.root,
        scopes: workspace.scopes,
        globs: workspace.globs,
        excludes: workspace.excludes,
        default_excludes: workspace.default_excludes,
        pytest_cmd: spec.pytest_cmd,
        policy: spec.policy,
        allow_worktrees: spec.allow_worktrees,
        managed_parent: spec.managed_parent,
    };
    let ret = normalized.clone();
    mutate(|specs| {
        specs.retain(|s| s.root != normalized.root);
        ensure_capacity(specs)?;
        specs.push(normalized);
        specs.sort_by(|a, b| a.root.cmp(&b.root));
        Ok(())
    })?;
    Ok(ret)
}

pub(crate) fn missing_managed_roots(specs: &[Spec]) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    for spec in specs.iter().filter(|spec| spec.managed_parent.is_some()) {
        match std::fs::symlink_metadata(&spec.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(spec.root.clone());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    Ok(missing)
}

pub(crate) fn ensure_capacity(specs: &mut Vec<Spec>) -> Result<()> {
    if specs.len() < MAX_REGISTERED_ROOTS {
        return Ok(());
    }
    let missing = missing_managed_roots(specs)?;
    specs.retain(|spec| !missing.contains(&spec.root));
    if specs.len() >= MAX_REGISTERED_ROOTS {
        return Err(invalid(format!(
            "{MAX_REGISTERED_ROOTS} registered roots reached; run tctx prune-worktrees"
        ))
        .into());
    }
    Ok(())
}

pub fn unregister(root: &Path) -> Result<()> {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && root.is_absolute()
                && !root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)) =>
        {
            root.to_path_buf()
        }
        Err(error) => return Err(error.into()),
    };
    mutate(|specs| {
        let count = specs.len();
        specs.retain(|s| s.root != root);
        if count == specs.len() {
            return Err(invalid("root is not registered").into());
        }
        Ok(())
    })
}

pub fn resolve_spec(path: &Path) -> Result<Spec> {
    let path = path.canonicalize()?;
    load()?
        .into_iter()
        .filter(|s| path.starts_with(&s.root))
        .max_by_key(|s| s.root.components().count())
        .ok_or_else(|| {
            invalid(format!(
                "workspace is not registered with test-context: {} (run `tctx --root {} register`)",
                path.display(),
                path.display()
            ))
            .into()
        })
}
