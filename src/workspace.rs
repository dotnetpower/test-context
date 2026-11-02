use crate::exclusion_policy::{is_excluded_dir_name, is_excluded_rel_path};
use crate::policy::MemoryPolicy;
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

pub const PYTEST_CONFIG_FILES: &[&str] = &[
    "pyproject.toml",
    "pytest.ini",
    "setup.cfg",
    "tox.ini",
    "uv.lock",
    "poetry.lock",
    "requirements.txt",
    "ruff.toml",
    ".ruff.toml",
];

pub const RUST_CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "rust-toolchain.toml",
    "rust-toolchain",
];

pub const FRONTEND_CONFIG_FILES: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "tsconfig.app.json",
    "tsconfig.node.json",
    "jsconfig.json",
    "vite.config.ts",
    "vite.config.js",
    "vite.config.mjs",
    "vitest.config.ts",
    "vitest.config.js",
    "vitest.config.mjs",
    "vitest.workspace.ts",
    "vitest.workspace.js",
    "jest.config.js",
    "jest.config.ts",
    "jest.config.mjs",
    "playwright.config.ts",
    "playwright.config.js",
    "cypress.config.ts",
    "cypress.config.js",
    "tailwind.config.js",
    "tailwind.config.ts",
    "biome.json",
    "biome.jsonc",
    "pnpm-workspace.yaml",
    "turbo.json",
    "nx.json",
];

pub const POLYGLOT_CONFIG_FILES: &[&str] = &[
    "go.mod",
    "go.work",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

pub const POLYGLOT_SOURCE_EXTENSIONS: &[&str] = &[
    ".go", ".cs", ".java", ".kt", ".c", ".cpp", ".cc", ".h", ".hpp", ".proto",
];

pub const FRONTEND_SOURCE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte",
];

