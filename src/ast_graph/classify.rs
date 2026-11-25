use super::*;

pub(super) fn node_has_provider_or_fixture(node: &FileNode) -> bool {
    node.is_config
        || !node.parsed.fixtures_defined.is_empty()
        || node.parsed.defined_symbols.iter().any(|s| {
            s.starts_with("api_route:")
                || s.starts_with("proto:")
                || s.starts_with("gql:")
                || s.starts_with("openapi:")
                || s.starts_with("pyo3:")
                || s.starts_with("wasm:")
                || s.starts_with("ctx_def:")
                || s.starts_with("css_class:")
                || s.starts_with("fe_extends:")
                || s.starts_with("fe_alias:")
                || s == "fe_has_paths"
                || s.starts_with("fe_vite_alias:")
                || s.starts_with("fe_vite_alias_cwd:")
                || s.starts_with("fe_baseurl:")
                || s.starts_with("reexport:")
                || s == "fe_has_side_effects"
                || s == "fe_mod_side_effects"
                || s.starts_with("cgo_inc:")
                || s.starts_with("go_embed:")
                || s.starts_with("fe_raw_import:")
                || s.starts_with("fe_raw_fs:")
                || s.starts_with("fe_raw_url_fs:")
                || s.starts_with("rust_raw_embed:")
                || s.starts_with("rust_raw_embed_crate:")
                || s.starts_with("rust_raw_fs:")
                || s.starts_with("py_raw_fs:")
                || s.starts_with("cargo_build_script:")
                || s == "fe_dynamic_import"
                || s == "fe_dynamic_env"
                || s == "py_dynamic_env"
                || s == "py_dynamic_import"
                || s == "py_dynamic_io"
                || s == "py_has_top_level_code"
                || s == "py_location_sensitive"
                || s == "rust_dynamic_io"
                || s == "rust_dynamic_env"
                || s == "rust_dynamic_include"
                || s == "go_dynamic_io"
                || s == "go_dynamic_env"
                || s == "cgo_dynamic_io"
                || s == "fortran_dynamic_mod"
                || s == "go_caller_sensitive"
                || s == "css_has_global"
        })
}

/// Splits a target string that may contain `::` in EITHER the literal filename/directory (e.g.
/// `tests/foo::bar.py`, `tests/a::b`, or `tests/foo::bar.py::test_a`) OR the node selector by
/// preferring the longest prefix present in `files` (as an exact file OR a directory prefix).
pub fn split_target_file_and_node_in_files(
    raw_target: &str,
    files: &BTreeMap<String, FileNode>,
) -> (String, bool) {
    let raw_trimmed = raw_target
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/');
    let norm_full = if cfg!(windows) {
        raw_trimmed.replace('\\', "/")
    } else {
        raw_trimmed.to_string()
    };
    if norm_full.is_empty() {
        return (norm_full, false);
    }
    let exists_as_file_or_dir = |cand: &str| -> bool {
        if files.contains_key(cand) {
            return true;
        }
        let dir_prefix = format!("{cand}/");
        files
            .range(dir_prefix.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&dir_prefix))
    };
    if exists_as_file_or_dir(&norm_full) {
        return (norm_full, false);
    }
    // Check `::` split boundaries from right to left (longest file/dir prefix first).
    let mut split_positions = Vec::new();
    let mut pos = 0usize;
    while let Some(rel_idx) = norm_full[pos..].find("::") {
        let abs_idx = pos + rel_idx;
        split_positions.push(abs_idx);
        pos = abs_idx + 2;
    }
    for &idx in split_positions.iter().rev() {
        let cand = norm_full[..idx].trim_end_matches('/').trim();
        if !cand.is_empty() && exists_as_file_or_dir(cand) {
            return (cand.to_string(), true);
        }
    }
    if let Some(&first_idx) = split_positions.first() {
        (
            norm_full[..first_idx].trim_end_matches('/').trim().to_string(),
            true,
        )
    } else {
        (norm_full, false)
    }
}

