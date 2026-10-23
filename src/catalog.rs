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
    let path = state_dir()?.join("workspaces.json");
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
    {
        let mut tmp = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        tmp.write_all(&bytes)?;
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, dir.join("workspaces.json"))?;
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
