use super::*;

pub fn effective_goenv_path() -> Option<std::path::PathBuf> {
    match std::env::var_os("GOENV") {
        Some(v) if v == "off" => None,
        Some(v) if !v.is_empty() => Some(std::path::PathBuf::from(v)),
        _ => std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(|xdg| std::path::PathBuf::from(xdg).join("go").join("env"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|home| {
                        std::path::PathBuf::from(home)
                            .join(".config")
                            .join("go")
                            .join("env")
                    })
            }),
    }
}

pub fn read_effective_goenv_var(key: &str) -> Option<String> {
    read_effective_goenv_lines()
        .into_iter()
        .find_map(|(k, v)| (k == key).then_some(v))
}

pub(super) fn read_effective_goenv_lines() -> Vec<(String, String)> {
    let Some(p) = effective_goenv_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (k, v) = trimmed.split_once('=')?;
            let clean_v = v.trim().trim_matches(['"', '\'']).to_string();
            Some((k.trim().to_string(), clean_v))
        })
        .collect()
}

pub(super) fn go_flags_have_overlay() -> bool {
    let check_val = |v: &str| {
        v.split_whitespace().any(|t| {
            t == "-overlay"
                || t.starts_with("-overlay=")
                || t == "-modfile"
                || t.starts_with("-modfile=")
        })
    };
    if let Ok(val) = std::env::var("GOFLAGS") {
        return check_val(&val);
    }
    read_effective_goenv_lines()
        .iter()
        .any(|(k, v)| k == "GOFLAGS" && check_val(v))
}

/// Returns true when `CGO_CFLAGS`/`CGO_CPPFLAGS`/`CGO_CXXFLAGS`/`CGO_FFLAGS` (from process env
/// OR persisted `go env -w` file) injects header search paths or force-included header files
/// (`-I`, `-include`, `-imacros`, `-isystem`, `-iquote`, `--sysroot`, `-isysroot`, `@response_file`)
/// whose contents are not tracked in AST.
pub(super) fn cgo_env_flags_have_untracked_headers() -> bool {
    let flag_has_header = |val: &str| {
        val.split_whitespace().any(|tok| {
            tok.starts_with("-I")
                || tok.starts_with("-include")
                || tok.starts_with("-imacros")
                || tok.starts_with("-isystem")
                || tok.starts_with("-iquote")
                || tok.starts_with("--sysroot")
                || tok.starts_with("-isysroot")
                || tok.starts_with('@')
        })
    };
    let ldflag_has_external_input = |val: &str| {
        val.split_whitespace().any(|tok| {
            tok.starts_with("-L")
                || tok.starts_with("-l")
                || tok.starts_with("-Wl,")
                || tok.starts_with("-T")
                || tok.starts_with("--script")
                || tok == "-Xlinker"
                || tok == "-framework"
                || tok.starts_with("--sysroot")
                || tok.starts_with("-isysroot")
                || tok.starts_with('@')
                || tok.ends_with(".a")
                || tok.ends_with(".so")
                || tok.ends_with(".dylib")
                || tok.ends_with(".o")
        })
    };
    let goenv_lines = read_effective_goenv_lines();
    for var in ["CGO_CFLAGS", "CGO_CPPFLAGS", "CGO_CXXFLAGS", "CGO_FFLAGS"] {
        if let Ok(val) = std::env::var(var) {
            if flag_has_header(&val) {
                return true;
            }
        } else if goenv_lines
            .iter()
            .any(|(k, v)| k == var && flag_has_header(v))
        {
            return true;
        }
    }
    if let Ok(val) = std::env::var("CGO_LDFLAGS") {
        if ldflag_has_external_input(&val) {
            return true;
        }
    } else if goenv_lines
        .iter()
        .any(|(k, v)| k == "CGO_LDFLAGS" && ldflag_has_external_input(v))
    {
        return true;
    }
    false
}


#[derive(Default)]
pub(super) struct PolyglotIndex {
    pub(super) go_modules: Vec<(String, String)>,
    /// Verified module-scoped `go.mod`/`go.work` `replace` mappings keyed by owning module dir.
    pub(super) go_replaces_by_mod: BTreeMap<String, Vec<(String, String)>>,
    pub(super) go_files_by_dir: BTreeMap<String, Vec<String>>,
    pub(super) cgo_native_by_dir: BTreeMap<String, Vec<String>>,
    /// Package-wide `#cgo CFLAGS: -I...` include search paths indexed by Go package directory,
    /// plus a flat set of all resolved workspace include search dirs for nested C/C++ headers.
    pub(super) cgo_inc_dirs_by_pkg: BTreeMap<String, BTreeSet<String>>,
    pub(super) all_cgo_inc_dirs: Vec<String>,
    pub(super) resolved_abs_c_includes: BTreeMap<String, String>,
    pub(super) all_paths: Vec<String>,
}

