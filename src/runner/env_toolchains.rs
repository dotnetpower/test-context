use super::*;

pub(super) fn partition_env_hash(base: &str, partition: Option<&str>) -> String {
    match partition {
        Some(part) if !base.is_empty() => {
            crate::ast_graph::sha256_hex(format!("{base}\0env_partition={part}").as_bytes())
        }
        Some(part) => crate::ast_graph::sha256_hex(format!("env_partition={part}").as_bytes()),
        None => base.to_string(),
    }
}

pub(super) fn environment_fingerprint_for_graph(spec: &Spec, graph: &DependencyGraph) -> String {
    let has_python =
        !spec.pytest_cmd.is_empty() || graph.files.keys().any(|path| path.ends_with(".py"));
    environment_fingerprint_with_python(spec, has_python, Some(graph))
}

pub(super) fn cargo_config_candidates_for_spec(
    spec: &Spec,
    graph: Option<&DependencyGraph>,
) -> BTreeSet<PathBuf> {
    let mut cands = BTreeSet::new();
    cands.insert(spec.root.join(".cargo/config.toml"));
    cands.insert(spec.root.join(".cargo/config"));
    for scope in &spec.scopes {
        if scope != "." {
            let mut cur = Some(Path::new(scope.as_str()));
            while let Some(dir) = cur {
                if dir.as_os_str().is_empty() {
                    break;
                }
                cands.insert(spec.root.join(dir).join(".cargo/config.toml"));
                cands.insert(spec.root.join(dir).join(".cargo/config"));
                cur = dir.parent();
            }
        }
    }
    if let Some(g) = graph {
        for path in g.files.keys() {
            if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
                let mut cur = Path::new(path).parent();
                while let Some(dir) = cur {
                    if dir.as_os_str().is_empty() {
                        break;
                    }
                    cands.insert(spec.root.join(dir).join(".cargo/config.toml"));
                    cands.insert(spec.root.join(dir).join(".cargo/config"));
                    cur = dir.parent();
                }
            }
        }
    }
    cands
}

/// Returns true if `cand` or its `.cargo` parent directory (or any component between `root` and
/// `cand`) is a symlink, so both symlinked config files AND symlinked `.cargo/` dirs are detected.
pub(super) fn cargo_config_traverses_symlink(root: &Path, cand: &Path) -> bool {
    let Ok(rel) = cand.strip_prefix(root) else {
        return false;
    };
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if let Ok(meta) = std::fs::symlink_metadata(&cur)
            && meta.file_type().is_symlink()
        {
            return true;
        }
    }
    false
}

pub(super) fn has_symlinked_cargo_config(spec: &Spec, graph: Option<&DependencyGraph>) -> bool {
    cargo_config_candidates_for_spec(spec, graph)
        .iter()
        .any(|cand| cargo_config_traverses_symlink(&spec.root, cand))
}

pub(super) fn crate_has_local_cargo_config(root: &Path, crate_dir: &str) -> bool {
    if crate_dir.is_empty() {
        return false;
    }
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let rustup_overrides: Vec<PathBuf> = std::env::var_os("RUSTUP_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".rustup"))
        })
        .and_then(|d| std::fs::read_to_string(d.join("settings.toml")).ok())
        .map(|txt| {
            parse_rustup_override_dirs(&txt)
                .into_iter()
                .map(|s| Path::new(&s).canonicalize().unwrap_or_else(|_| PathBuf::from(s)))
                .filter(|p| p.starts_with(&root_canon) && *p != root_canon)
                .collect()
        })
        .unwrap_or_default();
    let mut cur = Some(Path::new(crate_dir));
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() {
            break;
        }
        let full = root.join(dir);
        if full.join(".cargo/config.toml").exists()
            || full.join(".cargo/config").exists()
            || full.join("rust-toolchain.toml").exists()
            || full.join("rust-toolchain").exists()
        {
            return true;
        }
        if !rustup_overrides.is_empty() {
            let full_canon = full.canonicalize().unwrap_or(full);
            if rustup_overrides.contains(&full_canon) {
                return true;
            }
        }
        cur = dir.parent();
    }
    false
}

