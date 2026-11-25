use super::*;

#[cfg(test)]
pub(super) fn environment_fingerprint(spec: &Spec) -> String {
    environment_fingerprint_with_python(spec, true, None)
}

pub(super) fn is_pytest_collection_override_token(a: &str) -> bool {
    matches!(
        a,
        "-o" | "--override-ini"
            | "-c"
            | "--config-file"
            | "--rootdir"
            | "--confcutdir"
            | "--pyargs"
            | "--doctest-modules"
            | "--doctest-glob"
            | "--ignore"
            | "--ignore-glob"
            | "--deselect"
            | "--import-mode"
            | "--version"
            | "-V"
            | "--help"
            | "-h"
            | "--fixtures"
            | "--fixtures-per-test"
            | "--markers"
            | "--co"
            | "--collect-only"
            | "--setup-plan"
    ) || (a.starts_with("-o") && a.len() > 2)
        || (a.starts_with("-c") && a.len() > 2 && !a.starts_with("--"))
        || a.starts_with("--override-ini=")
        || a.starts_with("--config-file=")
        || a.starts_with("--rootdir=")
        || a.starts_with("--confcutdir=")
        || a.starts_with("--doctest-glob=")
        || a.starts_with("--ignore=")
        || a.starts_with("--ignore-glob=")
        || a.starts_with("--deselect=")
        || a.starts_with("--import-mode=")
}