/// Strips trailing `//` comments outside Go quoted/backtick string literals in `go.mod`/`go.work`.
pub(super) fn strip_go_mod_line_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        if let Some(q) = quote {
            if q == b'"' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'`' {
            quote = Some(bytes[i]);
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return &raw[..i];
        }
        i += 1;
    }
    raw
}

/// Tokenizes a `go.mod` / `go.work` line preserving `"..."` (with Go escapes) and `` `...` ``
/// tokens intact (e.g. `replace example.com/dep => "./my dep"`).
pub(super) fn tokenize_go_mod_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < len {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let ch = line[i..].chars().next().unwrap_or(' ');
                    i += ch.len_utf8();
                    continue;
                }
                let ch = line[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            out.push(decode_go_double_quoted(&line[start..i]));
            if i < len {
                i += 1;
            }
        } else if bytes[i] == b'`' {
            i += 1;
            let start = i;
            while i < len && bytes[i] != b'`' {
                i += 1;
            }
            out.push(line[start..i].to_string());
            if i < len {
                i += 1;
            }
        } else {
            let start = i;
            while i < len && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(line[start..i].to_string());
        }
    }
    out
}

/// Resolves a local `go.mod` / `go.work` target directory relative to `base_dir` (or absolute),
/// canonicalizes it, and verifies BOTH that it stays inside `workspace_root` AND that no path
/// component beneath `workspace_root` is a symlink. Returns the normalized workspace-relative
/// directory (or `""` for workspace root) if safe and watchable, or `None` to fail closed.
pub(super) fn resolve_go_local_dir_no_symlink(
    workspace_root: &Path,
    base_dir: &Path,
    raw_target: &str,
) -> Option<String> {
    let joined = if Path::new(raw_target).is_absolute() {
        std::path::PathBuf::from(raw_target)
    } else {
        base_dir.join(raw_target)
    };
    // Reject if ANY intermediate OR final lexical component along `raw_target` is a symlink
    // (e.g. `./link/sub` where `link` is a symlink pointing back into the workspace).
    let mut lex_probe = if Path::new(raw_target).is_absolute() {
        std::path::PathBuf::new()
    } else {
        base_dir.to_path_buf()
    };
    for comp in Path::new(raw_target).components() {
        match comp {
            std::path::Component::CurDir => continue,
            other => {
                lex_probe.push(other);
                if matches!(other, std::path::Component::Normal(_))
                    && let Ok(meta) = std::fs::symlink_metadata(&lex_probe)
                    && meta.file_type().is_symlink()
                {
                    return None;
                }
            }
        }
    }
    let canon_root = std::fs::canonicalize(workspace_root).ok()?;
    let canon_target = std::fs::canonicalize(&joined).ok()?;
    if !canon_target.is_dir() || !canon_target.starts_with(&canon_root) {
        return None;
    }
    let rel = canon_target.strip_prefix(&canon_root).ok()?;
    let rel_str = normalize_rel_str(rel);
    // Reject if ANY component from `workspace_root` down to the canonical target is a symlink.
    let mut probe = workspace_root.to_path_buf();
    for comp in rel.components() {
        probe.push(comp);
        let meta = std::fs::symlink_metadata(&probe).ok()?;
        if meta.file_type().is_symlink() {
            return None;
        }
    }
    Some(rel_str)
}

pub(crate) fn is_cgo_native_source_path(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some(
            "c" | "cc"
                | "cpp"
                | "cxx"
                | "h"
                | "hh"
                | "hpp"
                | "hxx"
                | "s"
                | "S"
                | "sx"
                | "m"
                | "mm"
                | "f"
                | "F"
                | "for"
                | "f90"
                | "f95"
                | "syso"
        )
    )
}

/// Decodes ALL Go double-quoted string escape sequences (`\a`, `\b`, `\f`, `\n`, `\r`, `\t`,
/// `\v`, `\\`, `\"`, `\xHH`, `\OOO`, `\uHHHH`, `\UHHHHHHHH`) into exact bytes. Returns `Err(())`
/// if raw byte escapes produce invalid UTF-8 (so callers like env-key extraction can fail closed).
pub(super) fn decode_go_double_quoted_checked(raw: &str) -> std::result::Result<String, ()> {
    if !raw.contains('\\') {
        return Ok(raw.to_string());
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            match bytes[i + 1] {
                b'x' if i + 3 < len => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4])
                        && let Ok(byte) = u8::from_str_radix(hex, 16)
                    {
                        out.push(byte);
                        i += 4;
                        continue;
                    }
                }
                b'u' if i + 5 < len => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 6])
                        && let Ok(code) = u32::from_str_radix(hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        i += 6;
                        continue;
                    }
                }
                b'U' if i + 9 < len => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 10])
                        && let Ok(code) = u32::from_str_radix(hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        i += 10;
                        continue;
                    }
                }
                b'0'..=b'7' if i + 3 < len => {
                    if let Ok(oct) = std::str::from_utf8(&bytes[i + 1..i + 4])
                        && let Ok(code) = u32::from_str_radix(oct, 8)
                        && code <= 0xFF
                    {
                        out.push(code as u8);
                        i += 4;
                        continue;
                    }
                }
                b'a' => {
                    out.push(0x07);
                    i += 2;
                    continue;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                    continue;
                }
                b'f' => {
                    out.push(0x0C);
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                    continue;
                }
                b'v' => {
                    out.push(0x0B);
                    i += 2;
                    continue;
                }
                b'\\' | b'"' => {
                    out.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| ())
}