/// Returns existing Cargo config files OUTSIDE `spec.root` that Cargo still applies:
/// ancestor `.cargo/config[.toml]` directories above `spec.root` up to `/` plus
/// `$CARGO_HOME/config[.toml]` (or `~/.cargo/config[.toml]`).
pub(super) fn effective_external_cargo_configs(spec: &Spec) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    let mut cur = spec.root.parent();
    while let Some(dir) = cur {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let p = dir.join(name);
            if p.is_file() {
                out.insert(p);
            }
        }
        cur = dir.parent();
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        });
    if let Some(ch) = cargo_home {
        for name in ["config.toml", "config"] {
            let p = ch.join(name);
            if p.is_file() && !p.starts_with(&spec.root) {
                out.insert(p);
            }
        }
    }
    out.into_iter().collect()
}

/// Checks whether a Cargo config file defines `[env]` (any TOML key/header quoting such as
/// `[env]`, `["env"]`, `['env']`, `env = { ... }`, `env.FOO = ...`) or a target `runner`
/// (`[target.*]` `runner = ...`, `target = { ... }`), failing closed on ambiguous/unquoted syntax.
pub(super) fn cargo_config_file_disables_direct_exe(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut in_target_table = false;
    let mut in_multiline_str = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if in_multiline_str {
            if trimmed.contains("\"\"\"") || trimmed.contains("'''") {
                in_multiline_str = false;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            let hdr = trimmed
                .split('#')
                .next()
                .unwrap_or(trimmed)
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            let first_seg = hdr
                .split('.')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\'']);
            if first_seg == "env" {
                return true;
            }
            in_target_table = first_seg == "target";
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key_clean = k.trim();
            let first_seg = key_clean
                .split('.')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\'']);
            let last_seg = key_clean
                .split('.')
                .next_back()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\'']);
            if first_seg == "env" {
                return true;
            }
            let stripped_key = key_clean.trim_matches(['"', '\'']);
            if (in_target_table && first_seg == "runner")
                || (first_seg == "target"
                    && (last_seg == "runner"
                        || stripped_key == "target"
                        || v.trim().starts_with('{')))
            {
                return true;
            }
            let val_trim = v.trim();
            let triple_dq = val_trim.matches("\"\"\"").count();
            let triple_sq = val_trim.matches("'''").count();
            if triple_dq % 2 == 1 || triple_sq % 2 == 1 {
                in_multiline_str = true;
            }
        } else {
            // Unrecognized non-table/non-assignment TOML construct (e.g. multi-line inline table
            // continuation): check conservatively for `runner` or `env` tokens.
            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("runner") && lower.contains('=') {
                return true;
            }
        }
    }
    false
}

pub(super) fn cargo_config_disables_direct_exe(root: &Path, crate_dir: &str) -> bool {
    // Check `CARGO_TARGET_*_RUNNER` env vars first (using `vars_os()` so non-UTF-8 Unix env vars
    // never panic!).
    if std::env::vars_os().any(|(k, v)| {
        let ks = k.to_string_lossy();
        ks.starts_with("CARGO_TARGET_")
            && ks.ends_with("_RUNNER")
            && !v.to_string_lossy().trim().is_empty()
    }) {
        return true;
    }
    // Walk ALL ancestor directories from `root.join(crate_dir)` up to filesystem root `/`,
    // matching Cargo's full config hierarchy.
    let start_dir = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate_dir)
    };
    let mut cur: Option<&Path> = Some(start_dir.as_path());
    while let Some(dir) = cur {
        if cargo_config_file_disables_direct_exe(&dir.join(".cargo/config.toml"))
            || cargo_config_file_disables_direct_exe(&dir.join(".cargo/config"))
        {
            return true;
        }
        cur = dir.parent();
    }
    // Also check `$CARGO_HOME/config[.toml]` or default `~/.cargo/config[.toml]`.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        });
    if let Some(ch) = cargo_home
        && (cargo_config_file_disables_direct_exe(&ch.join("config.toml"))
            || cargo_config_file_disables_direct_exe(&ch.join("config")))
    {
        return true;
    }
    false
}

pub(super) fn non_python_environment_fingerprint_for_graph(
    spec: &Spec,
    graph: Option<&DependencyGraph>,
) -> String {
    environment_fingerprint_with_python(spec, false, graph)
}

pub(super) static RUSTC_TOOLCHAIN_CACHE: OnceLock<Mutex<BTreeMap<String, (Instant, String)>>> =
    OnceLock::new();