pub fn path_matches_testpaths(rel_path: &str, testpaths: &[String]) -> bool {
    if testpaths.is_empty() {
        return false;
    }
    testpaths.iter().any(|tp| {
        let trimmed = tp.trim().trim_start_matches("./").trim_end_matches('/');
        if trimmed.is_empty() {
            return false;
        }
        if trimmed.contains(['*', '?', '[']) {
            let file_m = globset::GlobBuilder::new(trimmed)
                .literal_separator(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
                .is_some_and(|m| m.is_match(rel_path));
            if file_m {
                return true;
            }
            return globset::GlobBuilder::new(&format!("{trimmed}/**"))
                .literal_separator(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
                .is_some_and(|m| m.is_match(rel_path));
        }
        rel_path == trimmed || rel_path.starts_with(&format!("{trimmed}/"))
    })
}

pub fn dir_could_match_testpaths(dir_rel: &str, testpaths: &[String]) -> bool {
    if testpaths.is_empty() || dir_rel.is_empty() {
        return false;
    }
    testpaths.iter().any(|tp| {
        let trimmed = tp.trim().trim_start_matches("./").trim_end_matches('/');
        if trimmed.is_empty() {
            return false;
        }
        go_embed_dir_could_match(trimmed, dir_rel)
    })
}

pub(super) fn is_standard_c_system_header(hdr: &str) -> bool {
    let norm = hdr.trim().replace('\\', "/");
    // Explicit curated whitelist of standard C, POSIX, Linux kernel ABI, macOS/BSD, and Win32
    // headers. Blanket prefixes like `sys/*` or `linux/*` are intentionally avoided so custom
    // external headers such as `sys/custom.h` fail closed.
    matches!(
        norm.as_str(),
        // C standard & POSIX root headers
        "assert.h"
            | "complex.h"
            | "ctype.h"
            | "dirent.h"
            | "dlfcn.h"
            | "errno.h"
            | "fcntl.h"
            | "fenv.h"
            | "float.h"
            | "fnmatch.h"
            | "glob.h"
            | "grp.h"
            | "iconv.h"
            | "inttypes.h"
            | "iso646.h"
            | "langinfo.h"
            | "libgen.h"
            | "limits.h"
            | "locale.h"
            | "math.h"
            | "netdb.h"
            | "nl_types.h"
            | "poll.h"
            | "pthread.h"
            | "pwd.h"
            | "regex.h"
            | "sched.h"
            | "semaphore.h"
            | "setjmp.h"
            | "signal.h"
            | "spawn.h"
            | "stdalign.h"
            | "stdarg.h"
            | "stdatomic.h"
            | "stdbool.h"
            | "stddef.h"
            | "stdint.h"
            | "stdio.h"
            | "stdlib.h"
            | "stdnoreturn.h"
            | "string.h"
            | "strings.h"
            | "syslog.h"
            | "termios.h"
            | "tgmath.h"
            | "time.h"
            | "uchar.h"
            | "unistd.h"
            | "utime.h"
            | "wchar.h"
            | "wctype.h"
            // Standard POSIX/BSD/Linux `sys/*`, `netinet/*`, `arpa/*`, `net/*`
            | "sys/auxv.h"
            | "sys/cdefs.h"
            | "sys/epoll.h"
            | "sys/errno.h"
            | "sys/event.h"
            | "sys/fcntl.h"
            | "sys/file.h"
            | "sys/inotify.h"
            | "sys/ioctl.h"
            | "sys/ipc.h"
            | "sys/mman.h"
            | "sys/mount.h"
            | "sys/msg.h"
            | "sys/param.h"
            | "sys/prctl.h"
            | "sys/ptrace.h"
            | "sys/random.h"
            | "sys/resource.h"
            | "sys/select.h"
            | "sys/sem.h"
            | "sys/sendfile.h"
            | "sys/shm.h"
            | "sys/signal.h"
            | "sys/socket.h"
            | "sys/stat.h"
            | "sys/statvfs.h"
            | "sys/syscall.h"
            | "sys/sysctl.h"
            | "sys/sysinfo.h"
            | "sys/sysmacros.h"
            | "sys/time.h"
            | "sys/times.h"
            | "sys/types.h"
            | "sys/uio.h"
            | "sys/un.h"
            | "sys/utsname.h"
            | "sys/wait.h"
            | "sys/xattr.h"
            | "arpa/inet.h"
            | "net/if.h"
            | "netinet/in.h"
            | "netinet/ip.h"
            | "netinet/tcp.h"
            | "netinet/udp.h"
            // Standard Linux UAPI headers
            | "linux/audit.h"
            | "linux/errno.h"
            | "linux/filter.h"
            | "linux/fs.h"
            | "linux/futex.h"
            | "linux/if.h"
            | "linux/if_ether.h"
            | "linux/if_tun.h"
            | "linux/ioctl.h"
            | "linux/limits.h"
            | "linux/magic.h"
            | "linux/netlink.h"
            | "linux/random.h"
            | "linux/rtnetlink.h"
            | "linux/sched.h"
            | "linux/seccomp.h"
            | "linux/sockios.h"
            | "linux/types.h"
            | "linux/unistd.h"
            | "linux/version.h"
            // Standard macOS / Darwin / Win32 headers
            | "mach/clock.h"
            | "mach/mach.h"
            | "mach/mach_time.h"
            | "mach/task.h"
            | "mach/vm_map.h"
            | "CoreFoundation/CoreFoundation.h"
            | "Security/Security.h"
            | "TargetConditionals.h"
            | "windows.h"
            | "winsock2.h"
            | "ws2tcpip.h"
    )
}


pub fn normalize_lexical_rel(path: &Path) -> Option<String> {
    if let Some(s) = path.to_str()
        && !s.is_empty()
        && !s.starts_with('/')
        && !s.contains('\\')
        && !s.contains("//")
        && !s.starts_with('.')
        && !s.contains("/.")
    {
        return Some(s.to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(os) => {
                let s = os.to_str()?;
                if !s.is_empty() {
                    parts.push(s);
                }
            }
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub fn is_frontend_source_path(rel_path: &str) -> bool {
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if FRONTEND_CONFIG_FILES.contains(&file_name) {
        return false;
    }
    FRONTEND_SOURCE_EXTENSIONS
        .iter()
        .any(|ext| file_name.ends_with(ext))
}

pub fn is_frontend_setup_file(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let stem = file_name.split('.').next().unwrap_or("");
    if matches!(
        stem,
        "setupTests" | "vitest.setup" | "jest.setup" | "test-setup" | "setup-tests"
    ) || file_name.starts_with("vitest.setup.")
        || file_name.starts_with("jest.setup.")
    {
        return true;
    }
    rel_path.ends_with("/test/setup.ts")
        || rel_path.ends_with("/test/setup.tsx")
        || rel_path.ends_with("/test/setup.js")
        || rel_path.ends_with("/tests/setup.ts")
        || rel_path.ends_with("/tests/setup.tsx")
        || rel_path.ends_with("/tests/setup.js")
        || rel_path == "test/setup.ts"
        || rel_path == "test/setup.tsx"
        || rel_path == "test/setup.js"
        || rel_path == "src/test/setup.ts"
        || rel_path == "src/test/setup.tsx"
}

pub fn is_frontend_test_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) || is_frontend_setup_file(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".test")
                || stem.ends_with(".spec")
                || stem.ends_with(".e2e")
                || stem.ends_with(".cy")
                || stem.ends_with("_test")
                || stem.ends_with("_spec"))
        {
            return true;
        }
    }
    (rel_path.starts_with("__tests__/")
        || rel_path.contains("/__tests__/")
        || rel_path.starts_with("e2e/")
        || rel_path.contains("/e2e/")
        || rel_path.starts_with("cypress/")
        || rel_path.contains("/cypress/"))
        && !file_name.starts_with("setup")
        && !file_name.starts_with("helper")
}

pub fn is_cypress_test_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext) && stem.ends_with(".cy") {
            return true;
        }
    }
    rel_path.starts_with("cypress/") || rel_path.contains("/cypress/")
}

