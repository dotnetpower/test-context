use crate::exclusion_policy::{is_excluded_dir_name, is_excluded_rel_path};
use crate::policy::MemoryPolicy;
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
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
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
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
    "Cargo.lock",
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
    "vite.config.mts",
    "vite.config.js",
    "vite.config.mjs",
    "vite.config.cjs",
    "vitest.config.ts",
    "vitest.config.mts",
    "vitest.config.js",
    "vitest.config.mjs",
    "vitest.config.cjs",
    "vitest.workspace.ts",
    "vitest.workspace.mts",
    "vitest.workspace.js",
    "vitest.workspace.mjs",
    "jest.config.js",
    "jest.config.ts",
    "jest.config.mjs",
    "jest.config.cjs",
    "playwright.config.ts",
    "playwright.config.js",
    "cypress.config.ts",
    "cypress.config.js",
    "tailwind.config.js",
    "tailwind.config.ts",
    "biome.json",
    "biome.jsonc",
    "pnpm-workspace.yaml",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "turbo.json",
    "nx.json",
];

pub const POLYGLOT_CONFIG_FILES: &[&str] = &[
    "go.mod",
    "go.sum",
    "go.work",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

pub const POLYGLOT_SOURCE_EXTENSIONS: &[&str] = &[
    ".go", ".cs", ".java", ".kt", ".c", ".cpp", ".cc", ".cxx", ".h", ".hh", ".hpp", ".hxx", ".s",
    ".S", ".sx", ".m", ".mm", ".f", ".F", ".for", ".f90", ".f95", ".syso", ".proto",
];

pub const FRONTEND_SOURCE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte",
];

pub const DATA_FILE_EXTENSIONS: &[&str] = &[
    ".json", ".jsonc", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".sql", ".rego", ".csv", ".snap",
    ".golden", ".proto", ".graphql", ".gql", ".txt", ".css", ".scss", ".sass", ".less", ".styl",
    ".stylus", ".svg", ".mdx", ".html",
];

#[derive(Clone, Debug)]
struct GitignoreMatcher {
    ignore: GlobSet,
    ignore_ranks: Vec<usize>,
    dir_only_ignore: GlobSet,
    dir_only_ignore_ranks: Vec<usize>,
    allow: GlobSet,
    allow_ranks: Vec<usize>,
    allow_ancestors: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub default_excludes: bool,
    include_matcher: GlobSet,
    exclude_matcher: GlobSet,
    gitignore: GitignoreMatcher,
}

fn compile_root_gitignore(root: &Path, enabled: bool) -> GitignoreMatcher {
    use std::os::unix::fs::OpenOptionsExt;

    let empty = || GitignoreMatcher {
        ignore: GlobSetBuilder::new().build().unwrap_or_default(),
        ignore_ranks: Vec::new(),
        dir_only_ignore: GlobSetBuilder::new().build().unwrap_or_default(),
        dir_only_ignore_ranks: Vec::new(),
        allow: GlobSetBuilder::new().build().unwrap_or_default(),
        allow_ranks: Vec::new(),
        allow_ancestors: BTreeSet::new(),
    };
    if !enabled {
        return empty();
    }
    type GitignoreCacheKey = (PathBuf, Option<crate::ast_graph::FileStamp>);
    static GITIGNORE_CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, (GitignoreCacheKey, GitignoreMatcher)>>,
    > = std::sync::OnceLock::new();
    let path = root.join(".gitignore");
    let stamp = crate::ast_graph::file_stamp(&path);
    let key = (path.clone(), stamp);
    let cache =
        GITIGNORE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Some((cached_key, matcher)) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(root)
        && *cached_key == key
    {
        return matcher.clone();
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let m = empty();
            let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
            if guard.len() > 256 {
                guard.clear();
            }
            guard.insert(root.to_path_buf(), (key, m.clone()));
            return m;
        }
        Err(_) => {
            // Do not cache transient open errors (e.g., EACCES/EMFILE) as an empty matcher
            return empty();
        }
    };
    let mut raw_bytes = Vec::new();
    if file.take(256 * 1024).read_to_end(&mut raw_bytes).is_err() {
        return empty();
    }
    let compiled = compile_root_gitignore_from_bytes(&raw_bytes, empty);
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if guard.len() > 256 {
        guard.clear();
    }
    guard.insert(root.to_path_buf(), (key, compiled.clone()));
    compiled
}