pub(super) fn decode_go_double_quoted(raw: &str) -> String {
    decode_go_double_quoted_checked(raw).unwrap_or_else(|_| raw.to_string())
}

pub(super) struct CompiledGoEmbed {
    pub(super) include_all: bool,
    pub(super) trimmed_pat: String,
    pub(super) has_glob: bool,
    pub(super) file_matcher: Option<globset::GlobMatcher>,
    pub(super) dir_prefix_matcher: Option<globset::GlobMatcher>,
    pub(super) seg_matchers: Vec<Option<globset::GlobMatcher>>,
    pub(super) pat_segments: Vec<String>,
}

/// Escapes `{` and `}` outside `[...]` character classes so `globset` follows Go's `path.Match`
/// semantics (where braces are literal characters, not brace-alternation syntax).
pub(super) fn escape_go_embed_glob_braces(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len() + 8);
    let mut in_class = false;
    let mut prev_bs = false;
    for ch in pat.chars() {
        if prev_bs {
            out.push(ch);
            prev_bs = false;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            prev_bs = true;
            continue;
        }
        if ch == '[' && !in_class {
            in_class = true;
            out.push(ch);
        } else if ch == ']' && in_class {
            in_class = false;
            out.push(ch);
        } else if !in_class && (ch == '{' || ch == '}') {
            out.push('[');
            out.push(ch);
            out.push(']');
        } else {
            out.push(ch);
        }
    }
    out
}

impl CompiledGoEmbed {
    pub(super) fn new(pattern: &str) -> Option<Self> {
        let (include_all, raw_pat) = if let Some(rest) = pattern.strip_prefix("all:") {
            (true, rest)
        } else {
            (false, pattern)
        };
        let trimmed_pat = raw_pat.trim_end_matches('/').to_string();
        if trimmed_pat.is_empty() {
            return None;
        }
        let has_glob = trimmed_pat.contains(['*', '?', '[', '\\']);
        let escaped_full = escape_go_embed_glob_braces(&trimmed_pat);
        let file_matcher = if has_glob {
            globset::GlobBuilder::new(&escaped_full)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        } else {
            None
        };
        let dir_prefix_matcher = if has_glob {
            globset::GlobBuilder::new(&format!("{escaped_full}/**"))
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        } else {
            None
        };
        let pat_segments: Vec<String> = trimmed_pat.split('/').map(ToOwned::to_owned).collect();
        let seg_matchers: Vec<Option<globset::GlobMatcher>> = pat_segments
            .iter()
            .map(|seg| {
                if seg.contains(['*', '?', '[', '\\']) {
                    let escaped_seg = escape_go_embed_glob_braces(seg);
                    globset::GlobBuilder::new(&escaped_seg)
                        .literal_separator(true)
                        .backslash_escape(true)
                        .build()
                        .ok()
                        .map(|g| g.compile_matcher())
                } else {
                    None
                }
            })
            .collect();
        Some(Self {
            include_all,
            trimmed_pat,
            has_glob,
            file_matcher,
            dir_prefix_matcher,
            seg_matchers,
            pat_segments,
        })
    }

    pub(super) fn explicitly_targets_dir(&self, dir_name: &str) -> bool {
        self.pat_segments.iter().any(|s| s == dir_name)
    }

    pub(super) fn has_excluded_descendant_segment(&self, rel_under: &str) -> bool {
        if self.include_all {
            return false;
        }
        // Per Go's `embed` spec, descending into a matched directory skips any file or
        // subdirectory whose name begins with `.` or `_` unless the `all:` prefix is used.
        rel_under
            .split('/')
            .skip(self.pat_segments.len())
            .any(|seg| seg.starts_with('.') || seg.starts_with('_'))
    }

    pub(super) fn matches_rel(&self, rel_under: &str) -> bool {
        let matched = if !self.has_glob {
            rel_under == self.trimmed_pat
                || (rel_under.starts_with(&self.trimmed_pat)
                    && rel_under.as_bytes().get(self.trimmed_pat.len()) == Some(&b'/'))
        } else {
            self.file_matcher
                .as_ref()
                .is_some_and(|m| m.is_match(rel_under))
                || self
                    .dir_prefix_matcher
                    .as_ref()
                    .is_some_and(|m| m.is_match(rel_under))
        };
        matched && !self.has_excluded_descendant_segment(rel_under)
    }