pub fn is_polyglot_source_path(rel_path: &str) -> bool {
    POLYGLOT_SOURCE_EXTENSIONS
        .iter()
        .any(|ext| rel_path.ends_with(ext))
        || rel_path.ends_with(".graphql")
        || rel_path.ends_with(".gql")
}

pub fn is_polyglot_test_path(rel_path: &str, parsed: &ParsedPythonFile) -> bool {
    if rel_path.ends_with("_test.go") {
        return true;
    }
    if (rel_path.ends_with(".cs")
        || rel_path.ends_with(".java")
        || rel_path.ends_with(".kt")
        || rel_path.ends_with(".cpp")
        || rel_path.ends_with(".cc")
        || rel_path.ends_with(".c"))
        && !parsed.test_symbols.is_empty()
    {
        return true;
    }
    false
}

pub fn is_frontend_barrel_file(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    file_name.starts_with("index.")
}

pub fn is_stylesheet_path(rel_path: &str) -> bool {
    rel_path.ends_with(".css")
        || rel_path.ends_with(".scss")
        || rel_path.ends_with(".sass")
        || rel_path.ends_with(".less")
        || rel_path.ends_with(".styl")
        || rel_path.ends_with(".stylus")
}

pub fn is_storybook_story_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".stories") || stem.ends_with(".story"))
        {
            return true;
        }
    }
    false
}