pub(super) fn tokens_have_pytest_collection_override(tokens: &[&str]) -> bool {
    let mut i = 0usize;
    while i < tokens.len() {
        let a = tokens[i];
        if is_pytest_collection_override_token(a) {
            return true;
        }
        // Enabling a plugin via `-p <plugin>` (NOT `-p no:<plugin>`) can register custom
        // collectors that generate non-Python or non-standard tests!
        if a == "-p" {
            if let Some(next) = tokens.get(i + 1)
                && !next.starts_with("no:")
            {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("-p")
            && !rest.is_empty()
            && !rest.starts_with('-')
            && !rest.starts_with("no:")
        {
            return true;
        }
        i += 1;
    }
    false
}

pub(super) fn pytest_cmd_arg_start(configured_pytest_cmd: &[String]) -> usize {
    if let Some(pos) = configured_pytest_cmd
        .iter()
        .position(|t| t == "pytest" || t.ends_with("/pytest") || t.ends_with("\\pytest"))
    {
        return pos + 1;
    }
    if configured_pytest_cmd.len() >= 3 && configured_pytest_cmd[1] == "-m" {
        3
    } else {
        1
    }
}

/// Splits `PYTEST_ADDOPTS` / `addopts=` values using pytest/POSIX `shlex.split`-compatible quoting
/// so `-k 'not --collect-only'` stays a single argument token instead of splitting into options!
pub(super) fn split_pytest_shlex(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    if let Some(&next) = chars.peek()
                        && matches!(next, '"' | '\\' | '$' | '`')
                    {
                        cur.push(chars.next().unwrap_or(c));
                    } else {
                        cur.push(c);
                    }
                } else {
                    cur.push(c);
                }
            }
            Some(_) => unreachable!(),
            None => {
                if c.is_whitespace() {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_token = true;
                } else if c == '\\' {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                        in_token = true;
                    }
                } else {
                    cur.push(c);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

pub(super) fn is_pytest_two_token_option(a: &str) -> bool {
    matches!(
        a,
        "-k" | "-m"
            | "-o"
            | "--override-ini"
            | "-c"
            | "--config-file"
            | "-p"
            | "--rootdir"
            | "--confcutdir"
            | "--doctest-glob"
            | "--ignore"
            | "--ignore-glob"
            | "--deselect"
            | "--import-mode"
            | "--maxfail"
            | "--tb"
            | "--capture"
            | "--durations"
            | "--durations-min"
            | "--junitxml"
            | "--junit-prefix"
            | "-r"
            | "-W"
            | "--filterwarnings"
            | "--basetemp"
            | "--log-level"
            | "--log-cli-level"
            | "--log-format"
            | "--log-date-format"
            | "--log-file"
            | "-n"
            | "--dist"
    )
}

pub(super) fn has_pytest_collection_override(pytest_args: &[String], configured_pytest_cmd: &[String]) -> bool {
    let req_refs: Vec<&str> = pytest_args.iter().map(String::as_str).collect();
    if tokens_have_pytest_collection_override(&req_refs) {
        return true;
    }
    let cmd_skip = pytest_cmd_arg_start(configured_pytest_cmd);
    let cmd_refs: Vec<&str> = configured_pytest_cmd
        .iter()
        .skip(cmd_skip)
        .map(String::as_str)
        .collect();
    if tokens_have_pytest_collection_override(&cmd_refs) {
        return true;
    }
    if let Ok(addopts) = std::env::var("PYTEST_ADDOPTS") {
        let env_toks = split_pytest_shlex(&addopts);
        let env_refs: Vec<&str> = env_toks.iter().map(String::as_str).collect();
        if tokens_have_pytest_collection_override(&env_refs) {
            return true;
        }
    }
    false
}

pub(super) struct StructuredPytestProbe {
    pub(super) exit_code: i32,
    pub(super) combined_output: String,
    pub(super) cancelled: bool,
    pub(super) duration_ms: u64,
    pub(super) collected_meta: Option<(Vec<PathBuf>, BTreeSet<String>)>,
}

pub(super) fn has_testless_cargo_targets_for_request(
    root: &Path,
    language_filter: Option<&str>,
    cargo_target: Option<&str>,
    norm_targets: &[String],
) -> bool {
    if !language_filter.is_none_or(|lf| lf == "rust") {
        return false;
    }
    let cands = if is_cargo_workspace_root(root) {
        requested_workspace_crate_dirs(root, norm_targets, &[])
    } else if root.join("Cargo.toml").is_file()
        && (norm_targets.is_empty()
            || norm_targets.iter().any(|t| {
                let c = t.trim().trim_start_matches("./").trim_end_matches('/');
                c.is_empty() || c == "."
            }))
    {
        vec![".".to_string()]
    } else {
        Vec::new()
    };
    cands.iter().any(|cd| {
        let abs = if cd == "." {
            root.to_path_buf()
        } else {
            root.join(cd)
        };
        if let Some(ct) = cargo_target {
            cargo_manifest_matches_explicit_target(&abs, ct)
        } else {
            cargo_manifest_has_any_testable_target(&abs)
        }
    })
}

pub(super) struct TempPyPluginDir {
    pub(super) path: PathBuf,
}

impl TempPyPluginDir {
    pub(super) fn random_hex_16() -> String {
        let mut bytes = [0u8; 16];
        #[cfg(unix)]
        {
            use std::io::Read;
            if let Ok(mut f) = std::fs::File::open("/dev/urandom")
                && f.read_exact(&mut bytes).is_ok()
            {
                let mut hex = String::with_capacity(32);
                for b in bytes {
                    let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
                }
                return hex;
            }
        }
        use std::hash::{BuildHasher, Hasher};
        let mut h1 = std::collections::hash_map::RandomState::new().build_hasher();
        h1.write_u32(std::process::id());
        let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
        h2.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        format!("{:016x}{:016x}", h1.finish(), h2.finish())
    }

    /// Atomically creates a brand-new private `0o700` directory with high-entropy name, rejecting
    /// any pre-existing path (`EEXIST`) so no other local user can pre-create or hijack it!
    pub(super) fn create(prefix: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let dir = base.join(format!("{prefix}_{}", Self::random_hex_16()));
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
                    continue;
                };
                let rc = unsafe { libc::mkdir(c_path.as_ptr(), 0o700) };
                if rc == 0 {
                    return Ok(Self { path: dir });
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(err.into());
            }
            #[cfg(not(unix))]
            {
                match std::fs::create_dir(&dir) {
                    Ok(()) => return Ok(Self { path: dir }),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(invalid("TEMP_PLUGIN_DIR_FAILED: could not atomically create private temp directory").into())
    }

    pub(super) fn prepend_to_pythonpath(&self, base: &std::ffi::OsStr) -> std::ffi::OsString {
        let mut combined = std::ffi::OsString::from(&self.path);
        if !base.is_empty() {
            combined.push(if cfg!(windows) { ";" } else { ":" });
            combined.push(base);
        }
        combined
    }
}

impl Drop for TempPyPluginDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Returns `true` ONLY for explicit non-Python file targets (`.rs`, `.go`, frontend sources)
/// that are NOT directories on disk (e.g. a directory named `integration.rs` containing Python
/// tests is NOT excluded!).
pub(super) fn is_explicitly_non_python_target(
    root: &Path,
    _graph: &DependencyGraph,
    raw_target: &str,
) -> bool {
    let clean = raw_target
        .split("::")
        .next()
        .unwrap_or(raw_target)
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .replace('\\', "/");
    if !clean.is_empty() && root.join(&clean).is_dir() {
        return false;
    }
    clean.ends_with(".rs")
        || clean.ends_with(".go")
        || crate::ast_graph::is_frontend_source_path(&clean)
}

/// Runs a structured pytest `--collect-only` probe using the configured `py_cmd` verbatim (so
/// wrappers like `uv run pytest` or `poetry run pytest` work identically!) with an injected
/// temporary plugin that dumps `session.items` in an `atexit` handler (AFTER all `hookwrapper`
/// `pytest_collection_finish` / `pytest_sessionfinish` post-yield hooks complete!) to a sidecar
/// JSON file immune to stdout capture or truncation.
pub(super) fn probe_pytest_collection_structured(
    root: &Path,
    py_cmd: &[String],
    pythonpath: &std::ffi::OsStr,
    pytest_args: &[String],
    timeout: Duration,
    active_pgid: &AtomicI32,
    active_pgids: &Mutex<BTreeSet<i32>>,
) -> Result<StructuredPytestProbe> {
    let Some((prog, base_args)) = py_cmd.split_first() else {
        return Err(invalid("PYTEST_CMD_EMPTY: configured pytest command is empty").into());
    };
    let tmp_dir = TempPyPluginDir::create("tctx_probe")?;
    let plugin_name = "_tctx_collect_probe";
    let plugin_file = tmp_dir.path.join(format!("{plugin_name}.py"));
    let out_file = tmp_dir.path.join("probe_meta.json");
    const PROBE_PLUGIN_SRC: &str = concat!(
        "import atexit, json, os\n",
        "_tctx_sess = [None]\n",
        "def pytest_sessionstart(session):\n",
        "    _tctx_sess[0] = session\n",
        "def _tctx_write_probe():\n",
        "    out_path = os.environ.get('_TCTX_PROBE_OUT_FILE', '')\n",
        "    if not out_path:\n",
        "        return\n",
        "    files, markers = set(), set()\n",
        "    sess = _tctx_sess[0]\n",
        "    if sess is not None:\n",
        "        for it in getattr(sess, 'items', ()) or ():\n",
        "            p = getattr(it, 'path', None) or getattr(it, 'fspath', None)\n",
        "            if p:\n",
        "                files.add(os.path.realpath(str(p)))\n",
        "            for m in it.iter_markers():\n",
        "                if getattr(m, 'name', None):\n",
        "                    markers.add(str(m.name))\n",
        "    try:\n",
        "        with open(out_path, 'w', encoding='utf-8') as fh:\n",
        "            json.dump({'files': sorted(files), 'markers': sorted(markers)}, fh, separators=(',', ':'))\n",
        "    except OSError:\n",
        "        pass\n",
        "atexit.register(_tctx_write_probe)\n",
    );
    std::fs::write(&plugin_file, PROBE_PLUGIN_SRC)?;
    let eff_pythonpath = tmp_dir.prepend_to_pythonpath(pythonpath);
    let mut cmd = Command::new(prog);
    cmd.current_dir(root)
        .env("PYTHONPATH", &eff_pythonpath)
        .env("_TCTX_PROBE_OUT_FILE", &out_file)
        .args(base_args)
        .arg("-p")
        .arg(plugin_name)
        .arg("--collect-only")
        .args(pytest_args);
    let out = run_command_with_timeout_tracked(
        &mut cmd,
        timeout,
        2 * 1024 * 1024,
        Some(active_pgid),
        Some(active_pgids),
    )?;
    let exit_code = out.status.code().unwrap_or(1);
    let stdout_s = String::from_utf8_lossy(&out.stdout);
    let stderr_s = String::from_utf8_lossy(&out.stderr);
    let parsed_meta: Option<(Vec<PathBuf>, BTreeSet<String>)> = std::fs::read_to_string(&out_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|val| {
            let files: Vec<PathBuf> = val["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            let markers: BTreeSet<String> = val["markers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            (files, markers)
        });
    let combined_output = match (stdout_s.trim().is_empty(), stderr_s.trim().is_empty()) {
        (true, _) => stderr_s.trim().to_string(),
        (_, true) => stdout_s.trim().to_string(),
        (false, false) => format!("{}\n{}", stdout_s.trim(), stderr_s.trim()),
    };
    Ok(StructuredPytestProbe {
        exit_code,
        combined_output,
        cancelled: out.cancelled,
        duration_ms: out.duration_ms,
        collected_meta: parsed_meta,
    })
}

pub(super) struct DirectPytestExecMeta {
    pub(super) passed: usize,
    pub(super) failed: usize,
    pub(super) skipped: usize,
    pub(super) errors: usize,
    pub(super) mutated_during_run: bool,
    pub(super) late_unreserved_resource: bool,
    pub(super) missing_resources: BTreeSet<String>,
    pub(super) pre_stamps: BTreeMap<PathBuf, Option<crate::ast_graph::FileStamp>>,
}

pub(super) struct DirectPytestRunOutput {
    pub(super) success: bool,
    pub(super) timed_out: bool,
    pub(super) cancelled: bool,
    pub(super) exit_code: i32,
    pub(super) duration_ms: u64,
    pub(super) combined_output: String,
    pub(super) exec_meta: Option<DirectPytestExecMeta>,
}

/// Runs direct pytest via the configured `py_cmd` verbatim, injecting a lightweight plugin (when
/// `!is_info_only`) that:
/// 1. Validates actual execution-time collection resource markers in `pytest_runtestloop(tryfirst=True)`
///    BEFORE any test runs (aborting with `missing_resources` if a hook added an unreserved resource
///    only outside `--collect-only`!),
/// 2. Pre-stamps opened/imported files via `sys.addaudithook('open')` BEFORE reads complete and
///    detects mid-run mutations at `atexit`,
/// 3. Emits exact passed/failed/skipped/errors counts and pre-read stamps to a sidecar JSON file!
#[allow(clippy::too_many_arguments)]
pub(super) fn run_direct_pytest_structured(
    root: &Path,
    py_cmd: &[String],
    pythonpath: &std::ffi::OsStr,
    pytest_args: &[String],
    is_info_only: bool,
    reserved_resources: &BTreeSet<String>,
    timeout: Duration,
    active_pgid: &AtomicI32,
    active_pgids: &Mutex<BTreeSet<i32>>,
) -> Result<DirectPytestRunOutput> {
    let Some((prog, base_args)) = py_cmd.split_first() else {
        return Err(invalid("PYTEST_CMD_EMPTY: configured pytest command is empty").into());
    };
    let mut _tmp_dir: Option<TempPyPluginDir> = None;
    let mut out_file: Option<PathBuf> = None;
    let mut cmd = Command::new(prog);
    cmd.current_dir(root);
    if !is_info_only {
        let td = TempPyPluginDir::create("tctx_exec")?;
        let plugin_name = "_tctx_exec_probe";
        let plugin_file = td.path.join(format!("{plugin_name}.py"));
        let of = td.path.join("exec_meta.json");
        let reserved_csv = reserved_resources
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        const EXEC_PLUGIN_SRC: &str = concat!(
            "import atexit, importlib.util, json, os, stat, sys, pytest\n",
            "_tctx_counts = {'passed': 0, 'failed': 0, 'skipped': 0, 'errors': 0}\n",
            "_tctx_sess = [None]\n",
            "_tctx_basetemp = [None]\n",
            "_tctx_pre = {}\n",
            "_tctx_missing_res = []\n",
            "_tctx_tests_started = [False]\n",
            "_tctx_late_res = [False]\n",
            "_tctx_plugin_dir = os.path.realpath(os.path.dirname(__file__))\n",
            "_tctx_skip_pfxs = ('/proc/', '/dev/', '/sys/', _tctx_plugin_dir + os.sep)\n",
            "def _tctx_stat(p):\n",
            "    try:\n",
            "        st = os.stat(p)\n",
            "        if not stat.S_ISREG(st.st_mode):\n",
            "            return None\n",
            "        ms, mn = divmod(st.st_mtime_ns, 1_000_000_000)\n",
            "        cs, cn = divmod(st.st_ctime_ns, 1_000_000_000)\n",
            "        return [ms, mn, st.st_size, st.st_ino, cs, cn]\n",
            "    except OSError:\n",
            "        return None\n",
            "def _tctx_is_basetemp(rp):\n",
            "    bt = _tctx_basetemp[0]\n",
            "    if bt is None:\n",
            "        sess = _tctx_sess[0]\n",
            "        cfg = getattr(sess, 'config', None) if sess is not None else None\n",
            "        tf = getattr(cfg, '_tmp_path_factory', None) if cfg is not None else None\n",
            "        # Only exclude pytest's basetemp AFTER `_tmp_path_factory._basetemp` has actually\n",
            "        # been initialized as scratch space (so pre-existing input files under `--basetemp`\n",
            "        # are still stamped if read before pytest initializes scratch dirs!).\n",
            "        raw_bt = getattr(tf, '_basetemp', None) if tf is not None else None\n",
            "        if raw_bt is not None:\n",
            "            try:\n",
            "                bt = os.path.realpath(str(raw_bt)) + os.sep\n",
            "                _tctx_basetemp[0] = bt\n",
            "            except Exception:\n",
            "                bt = None\n",
            "    return bool(bt and rp.startswith(bt))\n",
            "def _tctx_record_one(rp):\n",
            "    if rp.startswith(_tctx_skip_pfxs) or _tctx_is_basetemp(rp):\n",
            "        return\n",
            "    if rp not in _tctx_pre:\n",
            "        st = _tctx_stat(rp)\n",
            "        if st is not None:\n",
            "            _tctx_pre[rp] = st\n",
            "def _tctx_record_pre(raw_p):\n",
            "    if not isinstance(raw_p, (str, bytes)):\n",
            "        return\n",
            "    try:\n",
            "        p = os.fsdecode(raw_p)\n",
            "        if not p:\n",
            "            return\n",
            "        rp = os.path.realpath(p)\n",
            "    except OSError:\n",
            "        return\n",
            "    if rp.endswith(('.pyc', '.pyo')):\n",
            "        try:\n",
            "            src_p = importlib.util.source_from_cache(rp)\n",
            "            if src_p:\n",
            "                src_rp = os.path.realpath(src_p)\n",
            "                if os.path.isfile(src_rp):\n",
            "                    _tctx_record_one(src_rp)\n",
            "                    return\n",
            "        except Exception:\n",
            "            pass\n",
            "        _tctx_record_one(rp)\n",
            "        return\n",
            "    _tctx_record_one(rp)\n",
            "def _tctx_audit(event, args):\n",
            "    if event == 'open' and len(args) >= 3:\n",
            "        path, mode, flags = args[0], args[1], args[2]\n",
            "        # Stamp ALL read-capable opens (including `'r+'` / `O_RDWR`), skipping only\n",
            "        # write-only / create-truncate opens (`'w'`, `'a'`, `'x'`, `O_WRONLY`, `O_CREAT`, `O_TRUNC`)!\n",
            "        if isinstance(mode, str) and any(c in mode for c in ('w', 'a', 'x')):\n",
            "            return\n",
            "        if isinstance(flags, int) and (flags & (os.O_WRONLY | os.O_CREAT | os.O_TRUNC)):\n",
            "            return\n",
            "        _tctx_record_pre(path)\n",
            "sys.addaudithook(_tctx_audit)\n",
            "def _tctx_marker_to_res(name):\n",
            "    if not name:\n",
            "        return None\n",
            "    s = str(name)\n",
            "    if s.startswith('resource_'):\n",
            "        return s[len('resource_'):]\n",
            "    if s in ('database', 'docker', 'network', 'serial', 'port'):\n",
            "        return s\n",
            "    return None\n",
            "def _tctx_validate_resources(items, late=False):\n",
            "    reserved = {k for k in os.environ.get('_TCTX_RESERVED_RESOURCES', '').split(',') if k}\n",
            "    needed = set()\n",
            "    for it in items:\n",
            "        p = getattr(it, 'path', None) or getattr(it, 'fspath', None)\n",
            "        if p:\n",
            "            _tctx_record_pre(str(p))\n",
            "        for m in it.iter_markers():\n",
            "            rk = _tctx_marker_to_res(getattr(m, 'name', None))\n",
            "            if rk:\n",
            "                needed.add(rk)\n",
            "    missing = sorted(needed - reserved)\n",
            "    if missing:\n",
            "        _tctx_missing_res[:] = missing\n",
            "        if late:\n",
            "            _tctx_late_res[0] = True\n",
            "            pytest.exit('tctx: unreserved resource marker added after tests started', returncode=65)\n",
            "        pytest.exit('tctx: unreserved resources discovered at execution collection', returncode=64)\n",
            "_tctx_post_collect = [False]\n",
            "_tctx_orig_add_marker = getattr(pytest.Node, 'add_marker', None)\n",
            "def _tctx_guarded_add_marker(self, marker, append=True):\n",
            "    if _tctx_post_collect[0]:\n",
            "        mname = marker if isinstance(marker, str) else getattr(marker, 'name', None)\n",
            "        rk = _tctx_marker_to_res(mname)\n",
            "        reserved = {k for k in os.environ.get('_TCTX_RESERVED_RESOURCES', '').split(',') if k}\n",
            "        if rk and rk not in reserved:\n",
            "            _tctx_missing_res[:] = [rk]\n",
            "            if _tctx_tests_started[0]:\n",
            "                _tctx_late_res[0] = True\n",
            "                pytest.exit('tctx: unreserved resource marker added after tests started', returncode=65)\n",
            "            pytest.exit('tctx: unreserved resource marker added after collection', returncode=64)\n",
            "    return _tctx_orig_add_marker(self, marker, append=append)\n",
            "if callable(_tctx_orig_add_marker):\n",
            "    pytest.Node.add_marker = _tctx_guarded_add_marker\n",
            "def pytest_sessionstart(session):\n",
            "    _tctx_sess[0] = session\n",
            "@pytest.hookimpl(hookwrapper=True)\n",
            "def pytest_collection_finish(session):\n",
            "    yield\n",
            "    _tctx_validate_resources(getattr(session, 'items', ()) or (), late=False)\n",
            "    _tctx_post_collect[0] = True\n",
            "@pytest.hookimpl(tryfirst=True)\n",
            "def pytest_runtestloop(session):\n",
            "    _tctx_validate_resources(getattr(session, 'items', ()) or (), late=False)\n",
            "    _tctx_post_collect[0] = True\n",
            "@pytest.hookimpl(tryfirst=True)\n",
            "def pytest_runtest_protocol(item, nextitem):\n",
            "    _tctx_post_collect[0] = True\n",
            "    _tctx_validate_resources((item,), late=_tctx_tests_started[0])\n",
            "    _tctx_tests_started[0] = True\n",
            "@pytest.hookimpl(hookwrapper=True)\n",
            "def pytest_runtest_setup(item):\n",
            "    _tctx_validate_resources((item,), late=_tctx_tests_started[0])\n",
            "    yield\n",
            "    _tctx_validate_resources((item,), late=True)\n",
            "@pytest.hookimpl(tryfirst=True)\n",
            "def pytest_runtest_call(item):\n",
            "    _tctx_validate_resources((item,), late=True)\n",
            "def pytest_runtest_logreport(report):\n",
            "    if report.when == 'call':\n",
            "        if report.passed:\n",
            "            _tctx_counts['passed'] += 1\n",
            "        elif report.failed:\n",
            "            _tctx_counts['failed'] += 1\n",
            "        elif report.skipped:\n",
            "            _tctx_counts['skipped'] += 1\n",
            "    elif report.when == 'setup':\n",
            "        if report.skipped:\n",
            "            _tctx_counts['skipped'] += 1\n",
            "        elif report.failed:\n",
            "            _tctx_counts['errors'] += 1\n",
            "    elif report.when == 'teardown':\n",
            "        if report.failed:\n",
            "            _tctx_counts['errors'] += 1\n",
            "def _tctx_write_exec():\n",
            "    out_path = os.environ.get('_TCTX_EXEC_OUT_FILE', '')\n",
            "    if not out_path:\n",
            "        return\n",
            "    for mod in list(sys.modules.values()):\n",
            "        f = getattr(mod, '__file__', None)\n",
            "        if isinstance(f, str) and f:\n",
            "            _tctx_record_pre(f)\n",
            "        sp = getattr(mod, '__spec__', None)\n",
            "        orig = getattr(sp, 'origin', None) if sp is not None else None\n",
            "        if isinstance(orig, str) and orig and orig not in ('built-in', 'frozen'):\n",
            "            _tctx_record_pre(orig)\n",
            "    mutated = False\n",
            "    for rp, pre_st in list(_tctx_pre.items()):\n",
            "        if _tctx_stat(rp) != pre_st:\n",
            "            mutated = True\n",
            "            break\n",
            "    try:\n",
            "        with open(out_path, 'w', encoding='utf-8') as fh:\n",
            "            json.dump({'counts': _tctx_counts, 'mutated': mutated, 'late_res': _tctx_late_res[0], 'missing_resources': _tctx_missing_res, 'pre_stamps': _tctx_pre}, fh, separators=(',', ':'))\n",
            "    except OSError:\n",
            "        pass\n",
            "atexit.register(_tctx_write_exec)\n",
        );
        std::fs::write(&plugin_file, EXEC_PLUGIN_SRC)?;
        let eff_pythonpath = td.prepend_to_pythonpath(pythonpath);
        cmd.env("PYTHONPATH", &eff_pythonpath)
            .env("_TCTX_EXEC_OUT_FILE", &of)
            .env("_TCTX_RESERVED_RESOURCES", &reserved_csv)
            .args(base_args)
            .arg("-p")
            .arg(plugin_name)
            .args(pytest_args);
        out_file = Some(of);
        _tmp_dir = Some(td);
    } else {
        cmd.env("PYTHONPATH", pythonpath)
            .args(base_args)
            .args(pytest_args);
    }
    let eout = run_command_with_timeout_tracked(
        &mut cmd,
        timeout,
        4 * 1024 * 1024,
        Some(active_pgid),
        Some(active_pgids),
    )?;
    let exit_code = eout.status.code().unwrap_or(1);
    let eerr = String::from_utf8_lossy(&eout.stderr);
    let eout_s = String::from_utf8_lossy(&eout.stdout);
    let combined_output = match (eout_s.trim().is_empty(), eerr.trim().is_empty()) {
        (true, _) => eerr.trim().to_string(),
        (_, true) => eout_s.trim().to_string(),
        (false, false) => format!("{}\n{}", eout_s.trim(), eerr.trim()),
    };
    let exec_meta = out_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|val| {
            let counts = &val["counts"];
            let mutated_during_run = val["mutated"].as_bool().unwrap_or(false);
            let late_unreserved_resource = val["late_res"].as_bool().unwrap_or(false);
            let missing_resources: BTreeSet<String> = val["missing_resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            let mut pre_stamps: BTreeMap<PathBuf, Option<crate::ast_graph::FileStamp>> =
                BTreeMap::new();
            if let Some(obj) = val["pre_stamps"].as_object() {
                for (k, v) in obj {
                    if let Some(arr) = v.as_array()
                        && arr.len() == 6
                    {
                        let stamp = crate::ast_graph::FileStamp {
                            mtime_sec: arr[0].as_i64().unwrap_or(0),
                            mtime_nsec: arr[1].as_i64().unwrap_or(0),
                            size: arr[2].as_u64().unwrap_or(0),
                            ino: arr[3].as_u64().unwrap_or(0),
                            ctime_sec: arr[4].as_i64().unwrap_or(0),
                            ctime_nsec: arr[5].as_i64().unwrap_or(0),
                        };
                        pre_stamps.insert(PathBuf::from(k), Some(stamp));
                    }
                }
            }
            DirectPytestExecMeta {
                passed: counts["passed"].as_u64().unwrap_or(0) as usize,
                failed: counts["failed"].as_u64().unwrap_or(0) as usize,
                skipped: counts["skipped"].as_u64().unwrap_or(0) as usize,
                errors: counts["errors"].as_u64().unwrap_or(0) as usize,
                mutated_during_run,
                late_unreserved_resource,
                missing_resources,
                pre_stamps,
            }
        });
    Ok(DirectPytestRunOutput {
        success: eout.status.success() && !eout.timed_out,
        timed_out: eout.timed_out,
        cancelled: eout.cancelled,
        exit_code,
        duration_ms: eout.duration_ms,
        combined_output,
        exec_meta,
    })
}

/// Executes direct pytest and, if execution-time collection hooks added resource markers that were
/// absent during `--collect-only`, atomically reserves the newly discovered resources before
/// running tests (up to 2 attempts, failing closed if markers keep changing).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_direct_pytest_with_resource_retry<'a>(
    managed_root: &'a ManagedRoot,
    root: &Path,
    py_cmd: &[String],
    pythonpath: &std::ffi::OsStr,
    pytest_args: &[String],
    is_info_only: bool,
    timeout: Duration,
    active_pgid: &AtomicI32,
    active_pgids: &Mutex<BTreeSet<i32>>,
    fg_guard: &mut Option<ForegroundResourceGuard<'a>>,
    scoped_cancelled: &AtomicBool,
) -> Result<DirectPytestRunOutput> {
    for attempt in 0..2 {
        let reserved: BTreeSet<String> = fg_guard
            .as_ref()
            .map(|g| g.keys.iter().cloned().collect())
            .unwrap_or_default();
        let erun = run_direct_pytest_structured(
            root,
            py_cmd,
            pythonpath,
            pytest_args,
            is_info_only,
            &reserved,
            timeout,
            active_pgid,
            active_pgids,
        )?;
        let (missing, is_late) = erun
            .exec_meta
            .as_ref()
            .map(|m| (m.missing_resources.clone(), m.late_unreserved_resource))
            .unwrap_or_default();
        if missing.is_empty() || is_info_only || erun.cancelled || scoped_cancelled.load(Ordering::Acquire) {
            return Ok(erun);
        }
        if is_late {
            return Err(invalid(format!(
                "TEST_RESOURCE_LATE_MUTATION: unreserved resource marker(s) [{}] added after tests started; refusing replay",
                missing.into_iter().collect::<Vec<_>>().join(",")
            ))
            .into());
        }
        if attempt == 1 {
            return Err(invalid(format!(
                "TEST_RESOURCE_UNSTABLE: execution collection added unreserved resources={}",
                missing.into_iter().collect::<Vec<_>>().join(",")
            ))
            .into());
        }
        // Prefix with `resource_` so `reserve_combined_pytest_resources` reserves every discovered
        // resource key atomically alongside existing reservations!
        let missing_markers: BTreeSet<String> = missing
            .into_iter()
            .map(|r| format!("resource_{r}"))
            .collect();
        reserve_combined_pytest_resources(
            managed_root,
            &missing_markers,
            fg_guard,
            true,
            scoped_cancelled,
        )?;
        if scoped_cancelled.load(Ordering::Acquire) {
            return Err(invalid(
                "TEST_CANCELLED: request cancelled while reserving execution-time pytest resources",
            )
            .into());
        }
    }
    unreachable!()
}

pub(super) struct ForegroundResourceGuard<'a> {
    pub(super) root: &'a ManagedRoot,
    pub(super) keys: Vec<String>,
}

impl Drop for ForegroundResourceGuard<'_> {
    fn drop(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let mut guard = self.root.lock_state();
        for k in &self.keys {
            if let Some(cnt) = guard.foreground_resources.get_mut(k) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    guard.foreground_resources.remove(k);
                }
            }
        }
        self.root.condvar.notify_all();
    }
}