pub(super) fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn access(pathname: *const std::ffi::c_char, mode: std::ffi::c_int) -> std::ffi::c_int;
        }
        const X_OK: std::ffi::c_int = 1;
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { access(c_path.as_ptr(), X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolves an executable name or relative path using the EXACT augmented `PATH` supplied to
/// Cargo/subprocesses (`crate::catalog::augmented_user_path()` first, falling back to process
/// `PATH`), resolving relative `PATH` entries and relative paths against Cargo's working directory
/// `cwd` and checking OS effective execute permission (`access(X_OK)` on Unix).
pub(super) fn resolve_executable_on_cargo_path(bin: &Path, cwd: &Path) -> PathBuf {
    if bin.is_absolute() {
        return bin.to_path_buf();
    }
    if bin.components().count() > 1 {
        return cwd.join(bin);
    }
    for path_var in [crate::catalog::augmented_user_path(), std::env::var_os("PATH")]
        .into_iter()
        .flatten()
    {
        for dir in std::env::split_paths(&path_var) {
            let base = if dir.as_os_str().is_empty() {
                cwd.to_path_buf()
            } else if dir.is_relative() {
                cwd.join(&dir)
            } else {
                dir
            };
            let cand = base.join(bin);
            if is_executable_file(&cand) {
                return cand;
            }
        }
    }
    bin.to_path_buf()
}

/// Extracts Cargo `[build]` compiler overrides (`rustc`, `rustc-wrapper`, `rustc-workspace-wrapper`)
/// walking `dir` up to `/` plus `$CARGO_HOME/config[.toml]` (nearest directory takes precedence).
pub(super) fn cargo_config_build_compiler_overrides(
    dir: &Path,
) -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let mut cfg_rustc: Option<PathBuf> = None;
    let mut cfg_wrapper: Option<PathBuf> = None;
    let mut cfg_ws_wrapper: Option<PathBuf> = None;
    let mut scan_cfg = |cfg_path: &Path| {
        let Ok(raw_text) = std::fs::read_to_string(cfg_path) else {
            return;
        };
        let norm_text = expand_cargo_toml_root_syntaxes(&raw_text);
        let mut in_build = false;
        for raw_line in norm_text.lines() {
            let line = strip_toml_comment_outside_quotes(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(h) = normalize_toml_table_header(line) {
                in_build = h == "[build]";
                continue;
            }
            if in_build
                && let Some((k, v)) = split_toml_kv_outside_quotes(line)
                && let Some(val) = unquote_toml_scalar(v.trim())
                && !val.trim().is_empty()
            {
                let key = unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
                let val_path = PathBuf::from(val.trim());
                let resolved = if val_path.components().count() > 1 && !val_path.is_absolute() {
                    cfg_path
                        .parent()
                        .and_then(Path::parent)
                        .map_or(val_path.clone(), |base| base.join(&val_path))
                } else {
                    val_path
                };
                match key.as_str() {
                    "rustc" if cfg_rustc.is_none() => cfg_rustc = Some(resolved),
                    "rustc-wrapper" if cfg_wrapper.is_none() => cfg_wrapper = Some(resolved),
                    "rustc-workspace-wrapper" if cfg_ws_wrapper.is_none() => {
                        cfg_ws_wrapper = Some(resolved)
                    }
                    _ => {}
                }
            }
        }
    };
    let mut cur: Option<&Path> = Some(dir);
    while let Some(d) = cur {
        // Cargo prefers legacy `.cargo/config` over `.cargo/config.toml` when both exist in the
        // same directory (and ignores `.cargo/config.toml` in that directory).
        let legacy = d.join(".cargo/config");
        if legacy.is_file() {
            scan_cfg(&legacy);
        } else {
            scan_cfg(&d.join(".cargo/config.toml"));
        }
        cur = d.parent();
    }
    if let Some(ch) = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        })
    {
        let legacy = ch.join("config");
        if legacy.is_file() {
            scan_cfg(&legacy);
        } else {
            scan_cfg(&ch.join("config.toml"));
        }
    }
    (cfg_rustc, cfg_wrapper, cfg_ws_wrapper)
}