pub fn is_playwright_e2e_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".e2e") || stem.ends_with(".e2e.spec") || stem.ends_with(".e2e.test"))
        {
            return true;
        }
    }
    rel_path.starts_with("e2e/") || rel_path.contains("/e2e/")
}

pub fn file_language_family(rel_path: &str) -> &'static str {
    if rel_path.ends_with(".rs") {
        "rust"
    } else if is_frontend_source_path(rel_path) || is_stylesheet_path(rel_path) {
        "frontend"
    } else if rel_path.ends_with(".go") {
        "go"
    } else if rel_path.ends_with(".cs") {
        "csharp"
    } else if rel_path.ends_with(".java") || rel_path.ends_with(".kt") {
        "jvm"
    } else if rel_path.ends_with(".c")
        || rel_path.ends_with(".cpp")
        || rel_path.ends_with(".cc")
        || rel_path.ends_with(".h")
        || rel_path.ends_with(".hpp")
    {
        "cpp"
    } else {
        "python"
    }
}

pub fn frontend_pkg_dir(rel_path: &str, files: &BTreeMap<String, FileNode>) -> String {
    let mut cur = Path::new(rel_path).parent();
    while let Some(dir) = cur {
        let dir_str = normalize_rel_str(dir);
        if dir_str.is_empty() {
            break;
        }
        if files.contains_key(&format!("{dir_str}/package.json"))
            || files.contains_key(&format!("{dir_str}/tsconfig.json"))
            || files.contains_key(&format!("{dir_str}/vite.config.ts"))
            || files.contains_key(&format!("{dir_str}/vitest.config.ts"))
        {
            return dir_str;
        }
        cur = dir.parent();
    }
    if let Some((first, rest)) = rel_path.split_once('/')
        && matches!(first, "packages" | "apps" | "services")
        && let Some((second, _)) = rest.split_once('/')
    {
        return format!("{first}/{second}");
    }
    if let Some((first, _)) = rel_path.split_once('/')
        && matches!(
            first,
            "frontend" | "web" | "client" | "ui" | "vscode-extension"
        )
    {
        return first.to_string();
    }
    String::new()
}