fn compile_root_gitignore_from_bytes(
    raw_bytes: &[u8],
    empty: fn() -> GitignoreMatcher,
) -> GitignoreMatcher {
    let text = String::from_utf8_lossy(raw_bytes);
    let mut ignore_builder = GlobSetBuilder::new();
    let mut ignore_ranks = Vec::new();
    let mut dir_only_builder = GlobSetBuilder::new();
    let mut dir_only_ranks = Vec::new();
    let mut allow_builder = GlobSetBuilder::new();
    let mut allow_ranks = Vec::new();
    let mut allow_ancestors = BTreeSet::new();

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed_start = line.trim_start();
        if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
            continue;
        }
        let (negated, rest) = if let Some(after_bang) = line.strip_prefix('!') {
            (true, after_bang)
        } else if let Some(escaped) = line.strip_prefix("\\!") {
            (false, escaped)
        } else if let Some(escaped_hash) = line.strip_prefix("\\#") {
            (false, escaped_hash)
        } else {
            (false, line)
        };
        let raw_trimmed = rest.trim();
        let dir_only = raw_trimmed.ends_with('/');
        let cleaned = raw_trimmed.trim_end_matches('/');
        if cleaned.is_empty() || cleaned == "." || cleaned == "**" {
            continue;
        }
        let anchored = cleaned.starts_with('/') || cleaned.contains('/');
        let base = cleaned.trim_start_matches('/');
        if base.is_empty() {
            continue;
        }
        let build_git_glob = |pat: &str| {
            globset::GlobBuilder::new(pat)
                .literal_separator(true)
                .build()
                .ok()
        };
        if negated {
            let mut cur = Path::new(base).parent();
            while let Some(anc) = cur {
                if anc.as_os_str().is_empty() {
                    break;
                }
                let clean_anc: PathBuf = anc
                    .components()
                    .take_while(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        !s.contains(['*', '?', '['])
                    })
                    .collect();
                if !clean_anc.as_os_str().is_empty() {
                    allow_ancestors.insert(clean_anc);
                }
                cur = anc.parent();
            }
            let candidates = if anchored {
                vec![base.to_string(), format!("{base}/**")]
            } else {
                vec![
                    base.to_string(),
                    format!("**/{base}"),
                    format!("{base}/**"),
                    format!("**/{base}/**"),
                ]
            };
            for cand in candidates {
                if let Some(glob) = build_git_glob(&cand) {
                    allow_builder.add(glob);
                    allow_ranks.push(line_idx + 1);
                }
            }
        } else if dir_only {
            let self_dir_cands = if anchored {
                vec![base.to_string()]
            } else {
                vec![base.to_string(), format!("**/{base}")]
            };
            for cand in self_dir_cands {
                if let Some(glob) = build_git_glob(&cand) {
                    dir_only_builder.add(glob);
                    dir_only_ranks.push(line_idx + 1);
                }
            }
            let desc_cands = if anchored {
                vec![format!("{base}/*"), format!("{base}/**/*")]
            } else {
                vec![
                    format!("{base}/*"),
                    format!("{base}/**/*"),
                    format!("**/{base}/*"),
                    format!("**/{base}/**/*"),
                ]
            };
            for cand in desc_cands {
                if let Some(glob) = build_git_glob(&cand) {
                    ignore_builder.add(glob);
                    ignore_ranks.push(line_idx + 1);
                }
            }
        } else {
            let candidates = if anchored {
                vec![base.to_string(), format!("{base}/**")]
            } else {
                vec![
                    base.to_string(),
                    format!("**/{base}"),
                    format!("{base}/**"),
                    format!("**/{base}/**"),
                ]
            };
            for cand in candidates {
                if let Some(glob) = build_git_glob(&cand) {
                    ignore_builder.add(glob);
                    ignore_ranks.push(line_idx + 1);
                }
            }
        }
    }

    let Ok(ignore) = ignore_builder.build() else {
        return empty();
    };
    let Ok(dir_only_ignore) = dir_only_builder.build() else {
        return empty();
    };
    let Ok(allow) = allow_builder.build() else {
        return empty();
    };
    GitignoreMatcher {
        ignore,
        ignore_ranks,
        dir_only_ignore,
        dir_only_ignore_ranks: dir_only_ranks,
        allow,
        allow_ranks,
        allow_ancestors,
    }
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub files: Vec<PathBuf>,
    pub file_stamps: BTreeMap<PathBuf, crate::ast_graph::FileStamp>,
    pub total_path_bytes: usize,
    pub metadata_source_bytes: usize,
    /// Number of permitted eligible files skipped because they exceeded `policy.max_file_bytes`.
    pub skipped_oversized_files: usize,
    /// Bounded sample (up to 256) of workspace-relative paths skipped for exceeding `max_file_bytes`
    /// so callers can track and auto-clear them per-file when they shrink or are deleted.
    pub skipped_oversized_paths: Vec<String>,
    /// Number of directories or directory entries that could not be enumerated due to I/O errors
    /// (e.g. `EACCES`), used to fail closed instead of caching against a partial inventory.
    pub skipped_unreadable_dirs: usize,
}

fn split_shell_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut in_token = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            in_token = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == '\\' && q == '"' {
                escaped = true;
            } else if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            in_token = true;
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                in_token = true;
            }
            '\'' | '"' => {
                quote = Some(ch);
                in_token = true;
            }
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            c => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

fn decode_toml_basic_escape(raw: &str, i: &mut usize, out: &mut String) {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    if *i >= len {
        return;
    }
    match bytes[*i] {
        b'b' => {
            out.push('\u{0008}');
            *i += 1;
        }
        b't' => {
            out.push('\t');
            *i += 1;
        }
        b'n' => {
            out.push('\n');
            *i += 1;
        }
        b'f' => {
            out.push('\u{000C}');
            *i += 1;
        }
        b'r' => {
            out.push('\r');
            *i += 1;
        }
        b'"' => {
            out.push('"');
            *i += 1;
        }
        b'\\' => {
            out.push('\\');
            *i += 1;
        }
        b'u' if *i + 5 <= len => {
            if let Ok(code) = u32::from_str_radix(&raw[*i + 1..*i + 5], 16)
                && let Some(ch) = char::from_u32(code)
            {
                out.push(ch);
                *i += 5;
            } else {
                out.push('u');
                *i += 1;
            }
        }
        b'U' if *i + 9 <= len => {
            if let Ok(code) = u32::from_str_radix(&raw[*i + 1..*i + 9], 16)
                && let Some(ch) = char::from_u32(code)
            {
                out.push(ch);
                *i += 9;
            } else {
                out.push('U');
                *i += 1;
            }
        }
        _ => {
            if let Some(ch) = raw[*i..].chars().next() {
                out.push(ch);
                *i += ch.len_utf8();
            }
        }
    }
}

fn parse_toml_key_segments(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        while i < len && (bytes[i].is_ascii_whitespace() || bytes[i] == b'.') {
            i += 1;
        }
        if i >= len {
            break;
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let q = bytes[i];
            i += 1;
            let mut seg = String::new();
            while i < len && bytes[i] != q {
                if q == b'"' && bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    decode_toml_basic_escape(raw, &mut i, &mut seg);
                    continue;
                }
                if let Some(ch) = raw[i..].chars().next() {
                    seg.push(ch);
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            if i < len && bytes[i] == q {
                i += 1;
            }
            if !seg.is_empty() {
                parts.push(seg);
            }
        } else {
            let start = i;
            while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'.' {
                i += 1;
            }
            let seg = raw[start..i].trim();
            if !seg.is_empty() {
                parts.push(seg.to_string());
            }
        }
    }
    parts
}

fn split_key_value_line(line: &str) -> Option<(Vec<String>, &str)> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (idx, &b) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' && q == b'"' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'=' || b == b':' {
            let raw_key = line[..idx].trim();
            let norm_key = parse_toml_key_segments(raw_key);
            if norm_key.is_empty() {
                return None;
            }
            return Some((norm_key, line[idx + 1..].trim()));
        }
    }
    None
}

fn is_pyproject_pytest_table(segs: &[String]) -> bool {
    let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
    matches!(
        refs.as_slice(),
        ["tool", "pytest", "ini_options"] | ["tool", "pytest"]
    )
}

fn is_pyproject_pytest_option_key(full_segs: &[String], option: &str) -> bool {
    let refs: Vec<&str> = full_segs.iter().map(String::as_str).collect();
    refs == ["tool", "pytest", "ini_options", option] || refs == ["tool", "pytest", option]
}

pub fn is_dotenv_filename(file_name: &str) -> bool {
    file_name == ".env" || file_name.starts_with(".env.")
}