pub(super) fn rust_toolchain_cache_key(dir: &Path) -> (PathBuf, String) {
    // Respect Cargo's full compiler/wrapper precedence (`RUSTC` > `CARGO_BUILD_RUSTC` > `[build] rustc`,
    // `RUSTC_WRAPPER` > `CARGO_BUILD_RUSTC_WRAPPER` > `[build] rustc-wrapper`, and
    // `RUSTC_WORKSPACE_WRAPPER` > `CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER` > `[build] rustc-workspace-wrapper`),
    // resolved on the EXACT augmented `PATH` supplied to Cargo!
    let (cfg_rustc, cfg_wrapper, cfg_ws_wrapper) = cargo_config_build_compiler_overrides(dir);
    let rustc_override = std::env::var_os("RUSTC")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_BUILD_RUSTC")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or(cfg_rustc)
        .unwrap_or_else(|| PathBuf::from("rustc"));
    let rustc_bin = resolve_executable_on_cargo_path(&rustc_override, dir);
    let rustc_real = rustc_bin.canonicalize().unwrap_or_else(|_| rustc_bin.clone());
    let rustc_stamp = file_stamp(&rustc_real).or_else(|| file_stamp(&rustc_bin));
    let wrapper_stamp = std::env::var_os("RUSTC_WRAPPER")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_BUILD_RUSTC_WRAPPER")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or(cfg_wrapper)
        .map(|w| {
            let wb = resolve_executable_on_cargo_path(&w, dir);
            let wr = wb.canonicalize().unwrap_or(wb);
            (wr.clone(), file_stamp(&wr))
        });
    let ws_wrapper_stamp = std::env::var_os("RUSTC_WORKSPACE_WRAPPER")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or(cfg_ws_wrapper)
        .map(|w| {
            let wb = resolve_executable_on_cargo_path(&w, dir);
            let wr = wb.canonicalize().unwrap_or(wb);
            (wr.clone(), file_stamp(&wr))
        });
    let tc_env = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();
    // Walk ancestor directories for inherited `rust-toolchain[.toml]` (resolving symlinks).
    let mut tc_file_stamp = None;
    let mut anc: Option<&Path> = Some(dir);
    while let Some(d) = anc {
        for fname in ["rust-toolchain.toml", "rust-toolchain"] {
            let cand = d.join(fname);
            if cand.exists() {
                let real = cand.canonicalize().unwrap_or(cand);
                tc_file_stamp = file_stamp(&real);
                break;
            }
        }
        if tc_file_stamp.is_some() {
            break;
        }
        anc = d.parent();
    }
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rustup")));
    let rustup_settings_stamp = rustup_home
        .as_ref()
        .and_then(|h| file_stamp(&h.join("settings.toml")));
    let rustup_tc_dir_stamp = rustup_home
        .as_ref()
        .and_then(|h| dir_stamp(&h.join("toolchains")));
    // Also stamp the resolved `bin/rustc` binary inside each installed/linked rustup toolchain
    // (including `rustup toolchain link` symlinked targets whose underlying compiler binary can be
    // replaced in-place without updating the `toolchains/` directory mtime!).
    let mut rustup_tc_rustc_stamps: Vec<(String, Option<crate::ast_graph::FileStamp>)> = Vec::new();
    if let Some(ref rh) = rustup_home
        && let Ok(rd) = std::fs::read_dir(rh.join("toolchains"))
    {
        for ent in rd.flatten() {
            let tc_path = ent.path();
            let tc_name = ent.file_name().to_string_lossy().into_owned();
            let rustc_cand = tc_path.join("bin").join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
            let real_cand = rustc_cand.canonicalize().unwrap_or(rustc_cand);
            rustup_tc_rustc_stamps.push((tc_name, file_stamp(&real_cand)));
        }
        rustup_tc_rustc_stamps.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let key = format!(
        "{}\0{}\0{}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}",
        dir.to_string_lossy(),
        rustc_real.to_string_lossy(),
        tc_env,
        rustc_stamp,
        wrapper_stamp,
        ws_wrapper_stamp,
        tc_file_stamp,
        rustup_settings_stamp,
        rustup_tc_dir_stamp,
        rustup_tc_rustc_stamps,
    );
    (rustc_bin, key)
}

pub(super) fn rust_toolchain_dirs_for_graph(root: &Path, graph: Option<&DependencyGraph>) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    dirs.insert(root.to_path_buf());
    if let Some(g) = graph {
        for path in g.files.keys() {
            if path.ends_with("/rust-toolchain.toml") || path.ends_with("/rust-toolchain") {
                if let Some(p) = Path::new(path).parent() {
                    dirs.insert(root.join(p));
                }
            } else if path.ends_with("/Cargo.toml")
                && let Some(p) = Path::new(path).parent()
            {
                // Include every nested Cargo crate directory (covering `rustup override` too).
                dirs.insert(root.join(p));
            }
        }
    }
    dirs.into_iter().collect()
}