pub fn is_rust_common_test_helper(rel_path: &str) -> bool {
    rel_path == "tests/common/mod.rs"
        || rel_path == "tests/common.rs"
        || rel_path.ends_with("/tests/common/mod.rs")
        || rel_path.ends_with("/tests/common.rs")
}

pub fn rust_integration_test_target_name(rel_path: &str) -> Option<String> {
    if !rel_path.ends_with(".rs")
        || rel_path == "tests/common/mod.rs"
        || rel_path.ends_with("/tests/common/mod.rs")
    {
        return None;
    }
    let rest = if let Some(r) = rel_path.strip_prefix("tests/") {
        r
    } else if let Some((_, r)) = rel_path.rsplit_once("/tests/") {
        r
    } else {
        return None;
    };
    if !rest.contains('/') {
        return rest
            .strip_suffix(".rs")
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
    }
    if let Some(dir_name) = rest.strip_suffix("/main.rs")
        && !dir_name.is_empty()
        && !dir_name.contains('/')
    {
        return Some(dir_name.to_string());
    }
    None
}

pub fn is_rust_integration_test_path(rel_path: &str) -> bool {
    rust_integration_test_target_name(rel_path).is_some()
}

pub fn conftest_scope_dir(path: &str) -> String {
    if path == "conftest.py" {
        return String::new();
    }
    if let Some(prefix) = path.strip_suffix("/conftest.py") {
        return prefix.to_string();
    }
    if path == "tests/common/mod.rs" || path == "tests/common.rs" {
        return "tests".to_string();
    }
    if let Some(prefix) = path.strip_suffix("/common/mod.rs") {
        return prefix.to_string();
    }
    if let Some(prefix) = path.strip_suffix("/common.rs") {
        return prefix.to_string();
    }
    if is_frontend_setup_file(path) {
        for suffix in [
            "/src/test/setup.ts",
            "/src/test/setup.tsx",
            "/src/test/setup.js",
            "/src/setupTests.ts",
            "/src/setupTests.tsx",
            "/src/setupTests.js",
            "/test/setup.ts",
            "/test/setup.tsx",
            "/test/setup.js",
        ] {
            if let Some(prefix) = path.strip_suffix(suffix) {
                return prefix.to_string();
            }
        }
        if path.starts_with("src/") || path.starts_with("test/") || path.starts_with("tests/") {
            return String::new();
        }
    }
    Path::new(path)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default()
}

pub fn is_test_filename(file_name: &str) -> bool {
    (file_name.starts_with("test_") && file_name.ends_with(".py"))
        || (file_name.ends_with("_test.py") && file_name.len() > "_test.py".len())
        || (file_name.starts_with("test_") && file_name.ends_with(".rs"))
        || (file_name.ends_with("_test.rs") && file_name.len() > "_test.rs".len())
        || is_frontend_test_path(file_name)
}

pub(super) fn matches_single_pattern_fast(file_name: &str, pattern: &str) -> Option<bool> {
    if pattern.contains(['?', '[', ']', '{', '}', '/']) {
        return None;
    }
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return Some(file_name == pattern);
    };
    if suffix.contains('*') {
        return None;
    }
    Some(
        file_name.len() >= prefix.len() + suffix.len()
            && file_name.starts_with(prefix)
            && file_name.ends_with(suffix),
    )
}

pub fn matches_test_filename(file_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if let Some(matched) = matches_single_pattern_fast(file_name, pattern) {
            return matched;
        }
        python_fnmatch_segment(file_name, pattern)
    })
}

pub(super) fn matches_pytest_function_name(sym_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if !pattern.contains(['*', '?', '[', '{']) {
            // Pytest treats non-glob `python_functions` entries (e.g. `check`) as prefix matches.
            sym_name.starts_with(pattern.as_str())
        } else {
            matches_test_filename(sym_name, std::slice::from_ref(pattern))
        }
    })
}