/// Atomically acquires the union of any already-held foreground resources (`fg_guard`) and newly
/// discovered pytest marker resources (`markers`), releasing `fg_guard` first if additional keys
/// are needed so two runs can NEVER AB-BA deadlock waiting for each other's partial reservations!
pub(super) fn reserve_combined_pytest_resources<'a>(
    root: &'a ManagedRoot,
    markers: &BTreeSet<String>,
    fg_guard: &mut Option<ForegroundResourceGuard<'a>>,
    include_serial_guard: bool,
    scoped_cancelled: &AtomicBool,
) -> Result<()> {
    let existing: BTreeSet<String> = fg_guard
        .as_ref()
        .map(|g| g.keys.iter().cloned().collect())
        .unwrap_or_default();
    let mut combined: BTreeSet<String> = existing.clone();
    for m in markers {
        if let Some(rest) = m.strip_prefix("resource_") {
            combined.insert(rest.to_string());
        } else if matches!(m.as_str(), "database" | "docker" | "network" | "serial" | "port") {
            combined.insert(m.clone());
        }
    }
    if include_serial_guard {
        combined.insert("__direct_pytest_serial__".to_string());
    }
    if combined == existing || combined.is_empty() {
        return Ok(());
    }
    // Release any partial reservation before waiting for the complete set!
    *fg_guard = None;
    let mut guard = root.lock_state();
    let deadline = Instant::now() + Duration::from_secs(30);
    while (!combined.is_disjoint(&guard.background_resources)
        || combined
            .iter()
            .any(|r| guard.foreground_resources.contains_key(r)))
        && Instant::now() < deadline
        && !scoped_cancelled.load(Ordering::Acquire)
    {
        guard = root.wait_state_timeout(guard, Duration::from_millis(50));
    }
    if scoped_cancelled.load(Ordering::Acquire) {
        return Err(
            invalid("TEST_CANCELLED: request cancelled while waiting for direct pytest resources")
                .into(),
        );
    }
    let busy: Vec<String> = combined
        .iter()
        .filter(|r| {
            guard.background_resources.contains(*r)
                || guard.foreground_resources.contains_key(*r)
        })
        .cloned()
        .collect();
    if !busy.is_empty() {
        return Err(invalid(format!("TEST_RESOURCE_BUSY resources={}", busy.join(","))).into());
    }
    for r in &combined {
        *guard.foreground_resources.entry(r.clone()).or_insert(0) += 1;
    }
    *fg_guard = Some(ForegroundResourceGuard {
        root,
        keys: combined.into_iter().collect(),
    });
    Ok(())
}