pub fn is_frontend_config_filename(file_name: &str) -> bool {
    FRONTEND_CONFIG_FILES.contains(&file_name)
        || is_dotenv_filename(file_name)
        || ((file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
            && (file_name.ends_with(".json") || file_name.ends_with(".jsonc")))
}

pub fn is_cargo_dot_config_rel(rel: &Path) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    let stripped = s.trim_start_matches("./");
    stripped == ".cargo/config.toml"
        || stripped == ".cargo/config"
        || stripped.ends_with("/.cargo/config.toml")
        || stripped.ends_with("/.cargo/config")
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
                "*.styl".to_string(),
                "*.stylus".to_string(),
                "*.go".to_string(),
                "*.cs".to_string(),
                "*.java".to_string(),
                "*.kt".to_string(),
                "*.c".to_string(),
                "*.cpp".to_string(),
                "*.cc".to_string(),
                "*.cxx".to_string(),
                "*.h".to_string(),
                "*.hh".to_string(),
                "*.hpp".to_string(),
                "*.hxx".to_string(),
                "*.s".to_string(),
                "*.S".to_string(),
                "*.sx".to_string(),
                "*.m".to_string(),
                "*.mm".to_string(),
                "*.f".to_string(),
                "*.F".to_string(),
                "*.for".to_string(),
                "*.f90".to_string(),
                "*.f95".to_string(),
                "*.syso".to_string(),
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
                "go.sum".to_string(),
                "package.json".to_string(),
                "package-lock.json".to_string(),
                "pnpm-lock.yaml".to_string(),
                "yarn.lock".to_string(),
                "bun.lock".to_string(),
                "tsconfig.json".to_string(),
                ".env".to_string(),
                ".env.*".to_string(),
                "vite.config.ts".to_string(),
                "vite.config.mts".to_string(),
                "vite.config.js".to_string(),
                "vite.config.mjs".to_string(),
                "vitest.config.ts".to_string(),
                "vitest.config.mts".to_string(),
                "vitest.config.js".to_string(),
                "vitest.config.mjs".to_string(),
                "jest.config.js".to_string(),
                "jest.config.ts".to_string(),
                "jest.config.mjs".to_string(),
                "jest.config.cjs".to_string(),
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
        let gitignore = compile_root_gitignore(&root, default_excludes);
        Ok(Self {
            root,
            scopes,
            globs,
            excludes,
            default_excludes,
            include_matcher,
            exclude_matcher,
            gitignore,
        })
    }

    pub fn is_gitignored_rel(&self, rel: &Path) -> bool {
        self.is_gitignored_entry(rel, false)
    }

    pub fn is_excluded_or_ignored_rel(&self, rel: &Path) -> bool {
        if self.default_excludes
            && rel.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .is_some_and(is_excluded_dir_name)
            })
        {
            return true;
        }
        self.exclude_matcher.is_match(rel) || self.is_gitignored_entry(rel, true)
    }

    fn is_gitignored_dir_prunable(&self, rel: &Path) -> bool {
        if self.gitignore.allow_ancestors.contains(rel) {
            return false;
        }
        self.is_gitignored_entry(rel, true)
    }

    fn is_gitignored_entry(&self, rel: &Path, is_dir: bool) -> bool {
        if !self.default_excludes
            || (self.gitignore.ignore_ranks.is_empty()
                && self.gitignore.dir_only_ignore_ranks.is_empty())
        {
            return false;
        }
        let mut max_ignore = self
            .gitignore
            .ignore
            .matches(rel)
            .into_iter()
            .filter_map(|idx| self.gitignore.ignore_ranks.get(idx).copied())
            .max()
            .unwrap_or(0);
        if is_dir && !self.gitignore.dir_only_ignore_ranks.is_empty() {
            let dir_rank = self
                .gitignore
                .dir_only_ignore
                .matches(rel)
                .into_iter()
                .filter_map(|idx| self.gitignore.dir_only_ignore_ranks.get(idx).copied())
                .max()
                .unwrap_or(0);
            max_ignore = max_ignore.max(dir_rank);
        }
        if max_ignore == 0 {
            return false;
        }
        if self.gitignore.allow_ranks.is_empty() {
            return true;
        }
        let max_allow = self
            .gitignore
            .allow
            .matches(rel)
            .into_iter()
            .filter_map(|idx| self.gitignore.allow_ranks.get(idx).copied())
            .max()
            .unwrap_or(0);
        max_ignore > max_allow
    }

    pub fn is_permitted_rel(&self, rel: &Path) -> bool {
        if rel.as_os_str().is_empty() {
            return false;
        }
        let is_cargo_cfg = is_cargo_dot_config_rel(rel);
        let is_dotenv = rel
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_dotenv_filename);
        if self.default_excludes {
            if is_cargo_cfg {
                // Allow `.cargo/config[.toml]` unless an ancestor above `.cargo` is excluded (e.g. `target/`).
                if let Some(above_cargo) = rel.parent().and_then(Path::parent)
                    && !above_cargo.as_os_str().is_empty()
                    && is_excluded_rel_path(above_cargo)
                {
                    return false;
                }
            } else if is_dotenv {
                // Allow `.env` / `.env.*` unless an ancestor directory is excluded.
                if let Some(parent_dir) = rel.parent()
                    && !parent_dir.as_os_str().is_empty()
                    && is_excluded_rel_path(parent_dir)
                {
                    return false;
                }
            } else if is_excluded_rel_path(rel) {
                return false;
            }
        }
        if self.exclude_matcher.is_match(rel)
            || (!is_cargo_cfg && self.is_gitignored_rel(rel))
        {
            return false;
        }
        let in_scope = self.scopes.iter().any(|scope| {
            if scope == "." || rel.starts_with(Path::new(scope)) {
                true
            } else if is_cargo_cfg {
                // Cargo inherits `.cargo/config[.toml]` from every ancestor directory of a scope
                // (e.g. root `.cargo/config.toml` applies when scope is `crates/a`).
                let cfg_scope_dir = rel
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or_else(|| Path::new(""));
                cfg_scope_dir.as_os_str().is_empty() || Path::new(scope).starts_with(cfg_scope_dir)
            } else {
                false
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
            || is_cargo_dot_config_rel(rel)
            || is_frontend_config_filename(file_name)
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
        if self.exclude_matcher.is_match(rel) || self.is_gitignored_rel(rel) {
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
        let mut file_stamps = BTreeMap::new();
        let mut stack = Vec::new();
        let mut seen_init_dirs = std::collections::HashSet::new();
        for scope in &self.scopes {
            let start = if scope == "." {
                self.root.clone()
            } else {
                // Also include ancestor `.cargo` directories for scoped workspaces (e.g. root
                // `.cargo/config.toml` when scope is `crates/a`) since Cargo inherits them.
                let mut anc = Path::new(scope).parent();
                while let Some(p) = anc {
                    let anc_cargo = if p.as_os_str().is_empty() {
                        self.root.join(".cargo")
                    } else {
                        self.root.join(p).join(".cargo")
                    };
                    if anc_cargo.is_dir() && seen_init_dirs.insert(anc_cargo.clone()) {
                        stack.push(anc_cargo);
                    }
                    if p.as_os_str().is_empty() {
                        break;
                    }
                    anc = p.parent();
                }
                self.root.join(scope)
            };
            if start.exists() && seen_init_dirs.insert(start.clone()) {
                stack.push(start);
            }
        }
        // Include explicitly configured hidden pytest `testpaths` (both literal `.hidden/tests`
        // and globbed `packages/*/.hidden/tests`) so discovery collects every test pytest does.
        let mut explicit_hidden_testpath_dirs: Vec<PathBuf> = Vec::new();
        let mut hidden_testpath_patterns: Vec<String> = Vec::new();
        for tp in self.pytest_testpaths() {
            if tp.split('/').any(|seg| seg.starts_with('.')) {
                hidden_testpath_patterns.push(tp.clone());
                if !tp.contains(['*', '?', '[', '{']) {
                    let full = self.root.join(&tp);
                    if full.is_dir() {
                        explicit_hidden_testpath_dirs.push(full.clone());
                        if seen_init_dirs.insert(full.clone()) {
                            stack.push(full);
                        }
                    }
                }
            }
        }
        let track_visited_dirs = stack.len() > 1;
        let visited_dirs = std::sync::Mutex::new(seen_init_dirs);
        let hidden_tp_ref = &explicit_hidden_testpath_dirs;
        let hidden_tp_pats_ref = &hidden_testpath_patterns;
        let mut total_path_bytes = 0usize;
        let mut metadata_source_bytes = 0usize;

        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4)
            .clamp(2, 8);
        let work_queue = std::sync::Mutex::new((stack, 0usize));
        let work_cvar = std::sync::Condvar::new();
        let results_acc: std::sync::Mutex<Vec<(PathBuf, usize, crate::ast_graph::FileStamp)>> =
            std::sync::Mutex::new(Vec::new());
        let shared_files = std::sync::atomic::AtomicUsize::new(0);
        let shared_path_bytes = std::sync::atomic::AtomicUsize::new(0);
        let skipped_oversized = std::sync::atomic::AtomicUsize::new(0);
        let skipped_unreadable_dirs = std::sync::atomic::AtomicUsize::new(0);
        let oversized_paths_acc: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let limit_exceeded = std::sync::atomic::AtomicU8::new(0); // 1 = max_files, 2 = max_total_path_bytes

        std::thread::scope(|scope_pool| {
            let queue_ref = &work_queue;
            let cvar_ref = &work_cvar;
            let results_ref = &results_acc;
            let files_cnt_ref = &shared_files;
            let path_bytes_ref = &shared_path_bytes;
            let oversized_ref = &skipped_oversized;
            let unreadable_dirs_ref = &skipped_unreadable_dirs;
            let oversized_paths_ref = &oversized_paths_acc;
            let limit_ref = &limit_exceeded;
            let visited_dirs_ref = &visited_dirs;
            for _ in 0..workers {
                scope_pool.spawn(move || {
                    use std::os::unix::fs::MetadataExt;
                    use std::sync::atomic::Ordering;
                    let mut local_files = Vec::new();
                    let mut local_dirs = Vec::new();
                    loop {
                        if limit_ref.load(Ordering::Acquire) != 0 {
                            break;
                        }
                        let dir = {
                            let mut guard = queue_ref.lock().unwrap_or_else(|p| p.into_inner());
                            loop {
                                if limit_ref.load(Ordering::Acquire) != 0 {
                                    cvar_ref.notify_all();
                                    break None;
                                }
                                if let Some(d) = guard.0.pop() {
                                    guard.1 += 1;
                                    break Some(d);
                                }
                                if guard.1 == 0 {
                                    cvar_ref.notify_all();
                                    break None;
                                }
                                guard = cvar_ref.wait(guard).unwrap_or_else(|p| p.into_inner());
                            }
                        };
                        let Some(dir) = dir else {
                            break;
                        };
                        local_dirs.clear();
                        match std::fs::read_dir(&dir) {
                            Ok(entries) => {
                                for entry_res in entries {
                                    if limit_ref.load(Ordering::Acquire) != 0 {
                                        break;
                                    }
                                    let Ok(entry) = entry_res else {
                                        unreadable_dirs_ref.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    };
                                    let Ok(file_type) = entry.file_type() else {
                                        unreadable_dirs_ref.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    };
                                if file_type.is_symlink() {
                                    continue;
                                }
                                let path = entry.path();
                                let Ok(rel) = path.strip_prefix(&self.root) else {
                                    continue;
                                };
                                if file_type.is_dir() {
                                    // Never descend into `.cargo/*` subdirectories (e.g. `.cargo/registry`
                                    // or `.cargo/git` when a project sets a local `CARGO_HOME`): only
                                    // direct `.cargo/config[.toml]` files are admitted.
                                    if dir.file_name().and_then(|n| n.to_str()) == Some(".cargo") {
                                        continue;
                                    }
                                    let rel_str_for_tp = if !hidden_tp_pats_ref.is_empty() {
                                        Some(rel.to_string_lossy().replace('\\', "/"))
                                    } else {
                                        None
                                    };
                                    let inside_explicit_hidden_tp = hidden_tp_ref
                                        .iter()
                                        .any(|tp_dir| path.starts_with(tp_dir))
                                        || rel_str_for_tp.as_deref().is_some_and(|rs| {
                                            crate::ast_graph::testpaths_allow_dir_prefix(
                                                rs,
                                                hidden_tp_pats_ref,
                                            )
                                        });
                                    if let Some(name) =
                                        path.file_name().and_then(|n| n.to_str())
                                        && self.default_excludes
                                        && is_excluded_dir_name(name)
                                        && name != ".cargo"
                                        && !(inside_explicit_hidden_tp
                                            && name.starts_with('.')
                                            && !matches!(name, ".git" | ".hg" | ".svn"))
                                    {
                                        continue;
                                    }
                                    let is_cargo_dir = path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        == Some(".cargo");
                                    if self.exclude_matcher.is_match(rel)
                                        || (!is_cargo_dir && self.is_gitignored_dir_prunable(rel))
                                    {
                                        continue;
                                    }
                                    if track_visited_dirs
                                        && !visited_dirs_ref
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner())
                                            .insert(path.clone())
                                    {
                                        continue;
                                    }
                                    local_dirs.push(path);
                                } else if file_type.is_file()
                                    && let Some(file_name) =
                                        path.file_name().and_then(|n| n.to_str())
                                    && !(dir.file_name().and_then(|n| n.to_str()) == Some(".cargo")
                                        && !is_cargo_dot_config_rel(rel))
                                    && !(self.default_excludes
                                        && is_excluded_dir_name(file_name)
                                        && !is_dotenv_filename(file_name))
                                    && self.matches_permitted_filename_or_glob(
                                        rel, file_name,
                                    )
                                {
                                    if self.exclude_matcher.is_match(rel)
                                        || (!is_cargo_dot_config_rel(rel)
                                            && self.is_gitignored_rel(rel))
                                    {
                                        continue;
                                    }
                                    let Ok(meta) = entry.metadata() else {
                                        unreadable_dirs_ref.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    };
                                    let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
                                    if len > policy.max_file_bytes {
                                        let prev = oversized_ref.fetch_add(1, Ordering::Relaxed);
                                        if prev < 256
                                            && let Ok(mut guard) = oversized_paths_ref.lock()
                                            && guard.len() < 256
                                        {
                                            guard.push(rel.to_string_lossy().replace('\\', "/"));
                                        }
                                        continue;
                                    }
                                    let rel_len = rel.as_os_str().len();
                                    let new_files =
                                        files_cnt_ref.fetch_add(1, Ordering::AcqRel) + 1;
                                    if new_files > policy.max_files {
                                        limit_ref.store(1, Ordering::Release);
                                        cvar_ref.notify_all();
                                        break;
                                    }
                                    let new_path_bytes = path_bytes_ref
                                        .fetch_add(rel_len, Ordering::AcqRel)
                                        .saturating_add(rel_len);
                                    if new_path_bytes > policy.max_total_path_bytes {
                                        limit_ref.store(2, Ordering::Release);
                                        cvar_ref.notify_all();
                                        break;
                                    }
                                    let stamp = crate::ast_graph::FileStamp {
                                        mtime_sec: meta.mtime(),
                                        mtime_nsec: meta.mtime_nsec(),
                                        size: meta.len(),
                                        ino: meta.ino(),
                                        ctime_sec: meta.ctime(),
                                        ctime_nsec: meta.ctime_nsec(),
                                    };
                                    local_files.push((rel.to_path_buf(), len, stamp));
                                }
                            }
                            }
                            Err(_) => {
                                unreadable_dirs_ref.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        let mut guard = queue_ref.lock().unwrap_or_else(|p| p.into_inner());
                        guard.1 -= 1;
                        if limit_ref.load(Ordering::Acquire) != 0 {
                            guard.0.clear();
                            cvar_ref.notify_all();
                            break;
                        }
                        if !local_dirs.is_empty() {
                            guard.0.append(&mut local_dirs);
                            cvar_ref.notify_all();
                        } else if guard.0.is_empty() && guard.1 == 0 {
                            cvar_ref.notify_all();
                        }
                    }
                    if limit_ref.load(Ordering::Acquire) == 0 && !local_files.is_empty() {
                        results_ref
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .extend(local_files);
                    }
                });
            }
        });

        match limit_exceeded.load(std::sync::atomic::Ordering::Acquire) {
            1 => return Err(invalid("workspace exceeds max_files policy limit").into()),
            2 => return Err(invalid("workspace exceeds max_total_path_bytes policy limit").into()),
            _ => {}
        }

        for (rel_buf, len, stamp) in results_acc.into_inner().unwrap_or_else(|p| p.into_inner()) {
            if files.insert(rel_buf.clone()) {
                total_path_bytes = total_path_bytes.saturating_add(rel_buf.as_os_str().len());
                metadata_source_bytes = metadata_source_bytes.saturating_add(len);
                if files.len() > policy.max_files {
                    return Err(invalid("workspace exceeds max_files policy limit").into());
                }
                if total_path_bytes > policy.max_total_path_bytes {
                    return Err(
                        invalid("workspace exceeds max_total_path_bytes policy limit").into(),
                    );
                }
                file_stamps.insert(rel_buf, stamp);
            }
        }

        let mut skipped_oversized_paths = oversized_paths_acc
            .into_inner()
            .unwrap_or_else(|p| p.into_inner());
        skipped_oversized_paths.sort();
        skipped_oversized_paths.dedup();
        Ok(Inventory {
            files: files.into_iter().collect(),
            file_stamps,
            total_path_bytes,
            metadata_source_bytes,
            skipped_oversized_files: skipped_oversized.load(std::sync::atomic::Ordering::Acquire),
            skipped_oversized_paths,
            skipped_unreadable_dirs: skipped_unreadable_dirs
                .load(std::sync::atomic::Ordering::Acquire),
        })
    }

    /// Enumerates all non-excluded workspace directories (including empty directories) bounded by
    /// `policy.max_watch_dirs`, used during overflow recovery so empty newly created directories
    /// are watched immediately before rescanning files.
    pub fn directories_limited(&self, policy: &MemoryPolicy) -> Vec<PathBuf> {
        self.directories_limited_checked(policy).0
    }

    pub fn directories_limited_checked(&self, policy: &MemoryPolicy) -> (Vec<PathBuf>, bool) {
        let mut dirs = BTreeSet::new();
        dirs.insert(self.root.clone());
        let mut stack: Vec<PathBuf> = Vec::new();
        let mut enqueued: BTreeSet<PathBuf> = BTreeSet::new();
        let mut truncated = false;
        for scope in &self.scopes {
            let start = if scope == "." {
                self.root.clone()
            } else {
                self.root.join(scope)
            };
            // Watch existing ancestors of every scope (even if the scope directory itself is
            // currently missing or later deleted and recreated) so recreation emits watcher events.
            let mut anc = start.parent();
            while let Some(dir) = anc {
                if !dir.starts_with(&self.root) {
                    break;
                }
                if dir.is_dir() {
                    dirs.insert(dir.to_path_buf());
                }
                if dir == self.root {
                    break;
                }
                anc = dir.parent();
            }
            if start.is_dir() && enqueued.insert(start.clone()) {
                dirs.insert(start.clone());
                stack.push(start);
            }
        }
        if stack.is_empty() && self.root.is_dir() {
            stack.push(self.root.clone());
        }
        while let Some(dir) = stack.pop() {
            let is_cargo_dir = dir.file_name().and_then(|n| n.to_str()) == Some(".cargo");
            if is_cargo_dir {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() || !ft.is_dir() {
                    continue;
                }
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&self.root) else {
                    continue;
                };
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if self.default_excludes && is_excluded_dir_name(name) && name != ".cargo" {
                    continue;
                }
                if self.exclude_matcher.is_match(rel)
                    || (name != ".cargo" && self.is_gitignored_dir_prunable(rel))
                {
                    continue;
                }
                if dirs.contains(&path) {
                    continue;
                }
                // Only report truncation upon discovering a real eligible directory beyond `max_watch_dirs`.
                if dirs.len() >= policy.max_watch_dirs {
                    truncated = true;
                    break;
                }
                if dirs.insert(path.clone()) && name != ".cargo" {
                    stack.push(path);
                }
            }
            if truncated {
                break;
            }
        }
        (dirs.into_iter().collect(), truncated)
    }

    pub fn watch_directories_for<'a>(
        &self,
        rel_files: impl Iterator<Item = &'a Path>,
        policy: &MemoryPolicy,
    ) -> Result<Vec<PathBuf>> {
        let mut dirs = BTreeSet::new();
        dirs.insert(self.root.clone());
        let root_cargo = self.root.join(".cargo");
        if root_cargo.is_dir() {
            dirs.insert(root_cargo);
        }
        for rel in rel_files {
            let is_cargo_toml = rel.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml");
            let mut cur = rel.parent();
            while let Some(parent) = cur {
                if is_cargo_toml {
                    // Cargo searches `.cargo/config[.toml]` at every ancestor directory of a crate.
                    let anc_cargo = if parent.as_os_str().is_empty() {
                        self.root.join(".cargo")
                    } else {
                        self.root.join(parent).join(".cargo")
                    };
                    if anc_cargo.is_dir() {
                        dirs.insert(anc_cargo);
                    }
                }
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
        if rel.is_absolute() {
            return Err(invalid("relative path required").into());
        }
        let mut components: Vec<&std::ffi::OsStr> = Vec::new();
        for comp in rel.components() {
            match comp {
                std::path::Component::Normal(name) => components.push(name),
                std::path::Component::CurDir => {}
                _ => return Err(invalid("relative path required").into()),
            }
        }
        if components.is_empty() {
            return Err(invalid("refusing symlink or non-regular file").into());
        }
        #[cfg(unix)]
        let file = {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            use std::os::unix::io::{FromRawFd, OwnedFd};

            let root_c = CString::new(self.root.as_os_str().as_bytes())
                .map_err(|_| invalid("workspace root contains NUL byte"))?;
            // Open the workspace root directory descriptor, then walk EVERY path component with
            // `openat(..., O_NOFOLLOW | O_CLOEXEC)` so neither intermediate symlinks
            // (`linked/secret.py`) nor final symlinks nor TOCTOU races can ever escape `self.root`!
            let root_fd = unsafe {
                libc::open(
                    root_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if root_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut cur_dir = unsafe { OwnedFd::from_raw_fd(root_fd) };
            let last_idx = components.len() - 1;
            for (idx, comp) in components.iter().enumerate() {
                let c_name = CString::new(comp.as_bytes())
                    .map_err(|_| invalid("path component contains NUL byte"))?;
                let is_last = idx == last_idx;
                let flags = if is_last {
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
                };
                let next_fd = unsafe {
                    use std::os::unix::io::AsRawFd;
                    libc::openat(cur_dir.as_raw_fd(), c_name.as_ptr(), flags)
                };
                if next_fd < 0 {
                    let err = std::io::Error::last_os_error();
                    if matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                        return Err(invalid("refusing symlink or non-regular file").into());
                    }
                    return Err(err.into());
                }
                cur_dir = unsafe { OwnedFd::from_raw_fd(next_fd) };
            }
            let f = std::fs::File::from(cur_dir);
            let meta = f.metadata()?;
            if !meta.is_file() {
                return Err(invalid("refusing symlink or non-regular file").into());
            }
            f
        };
        #[cfg(not(unix))]
        let file = {
            let mut cur = self.root.clone();
            for comp in &components {
                cur.push(comp);
                let meta = std::fs::symlink_metadata(&cur)?;
                if meta.file_type().is_symlink() {
                    return Err(invalid("refusing symlink or non-regular file").into());
                }
            }
            let meta = std::fs::symlink_metadata(&cur)?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(invalid("refusing symlink or non-regular file").into());
            }
            std::fs::File::open(&cur)?
        };
        let mut buf = Vec::new();
        file.take(max_bytes as u64 + 1).read_to_end(&mut buf)?;
        if buf.len() > max_bytes {
            return Err(invalid("file exceeds max_file_bytes").into());
        }
        Ok(buf)
    }

    fn effective_pytest_config(&self) -> Option<(&'static str, String)> {
        // Pytest 9+ selects a single configuration file by precedence:
        // pytest.toml > .pytest.toml > pytest.ini > .pytest.ini > pyproject.toml ([tool.pytest.ini_options] / [tool.pytest] / dotted tool.pytest.*) > tox.ini ([pytest]) > setup.cfg ([tool:pytest])
        fn strip_line_comment(raw: &str) -> &str {
            let trimmed = raw.trim();
            if let Some(pos) = trimmed.find('#') {
                trimmed[..pos].trim()
            } else {
                trimmed
            }
        }
        for name in [
            "pytest.toml",
            ".pytest.toml",
            "pytest.ini",
            ".pytest.ini",
            "pyproject.toml",
            "tox.ini",
            "setup.cfg",
        ] {
            let path = self.root.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let has_section = match name {
                "pytest.toml" | ".pytest.toml" | "pytest.ini" | ".pytest.ini" => true,
                "pyproject.toml" => {
                    let mut cur_section: Vec<String> = Vec::new();
                    text.lines().any(|l| {
                        let s = strip_line_comment(l);
                        if s.starts_with('[') && s.ends_with(']') {
                            cur_section = parse_toml_key_segments(&s[1..s.len() - 1]);
                            return is_pyproject_pytest_table(&cur_section);
                        }
                        if let Some((key_segs, _)) = split_key_value_line(s) {
                            let mut full = cur_section.clone();
                            full.extend(key_segs);
                            let refs: Vec<&str> = full.iter().map(String::as_str).collect();
                            return refs.len() >= 3
                                && (refs.starts_with(&["tool", "pytest", "ini_options"])
                                    || refs.starts_with(&["tool", "pytest"]));
                        }
                        false
                    })
                }
                "tox.ini" => text.lines().any(|l| strip_line_comment(l) == "[pytest]"),
                "setup.cfg" => text
                    .lines()
                    .any(|l| strip_line_comment(l) == "[tool:pytest]"),
                _ => false,
            };
            if has_section {
                return Some((name, text));
            }
        }
        None
    }

    fn pytest_option_tokens(&self, option: &str) -> Vec<String> {
        let mut values = Vec::new();
        let Some((name, text)) = self.effective_pytest_config() else {
            return values;
        };
        let is_toml = matches!(name, "pytest.toml" | ".pytest.toml" | "pyproject.toml");
        let strip_unquoted_comment = |s: &str| -> String {
            let mut out = String::with_capacity(s.len());
            let mut quote: Option<char> = None;
            let mut escaped = false;
            for ch in s.chars() {
                if let Some(q) = quote {
                    out.push(ch);
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' && q == '"' {
                        escaped = true;
                    } else if ch == q {
                        quote = None;
                    }
                } else if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    out.push(ch);
                } else if ch == '#' {
                    break;
                } else {
                    out.push(ch);
                }
            }
            out
        };
        let find_unquoted_byte = |s: &str, target: u8| -> Option<usize> {
            let bytes = s.as_bytes();
            let mut quote: Option<u8> = None;
            let mut escaped = false;
            for (idx, &b) in bytes.iter().enumerate() {
                if let Some(q) = quote {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' && q == b'"' {
                        escaped = true;
                    } else if b == q {
                        quote = None;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == target {
                    return Some(idx);
                }
            }
            None
        };
        let push_tokens_from_fragment = |frag: &str, toml_mode: bool, toml_array: bool, out: &mut Vec<String>| {
            let clean = strip_unquoted_comment(frag);
            let trimmed = clean.trim();
            if trimmed.is_empty() {
                return;
            }
            // For INI configs (`pytest.ini`, `tox.ini`, `setup.cfg`), pytest uses shell-style
            // splitting on the entire value (preserving `'integration tests'` as one path).
            if !toml_mode {
                for part in split_shell_tokens(trimmed) {
                    if !part.is_empty()
                        && part.len() <= 256
                        && !part.contains(['\0', '\r', '\n'])
                    {
                        out.push(part);
                    }
                }
                return;
            }
            let bytes = trimmed.as_bytes();
            let mut i = 0usize;
            let len = bytes.len();
            while i < len {
                while i < len
                    && (bytes[i].is_ascii_whitespace()
                        || bytes[i] == b','
                        || (toml_array && matches!(bytes[i], b'[' | b']')))
                {
                    i += 1;
                }
                if i >= len {
                    break;
                }
                if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let q = bytes[i];
                    i += 1;
                    let mut token = String::new();
                    while i < len && bytes[i] != q {
                        if q == b'"' && bytes[i] == b'\\' && i + 1 < len {
                            i += 1;
                            decode_toml_basic_escape(trimmed, &mut i, &mut token);
                            continue;
                        }
                        if let Some(ch) = trimmed[i..].chars().next() {
                            token.push(ch);
                            i += ch.len_utf8();
                        } else {
                            break;
                        }
                    }
                    if i < len && bytes[i] == q {
                        i += 1;
                    }
                    if toml_array {
                        if !token.is_empty()
                            && token.len() <= 256
                            && !token.contains(['\0', '\r', '\n'])
                        {
                            out.push(token);
                        }
                    } else {
                        // Legacy scalar string in TOML (`testpaths = "tests 'integration tests'"`):
                        // pytest uses shell-style (`shlex`) splitting for scalar lists.
                        for part in split_shell_tokens(&token) {
                            if !part.is_empty()
                                && part.len() <= 256
                                && !part.contains(['\0', '\r', '\n'])
                            {
                                out.push(part);
                            }
                        }
                    }
                } else {
                    let start = i;
                    while i < len
                        && !bytes[i].is_ascii_whitespace()
                        && bytes[i] != b','
                        && !(toml_array && matches!(bytes[i], b'[' | b']'))
                    {
                        i += 1;
                    }
                    let token = trimmed[start..i].trim();
                    if !token.is_empty()
                        && token.len() <= 256
                        && !token.contains(['\0', '\r', '\n'])
                    {
                        out.push(token.to_string());
                    }
                }
            }
        };
        let mut in_pytest_section = matches!(
            name,
            "pytest.toml" | ".pytest.toml" | "pytest.ini" | ".pytest.ini"
        );
        let mut current_section: Vec<String> = Vec::new();
        let mut collecting = false;
        let mut bracketed = false;
        for raw in text.lines() {
            let uncommented = strip_unquoted_comment(raw);
            let line = uncommented.trim();
            if line.starts_with('[') && line.ends_with(']') && !bracketed {
                current_section = parse_toml_key_segments(&line[1..line.len() - 1]);
                in_pytest_section = match name {
                    "pyproject.toml" => is_pyproject_pytest_table(&current_section),
                    "setup.cfg" => {
                        current_section.len() == 1 && current_section[0] == "tool:pytest"
                    }
                    _ => current_section.len() == 1 && current_section[0] == "pytest",
                };
                collecting = false;
                bracketed = false;
                continue;
            }
            // Match both direct keys (bare or quoted, e.g. `"python_files" = ...`) inside a pytest
            // section AND dotted TOML keys like `tool.pytest.ini_options.<option> = ...` while
            // strictly preserving TOML key segment boundaries.
            let matched_val = if !bracketed {
                split_key_value_line(line).and_then(|(key_segs, val)| {
                    if in_pytest_section && key_segs.len() == 1 && key_segs[0] == option {
                        Some(val)
                    } else if name == "pyproject.toml" {
                        let mut full = current_section.clone();
                        full.extend(key_segs);
                        if is_pyproject_pytest_option_key(&full, option) {
                            Some(val)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            } else {
                None
            };
            let mut is_array_frag = bracketed;
            let value = if let Some(val) = matched_val {
                let opens_array = is_toml && val.starts_with('[');
                let closes_array = opens_array && find_unquoted_byte(val, b']').is_some();
                is_array_frag = opens_array;
                collecting = val.is_empty() || (opens_array && !closes_array);
                bracketed = opens_array && !closes_array;
                Some(val)
            } else if collecting && (bracketed || raw.starts_with([' ', '\t'])) {
                if bracketed && find_unquoted_byte(line, b']').is_some() {
                    collecting = false;
                    bracketed = false;
                }
                Some(line)
            } else {
                collecting = false;
                bracketed = false;
                None
            };
            if let Some(val) = value {
                push_tokens_from_fragment(val, is_toml, is_array_frag, &mut values);
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

    pub fn pytest_function_patterns(&self) -> Vec<String> {
        let mut patterns = self.pytest_option_tokens("python_functions");
        if patterns.is_empty() {
            patterns.push("test_*".to_string());
        }
        patterns
    }

    pub fn pytest_testpaths(&self) -> Vec<String> {
        let mut normalized = Vec::new();
        for raw in self.pytest_option_tokens("testpaths") {
            let slash = raw.replace('\\', "/");
            let trimmed = slash.trim();
            if trimmed == "." || trimmed == "./" {
                // Workspace root testpath matches all directories.
                return Vec::new();
            }
            // Check absolute paths BEFORE trimming leading slashes; resolve inside workspace root only.
            let rel_candidate = if Path::new(trimmed).is_absolute() {
                let Ok(stripped) = Path::new(trimmed).strip_prefix(&self.root) else {
                    continue;
                };
                let rel = stripped.to_string_lossy().replace('\\', "/");
                if rel.is_empty() || rel == "." {
                    return Vec::new();
                }
                rel
            } else {
                trimmed.to_string()
            };
            let path = rel_candidate
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string();
            if path == "." || path.is_empty() {
                if !trimmed.is_empty() && trimmed.chars().all(|c| c == '.' || c == '/') {
                    return Vec::new();
                }
                continue;
            }
            if !Path::new(&path).is_absolute()
                && !Path::new(&path)
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                normalized.push(path);
            }
        }
        normalized
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

fn drain_bounded_interruptible(
    mut reader: impl Read + std::os::unix::io::AsRawFd,
    max_output_bytes: usize,
    stop_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Vec<u8> {
    let fd = reader.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut kept = Vec::with_capacity(max_output_bytes.min(64 * 1024));
    let mut chunk = [0_u8; 64 * 1024];
    let mut stop_deadline: Option<Instant> = None;
    loop {
        if stop_flag.load(Ordering::Acquire) {
            let deadline = *stop_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
            if Instant::now() >= deadline {
                break;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_ms = if stop_deadline.is_some() { 25 } else { 50 };
        let rc = unsafe { libc::poll(&mut pfd, 1, poll_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        let mut hit_eof = false;
        loop {
            if stop_flag.load(Ordering::Acquire) {
                let deadline = *stop_deadline
                    .get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                if Instant::now() >= deadline {
                    hit_eof = true;
                    break;
                }
            }
            match reader.read(&mut chunk) {
                Ok(0) => {
                    hit_eof = true;
                    break;
                }
                Ok(read) => {
                    if kept.len() < max_output_bytes {
                        let remaining = max_output_bytes - kept.len();
                        kept.extend_from_slice(&chunk[..read.min(remaining)]);
                    }
                    if stop_flag.load(Ordering::Acquire) {
                        let deadline = *stop_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                        if Instant::now() >= deadline {
                            hit_eof = true;
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    hit_eof = true;
                    break;
                }
            }
        }
        if hit_eof || (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0) {
            break;
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
    if command.get_envs().all(|(k, _)| k != "PATH")
        && let Some(aug_path) = crate::catalog::augmented_user_path()
    {
        command.env("PATH", aug_path);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Some(active) = active_pgid
        && let Ok(pgid) = i32::try_from(pid)
    {
        active.store(pgid, Ordering::Release);
    }
    let mut already_cancelled = false;
    if let Some(active) = active_pgids
        && let Ok(pgid) = i32::try_from(pid)
    {
        let mut guard = active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.contains(&i32::MIN) {
            already_cancelled = true;
        } else {
            guard.insert(pgid);
        }
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
    if already_cancelled
        && let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let stop_draining = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_stdout = std::sync::Arc::clone(&stop_draining);
    let stop_stderr = std::sync::Arc::clone(&stop_draining);
    let stdout_handle =
        std::thread::spawn(move || drain_bounded_interruptible(stdout_pipe, max_output_bytes, stop_stdout));
    let stderr_handle =
        std::thread::spawn(move || drain_bounded_interruptible(stderr_pipe, max_output_bytes, stop_stderr));

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
    // Ensure any orphaned descendant in the child's process group that inherited stdout/stderr is terminated,
    // and bound pipe draining so even a setsid() grandchild keeping stdout/stderr open cannot hang join().
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGKILL);
        }
    }
    stop_draining.store(true, Ordering::Release);

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

    #[test]
    fn root_gitignore_excludes_mutable_runtime_dirs_and_honors_negation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "workspaces/\ndata/eval/*\n!data/eval/golden_fixture.json\ndata/autonomy/x_social_state.json\n",
        )
        .unwrap();
        let workspace = Workspace::new(root.path(), Vec::new(), Vec::new()).unwrap();
        assert!(!workspace.is_permitted_rel(Path::new("workspaces/wt1/tests/test_a.py")));
        assert!(!workspace.is_permitted_data_rel(Path::new("data/eval/live_state.json")));
        assert!(!workspace.is_permitted_data_rel(Path::new("data/autonomy/x_social_state.json")));
        assert!(workspace.is_permitted_data_rel(Path::new("data/eval/golden_fixture.json")));
        assert!(workspace.is_permitted_data_rel(Path::new("config/schema.json")));
        assert!(!workspace.is_gitignored_entry(Path::new("workspaces"), false));
        assert!(workspace.is_gitignored_entry(Path::new("workspaces"), true));
        assert!(workspace.is_gitignored_entry(Path::new("workspaces/wt1/a.py"), false));

        // Verify deep negation (`/*` + `!/src/nested/keep.py`) is not prematurely pruned at `src/`.
        let root2 = tempfile::tempdir().unwrap();
        std::fs::write(
            root2.path().join(".gitignore"),
            "/*\n!/src/nested/keep.py\n",
        )
        .unwrap();
        std::fs::create_dir_all(root2.path().join("src/nested")).unwrap();
        std::fs::write(root2.path().join("src/nested/keep.py"), "def ok(): pass\n").unwrap();
        std::fs::write(root2.path().join("src/ignored.py"), "def no(): pass\n").unwrap();
        let ws2 = Workspace::new(root2.path(), Vec::new(), Vec::new()).unwrap();
        let inv2 = ws2.files_limited(&MemoryPolicy::default()).unwrap();
        assert_eq!(inv2.files, vec![PathBuf::from("src/nested/keep.py")]);
    }

    #[cfg(unix)]
    #[test]
    fn read_known_with_limit_rejects_intermediate_symlink_escape() {
        let ws_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.py"), "SECRET = 1\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws_dir.path().join("linked")).unwrap();
        std::fs::write(ws_dir.path().join("ok.py"), "OK = 1\n").unwrap();

        let ws = Workspace::new(ws_dir.path(), Vec::new(), Vec::new()).unwrap();
        assert!(ws.read_known_with_limit(Path::new("ok.py"), 1024).is_ok());
        assert!(
            ws.read_known_with_limit(Path::new("linked/secret.py"), 1024)
                .is_err()
        );
    }
}