/// Pure-Rust Python `fnmatch.fnmatchcase` matcher for a single path segment:
/// supports `*`, `?`, `[seq]`, `[!seq]`, ranges `[a-z]`, treats `{`/`}` literally, and treats
/// unclosed `[` (e.g. `te[st*`) literally just like Python's `fnmatch` / `glob`.
pub(super) fn python_fnmatch_segment(seg: &str, pat: &str) -> bool {
    let s: Vec<char> = seg.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (sl, pl) = (s.len(), p.len());
    // Parse pattern into simple match atoms
    enum Atom {
        Star,
        Any,
        Lit(char),
        Class { neg: bool, chars: Vec<char>, ranges: Vec<(char, char)> },
    }
    let mut atoms = Vec::with_capacity(pl);
    let mut pi = 0usize;
    while pi < pl {
        match p[pi] {
            '*' => {
                if !matches!(atoms.last(), Some(Atom::Star)) {
                    atoms.push(Atom::Star);
                }
                pi += 1;
            }
            '?' => {
                atoms.push(Atom::Any);
                pi += 1;
            }
            '[' => {
                // Find closing `]` per Python `fnmatch`: immediate `]` after `[` or `[!` is literal
                let mut j = pi + 1;
                if j < pl && p[j] == '!' {
                    j += 1;
                }
                if j < pl && p[j] == ']' {
                    j += 1;
                }
                while j < pl && p[j] != ']' {
                    j += 1;
                }
                if j >= pl {
                    // Unclosed `[` is matched literally in Python `fnmatch`!
                    atoms.push(Atom::Lit('['));
                    pi += 1;
                } else {
                    // Exact port of CPython's `fnmatch.translate` character-class chunking,
                    // empty-range elimination, and post-elimination `!` negation handling.
                    let stuff = &p[pi + 1..j];
                    let mut toks: Vec<(char, bool)> = Vec::new();
                    if !stuff.contains(&'-') {
                        for &c in stuff {
                            toks.push((c, false));
                        }
                    } else {
                        let mut chunks: Vec<Vec<char>> = Vec::new();
                        let mut ci = 0usize;
                        let mut k = if stuff[0] == '!' { 2 } else { 1 };
                        while let Some(rel) =
                            stuff.get(k..).and_then(|sl| sl.iter().position(|&c| c == '-'))
                        {
                            let pos = k + rel;
                            chunks.push(stuff[ci..pos].to_vec());
                            ci = pos + 1;
                            k = pos + 3;
                        }
                        let tail = &stuff[ci..];
                        if !tail.is_empty() {
                            chunks.push(tail.to_vec());
                        } else if let Some(last) = chunks.last_mut() {
                            last.push('-');
                        }
                        for idx in (1..chunks.len()).rev() {
                            let lo = *chunks[idx - 1].last().unwrap();
                            let hi = chunks[idx][0];
                            if lo > hi {
                                chunks[idx - 1].pop();
                                let rest = chunks[idx][1..].to_vec();
                                chunks[idx - 1].extend(rest);
                                chunks.remove(idx);
                            }
                        }
                        for (idx, chunk) in chunks.iter().enumerate() {
                            if idx > 0 {
                                toks.push(('-', true));
                            }
                            for &c in chunk {
                                toks.push((c, false));
                            }
                        }
                    }
                    let (final_neg, slice) = if toks.is_empty() {
                        (false, &toks[..])
                    } else if toks[0] == ('!', false) {
                        (true, &toks[1..])
                    } else {
                        (false, &toks[..])
                    };
                    let mut chars = Vec::new();
                    let mut ranges = Vec::new();
                    let mut ti = 0usize;
                    while ti < slice.len() {
                        if ti + 2 < slice.len() && slice[ti + 1] == ('-', true) {
                            ranges.push((slice[ti].0, slice[ti + 2].0));
                            ti += 3;
                        } else {
                            chars.push(slice[ti].0);
                            ti += 1;
                        }
                    }
                    atoms.push(Atom::Class {
                        neg: final_neg,
                        chars,
                        ranges,
                    });
                    pi = j + 1;
                }
            }
            c => {
                atoms.push(Atom::Lit(c));
                pi += 1;
            }
        }
    }
    let al = atoms.len();
    let mut dp = vec![false; sl + 1];
    dp[0] = true;
    for atom in &atoms {
        let mut next = vec![false; sl + 1];
        match atom {
            Atom::Star => {
                let mut seen = false;
                for si in 0..=sl {
                    seen |= dp[si];
                    next[si] = seen;
                }
            }
            Atom::Any => {
                for si in 0..sl {
                    if dp[si] {
                        next[si + 1] = true;
                    }
                }
            }
            Atom::Lit(c) => {
                for si in 0..sl {
                    if dp[si] && s[si] == *c {
                        next[si + 1] = true;
                    }
                }
            }
            Atom::Class { neg, chars, ranges } => {
                for si in 0..sl {
                    if dp[si] {
                        let ch = s[si];
                        let hit = chars.contains(&ch)
                            || ranges.iter().any(|&(lo, hi)| ch >= lo && ch <= hi);
                        if hit != *neg {
                            next[si + 1] = true;
                        }
                    }
                }
            }
        }
        dp = next;
        let _ = al;
    }
    dp[sl]
}

