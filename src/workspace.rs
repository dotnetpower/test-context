use crate::exclusion_policy::{is_excluded_dir_name, is_excluded_rel_path};
use crate::policy::MemoryPolicy;
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const PYTEST_CONFIG_FILES: &[&str] = &[
    "pyproject.toml",
    "pytest.ini",
    "setup.cfg",
    "tox.ini",
    "uv.lock",
    "poetry.lock",
    "requirements.txt",
];

pub const DATA_FILE_EXTENSIONS: &[&str] = &[
    ".json", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".sql", ".rego", ".csv",
];

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub default_excludes: bool,
    exclude_matcher: GlobSet,
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub files: Vec<PathBuf>,
    pub total_path_bytes: usize,
    pub metadata_source_bytes: usize,
}

impl Workspace {
    pub fn new(root: &Path, scopes: Vec<String>, globs: Vec<String>) -> Result<Self> {
        Self::new_with_excludes(root, scopes, globs, Vec::new(), true)
    }

    pub fn new_with_excludes(
        root: &Path,
        scopes: Vec<String>,
        globs: Vec<String>,
        excludes: Vec<String>,
        default_excludes: bool,
    ) -> Result<Self> {
        let root = root.canonicalize().map_err(|e| {
            invalid(format!(
                "workspace root does not exist or cannot be canonicalized ({}): {e}",
                root.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(invalid("workspace root must be a directory").into());
        }
        let scopes = if scopes.is_empty() {
            vec![".".to_string()]
        } else {
            let mut normalized = Vec::new();
            for s in scopes {
                let trimmed = s.trim().trim_matches('/').to_string();
                let entry = if trimmed.is_empty() {
                    ".".to_string()
                } else {
                    trimmed
                };
                if entry.contains("..") {
                    return Err(invalid("scope cannot contain '..'").into());
                }
                if !normalized.contains(&entry) {
                    normalized.push(entry);
                }
            }
            normalized
        };
        let globs = if globs.is_empty() {
            vec![
                "*.py".to_string(),
                "pyproject.toml".to_string(),
                "pytest.ini".to_string(),
                "setup.cfg".to_string(),
                "tox.ini".to_string(),
            ]
        } else {
            globs
        };
        let mut builder = GlobSetBuilder::new();
        for pat in &excludes {
            builder.add(Glob::new(pat).map_err(|e| invalid(format!("invalid exclude glob: {e}")))?);
        }
        let exclude_matcher = builder.build()?;
        Ok(Self {
            root,
            scopes,
            globs,
            excludes,
            default_excludes,
            exclude_matcher,
        })
    }

    pub fn is_permitted_rel(&self, rel: &Path) -> bool {
        if rel.as_os_str().is_empty() {
            return false;
        }
        if self.default_excludes && is_excluded_rel_path(rel) {
            return false;
        }
        if self.exclude_matcher.is_match(rel) {
            return false;
        }
        let in_scope = self.scopes.iter().any(|scope| {
            if scope == "." {
                true
            } else {
                rel.starts_with(Path::new(scope))
            }
        });
        if !in_scope {
            return false;
        }
        let Some(file_name) = rel.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        file_name.ends_with(".py") || PYTEST_CONFIG_FILES.contains(&file_name)
    }

    pub fn is_permitted_data_rel(&self, rel: &Path) -> bool {
        if rel.as_os_str().is_empty()
            || rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        if self.default_excludes && is_excluded_rel_path(rel) {
            return false;
        }
        if self.exclude_matcher.is_match(rel) {
            return false;
        }
        let Some(file_name) = rel.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        DATA_FILE_EXTENSIONS
            .iter()
            .any(|ext| file_name.ends_with(ext))
    }

    pub fn files_limited(&self, policy: &MemoryPolicy) -> Result<Inventory> {
        let mut files = BTreeSet::new();
        let mut stack = Vec::new();
        for scope in &self.scopes {
            let start = if scope == "." {
                self.root.clone()
            } else {
                self.root.join(scope)
            };
            if start.exists() {
                stack.push(start);
            }
        }
        let mut total_path_bytes = 0usize;
        let mut metadata_source_bytes = 0usize;

        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let file_type = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&self.root) else {
                    continue;
                };
                if self.exclude_matcher.is_match(rel) {
                    continue;
                }
                if file_type.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && self.default_excludes
                        && is_excluded_dir_name(name)
                    {
                        continue;
                    }
                    stack.push(path);
                } else if file_type.is_file() && self.is_permitted_rel(rel) {
                    let meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
                    if len > policy.max_file_bytes {
                        continue;
                    }
                    let rel_buf = rel.to_path_buf();
                    if files.insert(rel_buf.clone()) {
                        total_path_bytes =
                            total_path_bytes.saturating_add(rel_buf.as_os_str().len());
                        metadata_source_bytes = metadata_source_bytes.saturating_add(len);
                        if files.len() > policy.max_files {
                            return Err(invalid("workspace exceeds max_files policy limit").into());
                        }
                        if total_path_bytes > policy.max_total_path_bytes {
                            return Err(invalid(
                                "workspace exceeds max_total_path_bytes policy limit",
                            )
                            .into());
                        }
                    }
                }
            }
        }

        Ok(Inventory {
            files: files.into_iter().collect(),
            total_path_bytes,
            metadata_source_bytes,
        })
    }

    pub fn watch_directories_for<'a>(
        &self,
        rel_files: impl Iterator<Item = &'a Path>,
        policy: &MemoryPolicy,
    ) -> Result<Vec<PathBuf>> {
        let mut dirs = BTreeSet::new();
        dirs.insert(self.root.clone());
        for rel in rel_files {
            let mut cur = rel.parent();
            while let Some(parent) = cur {
                if parent.as_os_str().is_empty() {
                    break;
                }
                dirs.insert(self.root.join(parent));
                cur = parent.parent();
            }
        }
        if dirs.len() > policy.max_watch_dirs {
            return Err(invalid("workspace exceeds max_watch_dirs policy limit").into());
        }
        Ok(dirs.into_iter().collect())
    }

    pub fn read_known_with_limit(&self, rel: &Path, max_bytes: usize) -> Result<Vec<u8>> {
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(invalid("relative path required").into());
        }
        let full = self.root.join(rel);
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(invalid("refusing symlink or non-regular file").into());
        }
        let file = std::fs::File::open(&full)?;
        let mut buf = Vec::new();
        file.take(max_bytes as u64 + 1).read_to_end(&mut buf)?;
        if buf.len() > max_bytes {
            return Err(invalid("file exceeds max_file_bytes").into());
        }
        Ok(buf)
    }
}

pub fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let output = run_command_with_timeout(command, timeout, 4 * 1024 * 1024)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(invalid(format!(
            "command failed ({}): {}",
            output.status,
            stderr.trim()
        ))
        .into());
    }
    Ok(output.stdout)
}

pub struct BoundedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
    pub timed_out: bool,
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<BoundedOutput> {
    let started = Instant::now();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe
            .take(max_output_bytes as u64)
            .read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe
            .take(max_output_bytes as u64)
            .read_to_end(&mut buf);
        buf
    });

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            if let Ok(pgid) = i32::try_from(pid)
                && pgid > 0
            {
                unsafe {
                    let _ = libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        duration_ms,
        timed_out,
    })
}