/// Fingerprints the effective Rust toolchain (`rustc -vV`) for workspace root AND any nested
/// crate directory defining its own `rust-toolchain[.toml]`, cached with stamp checks so
/// `rustup update` immediately invalidates cached Rust passes.
pub(super) fn rust_toolchain_fingerprint(dir: &Path) -> String {
    let (rustc_bin, key) = rust_toolchain_cache_key(dir);
    let cache = RUSTC_TOOLCHAIN_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some((captured, val)) = guard.get(&key)
        && captured.elapsed() < Duration::from_secs(300)
    {
        return val.clone();
    }
    let mut cmd = Command::new(&rustc_bin);
    cmd.current_dir(dir)
        .arg("-vV")
        .stdin(std::process::Stdio::null());
    let raw_version = crate::workspace::run_command_with_timeout(
        &mut cmd,
        Duration::from_secs(5),
        64 * 1024,
    )
    .ok()
    .filter(|o| o.status.success() && !o.timed_out)
    .map(|o| sha256_hex(&o.stdout))
    .unwrap_or_else(|| format!("rustc-unavailable:{}", now_unix_ms()));
    // Combine `rustc -vV` digest with `key` digest (which encodes resolved `RUSTC_WRAPPER` and
    // `RUSTC_WORKSPACE_WRAPPER` executable stamps!) so in-place wrapper edits invalidate caches.
    let version = sha256_hex(format!("{raw_version}\0{key}").as_bytes());
    if let Ok(mut guard) = cache.lock() {
        // Bound cache at 2048 entries (pruning expired entries first) so large multi-crate
        // workspaces never thrash `rustc -vV` probes on every request.
        if guard.len() >= 2048 {
            guard.retain(|_, (captured, _)| captured.elapsed() < Duration::from_secs(300));
            if guard.len() >= 2048 {
                guard.clear();
            }
        }
        guard.insert(key, (Instant::now(), version.clone()));
    }
    version
}

/// Fingerprints an external runtime binary (`node` or `go`) in an optional working directory
/// (important for `go version` under `go.mod`/`go.work` automatic toolchain selection!) by its
/// resolved canonical binary stamp + version output, cached with a 300s TTL.
pub(super) fn runtime_binary_fingerprint_in_dir(
    bin_path: &Path,
    version_arg: &str,
    work_dir: Option<&Path>,
) -> String {
    static RT_BIN_CACHE: OnceLock<Mutex<BTreeMap<String, (Instant, String)>>> = OnceLock::new();
    let bin = resolve_executable_on_path(bin_path);
    let real = bin.canonicalize().unwrap_or_else(|_| bin.clone());
    let stamp = file_stamp(&real).or_else(|| file_stamp(&bin));
    let dir_str = work_dir
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = format!(
        "{}\0{}\0{:?}\0{dir_str}",
        bin_path.to_string_lossy(),
        real.to_string_lossy(),
        stamp
    );
    let cache = RT_BIN_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some((captured, val)) = guard.get(&key)
        && captured.elapsed() < Duration::from_secs(300)
    {
        return val.clone();
    }
    let mut cmd = Command::new(&bin);
    if let Some(d) = work_dir
        && d.is_dir()
    {
        cmd.current_dir(d);
    }
    cmd.arg(version_arg).stdin(std::process::Stdio::null());
    let out_digest = crate::workspace::run_command_with_timeout(
        &mut cmd,
        Duration::from_secs(5),
        16 * 1024,
    )
    .ok()
    .filter(|o| o.status.success() && !o.timed_out)
    .map(|o| sha256_hex(&o.stdout))
    .unwrap_or_else(|| "unavailable".to_string());
    let fp = sha256_hex(format!("{key}\0{out_digest}").as_bytes());
    if let Ok(mut guard) = cache.lock() {
        // Bound cache at 2048 entries, evicting expired entries first instead of clearing all.
        if guard.len() >= 2048 {
            guard.retain(|_, (captured, _)| captured.elapsed() < Duration::from_secs(300));
            if guard.len() >= 2048 {
                let overflow = guard.len() - 1536;
                let oldest: Vec<String> = guard.keys().take(overflow).cloned().collect();
                for k in oldest {
                    guard.remove(&k);
                }
            }
        }
        guard.insert(key, (Instant::now(), fp.clone()));
    }
    fp
}