pub(super) fn python_glob_seg_matches(seg: &str, pat: &str) -> bool {
    // Python `glob.iglob` / `fnmatch` excludes dot-prefixed entries unless the pattern segment
    // itself starts with `.`, treats `{`/`}` and unclosed `[` literally.
    if seg.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    if pat == "*" || (!pat.contains(['*', '?', '[']) && pat == seg) {
        return true;
    }
    python_fnmatch_segment(seg, pat)
}

pub(super) fn python_glob_matches_parts(rel_parts: &[&str], pat_parts: &[&str], allow_prefix: bool) -> bool {
    let r_len = rel_parts.len();
    let p_len = pat_parts.len();
    // DP table: dp[ri][pi] = whether rel_parts[..ri] matches pat_parts[..pi]
    let mut dp = vec![vec![false; p_len + 1]; r_len + 1];
    dp[0][0] = true;
    for pi in 0..p_len {
        if pat_parts[pi] == "**" && dp[0][pi] {
            dp[0][pi + 1] = true;
        }
    }
    for ri in 0..r_len {
        for pi in 0..p_len {
            if !dp[ri][pi] {
                continue;
            }
            if pat_parts[pi] == "**" {
                // `**` matches 0 segments or 1 non-hidden segment (matching Python `glob.iglob`).
                dp[ri][pi + 1] = true;
                if !rel_parts[ri].starts_with('.') {
                    dp[ri + 1][pi] = true;
                }
            } else if python_glob_seg_matches(rel_parts[ri], pat_parts[pi]) {
                dp[ri + 1][pi + 1] = true;
            }
        }
        // Propagate zero-length `**` transitions at row `ri + 1`
        for pi in 0..p_len {
            if pat_parts[pi] == "**" && dp[ri + 1][pi] {
                dp[ri + 1][pi + 1] = true;
            }
        }
    }
    if allow_prefix {
        // True if `rel_parts` matches a prefix of `pat_parts` (could lead to a match deeper)
        // OR already matched the full `pat_parts` (inside a matched directory).
        dp[r_len].iter().any(|&ok| ok)
    } else {
        dp[r_len][p_len]
    }
}