pub const DATA_FILE_EXTENSIONS: &[&str] = &[
    ".json", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".sql", ".rego", ".csv", ".snap",
    ".golden", ".proto", ".graphql", ".gql", ".txt", ".css", ".scss", ".sass", ".less", ".svg",
    ".mdx", ".html",
];

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub default_excludes: bool,
    include_matcher: GlobSet,
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
                "*.rs".to_string(),
                "*.ts".to_string(),
                "*.tsx".to_string(),
                "*.js".to_string(),
                "*.jsx".to_string(),
                "*.mjs".to_string(),
                "*.cjs".to_string(),
                "*.mts".to_string(),
                "*.cts".to_string(),
                "*.vue".to_string(),
                "*.svelte".to_string(),
                "*.css".to_string(),
                "*.scss".to_string(),
                "*.sass".to_string(),
                "*.less".to_string(),
                "*.go".to_string(),
                "*.cs".to_string(),
                "*.java".to_string(),
                "*.kt".to_string(),
                "*.c".to_string(),
                "*.cpp".to_string(),
                "*.cc".to_string(),
                "*.h".to_string(),
                "*.hpp".to_string(),
                "*.proto".to_string(),
                "*.graphql".to_string(),
                "*.gql".to_string(),
                "*.csproj".to_string(),
                "pyproject.toml".to_string(),
                "pytest.ini".to_string(),
                "setup.cfg".to_string(),
                "tox.ini".to_string(),
                "Cargo.toml".to_string(),
                "rust-toolchain.toml".to_string(),
                "go.mod".to_string(),
                "package.json".to_string(),
                "tsconfig.json".to_string(),
                "vite.config.ts".to_string(),
                "vite.config.js".to_string(),
                "vitest.config.ts".to_string(),
                "vitest.config.js".to_string(),
                "jest.config.js".to_string(),
                "jest.config.ts".to_string(),
            ]
        } else {
            globs
        };
        let mut include_builder = GlobSetBuilder::new();
        for pattern in &globs {
            include_builder.add(
                Glob::new(pattern).map_err(|e| invalid(format!("invalid include glob: {e}")))?,
            );
        }
        let include_matcher = include_builder.build()?;
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
            include_matcher,
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
        self.matches_permitted_filename_or_glob(rel, file_name)
    }

    fn matches_permitted_filename_or_glob(&self, rel: &Path, file_name: &str) -> bool {
        self.include_matcher.is_match(rel)
            || self.include_matcher.is_match(Path::new(file_name))
            || PYTEST_CONFIG_FILES.contains(&file_name)
            || RUST_CONFIG_FILES.contains(&file_name)
            || FRONTEND_CONFIG_FILES.contains(&file_name)
            || POLYGLOT_CONFIG_FILES.contains(&file_name)
            || file_name.ends_with(".csproj")
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
                } else if file_type.is_file()
                    && let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                    && !(self.default_excludes && is_excluded_dir_name(file_name))
                    && self.matches_permitted_filename_or_glob(rel, file_name)
                {
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

    fn pytest_option_tokens(&self, option: &str) -> Vec<String> {
        let mut values = Vec::new();
        for name in ["pyproject.toml", "pytest.ini", "setup.cfg", "tox.ini"] {
            let path = self.root.join(name);
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut in_pytest_section = name == "pyproject.toml";
            let mut collecting = false;
            let mut bracketed = false;
            for raw in text.lines() {
                let line = raw.trim();
                if line.starts_with('[') && line.ends_with(']') {
                    if collecting && bracketed && line == "]" {
                        collecting = false;
                        bracketed = false;
                        continue;
                    }
                    in_pytest_section = matches!(
                        line,
                        "[tool.pytest.ini_options]" | "[pytest]" | "[tool:pytest]"
                    );
                    collecting = false;
                    bracketed = false;
                    continue;
                }
                if !in_pytest_section {
                    continue;
                }
                let value = if line.starts_with(option)
                    && line[option.len()..].trim_start().starts_with(['=', ':'])
                {
                    let rest = line[option.len()..].trim_start();
                    let value = rest[1..].trim();
                    collecting =
                        value.is_empty() || (value.starts_with('[') && !value.contains(']'));
                    bracketed = value.starts_with('[');
                    Some(value)
                } else if collecting && (bracketed || raw.starts_with([' ', '\t'])) {
                    if bracketed && line.contains(']') {
                        collecting = false;
                        bracketed = false;
                    }
                    Some(line)
                } else {
                    collecting = false;
                    bracketed = false;
                    None
                };
                let Some(value) = value else {
                    continue;
                };
                for token in value
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches(['[', ']'])
                    .split(|c: char| c == ',' || c.is_whitespace())
                {
                    let pattern = token.trim().trim_matches(['\'', '"']);
                    if !pattern.is_empty()
                        && pattern.len() <= 256
                        && !pattern.contains(['\0', '\r', '\n'])
                    {
                        values.push(pattern.to_string());
                    }
                }
            }
        }
        values.sort();
        values.dedup();
        values
    }

    pub fn pytest_file_patterns(&self) -> Vec<String> {
        let mut patterns = self.pytest_option_tokens("python_files");
        if patterns.is_empty() {
            patterns.extend(["test_*.py".to_string(), "*_test.py".to_string()]);
        }
        patterns
    }

    pub fn pytest_testpaths(&self) -> Vec<String> {
        self.pytest_option_tokens("testpaths")
            .into_iter()
            .map(|path| path.trim_matches('/').replace('\\', "/"))
            .filter(|path| {
                !path.is_empty()
                    && path != "."
                    && !Path::new(path).is_absolute()
                    && !Path::new(path)
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
            })
            .collect()
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
    pub structured: Option<Vec<u8>>,
    pub cancelled: bool,
}

fn drain_bounded(mut reader: impl Read, max_output_bytes: usize) -> Vec<u8> {
    let mut kept = Vec::with_capacity(max_output_bytes.min(64 * 1024));
    let mut chunk = [0_u8; 64 * 1024];
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = max_output_bytes.saturating_sub(kept.len());
        if remaining > 0 {
            kept.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    }
    kept
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<BoundedOutput> {
    run_command_with_timeout_tracked(command, timeout, max_output_bytes, None, None)
}

pub fn run_command_with_timeout_tracked(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> Result<BoundedOutput> {
    let started = Instant::now();
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Some(active) = active_pgid
        && let Ok(pgid) = i32::try_from(pid)
    {
        active.store(pgid, Ordering::Release);
    }
    if let Some(active) = active_pgids
        && let Ok(pgid) = i32::try_from(pid)
    {
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pgid);
    }
    struct PgidReset<'a> {
        single: Option<&'a AtomicI32>,
        many: Option<&'a Mutex<BTreeSet<i32>>>,
        pid: u32,
    }
    impl Drop for PgidReset<'_> {
        fn drop(&mut self) {
            if let Some(active) = self.single {
                active.store(0, Ordering::Release);
            }
            if let Some(active) = self.many
                && let Ok(pgid) = i32::try_from(self.pid)
            {
                active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&pgid);
            }
        }
    }
    let _pgid_reset = PgidReset {
        single: active_pgid,
        many: active_pgids,
        pid,
    };
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let stdout_handle = std::thread::spawn(move || drain_bounded(stdout_pipe, max_output_bytes));
    let stderr_handle = std::thread::spawn(move || drain_bounded(stderr_pipe, max_output_bytes));

    let mut timed_out = false;
    let mut poll_sleep = Duration::from_micros(250);
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
        std::thread::sleep(poll_sleep);
        poll_sleep = (poll_sleep * 2).min(Duration::from_millis(10));
    };
    // Ensure any orphaned descendant in the child's process group that inherited stdout/stderr is terminated
    // so stdout_handle.join() and stderr_handle.join() cannot block indefinitely.
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGKILL);
        }
    }

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let cancelled = !timed_out
        && status
            .signal()
            .is_some_and(|signal| matches!(signal, libc::SIGKILL | libc::SIGTERM));
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        duration_ms,
        timed_out,
        structured: None,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
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
}
