use super::*;

pub(super) fn environment_fingerprint_with_python(
    spec: &Spec,
    include_python: bool,
    graph: Option<&DependencyGraph>,
) -> String {
    let has_rs_files = graph.is_some_and(|g| {
        g.files
            .keys()
            .any(|p| p.ends_with(".rs") || p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
    });
    let rs_tc_dirs = if has_rs_files {
        rust_toolchain_dirs_for_graph(&spec.root, graph)
    } else {
        Vec::new()
    };
    let cargo_cfg_cands = cargo_config_candidates_for_spec(spec, graph);
    environment_fingerprint_from_parts(spec, include_python, has_rs_files, &rs_tc_dirs, &cargo_cfg_cands)
}

pub(super) fn environment_fingerprint_from_parts(
    spec: &Spec,
    include_python: bool,
    has_rs_files: bool,
    rs_tc_dirs: &[PathBuf],
    cargo_cfg_cands: &BTreeSet<PathBuf>,
) -> String {
    let (pytest_sig, runtime_identity, plugins) = if include_python {
        let pytest_cmd = spec.effective_pytest_cmd();
        let (ident, plugs) = python_runtime_identity(spec);
        (pytest_cmd.join("\0"), ident, plugs)
    } else {
        (
            "non-python".to_string(),
            "rust-only".to_string(),
            Vec::new(),
        )
    };
    let mut evidence = Vec::new();
    evidence.extend_from_slice(b"tctx-env-v4\n");
    evidence.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
    evidence.push(b'\n');
    evidence.extend_from_slice(spec.root.to_string_lossy().as_bytes());
    evidence.push(b'\n');
    evidence.extend_from_slice(pytest_sig.as_bytes());
    evidence.push(b'\n');

    evidence.extend_from_slice(runtime_identity.as_bytes());
    for plugin in plugins {
        evidence.extend_from_slice(b"plugin:");
        evidence.extend_from_slice(plugin.as_bytes());
        evidence.push(b'\n');
    }

    // Fingerprint any symlinked `.cargo/config[.toml]` OR symlinked `.cargo/` directory at the
    // workspace root, scope ancestors, or indexed subcrate dirs (which inventory skips under
    // `O_NOFOLLOW`) by link target + resolved content digest so edits/retargets invalidate cache.
    for cand in cargo_cfg_cands {
        if cargo_config_traverses_symlink(&spec.root, cand) {
            evidence.extend_from_slice(b"cargo-symlink:");
            evidence.extend_from_slice(cand.to_string_lossy().as_bytes());
            evidence.push(b'=');
            if let Ok(real_p) = std::fs::canonicalize(cand) {
                evidence.extend_from_slice(real_p.to_string_lossy().as_bytes());
            } else if let Ok(target) = std::fs::read_link(cand) {
                evidence.extend_from_slice(target.to_string_lossy().as_bytes());
            }
            evidence.push(b':');
            if let Ok(mut f) = std::fs::File::open(cand) {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(
                    &mut std::io::Read::take(&mut f, 1024 * 1024),
                    &mut buf,
                );
                evidence.extend_from_slice(sha256_hex(&buf).as_bytes());
            } else {
                evidence.extend_from_slice(b"unreadable");
            }
            evidence.push(b'\n');
        }
    }

    // Fingerprint effective external Cargo configs (ancestor `.cargo/config[.toml]` above root
    // and `$CARGO_HOME`/`~/.cargo/config[.toml]`) so external `[env]` / `rustflags` changes
    // invalidate cached Rust test results immediately.
    for ext_cfg in effective_external_cargo_configs(spec) {
        evidence.extend_from_slice(b"cargo-ext:");
        evidence.extend_from_slice(ext_cfg.to_string_lossy().as_bytes());
        evidence.push(b'=');
        if let Ok(mut f) = std::fs::File::open(&ext_cfg) {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(
                &mut std::io::Read::take(&mut f, 1024 * 1024),
                &mut buf,
            );
            evidence.extend_from_slice(sha256_hex(&buf).as_bytes());
        } else {
            evidence.extend_from_slice(b"unreadable");
        }
        evidence.push(b'\n');
    }

    let mut env_keys = vec![
        "TZ".to_string(),
        "LANG".to_string(),
        "LC_ALL".to_string(),
        "RUSTC".to_string(),
        "RUSTC_BOOTSTRAP".to_string(),
        "RUSTC_WRAPPER".to_string(),
        "RUSTC_WORKSPACE_WRAPPER".to_string(),
        "RUSTDOC".to_string(),
        "RUSTDOCFLAGS".to_string(),
        "CARGO_ENCODED_RUSTDOCFLAGS".to_string(),
        "RUSTFLAGS".to_string(),
        "CARGO_BUILD_RUSTFLAGS".to_string(),
        "CARGO_ENCODED_RUSTFLAGS".to_string(),
        "CARGO_BUILD_TARGET".to_string(),
        "CARGO_TARGET_DIR".to_string(),
        "NODE_ENV".to_string(),
    ];
    if has_rs_files {
        fingerprint_runner_cmd(&mut evidence, "cargo_runner_cmd", &spec.effective_cargo_cmd());
        // Fingerprint all active `CARGO_TARGET_*_{RUSTFLAGS,LINKER,RUNNER}` AND `CARGO_PROFILE_*`
        // env overrides (sorted deterministically) plus the resolved `rustc -vV` toolchain for root
        // and any nested crate with its own toolchain.
        let mut cargo_override_envs: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            let ks = k.to_string_lossy();
            let is_target_override = ks.starts_with("CARGO_TARGET_")
                && (ks.ends_with("_RUSTFLAGS") || ks.ends_with("_LINKER") || ks.ends_with("_RUNNER"));
            let is_profile_override = ks.starts_with("CARGO_PROFILE_");
            if is_target_override || is_profile_override {
                cargo_override_envs.insert(ks.into_owned(), sha256_hex(v.as_encoded_bytes()));
            }
        }
        for (ks, digest) in cargo_override_envs {
            evidence.extend_from_slice(ks.as_bytes());
            evidence.push(b'=');
            evidence.extend_from_slice(digest.as_bytes());
            evidence.push(b'\n');
        }
        for tc_dir in rs_tc_dirs {
            let rustc_id = rust_toolchain_fingerprint(tc_dir);
            evidence.extend_from_slice(b"rustc_toolchain:");
            evidence.extend_from_slice(tc_dir.to_string_lossy().as_bytes());
            evidence.push(b'=');
            evidence.extend_from_slice(rustc_id.as_bytes());
            evidence.push(b'\n');
        }
    }
    if include_python {
        for py_key in [
            "PYTHONPATH",
            "PYTHONHASHSEED",
            "PYTHONOPTIMIZE",
            "PYTHONNOUSERSITE",
            "PYTHONUSERBASE",
            "PYTHONUTF8",
            "PYTHONCOERCECLOCALE",
            "PYTHONWARNINGS",
            "PYTHONDEVMODE",
            "PYTHONFAULTHANDLER",
            "PYTHONDONTWRITEBYTECODE",
            "PYTHONSAFEPATH",
            "PYTHONIOENCODING",
            "PYTEST_ADDOPTS",
            "PYTEST_PLUGINS",
            "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
        ] {
            env_keys.push(py_key.to_string());
        }
    }
    if let Some(extra) = std::env::var_os("TCTX_ENV_KEYS") {
        env_keys.extend(
            extra
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|value| {
                    !value.is_empty()
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .map(ToOwned::to_owned),
        );
    }
    env_keys.sort();
    env_keys.dedup();
    for key in env_keys {
        evidence.extend_from_slice(key.as_bytes());
        evidence.push(b'=');
        if let Some(value) = std::env::var_os(&key) {
            evidence.extend_from_slice(sha256_hex(value.to_string_lossy().as_bytes()).as_bytes());
        }
        evidence.push(b'\n');
    }
    sha256_hex(&evidence)
}

pub(super) fn pytest_plugin_manifest(spec: &Spec) -> Vec<String> {
    python_runtime_identity(spec).1
}

pub(super) fn pytest_plugin_manifest_for_targets(spec: &Spec, targets: &[String]) -> Vec<String> {
    if targets.iter().any(|t| {
        let f = t.split("::").next().unwrap_or(t);
        !f.ends_with(".rs") && !crate::ast_graph::is_frontend_source_path(f)
    }) {
        pytest_plugin_manifest(spec)
    } else {
        Vec::new()
    }
}

#[derive(Clone)]
pub(super) struct CachedPythonRuntime {
    pub(super) captured: Instant,
    pub(super) captured_unix_ms: u64,
    pub(super) identity: String,
    pub(super) plugins: Vec<String>,
    pub(super) has_editable_plugins: bool,
    pub(super) plugin_locs: Vec<PathBuf>,
    pub(super) plugin_loc_digest: String,
    pub(super) sys_path_dirs: Vec<PathBuf>,
    pub(super) sys_path_digest: String,
    pub(super) missing_pth_paths: Vec<PathBuf>,
}

/// Resolves an executable name or relative path using the EXACT augmented `PATH` supplied to
/// subprocesses (`crate::catalog::augmented_user_path()` first, falling back to live `PATH`),
/// resolving relative paths and relative `PATH` entries against `cwd` and checking OS effective
/// execute permission (`access(X_OK)` on Unix).
pub(crate) fn resolve_executable_on_path_in_dir(bin: &Path, cwd: &Path) -> PathBuf {
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

pub(crate) fn resolve_executable_on_path(bin: &Path) -> PathBuf {
    resolve_executable_on_path_in_dir(bin, Path::new("."))
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct PersistedPythonRuntime {
    pub(super) identity: String,
    pub(super) plugins: Vec<String>,
    pub(super) updated_unix_ms: u64,
    #[serde(default)]
    pub(super) has_editable_plugins: bool,
    #[serde(default)]
    pub(super) plugin_locs: Vec<PathBuf>,
    #[serde(default)]
    pub(super) plugin_loc_digest: String,
    #[serde(default)]
    pub(super) sys_path_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub(super) sys_path_digest: String,
    #[serde(default)]
    pub(super) missing_pth_paths: Vec<PathBuf>,
}

/// Fast Rust stat digest over external `sys.path` entries (directories including `.pth`-added
/// plugin dirs AND archive files like `.zip`/`.whl` on `PYTHONPATH`, resolved from their lexical
/// paths so retargeting a symlinked archive invalidates immediately).
pub(super) fn stamp_sys_path_dirs_digest(entries_list: &[PathBuf]) -> Option<String> {
    if entries_list.is_empty() {
        return Some("empty".to_string());
    }
    const DIST_META_FILES: &[&str] = &[
        "entry_points.txt",
        "RECORD",
        "METADATA",
        "PKG-INFO",
        "direct_url.json",
        "INSTALLER",
        "WHEEL",
        "top_level.txt",
        "requires.txt",
        "dependency_links.txt",
    ];
    let mut entries: BTreeMap<PathBuf, (PathBuf, crate::ast_graph::FileStamp)> = BTreeMap::new();
    for entry_path in entries_list {
        let resolved = entry_path.canonicalize().ok()?;
        let meta = std::fs::metadata(&resolved).ok()?;
        if meta.is_file() {
            let stamp = file_stamp(&resolved)?;
            entries.insert(entry_path.clone(), (resolved, stamp));
            continue;
        }
        let stamp = dir_stamp(&resolved)?;
        entries.insert(entry_path.clone(), (resolved.clone(), stamp));
        let rd = std::fs::read_dir(&resolved).ok()?;
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.ends_with(".pth")
                || matches!(name_str, "sitecustomize.py" | "usercustomize.py")
            {
                let p = entry.path();
                if let Some(fs) = file_stamp(&p) {
                    let pr = p.canonicalize().unwrap_or_else(|_| p.clone());
                    entries.insert(p, (pr, fs));
                }
            } else if name_str.ends_with(".dist-info") || name_str.ends_with(".egg-info") {
                for meta_name in DIST_META_FILES {
                    let mp = entry.path().join(meta_name);
                    if let Some(fs) = file_stamp(&mp) {
                        let mpr = mp.canonicalize().unwrap_or_else(|_| mp.clone());
                        entries.insert(mp, (mpr, fs));
                    }
                }
            }
        }
    }
    Some(sha256_hex(format!("{entries:?}").as_bytes()))
}

/// Fast Rust recursive stat digest over installed `pytest11` plugin source trees under
/// `site-packages`/`dist-packages` (including subdirectories so empty subdir creation invalidates,
/// AND sourceless `.pyc`/`.pyo` modules outside `__pycache__`). Returns `None` on symlinks, I/O
/// errors, or oversized trees so the caller immediately re-runs the full Python fingerprint probe.
pub(super) fn stamp_plugin_locs_digest(locs: &[PathBuf]) -> Option<String> {
    if locs.is_empty() {
        return Some("empty".to_string());
    }
    let mut entries: BTreeMap<PathBuf, crate::ast_graph::FileStamp> = BTreeMap::new();
    let mut stack: Vec<PathBuf> = locs.to_vec();
    let mut count = 0usize;
    while let Some(path) = stack.pop() {
        let sym_meta = std::fs::symlink_metadata(&path).ok()?;
        if sym_meta.file_type().is_symlink() {
            return None;
        }
        if sym_meta.is_file() {
            count += 1;
            if count > 2048 {
                return None;
            }
            let stamp = file_stamp(&path)?;
            entries.insert(path, stamp);
        } else if sym_meta.is_dir() {
            let dstamp = dir_stamp(&path)?;
            entries.insert(path.clone(), dstamp);
            let rd = std::fs::read_dir(&path).ok()?;
            for entry in rd {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let name_s = name.to_string_lossy();
                let ep = entry.path();
                let em = std::fs::symlink_metadata(&ep).ok()?;
                if em.file_type().is_symlink() {
                    return None;
                }
                if em.is_dir() {
                    if matches!(
                        name_s.as_ref(),
                        "__pycache__"
                            | ".git"
                            | ".hg"
                            | ".svn"
                            | ".venv"
                            | "node_modules"
                            | ".pytest_cache"
                            | ".mypy_cache"
                            | ".ruff_cache"
                            | ".eggs"
                    ) || (name_s.as_ref() == "target"
                        && (path.join("Cargo.toml").is_file()
                            || ep.join("CACHEDIR.TAG").is_file()))
                        || name_s.ends_with(".egg-info")
                        || name_s.ends_with(".dist-info")
                    {
                        continue;
                    }
                    stack.push(ep);
                } else if em.is_file() {
                    if name_s == ".DS_Store" {
                        continue;
                    }
                    if (name_s.ends_with(".pyc") || name_s.ends_with(".pyo"))
                        && ep.with_extension("py").is_file()
                    {
                        continue;
                    }
                    count += 1;
                    if count > 2048 {
                        return None;
                    }
                    let stamp = file_stamp(&ep)?;
                    entries.insert(ep, stamp);
                }
            }
        }
    }
    Some(sha256_hex(format!("{entries:?}").as_bytes()))
}

pub(super) fn dir_stamp(path: &Path) -> Option<crate::ast_graph::FileStamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some(crate::ast_graph::FileStamp {
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        size: meta.len(),
        ino: meta.ino(),
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
}

pub(super) fn detect_python_site_packages_stamps(
    workspace_root: &Path,
    python_bin: &Path,
    pythonpath: &std::ffi::OsStr,
) -> Vec<(PathBuf, crate::ast_graph::FileStamp)> {
    // Check ALL relevant site-packages / dist-packages directories (venv prefix, canonical system
    // prefix, user `~/.local/lib/python*`, and `PYTHONPATH` dirs) via `dir_stamp` so installing a
    // new pytest11 plugin anywhere immediately invalidates the Python runtime cache key.
    let mut prefixes = Vec::new();
    if let Some(p) = python_bin.parent().and_then(Path::parent) {
        prefixes.push(p.to_path_buf());
    }
    if let Ok(resolved) = python_bin.canonicalize()
        && let Some(p) = resolved.parent().and_then(Path::parent)
        && !prefixes.iter().any(|existing| existing == p)
    {
        prefixes.push(p.to_path_buf());
    }
    if let Some(userbase) = std::env::var_os("PYTHONUSERBASE")
        && !userbase.is_empty()
    {
        let ub = PathBuf::from(userbase);
        if !prefixes.iter().any(|existing| existing == &ub) {
            prefixes.push(ub);
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let local_prefix = home.join(".local");
        if !prefixes.iter().any(|existing| existing == &local_prefix) {
            prefixes.push(local_prefix);
        }
    }
    let mut stamps: BTreeMap<PathBuf, crate::ast_graph::FileStamp> = BTreeMap::new();
    let record_sp_and_startup_files =
        |dir: &Path, out: &mut BTreeMap<PathBuf, crate::ast_graph::FileStamp>| {
            if let Some(stamp) = dir_stamp(dir) {
                out.insert(dir.to_path_buf(), stamp);
            }
            // Stamp ALL `.pth`, `sitecustomize.py`, `usercustomize.py`, and top-level `.py` /
            // `<pkg>/__init__.py` entrypoints inside site-packages/PYTHONPATH directories (no cap
            // on `.pth`/customize files!) so in-place edits invalidate Python runtime identity.
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    if name_str.ends_with(".pth")
                        || name_str.ends_with(".py")
                        || matches!(name_str, "sitecustomize.py" | "usercustomize.py")
                    {
                        let p = entry.path();
                        if let Some(fs) = file_stamp(&p) {
                            out.insert(p, fs);
                        }
                    } else if name_str.ends_with(".dist-info") || name_str.ends_with(".egg-info") {
                        for meta_name in [
                            "entry_points.txt",
                            "RECORD",
                            "METADATA",
                            "PKG-INFO",
                            "direct_url.json",
                            "INSTALLER",
                            "WHEEL",
                            "top_level.txt",
                            "requires.txt",
                            "dependency_links.txt",
                        ] {
                            let mp = entry.path().join(meta_name);
                            if let Some(fs) = file_stamp(&mp) {
                                out.insert(mp, fs);
                            }
                        }
                    } else if !name_str.starts_with('.') && name_str != "__pycache__" {
                        let init_p = entry.path().join("__init__.py");
                        if let Some(fs) = file_stamp(&init_p) {
                            out.insert(init_p, fs);
                        }
                    }
                }
            }
        };
    for prefix in prefixes {
        // Also check Conda/Windows-style `<prefix>/Lib/site-packages`.
        let direct_lib_sp = prefix.join("Lib").join("site-packages");
        record_sp_and_startup_files(&direct_lib_sp, &mut stamps);
        for lib_name in ["lib", "lib64"] {
            let lib_dir = prefix.join(lib_name);
            let Ok(entries) = std::fs::read_dir(lib_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if name_str.starts_with("python") {
                    for sub in ["site-packages", "dist-packages"] {
                        let sp = entry.path().join(sub);
                        record_sp_and_startup_files(&sp, &mut stamps);
                    }
                }
            }
        }
    }
    // Only treat `site-packages` / `dist-packages` dirs (not the workspace root itself, whose
    // mtime changes on every file creation like `src/ui/button.test.mjs`!) as site dirs when
    // building the Python runtime identity key.
    let ws_root_canon = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    for dir in std::env::split_paths(pythonpath) {
        if !dir.as_os_str().is_empty() {
            let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if dir_canon == ws_root_canon || dir_canon.starts_with(&ws_root_canon) {
                // Only stamp startup files inside workspace `PYTHONPATH` dirs, NOT the dir mtime
                // itself (which changes whenever any test/source file is added to the repo!).
                for fname in ["sitecustomize.py", "usercustomize.py"] {
                    let p = dir.join(fname);
                    if let Some(fs) = file_stamp(&p) {
                        stamps.insert(p, fs);
                    }
                }
            } else {
                record_sp_and_startup_files(&dir, &mut stamps);
            }
        }
    }
    stamps.into_iter().collect()
}

pub(super) static PYTHON_RUNTIME_CACHE: OnceLock<Mutex<BTreeMap<String, CachedPythonRuntime>>> =
    OnceLock::new();

pub(super) fn compute_python_runtime_cache_key(
    spec: &Spec,
) -> (PathBuf, std::ffi::OsString, String, String) {
    let python_raw =
        crate::zygote::resolve_python_bin_for_root(&spec.root, &spec.effective_pytest_cmd());
    let python = resolve_executable_on_path_in_dir(&python_raw, &spec.root);
    // Resolve symlinks before stamping so in-place target replacement of a symlinked Python
    // binary (e.g. `.venv/bin/python -> /usr/bin/python3.12`) invalidates immediately.
    let python_real = python.canonicalize().unwrap_or_else(|_| python.clone());
    let bin_mtime = file_stamp(&python_real).or_else(|| file_stamp(&python));
    let pythonpath = build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH"));
    let sp_stamps = detect_python_site_packages_stamps(&spec.root, &python, &pythonpath);
    let startup_env_fp: String = [
        "PYTHONOPTIMIZE",
        "PYTHONNOUSERSITE",
        "PYTHONUSERBASE",
        "PYTHONUTF8",
        "PYTHONWARNINGS",
        "PYTHONDEVMODE",
        "PYTHONDONTWRITEBYTECODE",
        "PYTHONSAFEPATH",
        "PYTHONIOENCODING",
        "PYTEST_PLUGINS",
    ]
    .iter()
    .map(|k| match std::env::var_os(k) {
        Some(v) => format!("{k}=S:{}", sha256_hex(v.as_encoded_bytes())),
        None => format!("{k}=U"),
    })
    .collect::<Vec<_>>()
    .join(";");
    let startup_files_digest = sha256_hex(format!("{sp_stamps:?}\0{startup_env_fp}").as_bytes());
    let cache_key = format!(
        "{}\0{}\0{}\0{:?}\0{}\0{}",
        spec.root.to_string_lossy(),
        pythonpath.to_string_lossy(),
        python.to_string_lossy(),
        bin_mtime,
        startup_files_digest,
        spec.effective_pytest_cmd().join("\0")
    );
    (python, pythonpath, cache_key, startup_files_digest)
}

pub(super) const PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL: &str = "__TCTX_PLUGIN_DISCOVERY_UNAVAILABLE__";

pub(super) fn python_runtime_identity(spec: &Spec) -> (String, Vec<String>) {
    static DISK_LOADED: OnceLock<()> = OnceLock::new();
    static FAILURE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unavailable = || {
        let nonce = FAILURE_NONCE.fetch_add(1, Ordering::Relaxed);
        (
            format!("runtime-identity-unavailable:{}:{nonce}", now_unix_ms()),
            vec![PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL.to_string()],
        )
    };
    let (python, pythonpath, cache_key, startup_files_digest) =
        compute_python_runtime_cache_key(spec);
    let cache = PYTHON_RUNTIME_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    DISK_LOADED.get_or_init(|| {
        if let Ok(state_dir) = crate::catalog::state_dir()
            && let Ok(bytes) = std::fs::read(state_dir.join("python-runtime-cache.json"))
            && let Ok(persisted) =
                serde_json::from_slice::<BTreeMap<String, PersistedPythonRuntime>>(&bytes)
        {
            let mut guard = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now_ms = now_unix_ms();
            let now = Instant::now();
            for (k, v) in persisted {
                let age_ms = now_ms.saturating_sub(v.updated_unix_ms);
                // Honor persisted expiry across restarts instead of resetting TTL to 1 hour.
                if age_ms >= 3_600_000 {
                    continue;
                }
                let captured = now
                    .checked_sub(Duration::from_millis(age_ms))
                    .unwrap_or(now);
                guard.entry(k).or_insert(CachedPythonRuntime {
                    captured,
                    captured_unix_ms: v.updated_unix_ms,
                    identity: v.identity,
                    plugins: v.plugins,
                    has_editable_plugins: v.has_editable_plugins,
                    plugin_locs: v.plugin_locs,
                    plugin_loc_digest: v.plugin_loc_digest,
                    sys_path_dirs: v.sys_path_dirs,
                    sys_path_digest: v.sys_path_digest,
                    missing_pth_paths: v.missing_pth_paths,
                });
            }
        }
    });
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        && !cached.has_editable_plugins
        && cached.captured.elapsed() < Duration::from_secs(300)
        && !cached.plugin_loc_digest.is_empty()
        && stamp_plugin_locs_digest(&cached.plugin_locs).as_deref()
            == Some(cached.plugin_loc_digest.as_str())
        && !cached.sys_path_digest.is_empty()
        && stamp_sys_path_dirs_digest(&cached.sys_path_dirs).as_deref()
            == Some(cached.sys_path_digest.as_str())
        && cached.missing_pth_paths.iter().all(|p| !p.exists())
    {
        return (cached.identity.clone(), cached.plugins.clone());
    }

    // Use top-level `find_spec` (never imports parent packages), hash ALL editable locations for
    // namespace packages so edits anywhere in a multi-location editable plugin invalidate cache,
    // and include a digest of ALL installed distributions (`m.distributions()`) — ALSO hashing any
    // external editable non-plugin dependencies (via PEP 610 `direct_url.json`) outside the
    // workspace so edits to e.g. `pip install -e ../shared-lib` immediately invalidate cached passes.
    let script = concat!(
        "import hashlib,importlib.metadata as m,importlib.util as u,json,os,platform,sys,time,urllib.parse,pytest\n",
        "ws_real=os.path.realpath(os.getcwd())\n",
        "dists=list(m.distributions())\n",
        "items=[ep for d in dists for ep in d.entry_points if ep.group=='pytest11']\n",
        "has_ed=False\n",
        "uncacheable=False\n",
        "sp_locs=set()\n",
        "def _dir_sig(root_dir):\n",
        "  global uncacheable\n",
        "  h=hashlib.sha256(); cnt=0; tot=0\n",
        "  for dp,dns,fns in os.walk(root_dir):\n",
        "    kept=[]\n",
        "    for d in sorted(dns):\n",
        "      if d in ('__pycache__','.git','.hg','.svn','.venv','node_modules','.pytest_cache','.mypy_cache','.ruff_cache','.eggs') or (d=='target' and (os.path.isfile(os.path.join(dp,'Cargo.toml')) or os.path.isfile(os.path.join(dp,d,'CACHEDIR.TAG')))) or d.endswith(('.egg-info','.dist-info')): continue\n",
        "      full_d=os.path.join(dp,d)\n",
        "      if os.path.islink(full_d): uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "      rel_d=os.path.relpath(full_d,root_dir).replace('\\\\','/')\n",
        "      h.update(b'D'); h.update(rel_d.encode('utf-8','surrogateescape')); h.update(b'\\0')\n",
        "      kept.append(d)\n",
        "    dns[:] = kept\n",
        "    for fn in sorted(fns):\n",
        "      if fn=='.DS_Store': continue\n",
        "      fp=os.path.join(dp,fn)\n",
        "      if fn.endswith(('.pyc','.pyo')) and os.path.isfile(fp[:-1]): continue\n",
        "      if os.path.islink(fp): uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "      rel=os.path.relpath(fp,root_dir).replace('\\\\','/')\n",
        "      cnt+=1\n",
        "      if cnt>2048: uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "      try:\n",
        "        with open(fp,'rb') as f: data=f.read(4194305)\n",
        "        tot+=len(data)\n",
        "        if len(data)>4194304 or tot>33554432: uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "        h.update(b'F'); h.update(rel.encode('utf-8','surrogateescape')); h.update(b'\\0'); h.update(data); h.update(b'\\0')\n",
        "      except OSError: uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "  return f'{cnt}:{h.hexdigest()}'\n",
        "def _file_sig(fp):\n",
        "  global uncacheable\n",
        "  try:\n",
        "    with open(fp,'rb') as f: data=f.read(4194305)\n",
        "    if len(data)>4194304: uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "    return hashlib.sha256(data).hexdigest()\n",
        "  except OSError: uncacheable=True; return '__TCTX_PY_UNCACHEABLE__'\n",
        "def _archive_of(p):\n",
        "  cur=p\n",
        "  while cur and cur!=os.path.dirname(cur):\n",
        "    if os.path.isfile(cur): return cur\n",
        "    cur=os.path.dirname(cur)\n",
        "  return None\n",
        "def _sig(ep):\n",
        "  global has_ed, uncacheable\n",
        "  v=getattr(getattr(ep,'dist',None),'version','')\n",
        "  st=''\n",
        "  try:\n",
        "    raw_mod=ep.value.split('[',1)[0].strip()\n",
        "    top=raw_mod.split(':',1)[0].strip().split('.',1)[0].strip()\n",
        "    sp=u.find_spec(top) if top else None\n",
        "    if sp is None and top:\n",
        "      has_ed=True; st=f'@UNRESOLVED:{time.time_ns()}'\n",
        "    else:\n",
        "      locs=list(getattr(sp,'submodule_search_locations',None) or []) if sp else []\n",
        "      dir_locs=[l for l in locs if isinstance(l,str) and os.path.isdir(l)]\n",
        "      o=getattr(sp,'origin',None) if sp else None\n",
        "      if o and isinstance(o,str) and not any(o.startswith(d.rstrip('/\\\\')+os.sep) for d in dir_locs): locs.append(o)\n",
        "      sts=[]\n",
        "      seen_archives=set()\n",
        "      loader_arch=getattr(getattr(sp,'loader',None),'archive',None) if sp else None\n",
        "      if isinstance(loader_arch,str) and loader_arch:\n",
        "        has_ed=True; seen_archives.add(loader_arch)\n",
        "        if os.path.isfile(loader_arch): sts.append(f'{loader_arch}={_file_sig(loader_arch)}')\n",
        "        else: uncacheable=True; sts.append(f'{loader_arch}=__TCTX_PY_UNCACHEABLE__')\n",
        "      for loc in sorted({l for l in locs if isinstance(l,str)}):\n",
        "        bn=os.path.basename(loc.rstrip('/\\\\'))\n",
        "        if bn in ('site-packages','dist-packages'): continue\n",
        "        in_sp=('site-packages' in loc or 'dist-packages' in loc)\n",
        "        if os.path.isdir(loc):\n",
        "          if not in_sp: has_ed=True\n",
        "          else: sp_locs.add(loc)\n",
        "          sts.append(f'{loc}={_dir_sig(loc)}')\n",
        "        elif os.path.isfile(loc):\n",
        "          if not in_sp: has_ed=True\n",
        "          else: sp_locs.add(loc)\n",
        "          sts.append(f'{loc}={_file_sig(loc)}')\n",
        "        else:\n",
        "          has_ed=True\n",
        "          arch=_archive_of(loc)\n",
        "          if arch:\n",
        "            if arch not in seen_archives:\n",
        "              seen_archives.add(arch); sts.append(f'{arch}={_file_sig(arch)}')\n",
        "          elif not seen_archives:\n",
        "            uncacheable=True; sts.append(f'{loc}=__TCTX_PY_UNCACHEABLE__')\n",
        "      if sts: st='@'+';'.join(sts)\n",
        "  except Exception:\n",
        "    has_ed=True; st=f'@ERR:{time.time_ns()}'\n",
        "  return f'{ep.name}={ep.value}=={v}{st}'\n",
        "def _dist_sig(d):\n",
        "  global has_ed, uncacheable\n",
        "  name=(getattr(d,'metadata',None) or {}).get('Name') or getattr(d,'name','')\n",
        "  ver=getattr(d,'version','')\n",
        "  mh=hashlib.sha256()\n",
        "  for mf in ('METADATA','PKG-INFO','entry_points.txt','RECORD','direct_url.json','INSTALLER','WHEEL','top_level.txt','requires.txt','dependency_links.txt'):\n",
        "    try:\n",
        "      txt=d.read_text(mf)\n",
        "      if txt is not None:\n",
        "        mh.update(mf.encode('ascii')); mh.update(b'='); mh.update(txt.encode('utf-8','surrogateescape')); mh.update(b'\\0')\n",
        "    except Exception: pass\n",
        "  ed_st=''\n",
        "  try:\n",
        "    du=d.read_text('direct_url.json')\n",
        "    if du:\n",
        "      info=json.loads(du)\n",
        "      if (info.get('dir_info') or {}).get('editable') and isinstance(info.get('url'),str) and info['url'].startswith('file://'):\n",
        "        ed_p=os.path.realpath(urllib.parse.unquote(urllib.parse.urlparse(info['url']).path))\n",
        "        if ed_p!=ws_real and not ed_p.startswith(ws_real+os.sep) and 'site-packages' not in ed_p and 'dist-packages' not in ed_p:\n",
        "          has_ed=True\n",
        "          if os.path.isdir(ed_p): ed_st=f'@{_dir_sig(ed_p)}'\n",
        "          elif os.path.isfile(ed_p): ed_st=f'@{_file_sig(ed_p)}'\n",
        "  except Exception: pass\n",
        "  return f'{name}=={ver}:{mh.hexdigest()}{ed_st}'\n",
        "pvers=sorted({_sig(ep) for ep in items})\n",
        "seen_pl=set(); ordered_pl=[]\n",
        "for ep in items:\n",
        "  val=ep.value.split('[',1)[0].strip()\n",
        "  entry=f'{ep.name}={val}'\n",
        "  if entry not in seen_pl: seen_pl.add(entry); ordered_pl.append(entry)\n",
        "dh=hashlib.sha256()\n",
        "dvers=sorted({_dist_sig(d) for d in dists})\n",
        "for dv in dvers: dh.update(dv.encode('utf-8','surrogateescape')); dh.update(b'\\n')\n",
        "stdlib_d=os.path.realpath(os.path.dirname(os.__file__))\n",
        "stdlib_dyn=os.path.realpath(os.path.join(stdlib_d,'lib-dynload'))\n",
        "sys_entries=set()\n",
        "sys_arch_files=set()\n",
        "ext_py_dirs=set()\n",
        "for p in sys.path:\n",
        "  if not isinstance(p,str) or not p: continue\n",
        "  lex=os.path.abspath(p)\n",
        "  rp=os.path.realpath(lex)\n",
        "  if rp==ws_real or rp.startswith(ws_real+os.sep) or rp in (stdlib_d,stdlib_dyn): continue\n",
        "  if os.path.isdir(lex):\n",
        "    sys_entries.add(lex)\n",
        "    if 'site-packages' not in rp and 'dist-packages' not in rp and not rp.startswith(stdlib_d+os.sep):\n",
        "      ext_py_dirs.add(rp)\n",
        "  elif os.path.isfile(lex):\n",
        "    sys_entries.add(lex)\n",
        "    sys_arch_files.add(rp)\n",
        "  else:\n",
        "    arch_lex=_archive_of(lex)\n",
        "    if arch_lex:\n",
        "      arch_rp=os.path.realpath(arch_lex)\n",
        "      if arch_rp!=ws_real and not arch_rp.startswith(ws_real+os.sep):\n",
        "        sys_entries.add(arch_lex)\n",
        "        sys_arch_files.add(arch_rp)\n",
        "ah=hashlib.sha256()\n",
        "for _sp_f in sorted(sys_arch_files):\n",
        "  ah.update(_sp_f.encode('utf-8','surrogateescape')); ah.update(b'='); ah.update(_file_sig(_sp_f).encode('utf-8','surrogateescape')); ah.update(b'\\n')\n",
        "eh=hashlib.sha256()\n",
        "for _ext_d in sorted(ext_py_dirs):\n",
        "  has_ed=True\n",
        "  eh.update(_ext_d.encode('utf-8','surrogateescape')); eh.update(b'='); eh.update(_dir_sig(_ext_d).encode('utf-8','surrogateescape')); eh.update(b'\\n')\n",
        "missing_pth=set()\n",
        "for _se in list(sys_entries):\n",
        "  if not os.path.isdir(_se): continue\n",
        "  try:\n",
        "    for _fn in os.listdir(_se):\n",
        "      if not _fn.endswith('.pth'): continue\n",
        "      with open(os.path.join(_se,_fn),'r',encoding='utf-8',errors='ignore') as _pf:\n",
        "        for _raw_ln in _pf:\n",
        "          _ln=_raw_ln.strip()\n",
        "          if not _ln or _ln.startswith('#') or _ln.startswith(('import ','import\\t')): continue\n",
        "          _cand=os.path.abspath(os.path.join(_se,_ln))\n",
        "          if not os.path.exists(_cand): missing_pth.add(_cand)\n",
        "  except OSError: pass\n",
        "ident_list=[sys.version,sys.executable,platform.platform(),pytest.__version__,f'dists:{dh.hexdigest()}',f'sys_archives:{ah.hexdigest()}',f'ext_sys_dirs:{eh.hexdigest()}',*pvers]\n",
        "if uncacheable: ident_list.append('__TCTX_PY_UNCACHEABLE__')\n",
        "print(json.dumps({'identity':ident_list,",
        "'plugins':ordered_pl,'editable':has_ed,'uncacheable':uncacheable,'plugin_locs':sorted(sp_locs),'sys_path_dirs':sorted(sys_entries),'missing_pth_paths':sorted(missing_pth)},separators=(',',':')))\n",
    );
    let mut command = Command::new(python);
    command
        .current_dir(&spec.root)
        .env("PYTHONPATH", &pythonpath)
        .arg("-c")
        .arg(script);
    let Ok(output) = run_command_with_timeout(&mut command, Duration::from_secs(5), 256 * 1024)
    else {
        return unavailable();
    };
    if !output.status.success() {
        return unavailable();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return unavailable();
    };
    let mut identity = value["identity"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    // Always include the site-packages / `.pth` / `sitecustomize` / Python startup-env stamp
    // digest directly in the returned runtime identity so in-place `.pth` or `sitecustomize.py`
    // edits immediately invalidate cached passes and restart the Zygote.
    if !identity.is_empty() {
        identity.push_str("\nstartup_stamps=");
        identity.push_str(&startup_files_digest);
    }
    // Preserve native `importlib.metadata` discovery order (never sorting, which would alter
    // hook execution order and duplicate entry-point precedence) while deduplicating in-order.
    // If any entry point cannot be losslessly transported over `TCTX_PYTEST_PLUGINS`, fall back
    // to native plugin discovery (`PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL`) instead of dropping it!
    let mut seen_plugins = BTreeSet::new();
    let mut plugins: Vec<String> = Vec::new();
    let mut has_untransportable_plugin = false;
    for raw_pl in value["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
    {
        if raw_pl.is_empty() {
            continue;
        }
        if raw_pl.len() > 4096 || raw_pl.contains(['\0', '\r', '\n', ',']) {
            has_untransportable_plugin = true;
            break;
        }
        if seen_plugins.insert(raw_pl.to_string()) {
            plugins.push(raw_pl.to_string());
        }
    }
    if has_untransportable_plugin {
        plugins = vec![PLUGIN_DISCOVERY_UNAVAILABLE_SENTINEL.to_string()];
    }
    if value["uncacheable"].as_bool() == Some(true)
        && !identity.contains(PY_ENV_UNCACHEABLE_SENTINEL)
    {
        identity.push('\n');
        identity.push_str(PY_ENV_UNCACHEABLE_SENTINEL);
    }
    let has_editable_plugins = value["editable"].as_bool().unwrap_or(false)
        || value["uncacheable"].as_bool().unwrap_or(false);
    let plugin_locs: Vec<PathBuf> = value["plugin_locs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let plugin_loc_digest = stamp_plugin_locs_digest(&plugin_locs).unwrap_or_default();
    let sys_path_dirs: Vec<PathBuf> = value["sys_path_dirs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let sys_path_digest = stamp_sys_path_dirs_digest(&sys_path_dirs).unwrap_or_default();
    let missing_pth_paths: Vec<PathBuf> = value["missing_pth_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    {
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let captured_ms = now_unix_ms();
        guard.insert(
            cache_key,
            CachedPythonRuntime {
                captured: Instant::now(),
                captured_unix_ms: captured_ms,
                identity: identity.clone(),
                plugins: plugins.clone(),
                has_editable_plugins,
                plugin_locs,
                plugin_loc_digest,
                sys_path_dirs,
                sys_path_digest,
                missing_pth_paths,
            },
        );
        if guard.len() > 64 {
            let excess = guard.len().saturating_sub(48);
            let drop_keys: Vec<String> = guard.keys().take(excess).cloned().collect();
            for k in drop_keys {
                guard.remove(&k);
            }
        }
        if let Ok(state_dir) = crate::catalog::state_dir() {
            let disk_map: BTreeMap<String, PersistedPythonRuntime> = guard
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        PersistedPythonRuntime {
                            identity: v.identity.clone(),
                            plugins: v.plugins.clone(),
                            updated_unix_ms: v.captured_unix_ms,
                            has_editable_plugins: v.has_editable_plugins,
                            plugin_locs: v.plugin_locs.clone(),
                            plugin_loc_digest: v.plugin_loc_digest.clone(),
                            sys_path_dirs: v.sys_path_dirs.clone(),
                            sys_path_digest: v.sys_path_digest.clone(),
                            missing_pth_paths: v.missing_pth_paths.clone(),
                        },
                    )
                })
                .collect();
            if let Ok(bytes) = serde_json::to_vec(&disk_map) {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;

                let tmp =
                    state_dir.join(format!("python-runtime-cache.json.tmp.{}", std::process::id()));
                let write_ok = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&tmp)
                    .and_then(|mut f| {
                        f.write_all(&bytes)?;
                        f.flush()
                    })
                    .is_ok();
                if write_ok {
                    let _ = std::fs::rename(&tmp, state_dir.join("python-runtime-cache.json"));
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
    }
    (identity, plugins)
}

pub(super) type GitDirtyCacheMap = BTreeMap<PathBuf, (Instant, Vec<String>)>;
pub(super) static GIT_DIRTY_CACHE: OnceLock<Mutex<GitDirtyCacheMap>> = OnceLock::new();
pub(super) static GIT_DIRTY_GATES: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
    OnceLock::new();
pub(super) static GIT_DIRTY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn invalidate_git_dirty_cache(root: &Path) {
    GIT_DIRTY_GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Some(cache) = GIT_DIRTY_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(root);
    }
}

pub(super) fn expand_dirty_paths_for_graph(graph: &DependencyGraph, dirty: &[String]) -> Vec<String> {
    let mut expanded = BTreeSet::new();
    for raw in dirty {
        let trimmed = raw.trim();
        let norm = if trimmed == "." || trimmed == "./" {
            ".".to_string()
        } else {
            trimmed
                .trim_start_matches("./")
                .trim_end_matches('/')
                .replace('\\', "/")
        };
        if norm.is_empty() {
            continue;
        }
        if norm == "." {
            // Fail-closed sentinel (e.g. `git status` output exceeded cap): mark all indexed files.
            expanded.extend(graph.files.keys().cloned());
            break;
        }
        if graph.files.contains_key(&norm) {
            expanded.insert(norm);
        } else {
            // If `git_dirty_files` fell back to a directory marker (e.g. >4096 untracked files),
            // expand it to all indexed descendants so `affected_tests` never omits tests.
            let prefix = format!("{norm}/");
            let mut matched_any = false;
            for (k, _) in graph
                .files
                .range(prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&prefix))
            {
                expanded.insert(k.clone());
                matched_any = true;
            }
            if !matched_any {
                expanded.insert(norm);
            }
        }
    }
    expanded.into_iter().collect()
}

pub(super) fn git_dirty_files(root: &Path) -> Result<Vec<String>> {
    let cache = GIT_DIRTY_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some((captured_at, cached_files)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(root)
        && captured_at.elapsed() < Duration::from_millis(1_500)
    {
        return Ok(cached_files.clone());
    }
    // Singleflight per root so concurrent callers don't spawn duplicate `git status` subprocesses
    let gate = {
        let gates = GIT_DIRTY_GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut guard = gates.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() > 128 {
            guard.retain(|_, g| Arc::strong_count(g) > 1);
        }
        Arc::clone(
            guard
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _singleflight = gate.lock().unwrap_or_else(|p| p.into_inner());
    let start_gen = GIT_DIRTY_GENERATION.load(Ordering::Acquire);
    if let Some((captured_at, cached_files)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(root)
        && captured_at.elapsed() < Duration::from_millis(1_500)
    {
        return Ok(cached_files.clone());
    }
    let mut cmd = Command::new("git");
    cmd.args(["--no-optional-locks", "-c", "core.quotePath=false", "-C"])
        .arg(root)
        .args(["status", "--porcelain", "-z", "--untracked-files=normal"]);
    const GIT_STATUS_MAX_BYTES: usize = 1024 * 1024;
    let out = run_command_with_timeout(&mut cmd, Duration::from_secs(5), GIT_STATUS_MAX_BYTES)?;
    if out.timed_out {
        return Err(invalid("GIT_STATUS_FAILED: git status timed out after 5s").into());
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Ok(Vec::new());
        }
        return Err(invalid(format!(
            "GIT_STATUS_FAILED: git status exited with {:?}: {}",
            out.status.code(),
            truncate_str(stderr.trim(), 512)
        ))
        .into());
    }
    if out.stdout.len() >= GIT_STATUS_MAX_BYTES {
        // Output hit the 1 MiB cap and may have dropped trailing modified paths; fail closed by
        // returning the workspace root sentinel `.` so callers expand all indexed files.
        return Ok(vec![".".to_string()]);
    }
    // Parse NUL-delimited `--porcelain -z` records so filenames containing spaces, quotes, or
    // literal ` -> ` are never split ambiguously; for renames/copies (`R`/`C`), skip the extra
    // following source-path NUL record.
    let mut files = Vec::new();
    let mut records = out.stdout.split(|&b| b == 0).peekable();
    while let Some(raw_rec) = records.next() {
        if raw_rec.len() < 4 {
            continue;
        }
        let status_x = raw_rec[0];
        let status_y = raw_rec[1];
        let actual_cow = String::from_utf8_lossy(&raw_rec[3..]);
        let actual = actual_cow.as_ref();
        if matches!(status_x, b'R' | b'C') || matches!(status_y, b'R' | b'C') {
            // Retain BOTH the new path AND the original pre-rename path so `affected_tests`
            // includes former importers of a renamed module!
            if let Some(orig_rec) = records.next() {
                let orig = String::from_utf8_lossy(orig_rec);
                if !orig.is_empty() {
                    files.push(orig.trim_start_matches("./").replace('\\', "/"));
                }
            }
        }
        if !actual.is_empty() {
            if actual.ends_with('/') {
                let dir_rel = actual.trim_end_matches('/');
                let mut stack = vec![root.join(dir_rel)];
                let mut expanded_count = 0usize;
                let mut truncated = false;
                while let Some(dir) = stack.pop() {
                    if expanded_count >= 4_096 {
                        truncated = true;
                        break;
                    }
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        if expanded_count >= 4_096 {
                            truncated = true;
                            break;
                        }
                        let Ok(ft) = entry.file_type() else {
                            continue;
                        };
                        if ft.is_symlink() {
                            continue;
                        }
                        let ep = entry.path();
                        if ft.is_dir() {
                            if let Some(name) = ep.file_name().and_then(|n| n.to_str())
                                && crate::exclusion_policy::is_excluded_dir_name(name)
                            {
                                continue;
                            }
                            stack.push(ep);
                        } else if ft.is_file()
                            && let Ok(rel) = ep.strip_prefix(root)
                        {
                            files.push(normalize_rel_str(rel));
                            expanded_count += 1;
                        }
                    }
                }
                if truncated && !dir_rel.is_empty() {
                    // Preserve the directory itself when >4096 untracked files exist so callers
                    // conservatively expand the directory instead of silently dropping later files.
                    files.push(dir_rel.to_string());
                }
            } else {
                files.push(actual.to_string());
            }
        }
    }
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if GIT_DIRTY_GENERATION.load(Ordering::Acquire) == start_gen {
        if guard.len() > 128 {
            guard.retain(|_, (ts, _)| ts.elapsed() < Duration::from_secs(30));
        }
        guard.insert(root.to_path_buf(), (Instant::now(), files.clone()));
    }
    Ok(files)
}

pub(super) type PythonPathCacheMap = BTreeMap<PathBuf, (Instant, Vec<PathBuf>)>;
pub(super) static PYTHONPATH_CACHE: OnceLock<Mutex<PythonPathCacheMap>> = OnceLock::new();
pub(super) static PYTHONPATH_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn invalidate_pythonpath_cache(root: &Path) {
    PYTHONPATH_GENERATION.fetch_add(1, Ordering::AcqRel);
    if let Some(cache) = PYTHONPATH_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(root);
    }
}

pub(super) fn build_pythonpath(root: &Path, existing: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let cache = PYTHONPATH_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let start_gen = PYTHONPATH_GENERATION.load(Ordering::Acquire);
    let base_paths = if let Some((captured_at, cached)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(root)
        && captured_at.elapsed() < Duration::from_secs(60)
    {
        cached.clone()
    } else {
        let mut paths = vec![root.to_path_buf()];
        let src = root.join("src");
        if src.is_dir() {
            paths.push(src);
        }
        for container in ["src", "services", "packages", "apps", "extensions"] {
            let dir = root.join(container);
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let sub = entry.path();
                    if !sub.is_dir() {
                        continue;
                    }
                    let sub_src = sub.join("src");
                    if sub_src.is_dir() && !paths.contains(&sub_src) {
                        paths.push(sub_src);
                    }
                    if (sub.join("requirements.txt").is_file()
                        || sub.join("pyproject.toml").is_file()
                        || sub.join("pytest.ini").is_file()
                        || sub.join("main.py").is_file()
                        || sub.join("conftest.py").is_file())
                        && !paths.contains(&sub)
                    {
                        paths.push(sub);
                    }
                }
            }
        }
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if PYTHONPATH_GENERATION.load(Ordering::Acquire) == start_gen {
            if guard.len() > 128 {
                guard.retain(|_, (ts, _)| ts.elapsed() < Duration::from_secs(60));
            }
            guard.insert(root.to_path_buf(), (Instant::now(), paths.clone()));
        }
        paths
    };
    let mut paths = base_paths;
    if let Some(old) = existing {
        for p in std::env::split_paths(&old) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    std::env::join_paths(paths).unwrap_or_default()
}