/// Returns true if directory `rel_dir` is a valid prefix (or inside a match) of any configured
/// hidden pytest `testpaths` glob pattern under Python `glob.iglob` hidden-file semantics.
pub fn testpaths_allow_dir_prefix(rel_dir: &str, testpaths: &[String]) -> bool {
    let rel_parts: Vec<&str> = rel_dir.split('/').filter(|s| !s.is_empty()).collect();
    if rel_parts.is_empty() {
        return true;
    }
    for tp in testpaths {
        let pat_parts: Vec<&str> = tp.split('/').filter(|s| !s.is_empty()).collect();
        if pat_parts.is_empty() {
            continue;
        }
        if python_glob_matches_parts(&rel_parts, &pat_parts, true) {
            return true;
        }
        // Also allow subdirectories strictly inside an already-matched testpath directory
        for prefix_len in 1..rel_parts.len() {
            if python_glob_matches_parts(&rel_parts[..prefix_len], &pat_parts, false) {
                return true;
            }
        }
    }
    false
}

pub(super) fn testpaths_match_any_on_disk(root: &Path, testpaths: &[String]) -> bool {
    for tp in testpaths {
        if tp == "." || tp.is_empty() {
            return true;
        }
        if !tp.contains(['*', '?', '[']) {
            if root.join(tp).exists() {
                return true;
            }
            continue;
        }
        // Globbed testpath (e.g. `tests/*` or `**/.hidden/tests`): match using exact Python
        // `glob.iglob` per-segment hidden-entry rules so `.hidden/.hidden/tests` never falsely
        // matches `**/.hidden/tests`, and `{` / `}` are matched literally.
        let tp_parts: Vec<&str> = tp.split('/').filter(|s| !s.is_empty()).collect();
        if tp_parts.is_empty() {
            continue;
        }
        let prefix_parts: Vec<&str> = tp_parts
            .iter()
            .copied()
            .take_while(|seg| !seg.contains(['*', '?', '[']))
            .collect();
        let start_rel = prefix_parts.join("/");
        let start_dir = if start_rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&start_rel)
        };
        if !start_dir.is_dir() {
            continue;
        }
        let has_recursive = tp_parts.contains(&"**");
        let max_depth = if has_recursive {
            64
        } else {
            tp_parts.len().saturating_sub(1)
        };
        let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(start_dir, prefix_parts.len())];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name_os = entry.file_name();
                let Some(name_str) = name_os.to_str() else {
                    continue;
                };
                let p = entry.path();
                let Ok(rel) = p.strip_prefix(root) else {
                    continue;
                };
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let rel_parts: Vec<&str> = rel_str.split('/').filter(|s| !s.is_empty()).collect();
                if python_glob_matches_parts(&rel_parts, &tp_parts, false) {
                    return true;
                }
                if depth < max_depth
                    && p.is_dir()
                    && !matches!(name_str, ".git" | ".hg" | ".svn")
                    && python_glob_matches_parts(&rel_parts, &tp_parts, true)
                {
                    stack.push((p, depth + 1));
                }
            }
        }
    }
    false
}

pub(super) fn matches_test_path(rel_path: &str, testpaths: &[String]) -> bool {
    testpaths.is_empty()
        || testpaths.iter().any(|testpath| {
            if testpath == "." || testpath.is_empty() {
                return true;
            }
            if testpath.contains(['*', '?', '[']) {
                let rel_parts: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
                let pat_parts: Vec<&str> = testpath.split('/').filter(|s| !s.is_empty()).collect();
                if python_glob_matches_parts(&rel_parts, &pat_parts, false) {
                    return true;
                }
                for prefix_len in 1..rel_parts.len() {
                    if python_glob_matches_parts(&rel_parts[..prefix_len], &pat_parts, false) {
                        return true;
                    }
                }
                return false;
            }
            rel_path == testpath || rel_path.starts_with(&format!("{testpath}/"))
        })
}

pub fn normalize_rel_str(path: &Path) -> String {
    let s = path.to_string_lossy();
    let trimmed = s.trim_start_matches("./");
    if cfg!(windows) {
        trimmed.replace('\\', "/")
    } else {
        trimmed.to_string()
    }
}