/// Filters `extra_args` for Cargo in mixed polyglot runs so pytest-only flags like `--maxfail=1`,
/// `-x`, `--tb=short`, `--asyncio-mode=auto`, etc. NEVER leak into `cargo test`!
pub(super) fn filter_cargo_args_from_mixed(extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut after_sep = false;
    while i < extra_args.len() {
        let a = extra_args[i].as_str();
        if a.starts_with("__tctx_") {
            out.push(extra_args[i].clone());
            i += 1;
            continue;
        }
        if after_sep {
            out.push(extra_args[i].clone());
            i += 1;
            continue;
        }
        if a == "--" {
            after_sep = true;
            out.push(extra_args[i].clone());
            i += 1;
            continue;
        }
        // Two-token Cargo flags
        if matches!(
            a,
            "-F" | "--features"
                | "--profile"
                | "--target"
                | "--target-dir"
                | "--manifest-path"
                | "--config"
                | "-j"
                | "--jobs"
                | "--bin"
                | "--example"
                | "--test"
                | "--bench"
                | "--package"
                | "--exclude"
                | "--color"
                | "--message-format"
                | "-Z"
        ) {
            out.push(extra_args[i].clone());
            if i + 1 < extra_args.len() {
                out.push(extra_args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Short `-p <val>` is ambiguous between pytest plugin (`-p custom_plugin`) and Cargo
        // package (`-p crate`): drop short `-p` in mixed runs (unambiguous `--package <crate>`
        // and pure-Rust `tctx cargo-test -p <crate>` remain preserved!).
        if a == "-p" {
            i += 2;
            continue;
        }
        // Single-token Cargo flags and `--flag=value` forms
        if matches!(
            a,
            "--all-features"
                | "--no-default-features"
                | "--release"
                | "--locked"
                | "--frozen"
                | "--offline"
                | "--workspace"
                | "--all-targets"
                | "--lib"
                | "--bins"
                | "--examples"
                | "--tests"
                | "--benches"
                | "--doc"
                | "--no-run"
                | "--no-fail-fast"
                | "--future-incompat-report"
                | "--keep-going"
                | "--unit-graph"
                | "--timings"
                | "-q"
                | "--quiet"
                | "-v"
                | "-vv"
                | "--verbose"
        ) || a.starts_with("--features=")
            || a.starts_with("--profile=")
            || a.starts_with("--target=")
            || a.starts_with("--target-dir=")
            || a.starts_with("--manifest-path=")
            || a.starts_with("--config=")
            || a.starts_with("--jobs=")
            || a.starts_with("--bin=")
            || a.starts_with("--example=")
            || a.starts_with("--test=")
            || a.starts_with("--bench=")
            || a.starts_with("--package=")
            || a.starts_with("--exclude=")
            || a.starts_with("--color=")
            || a.starts_with("--message-format=")
            || a.starts_with("--timings=")
            || a.starts_with("-Z")
            || a.starts_with("-F")
            || a.starts_with("-j")
        {
            out.push(extra_args[i].clone());
            i += 1;
            continue;
        }
        // Skip two-token pytest flags so their value token isn't misinterpreted either!
        if matches!(
            a,
            "-o" | "--override-ini"
                | "-c"
                | "--config-file"
                | "--rootdir"
                | "--confcutdir"
                | "--doctest-glob"
                | "--ignore"
                | "--ignore-glob"
                | "--deselect"
                | "--import-mode"
                | "--maxfail"
                | "--tb"
                | "--capture"
                | "--durations"
                | "--durations-min"
                | "--junitxml"
                | "--junit-prefix"
                | "-r"
                | "-W"
                | "--filterwarnings"
                | "--basetemp"
                | "--log-level"
                | "--log-cli-level"
                | "--log-format"
                | "--log-date-format"
                | "--log-file"
                | "-n"
                | "--dist"
                | "-k"
                | "-m"
        ) {
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Strips pytest-specific flags from `extra_args` before passing to Cargo/Frontend runners in a
/// mixed polyglot suite.
pub(super) fn strip_pytest_only_flags_for_non_py(extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < extra_args.len() {
        let a = extra_args[i].as_str();
        if matches!(
            a,
            "-o" | "--override-ini"
                | "-c"
                | "--config-file"
                | "--rootdir"
                | "--confcutdir"
                | "--doctest-glob"
                | "--ignore"
                | "--ignore-glob"
                | "--deselect"
                | "--import-mode"
                | "--maxfail"
                | "--tb"
                | "--capture"
                | "--durations"
                | "--durations-min"
                | "--junitxml"
                | "--junit-prefix"
                | "-r"
                | "-W"
                | "--filterwarnings"
                | "--basetemp"
                | "--log-level"
                | "--log-cli-level"
                | "--log-format"
                | "--log-date-format"
                | "--log-file"
                | "-n"
                | "--dist"
        ) {
            i += 2;
            continue;
        }
        // Strip BOTH separated (`-p custom_plugin`) and attached (`-pcustom_plugin`) pytest
        // plugin flags so they never leak into Node/Vitest/Jest/Cargo in mixed runs!
        if a == "-p" {
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("-p")
            && !rest.is_empty()
            && !rest.starts_with('-')
        {
            i += 1;
            continue;
        }
        if is_pytest_collection_override_token(a)
            || matches!(
                a,
                "-x" | "--exitfirst"
                    | "-s"
                    | "--lf"
                    | "--last-failed"
                    | "--ff"
                    | "--failed-first"
                    | "--nf"
                    | "--new-first"
                    | "--sw"
                    | "--stepwise"
                    | "--stepwise-skip"
                    | "-l"
                    | "--showlocals"
                    | "--full-trace"
                    | "--strict"
                    | "--strict-markers"
                    | "--strict-config"
                    | "--cache-clear"
                    | "--cache-show"
                    | "--pdb"
                    | "--trace"
                    | "--no-cov"
                    | "--setup-only"
                    | "--setup-plan"
                    | "--setup-show"
                    | "--fixtures"
                    | "--markers"
            )
            || a.starts_with("--maxfail=")
            || a.starts_with("--tb=")
            || a.starts_with("--capture=")
            || a.starts_with("--durations=")
            || a.starts_with("--durations-min=")
            || a.starts_with("--junitxml=")
            || a.starts_with("--junit-prefix=")
            || a.starts_with("--filterwarnings=")
            || a.starts_with("--basetemp=")
            || a.starts_with("--log-")
            || a.starts_with("--cov")
            || a.starts_with("-r")
            || a.starts_with("-W")
        {
            i += 1;
            continue;
        }
        out.push(extra_args[i].clone());
        i += 1;
    }
    out
}

pub(super) fn is_pytest_info_or_collect_only_token(a: &str) -> bool {
    matches!(
        a,
        "--version"
            | "-V"
            | "--help"
            | "-h"
            | "--fixtures"
            | "--fixtures-per-test"
            | "--markers"
            | "--co"
            | "--collect-only"
            | "--setup-plan"
    )
}

pub(super) fn tokens_have_pytest_info_or_collect_only(tokens: &[&str]) -> bool {
    let mut i = 0usize;
    while i < tokens.len() {
        let a = tokens[i];
        if a == "--" {
            break;
        }
        if is_pytest_info_or_collect_only_token(a) {
            return true;
        }
        // Inspect `-o addopts=...`, `--override-ini addopts=...`, `--override-ini=addopts=...`!
        let ini_val: Option<&str> = if matches!(a, "-o" | "--override-ini") {
            let v = tokens.get(i + 1).copied();
            i += 2;
            v
        } else if let Some(v) = a.strip_prefix("--override-ini=") {
            i += 1;
            Some(v)
        } else if let Some(v) = a.strip_prefix("-o")
            && !v.is_empty()
            && !v.starts_with('-')
        {
            i += 1;
            Some(v)
        } else if is_pytest_two_token_option(a) {
            // Skip value token of `-k 'not --collect-only'`, `-m ...`, etc.!
            i += 2;
            None
        } else {
            i += 1;
            None
        };
        if let Some(iv) = ini_val
            && let Some((k, val)) = iv.split_once('=')
            && k.trim() == "addopts"
        {
            let nested = split_pytest_shlex(val);
            let nested_refs: Vec<&str> = nested.iter().map(String::as_str).collect();
            if tokens_have_pytest_info_or_collect_only(&nested_refs) {
                return true;
            }
        }
    }
    false
}

pub(super) fn is_pytest_info_or_collect_only_flag_effective(
    pytest_args: &[String],
    configured_pytest_cmd: &[String],
) -> bool {
    let req_refs: Vec<&str> = pytest_args.iter().map(String::as_str).collect();
    if tokens_have_pytest_info_or_collect_only(&req_refs) {
        return true;
    }
    let cmd_skip = pytest_cmd_arg_start(configured_pytest_cmd);
    let cmd_refs: Vec<&str> = configured_pytest_cmd
        .iter()
        .skip(cmd_skip)
        .map(String::as_str)
        .collect();
    if tokens_have_pytest_info_or_collect_only(&cmd_refs) {
        return true;
    }
    if let Ok(addopts) = std::env::var("PYTEST_ADDOPTS") {
        let env_toks = split_pytest_shlex(&addopts);
        let env_refs: Vec<&str> = env_toks.iter().map(String::as_str).collect();
        if tokens_have_pytest_info_or_collect_only(&env_refs) {
            return true;
        }
    }
    false
}