    pub(super) fn dir_could_match(&self, dir_rel: &str) -> bool {
        if self.has_excluded_descendant_segment(dir_rel) {
            return false;
        }
        if self.matches_rel(dir_rel) {
            return true;
        }
        let dir_parts: Vec<&str> = dir_rel.split('/').collect();
        for (idx, dp) in dir_parts.iter().enumerate() {
            let Some(pat_seg) = self.pat_segments.get(idx) else {
                return false;
            };
            // Recursive globstar (`**`, e.g. `.hidden/**/tests`) can match arbitrary nesting
            // depths below this directory prefix, so always descend into matching prefixes.
            if pat_seg == "**" {
                return true;
            }
            if let Some(ref m) = self.seg_matchers[idx] {
                if !m.is_match(dp) {
                    return false;
                }
            } else if *dp != *pat_seg {
                return false;
            }
        }
        true
    }
}

pub(super) fn go_embed_pattern_matches_rel(pattern: &str, rel_under: &str) -> bool {
    CompiledGoEmbed::new(pattern).is_some_and(|c| c.matches_rel(rel_under))
}

pub(super) fn go_embed_dir_could_match(pattern: &str, dir_rel: &str) -> bool {
    CompiledGoEmbed::new(pattern).is_some_and(|c| c.dir_could_match(dir_rel))
}

pub(super) fn collect_python_raw_fs_files(files: &BTreeMap<String, FileNode>) -> BTreeSet<String> {
    let mut embedded = BTreeSet::new();
    let insert_file_or_dir = |cand: &str, out: &mut BTreeSet<String>| {
        if files.contains_key(cand) {
            out.insert(cand.to_string());
        }
        let dir_pfx = format!("{cand}/");
        for (k, _) in files
            .range(dir_pfx.clone()..)
            .take_while(|(k, _)| k.starts_with(&dir_pfx))
        {
            out.insert(k.clone());
        }
    };
    for (path, node) in files {
        if !path.ends_with(".py") {
            continue;
        }
        let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
        for sym in &node.parsed.defined_symbols {
            if let Some(fs_ref) = sym.strip_prefix("py_raw_fs:") {
                if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                    insert_file_or_dir(&norm_root, &mut embedded);
                }
                if !parent.as_os_str().is_empty()
                    && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                {
                    insert_file_or_dir(&norm_parent, &mut embedded);
                }
            }
        }
    }
    embedded
}