pub(super) fn fingerprint_runner_cmd(evidence: &mut Vec<u8>, label: &str, cmd: &[String]) {
    evidence.extend_from_slice(label.as_bytes());
    evidence.push(b'=');
    if let Some((prog, args)) = cmd.split_first() {
        let bin = resolve_executable_on_path(Path::new(prog));
        let real = bin.canonicalize().unwrap_or_else(|_| bin.clone());
        let stamp = file_stamp(&real).or_else(|| file_stamp(&bin));
        evidence.extend_from_slice(prog.as_bytes());
        evidence.push(b'|');
        evidence.extend_from_slice(real.to_string_lossy().as_bytes());
        evidence.push(b'|');
        evidence.extend_from_slice(format!("{stamp:?}").as_bytes());
        for arg in args {
            evidence.push(0);
            evidence.extend_from_slice(arg.as_bytes());
        }
    }
    evidence.push(b'\n');
}

pub(super) fn collect_frontend_env_dirs(spec: &Spec, graph: Option<&DependencyGraph>) -> BTreeSet<PathBuf> {
    let mut fe_dirs = BTreeSet::new();
    fe_dirs.insert(spec.root.clone());
    if let Some(g) = graph {
        for (p, node) in &g.files {
            if p.ends_with("/package.json")
                || p.contains("/vite.config.")
                || p.contains("/vitest.config.")
                || p.starts_with("vite.config.")
                || p.starts_with("vitest.config.")
            {
                let cfg_dir = Path::new(p)
                    .parent()
                    .map(|d| spec.root.join(d))
                    .unwrap_or_else(|| spec.root.clone());
                fe_dirs.insert(cfg_dir.clone());
                let mut effective_roots: Vec<PathBuf> = vec![cfg_dir.clone()];
                for sym in &node.parsed.defined_symbols {
                    if let Some(vr) = sym.strip_prefix("fe_vite_root:") {
                        let cand = cfg_dir.join(vr);
                        if cand.is_dir() {
                            effective_roots.push(cand.clone());
                            fe_dirs.insert(cand);
                        }
                    } else if let Some(vr_cwd) = sym.strip_prefix("fe_vite_root_cwd:") {
                        for base in [&cfg_dir, &spec.root] {
                            let cand = base.join(vr_cwd);
                            if cand.is_dir() {
                                if !effective_roots.contains(&cand) {
                                    effective_roots.push(cand.clone());
                                }
                                fe_dirs.insert(cand);
                            }
                        }
                    }
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(ed) = sym.strip_prefix("fe_vite_env_dir:") {
                        for eff_r in &effective_roots {
                            let cand = eff_r.join(ed);
                            if cand.is_dir() {
                                fe_dirs.insert(cand);
                            }
                        }
                    } else if let Some(ed_cfg) = sym.strip_prefix("fe_vite_env_dir_cfg:") {
                        let cand = cfg_dir.join(ed_cfg);
                        if cand.is_dir() {
                            fe_dirs.insert(cand);
                        }
                    }
                }
            }
        }
    }
    fe_dirs
}