impl PolyglotIndex {
    pub(super) fn collect_all_go_embed_files(&self, files: &BTreeMap<String, FileNode>) -> BTreeSet<String> {
        let mut embedded = BTreeSet::new();
        for (path, node) in files {
            if path.ends_with(".go") {
                let parent_str = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                for sym in &node.parsed.defined_symbols {
                    if let Some(embed_pat) = sym.strip_prefix("go_embed:") {
                        self.expand_go_embed(&parent_str, embed_pat, &mut embedded);
                    }
                }
            } else if path.ends_with(".rs") {
                let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
                let crate_dirs = owning_rust_crate_dirs(path, files);
                let insert_file_or_dir = |cand: &str, out: &mut BTreeSet<String>| {
                    if files.contains_key(cand) {
                        out.insert(cand.to_string());
                    }
                    let dir_pfx = format!("{cand}/");
                    for (k, _) in files
                        .range(dir_pfx.clone()..)
                        .take_while(|(k, _)| k.starts_with(&dir_pfx))
                    {
                        out.insert(k.clone());
                    }
                };
                for sym in &node.parsed.defined_symbols {
                    if let Some(rel_ref) = sym.strip_prefix("rust_raw_embed:") {
                        if let Some(norm) = normalize_lexical_rel(&parent.join(rel_ref))
                            && files.contains_key(&norm)
                        {
                            embedded.insert(norm);
                        }
                        if files.contains_key(rel_ref) {
                            embedded.insert(rel_ref.to_string());
                        }
                    } else if let Some(crate_ref) = sym.strip_prefix("rust_raw_embed_crate:") {
                        for crate_dir in &crate_dirs {
                            let base = if crate_dir.is_empty() {
                                Path::new(crate_ref).to_path_buf()
                            } else {
                                Path::new(crate_dir).join(crate_ref)
                            };
                            if let Some(norm) = normalize_lexical_rel(&base)
                                && files.contains_key(&norm)
                            {
                                embedded.insert(norm);
                            }
                        }
                    } else if let Some(fs_ref) = sym.strip_prefix("rust_raw_fs:") {
                        // Runtime Rust filesystem reads resolve relative to `CARGO_MANIFEST_DIR`
                        // (each owning crate dir), workspace root, or source parent; raw-hash EVERY
                        // viable file and directory descendant candidate without short-circuiting.
                        if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                            insert_file_or_dir(&norm_root, &mut embedded);
                        }
                        if !parent.as_os_str().is_empty()
                            && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                        {
                            insert_file_or_dir(&norm_parent, &mut embedded);
                        }
                        for crate_dir in &crate_dirs {
                            let base = if crate_dir.is_empty() {
                                Path::new(fs_ref).to_path_buf()
                            } else {
                                Path::new(crate_dir).join(fs_ref)
                            };
                            if let Some(norm_crate) = normalize_lexical_rel(&base) {
                                insert_file_or_dir(&norm_crate, &mut embedded);
                            }
                        }
                    }
                }
            } else if path.ends_with(".py") {
                let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
                let insert_file_or_dir = |cand: &str, out: &mut BTreeSet<String>| {
                    if files.contains_key(cand) {
                        out.insert(cand.to_string());
                    }
                    let dir_pfx = format!("{cand}/");
                    for (k, _) in files
                        .range(dir_pfx.clone()..)
                        .take_while(|(k, _)| k.starts_with(&dir_pfx))
                    {
                        out.insert(k.clone());
                    }
                };
                for sym in &node.parsed.defined_symbols {
                    if let Some(fs_ref) = sym.strip_prefix("py_raw_fs:") {
                        if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                            insert_file_or_dir(&norm_root, &mut embedded);
                        }
                        if !parent.as_os_str().is_empty()
                            && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                        {
                            insert_file_or_dir(&norm_parent, &mut embedded);
                        }
                    }
                }
            }
        }
        embedded
    }

    pub(super) fn build(
        files: &BTreeMap<String, FileNode>,
        resolved_cgo_inc_by_pkg: &BTreeMap<String, BTreeSet<String>>,
        resolved_abs_c_includes: &BTreeMap<String, String>,
        resolved_go_replaces_by_mod: &BTreeMap<String, Vec<(String, String)>>,
    ) -> Self {
        let mut go_modules: Vec<(String, String)> = Vec::new();
        let mut go_replaces_by_mod: BTreeMap<String, Vec<(String, String)>> =
            resolved_go_replaces_by_mod.clone();
        for list in go_replaces_by_mod.values_mut() {
            list.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        }
        let mut go_files_by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut cgo_native_by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut cgo_inc_dirs_by_pkg: BTreeMap<String, BTreeSet<String>> =
            resolved_cgo_inc_by_pkg.clone();
        let mut all_cgo_inc_set: BTreeSet<String> = resolved_cgo_inc_by_pkg
            .values()
            .flatten()
            .cloned()
            .collect();
        let mut all_paths = Vec::with_capacity(files.len());
        for (path, node) in files {
            all_paths.push(path.clone());
            if path == "go.mod" || path.ends_with("/go.mod") {
                let cfg_dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                for sym in &node.parsed.defined_symbols {
                    if let Some(mod_name) = sym.strip_prefix("go_mod:") {
                        go_modules.push((mod_name.to_string(), cfg_dir.clone()));
                    }
                }
            } else if path.ends_with(".go") {
                let dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                go_files_by_dir.entry(dir.clone()).or_default().push(path.clone());
                let mut has_cgo = node.parsed.imports.iter().any(|imp| imp.module == "C");
                for sym in &node.parsed.defined_symbols {
                    if let Some(inc_dir) = sym.strip_prefix("cgo_inc:") {
                        has_cgo = true;
                        cgo_inc_dirs_by_pkg
                            .entry(dir.clone())
                            .or_default()
                            .insert(inc_dir.to_string());
                        if let Some(resolved) = normalize_lexical_rel(&Path::new(&dir).join(inc_dir)) {
                            all_cgo_inc_set.insert(resolved);
                        }
                        if let Some(resolved) = normalize_lexical_rel(Path::new(inc_dir)) {
                            all_cgo_inc_set.insert(resolved);
                        }
                    } else if sym.starts_with("c_include:")
                        || sym.starts_with("c_include_sys:")
                        || sym.starts_with("c_include_opt:")
                    {
                        has_cgo = true;
                    }
                }
                // Retain cgo's implicit `-I <pkg_dir>` directory so nested headers like
                // `pkg/sub/outer.h` including `shared/feature.h` resolve `pkg/shared/feature.h`.
                if has_cgo {
                    cgo_inc_dirs_by_pkg
                        .entry(dir.clone())
                        .or_default()
                        .insert(dir.clone());
                    all_cgo_inc_set.insert(dir);
                }
            } else if is_cgo_native_source_path(path) {
                let dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                cgo_native_by_dir.entry(dir).or_default().push(path.clone());
            }
        }
        go_modules.sort_by_key(|(m, _)| std::cmp::Reverse(m.len()));
        Self {
            go_modules,
            go_replaces_by_mod,
            go_files_by_dir,
            cgo_native_by_dir,
            cgo_inc_dirs_by_pkg,
            all_cgo_inc_dirs: all_cgo_inc_set.into_iter().collect(),
            resolved_abs_c_includes: resolved_abs_c_includes.clone(),
            all_paths,
        }
    }

    pub(super) fn expand_go_embed(&self, parent_str: &str, pattern: &str, deps: &mut BTreeSet<String>) {
        let prefix = if parent_str.is_empty() {
            String::new()
        } else {
            format!("{parent_str}/")
        };
        let start_idx = self.all_paths.partition_point(|p| p.as_str() < prefix.as_str());
        // Reuse the exact same `CompiledGoEmbed` matcher (with Go-compatible literal braces)
        // for BOTH discovery and dependency expansion so edges never diverge from discovery.
        let Some(compiled) = CompiledGoEmbed::new(pattern) else {
            return;
        };
        for cand in &self.all_paths[start_idx..] {
            if !prefix.is_empty() && !cand.starts_with(&prefix) {
                break;
            }
            let rel = if prefix.is_empty() {
                cand.as_str()
            } else {
                &cand[prefix.len()..]
            };
            if compiled.matches_rel(rel) {
                deps.insert(cand.clone());
            }
        }
    }
}

pub(super) fn resolve_polyglot_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
    polyglot_index: &PolyglotIndex,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    let parent_str = normalize_rel_str(parent);

    // 1. Go: `_test.go` files implicitly compile with ALL sibling `.go` files (including
    // `helper_test.go`) and sibling cgo C/C++/header/asm files in the same package directory;
    // local `go.mod` imports and `//go:embed` globs/directories resolve via prebuilt O(log N) indexes.
    if path.ends_with(".go") {
        if path.ends_with("_test.go")
            && let Some(siblings) = polyglot_index.go_files_by_dir.get(&parent_str)
        {
            for candidate in siblings {
                if candidate != path {
                    deps.insert(candidate.clone());
                }
            }
        }
        if let Some(native_siblings) = polyglot_index.cgo_native_by_dir.get(&parent_str) {
            for candidate in native_siblings {
                deps.insert(candidate.clone());
            }
        }
        for sym in &node.parsed.defined_symbols {
            if let Some(embed_pat) = sym.strip_prefix("go_embed:") {
                polyglot_index.expand_go_embed(&parent_str, embed_pat, &mut deps);
            }
        }
        // Use PACKAGE-WIDE `#cgo CFLAGS: -I...` dirs across ALL `.go` files in `parent_str`.
        let pkg_cgo_inc_dirs = polyglot_index.cgo_inc_dirs_by_pkg.get(&parent_str);
        for spec in &node.parsed.imports {
            // Resolve cgo preamble `#include "config.h"` / `#include <config.h>` / absolute
            // workspace `#include "/repo/shared/config.h"` against both the package directory AND
            // all package-wide `#cgo CFLAGS: -I...` search paths.
            let mut hdr_candidates: Vec<std::path::PathBuf> = vec![parent.join(&spec.module)];
            if let Some(mapped_abs) = polyglot_index.resolved_abs_c_includes.get(&spec.module) {
                hdr_candidates.push(Path::new(mapped_abs).to_path_buf());
            }
            if let Some(inc_dirs) = pkg_cgo_inc_dirs {
                for inc_dir in inc_dirs {
                    hdr_candidates.push(parent.join(inc_dir).join(&spec.module));
                    hdr_candidates.push(Path::new(inc_dir).join(&spec.module));
                }
            }
            let mut matched_cgo_hdr = false;
            for cand in hdr_candidates {
                if let Some(norm_hdr) = normalize_lexical_rel(&cand)
                    && files.contains_key(&norm_hdr)
                    && norm_hdr != path
                {
                    deps.insert(norm_hdr.clone());
                    for hdr_ext in [".hpp", ".hxx", ".hh", ".h"] {
                        if let Some(stem) = norm_hdr.strip_suffix(hdr_ext) {
                            for impl_ext in [".cpp", ".cc", ".cxx", ".c"] {
                                let impl_cand = format!("{stem}{impl_ext}");
                                if files.contains_key(&impl_cand) && impl_cand != path {
                                    deps.insert(impl_cand);
                                }
                            }
                        }
                    }
                    matched_cgo_hdr = true;
                }
            }
            if matched_cgo_hdr {
                continue;
            }
            // Resolve module-scoped `replace` rules (nearest enclosing `go.mod` / `go.work`
            // directories, deepest first) BEFORE global `go_modules` so unrelated modules never
            // shadow each other's replacements.
            let mut ancestor_dirs: Vec<String> = Vec::new();
            let mut cur_dir = parent_str.as_str();
            loop {
                ancestor_dirs.push(cur_dir.to_string());
                if cur_dir.is_empty() {
                    break;
                }
                cur_dir = cur_dir.rsplit_once('/').map_or("", |(p, _)| p);
            }
            // Resolve Go workspace `use` modules and `replace` rules honoring `GOWORK`:
            // - `GOWORK=off`: ignore ALL `go.work` entries (only `go.mod` applies).
            // - Explicit `GOWORK=<path>`: use ONLY `__gowork_explicit__:` / `__gowork_explicit_use__`.
            // - Unset/`GOWORK=auto`: union enclosing `go.work` scopes, sibling main workspace
            //   module `go.mod` replacements (Go applies replacements from every main module in an
            //   active `go.work`!), AND active `go.work use` modules alongside replacement targets
            //   so a `go.mod` replacement never suppresses an active workspace module!
            let gowork_env_val = effective_gowork_value();
            let gowork_is_off = gowork_env_val.as_deref().map(str::trim) == Some("off");
            let gowork_is_explicit = gowork_env_val
                .as_deref()
                .map(str::trim)
                .is_some_and(|v| !v.is_empty() && v != "auto" && v != "off");
            let active_gowork_dirs: BTreeSet<&str> = if gowork_is_off {
                BTreeSet::new()
            } else {
                let want_use_key = if gowork_is_explicit {
                    "__gowork_explicit_use__"
                } else {
                    "__gowork_use__"
                };
                let scope_ancs: Vec<&str> = if gowork_is_explicit {
                    vec![""]
                } else {
                    ancestor_dirs.iter().map(String::as_str).collect()
                };
                let mut uses = BTreeSet::new();
                for anc in scope_ancs {
                    if let Some(entries) = polyglot_index.go_replaces_by_mod.get(anc) {
                        for (k, d) in entries {
                            if k == want_use_key {
                                uses.insert(d.as_str());
                            }
                        }
                    }
                }
                uses
            };
            let mut matched_go_target_dirs: Vec<String> = Vec::new();
            'replace_search: for want_go_work in [true, false] {
                if want_go_work && gowork_is_off {
                    continue;
                }
                let mut scope_ancs: Vec<&str> = if want_go_work && gowork_is_explicit {
                    vec![""]
                } else {
                    ancestor_dirs.iter().map(String::as_str).collect()
                };
                // Go applies `replace` directives from EVERY main module in the active `go.work`
                // workspace, not just ancestors of the importing file!
                if !want_go_work && !active_gowork_dirs.is_empty() {
                    for ws_mod_dir in &active_gowork_dirs {
                        if !scope_ancs.contains(ws_mod_dir) {
                            scope_ancs.push(ws_mod_dir);
                        }
                    }
                }
                for anc in scope_ancs {
                    if let Some(replaces) = polyglot_index.go_replaces_by_mod.get(anc) {
                        for (raw_old_mod, repl_dir) in replaces {
                            if raw_old_mod == "__gowork_use__"
                                || raw_old_mod == "__gowork_explicit_use__"
                            {
                                continue;
                            }
                            let (is_gw, old_mod) = if let Some(stripped) =
                                raw_old_mod.strip_prefix("__gowork_explicit__:")
                            {
                                if !gowork_is_explicit {
                                    continue;
                                }
                                (true, stripped)
                            } else if let Some(stripped) = raw_old_mod.strip_prefix("__gowork__:") {
                                if gowork_is_explicit {
                                    continue;
                                }
                                (true, stripped)
                            } else {
                                (false, raw_old_mod.as_str())
                            };
                            if is_gw != want_go_work {
                                continue;
                            }
                            if let Some(rest) = spec.module.strip_prefix(old_mod)
                                && (rest.is_empty() || rest.starts_with('/'))
                            {
                                let sub = rest.trim_start_matches('/');
                                let target_dir = if repl_dir.is_empty() {
                                    sub.to_string()
                                } else if sub.is_empty() {
                                    repl_dir.clone()
                                } else {
                                    format!("{repl_dir}/{sub}")
                                };
                                if !matched_go_target_dirs.contains(&target_dir) {
                                    matched_go_target_dirs.push(target_dir);
                                }
                            }
                        }
                    }
                }
                // Only short-circuit `go.mod` when an explicit `GOWORK` matched; under `auto`,
                // also check `go.mod` (and sibling workspace modules) so an inactive outer
                // `go.work` never masks a nested module's active replacement.
                if want_go_work && gowork_is_explicit && !matched_go_target_dirs.is_empty() {
                    break 'replace_search;
                }
            }
            // Always also include matching modules from `active_gowork_dirs` (`go.work use`) even
            // if a `go.mod` replacement matched, because Go prefers workspace main modules over
            // `go.mod` replacements! Fall back to non-workspace indexed modules only when nothing matched.
            if !active_gowork_dirs.is_empty() {
                for (mod_name, mod_dir) in &polyglot_index.go_modules {
                    if !active_gowork_dirs.contains(mod_dir.as_str()) {
                        continue;
                    }
                    if let Some(rest) = spec.module.strip_prefix(mod_name.as_str())
                        && (rest.is_empty() || rest.starts_with('/'))
                    {
                        let sub = rest.trim_start_matches('/');
                        let target_dir = if mod_dir.is_empty() {
                            sub.to_string()
                        } else if sub.is_empty() {
                            mod_dir.clone()
                        } else {
                            format!("{mod_dir}/{sub}")
                        };
                        if !matched_go_target_dirs.contains(&target_dir) {
                            matched_go_target_dirs.push(target_dir);
                        }
                    }
                }
            }
            if matched_go_target_dirs.is_empty() {
                for (mod_name, mod_dir) in &polyglot_index.go_modules {
                    if let Some(rest) = spec.module.strip_prefix(mod_name.as_str())
                        && (rest.is_empty() || rest.starts_with('/'))
                    {
                        let sub = rest.trim_start_matches('/');
                        let target_dir = if mod_dir.is_empty() {
                            sub.to_string()
                        } else if sub.is_empty() {
                            mod_dir.clone()
                        } else {
                            format!("{mod_dir}/{sub}")
                        };
                        if !matched_go_target_dirs.contains(&target_dir) {
                            matched_go_target_dirs.push(target_dir);
                        }
                    }
                }
            }
            for target_dir in matched_go_target_dirs {
                if let Some(pkg_go) = polyglot_index.go_files_by_dir.get(&target_dir) {
                    for candidate in pkg_go {
                        if candidate != path && !candidate.ends_with("_test.go") {
                            deps.insert(candidate.clone());
                        }
                    }
                }
                if let Some(pkg_native) = polyglot_index.cgo_native_by_dir.get(&target_dir) {
                    for candidate in pkg_native {
                        deps.insert(candidate.clone());
                    }
                }
            }
        }
        return deps;
    }

    // 2. Relative header/proto/graphql imports (`#include "foo.h"`, `import "common.proto"`, `#import "./frag.graphql"`)
    // Also inherit workspace/package `#cgo CFLAGS: -I...` search paths for nested C/C++ headers.
    for spec in &node.parsed.imports {
        let mut include_cands: Vec<std::path::PathBuf> = vec![parent.join(&spec.module)];
        if let Some(mapped_abs) = polyglot_index.resolved_abs_c_includes.get(&spec.module) {
            include_cands.push(Path::new(mapped_abs).to_path_buf());
        }
        if is_cgo_native_source_path(path)
            || node.parsed.defined_symbols.iter().any(|s| {
                s.starts_with("c_include:")
                    || s.starts_with("c_include_sys:")
                    || s.starts_with("c_include_opt:")
            })
        {
            if let Some(inc_dirs) = polyglot_index.cgo_inc_dirs_by_pkg.get(&parent_str) {
                for inc_dir in inc_dirs {
                    include_cands.push(parent.join(inc_dir).join(&spec.module));
                    include_cands.push(Path::new(inc_dir).join(&spec.module));
                }
            }
            for inc_dir in &polyglot_index.all_cgo_inc_dirs {
                include_cands.push(Path::new(inc_dir).join(&spec.module));
            }
        }
        for cand in include_cands {
            if let Some(norm_rel) = normalize_lexical_rel(&cand)
                && files.contains_key(&norm_rel)
                && norm_rel != path
            {
                deps.insert(norm_rel.clone());
                // For `.h` / `.hpp` includes, also link matching `.c` / `.cpp` / `.cc` implementation file
                for hdr_ext in [".hpp", ".hxx", ".hh", ".h"] {
                    if let Some(stem) = norm_rel.strip_suffix(hdr_ext) {
                        for impl_ext in [".cpp", ".cc", ".cxx", ".c"] {
                            let impl_cand = format!("{stem}{impl_ext}");
                            if files.contains_key(&impl_cand) && impl_cand != path {
                                deps.insert(impl_cand);
                            }
                        }
                    }
                }
            }
        }
        if let Some(norm_root) = normalize_lexical_rel(Path::new(&spec.module))
            && files.contains_key(&norm_root)
            && norm_root != path
        {
            deps.insert(norm_root);
        }
        // Java/Kotlin `import com.acme.Service` or C# `using Acme.Service` -> match defined class/symbol names
        if let Some(last_seg) = spec.module.rsplit('.').next()
            && !last_seg.is_empty()
            && last_seg != "*"
        {
            let lang_fam = file_language_family(path);
            for (cand_path, cand_node) in files {
                if cand_path != path
                    && file_language_family(cand_path) == lang_fam
                    && cand_node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s == last_seg)
                {
                    deps.insert(cand_path.clone());
                }
            }
        }
    }
    deps
}