/// Checks whether `node_modules` under `dir` contains a symlink pointing outside `canon_root`
/// (e.g. `npm link ../external-pkg` or `file:../outside`), whose source edits are invisible to
/// workspace watchers.
/// Checks whether `node_modules` under `dir` contains any symlink that points outside
/// `canon_root`, points to an unresolvable target, OR points to a workspace-internal package
/// whose target or built artifacts (`dist`/`build`/`out`) are gitignored/excluded (excluding
/// pnpm's internal `node_modules/.pnpm` store links). Also checks symlinked `node_modules/`
/// and `node_modules/@scope/` directories themselves and fails closed at the scan budget.
pub(super) fn dir_has_external_linked_node_modules(
    dir: &Path,
    canon_root: &Path,
    workspace: Option<&Workspace>,
) -> bool {
    let nm = dir.join("node_modules");
    let link_target_is_untracked = |link_path: &Path| -> bool {
        let Ok(target) = std::fs::canonicalize(link_path) else {
            return true;
        };
        if !target.starts_with(canon_root) {
            return true;
        }
        let Ok(rel) = target.strip_prefix(canon_root) else {
            return true;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        // pnpm internal store symlinks (`node_modules/.pnpm/...`) are lockfile-pinned packages.
        if rel_str.contains("node_modules/.pnpm/") || rel_str.starts_with("node_modules/.pnpm/") {
            return false;
        }
        // Workspace-linked package (`packages/shared`) whose root or compiled output (`dist/`,
        // `build/`, `out/`) is excluded/gitignored cannot be safely tracked via source AST alone.
        if let Some(ws) = workspace {
            if ws.is_excluded_or_ignored_rel(rel) {
                return true;
            }
            for gen_dir in ["dist", "build", "out", "lib"] {
                let gd = target.join(gen_dir);
                if gd.is_dir() && ws.is_excluded_or_ignored_rel(&rel.join(gen_dir)) {
                    return true;
                }
            }
        }
        false
    };
    if let Ok(nm_meta) = std::fs::symlink_metadata(&nm)
        && nm_meta.file_type().is_symlink()
    {
        return link_target_is_untracked(&nm);
    }
    let Ok(entries) = std::fs::read_dir(&nm) else {
        return false;
    };
    let mut checked = 0usize;
    for entry in entries.flatten() {
        checked += 1;
        if checked > 2048 {
            return true;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with('.') {
            continue;
        }
        let p = entry.path();
        if name_s.starts_with('@') {
            // Check if the `@scope` directory ITSELF is a symlink (e.g. externally linked scope)!
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
                && link_target_is_untracked(&p)
            {
                return true;
            }
            if let Ok(scoped) = std::fs::read_dir(&p) {
                for se in scoped.flatten() {
                    checked += 1;
                    if checked > 2048 {
                        return true;
                    }
                    let sp = se.path();
                    if std::fs::symlink_metadata(&sp)
                        .is_ok_and(|m| m.file_type().is_symlink())
                        && link_target_is_untracked(&sp)
                    {
                        return true;
                    }
                }
            }
        } else if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
            && link_target_is_untracked(&p)
        {
            return true;
        }
    }
    false
}

pub(super) const PY_ENV_UNCACHEABLE_SENTINEL: &str = "__TCTX_PY_UNCACHEABLE__";

pub(super) fn node_options_has_untracked_preload() -> bool {
    let Ok(val) = std::env::var("NODE_OPTIONS") else {
        return false;
    };
    // Parse Node's quoted option syntax (e.g. `"--require" "/outside/preload.cjs"`) as well as
    // standard unquoted tokens so quoted preload flags never bypass fail-closed tracking.
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_q: Option<char> = None;
    let mut chars = val.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(q) = in_q {
            if ch == '\\' && q == '"' && let Some(next_ch) = chars.next() {
                cur.push(next_ch);
            } else if ch == q {
                in_q = None;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_q = Some(ch);
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens.iter().any(|tok| {
        tok == "-r"
            || tok == "--require"
            || tok.starts_with("--require=")
            || tok == "--import"
            || tok.starts_with("--import=")
            || tok == "--loader"
            || tok.starts_with("--loader=")
            || tok == "--experimental-loader"
            || tok.starts_with("--experimental-loader=")
    })
}

pub(super) fn frontend_env_is_uncacheable(
    spec: &Spec,
    graph: Option<&DependencyGraph>,
    workspace: Option<&Workspace>,
) -> bool {
    let has_fe_files = graph.is_some_and(|g| {
        g.files
            .keys()
            .any(|p| crate::ast_graph::is_frontend_source_path(p))
    });
    if !has_fe_files {
        return false;
    }
    let fe_dirs = collect_frontend_env_dirs(spec, graph);
    frontend_environment_fingerprint_and_cacheability(spec, &fe_dirs, workspace).1
}

pub(super) fn python_env_is_uncacheable(spec: &Spec, graph: &DependencyGraph) -> bool {
    let has_py = !spec.pytest_cmd.is_empty() || graph.files.keys().any(|p| p.ends_with(".py"));
    if !has_py {
        return false;
    }
    let (ident, plugs) = python_runtime_identity(spec);
    ident.contains(PY_ENV_UNCACHEABLE_SENTINEL)
        || plugs.iter().any(|p| p.contains(PY_ENV_UNCACHEABLE_SENTINEL))
}

pub(super) fn frontend_environment_fingerprint_and_cacheability(
    spec: &Spec,
    fe_dirs: &BTreeSet<PathBuf>,
    workspace: Option<&Workspace>,
) -> (String, bool) {
    let mut evidence = Vec::new();
    let node_bin = std::env::var_os("TCTX_NODE_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"));
    evidence.extend_from_slice(b"fe-env-v4\nnode_runtime=");
    evidence.extend_from_slice(
        runtime_binary_fingerprint_in_dir(&node_bin, "-v", Some(&spec.root)).as_bytes(),
    );
    evidence.push(b'\n');
    let root_fe_cmd = spec.effective_frontend_test_cmd();
    fingerprint_runner_cmd(&mut evidence, "fe_runner_cmd", &root_fe_cmd);
    for key in [
        "NODE_OPTIONS",
        "NODE_ENV",
        "NODE_PATH",
        "NODE_EXTRA_CA_CERTS",
        "CI",
        "TZ",
        "LANG",
        "LC_ALL",
    ] {
        evidence.extend_from_slice(key.as_bytes());
        evidence.push(b'=');
        if let Some(value) = std::env::var_os(key) {
            evidence.extend_from_slice(sha256_hex(value.as_encoded_bytes()).as_bytes());
        }
        evidence.push(b'\n');
    }
    let has_node_path = std::env::var_os("NODE_PATH").is_some_and(|v| !v.is_empty());
    let mut uncacheable = node_options_has_untracked_preload() || has_node_path;
    let canon_root = std::fs::canonicalize(&spec.root).unwrap_or_else(|_| spec.root.clone());
    for dir in fe_dirs {
        // Also fingerprint any nested package's effective frontend runner (e.g. `packages/web/node_modules/.bin/vitest`).
        if let Ok(rel_pkg) = dir.strip_prefix(&spec.root) {
            let rel_s = rel_pkg.to_string_lossy().replace('\\', "/");
            if !rel_s.is_empty() {
                let (pkg_cmd, _) = crate::catalog::detect_frontend_runner_for_dir(
                    &spec.root,
                    &rel_s,
                    spec.managed_parent.as_deref(),
                );
                fingerprint_runner_cmd(
                    &mut evidence,
                    &format!("fe_pkg_runner:{rel_s}"),
                    &pkg_cmd,
                );
            }
        }
        if dir_has_external_linked_node_modules(dir, &canon_root, workspace) {
            // Mark frontend tests uncacheable rather than rotating a nonce inside `dep_hash`,
            // so pre- and post-run hashes remain stable and never trigger false SOURCE_CHANGED_DURING_TEST!
            uncacheable = true;
            evidence.extend_from_slice(b"fe_ext_link=true\n");
        }
        // Stamp package-manager integrity manifests inside `node_modules/` so package reinstalls
        // or in-place updates without lockfile edits invalidate cached frontend results.
        for nm_manifest in [
            "node_modules/.package-lock.json",
            "node_modules/.modules.yaml",
            "node_modules/.yarn-integrity",
            "node_modules/.pnpm/lock.yaml",
        ] {
            let mp = dir.join(nm_manifest);
            if let Some(st) = file_stamp(&mp) {
                evidence.extend_from_slice(b"fe_nm_manifest:");
                evidence.extend_from_slice(mp.to_string_lossy().as_bytes());
                evidence.push(b'=');
                evidence.extend_from_slice(format!("{st:?}").as_bytes());
                evidence.push(b'\n');
            }
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut env_files: Vec<PathBuf> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name();
                    let s = name.to_str()?;
                    (s == ".env" || s.starts_with(".env.")).then(|| e.path())
                })
                .collect();
            env_files.sort();
            for cand in env_files {
                if cand.is_file() {
                    evidence.extend_from_slice(b"fe_dotenv:");
                    evidence.extend_from_slice(cand.to_string_lossy().as_bytes());
                    evidence.push(b'=');
                    if let Ok(bytes) = std::fs::read(&cand) {
                        evidence.extend_from_slice(sha256_hex(&bytes).as_bytes());
                    } else {
                        evidence.extend_from_slice(b"unreadable");
                    }
                    evidence.push(b'\n');
                }
            }
        }
    }
    (sha256_hex(&evidence), uncacheable)
}

pub(super) fn frontend_environment_fingerprint_for_graph(
    spec: &Spec,
    graph: Option<&DependencyGraph>,
) -> String {
    let has_fe_files = graph.is_some_and(|g| {
        g.files
            .keys()
            .any(|p| crate::ast_graph::is_frontend_source_path(p))
    });
    if !has_fe_files {
        return String::new();
    }
    let fe_dirs = collect_frontend_env_dirs(spec, graph);
    frontend_environment_fingerprint_and_cacheability(spec, &fe_dirs, None).0
}
