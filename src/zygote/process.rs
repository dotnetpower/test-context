use super::*;

#[derive(Serialize)]
pub(super) struct ZygoteRequest<'a> {
    pub(super) args: &'a [String],
    pub(super) timeout_secs: u64,
    pub(super) capture_dynamic_deps: bool,
}

#[derive(Deserialize)]
pub(super) struct ZygoteReply {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: Option<String>,
    pub(super) stderr: Option<String>,
    pub(super) duration_ms: Option<u64>,
    pub(super) timed_out: Option<bool>,
    pub(super) cancelled: Option<bool>,
    pub(super) reports: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) collected_counts: Option<std::collections::BTreeMap<String, usize>>,
    pub(super) dynamic_deps: Option<Vec<String>>,
    pub(super) shared_dynamic_deps: Option<Vec<String>>,
    pub(super) module_dynamic_deps: Option<std::collections::BTreeMap<String, Vec<String>>>,
    pub(super) dynamic_deps_incomplete: Option<bool>,
    #[serde(default)]
    pub(super) external_module_entries: Option<Vec<serde_json::Value>>,
    pub(super) plugin_errors: Option<Vec<String>>,
    pub(super) error: Option<String>,
}

pub struct PytestZygote {
    pub(super) child: Child,
    pub(super) stdin: ChildStdin,
    pub(super) stdout: BufReader<ChildStdout>,
    pub(super) python_bin: PathBuf,
    pub(super) pythonpath: std::ffi::OsString,
    pub(super) preload_modules: BTreeSet<String>,
    pub(super) pytest_plugins: String,
    pub(super) environment_hash: String,
    pub(super) root: PathBuf,
    pub(super) local_py_modules_snapshot: BTreeMap<String, PathBuf>,
    pub(super) parent_loaded_modules: BTreeSet<String>,
    pub(super) parent_loaded_local_stamps: BTreeMap<String, Option<crate::ast_graph::FileStamp>>,
    pub(super) parent_loaded_py_module_files: Vec<PathBuf>,
    pub(super) startup_untrackable: bool,
    pub(super) plugin_errors: Vec<String>,
    pub(super) runs: u32,
}

/// Maps each top-level local Python module/package name visible under `root` and `pythonpath`
/// to its highest-priority resolved origin path (package `__init__.py` takes precedence over
/// `<name>.py` within the same directory, matching Python's import resolution order). Comparing
/// resolved origins detects BOTH newly added modules AND adding `foo/__init__.py` alongside an
/// existing `foo.py` (or adding an earlier `PYTHONPATH` shadowing entry).
pub(super) fn collect_local_python_modules(
    root: &Path,
    pythonpath: &std::ffi::OsStr,
) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let mut search_dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    if !pythonpath.is_empty() {
        for dir in std::env::split_paths(pythonpath) {
            if !dir.as_os_str().is_empty() && !search_dirs.contains(&dir) {
                search_dirs.push(dir);
            }
        }
    }
    for dir in &search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut dir_pkgs: Vec<(String, PathBuf)> = Vec::new();
        let mut dir_mods: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Follow symlinks via `std::fs::metadata` so symlinked local modules/packages
            // shadowing a preloaded dependency are detected too.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                let init_py = path.join("__init__.py");
                if init_py.is_file() {
                    dir_pkgs.push((name_str.to_string(), init_py));
                }
            } else if meta.is_file()
                && let Some(stem) = name_str.strip_suffix(".py")
                && !stem.is_empty()
            {
                dir_mods.push((stem.to_string(), path));
            }
        }
        // Within each search directory, packages (`foo/__init__.py`) shadow modules (`foo.py`).
        for (name, origin) in dir_pkgs {
            out.entry(name).or_insert(origin);
        }
        for (name, origin) in dir_mods {
            out.entry(name).or_insert(origin);
        }
    }
    out
}

pub(super) fn zygote_path_stamp(path: &Path) -> Option<crate::ast_graph::FileStamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(crate::ast_graph::FileStamp {
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            size: meta.len(),
            ino: meta.ino(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub(super) fn preloaded_modules_shadowed_by_workspace(
    root: &Path,
    pythonpath: &std::ffi::OsStr,
    preloaded: &BTreeSet<String>,
    spawn_snapshot: &BTreeMap<String, PathBuf>,
    parent_loaded_modules: &BTreeSet<String>,
    parent_loaded_local_stamps: &BTreeMap<String, Option<crate::ast_graph::FileStamp>>,
    parent_loaded_py_module_files: &[PathBuf],
) -> bool {
    // 1. If ANY local file (source or startup data) loaded into the Zygote parent changed,
    // was replaced, or was deleted on disk, restart the parent.
    for (local_path, prev_stamp) in parent_loaded_local_stamps {
        if zygote_path_stamp(Path::new(local_path)) != *prev_stamp {
            return true;
        }
    }
    // 2. Detect nested submodule package shadowing: if the parent loaded `foo/bar.py` and a
    // sibling package `foo/bar/__init__.py` now exists on disk, restart the parent.
    for mod_file in parent_loaded_py_module_files {
        if let Some(stem) = mod_file.file_stem()
            && let Some(parent_dir) = mod_file.parent()
            && parent_dir.join(stem).join("__init__.py").is_file()
        {
            return true;
        }
    }
    if preloaded.is_empty() && parent_loaded_modules.is_empty() {
        return false;
    }
    let current = collect_local_python_modules(root, pythonpath);
    // 2. If ANY top-level local Python module or package appeared OR changed its resolved origin
    // (e.g. `foo/__init__.py` added alongside `foo.py`, or a symlinked module shadowing a
    // preloaded / transitively loaded module in `sys.modules`), restart the parent.
    for (m, origin) in &current {
        if spawn_snapshot.get(m) != Some(origin)
            && (!preloaded.is_empty() || parent_loaded_modules.contains(m))
        {
            return true;
        }
    }
    for module in preloaded {
        let top = module.split('.').next().unwrap_or(module).trim();
        if !top.is_empty() && current.contains_key(top) {
            return true;
        }
    }
    false
}

impl PytestZygote {
    pub fn spawn(
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> Result<Self> {
        Self::spawn_with_env(spec, pythonpath, preload_modules, pytest_plugins, "")
    }

    pub fn spawn_with_env(
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
        environment_hash: &str,
    ) -> Result<Self> {
        let python_bin = crate::runner::resolve_executable_on_path_in_dir(
            &resolve_python_bin_for_root(&spec.root, &spec.effective_pytest_cmd()),
            &spec.root,
        );
        let preload_str = preload_modules.join(",");
        let plugin_str = pytest_plugins.join(",");
        // Preserve caller's `PYTHONDONTWRITEBYTECODE` environment in forked children while
        // keeping `sys.dont_write_bytecode = True` during parent startup ONLY if unset, so
        // preloading doesn't pollute `__pycache__` in workspace/tempdirs and tests see the
        // exact caller `os.environ` and `sys.flags`.
        let mut cmd = Command::new(&python_bin);
        if let Some(aug_path) = crate::catalog::augmented_user_path() {
            cmd.env("PATH", aug_path);
        }
        cmd.current_dir(&spec.root)
            .env("PYTHONPATH", &pythonpath)
            .env("TCTX_PRELOAD_MODULES", &preload_str)
            .env("TCTX_PYTEST_PLUGINS", &plugin_str)
            .arg("-c")
            .arg(ZYGOTE_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Note: Do NOT set `PR_SET_PDEATHSIG` on the persistent zygote parent process because on
        // Linux `PR_SET_PDEATHSIG` fires when the spawning OS *thread* exits, killing a healthy
        // shared zygote whenever a short-lived worker thread finishes. Instead, the zygote parent
        // exits cleanly on stdin EOF when the owning process exits or `PytestZygote::drop` runs,
        // and the single-threaded zygote parent still sets `PR_SET_PDEATHSIG` on each forked child.

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("missing zygote stdin"))?;
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing zygote stdout"))?;
        let mut stdout = BufReader::new(stdout_pipe);
        let startup_deadline = Instant::now() + ZYGOTE_STARTUP_TIMEOUT;
        let banner = match read_line_with_deadline(
            &mut stdout,
            2 * 1024 * 1024,
            startup_deadline,
            "zygote readiness handshake",
        ) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let trimmed_banner = banner.trim();
        let Some(banner_rest) = trimmed_banner.strip_prefix("TCTX_ZYGOTE_READY") else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(format!(
                "zygote failed readiness handshake: {}",
                trimmed_banner
            ))
            .into());
        };
        let mut parent_loaded_modules = BTreeSet::new();
        let mut parent_loaded_local_stamps = BTreeMap::new();
        let mut parent_loaded_py_module_files = Vec::new();
        let mut startup_untrackable = false;
        let rest_json = banner_rest.trim();
        if !rest_json.is_empty()
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(rest_json)
        {
            startup_untrackable = val
                .get("untrackable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(mods) = val.get("modules").and_then(|v| v.as_array()) {
                for m in mods {
                    if let Some(s) = m.as_str() {
                        parent_loaded_modules.insert(s.to_string());
                    }
                }
            }
            let parsed_ready_stamps = val.get("local_stamps").and_then(|v| v.as_object());
            if let Some(lfiles) = val.get("local_files").and_then(|v| v.as_array()) {
                for f in lfiles {
                    if let Some(s) = f.as_str() {
                        let live_stamp = zygote_path_stamp(Path::new(s));
                        // Prefer the exact pre-read stamp captured by Python BEFORE startup reads
                        // completed and immediately compare against `live_stamp` to close the
                        // handoff race between Python startup and Rust parent acceptance.
                        let pre_stamp = parsed_ready_stamps
                            .and_then(|m| m.get(s))
                            .and_then(|v| v.as_array())
                            .filter(|arr| arr.len() == 6)
                            .and_then(|arr| {
                                Some(crate::ast_graph::FileStamp {
                                    mtime_sec: arr[0].as_i64()?,
                                    mtime_nsec: arr[1].as_i64()?,
                                    size: arr[2].as_u64()?,
                                    ino: arr[3].as_u64()?,
                                    ctime_sec: arr[4].as_i64()?,
                                    ctime_nsec: arr[5].as_i64()?,
                                })
                            });
                        if let Some(ps) = pre_stamp {
                            if live_stamp != Some(ps) {
                                startup_untrackable = true;
                            }
                            parent_loaded_local_stamps.insert(s.to_string(), Some(ps));
                        } else {
                            parent_loaded_local_stamps.insert(s.to_string(), live_stamp);
                        }
                    }
                }
            }
            if let Some(pfiles) = val.get("py_module_files").and_then(|v| v.as_array()) {
                for f in pfiles {
                    if let Some(s) = f.as_str() {
                        parent_loaded_py_module_files.push(PathBuf::from(s));
                    }
                }
            }
        }

        if startup_untrackable {
            // Do NOT use a newly spawned parent whose startup performed untrackable external/dir
            // probes, spawned surviving background threads, or observed a pre-read stamp mismatch:
            // terminate it immediately so the caller falls back cleanly to a fresh subprocess.
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(
                "ZYGOTE_PRE_START: untrackable or concurrently modified zygote startup state",
            )
            .into());
        }

        let local_py_modules_snapshot = collect_local_python_modules(&spec.root, &pythonpath);
        Ok(Self {
            child,
            stdin,
            stdout,
            python_bin,
            pythonpath,
            preload_modules: preload_modules.iter().cloned().collect(),
            pytest_plugins: plugin_str,
            environment_hash: environment_hash.to_string(),
            root: spec.root.clone(),
            local_py_modules_snapshot,
            parent_loaded_modules,
            parent_loaded_local_stamps,
            parent_loaded_py_module_files,
            startup_untrackable,
            plugin_errors: Vec::new(),
            runs: 0,
        })
    }

    pub fn matches_config(
        &mut self,
        spec: &Spec,
        pythonpath: &std::ffi::OsStr,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> bool {
        self.matches_config_with_env(spec, pythonpath, preload_modules, pytest_plugins, "")
    }

    pub fn matches_config_with_env(
        &mut self,
        spec: &Spec,
        pythonpath: &std::ffi::OsStr,
        preload_modules: &[String],
        pytest_plugins: &[String],
        environment_hash: &str,
    ) -> bool {
        self.child.try_wait().ok().flatten().is_none()
            && !self.startup_untrackable
            && self.runs < 200
            && self.root == spec.root
            && self.pythonpath == pythonpath
            && self.python_bin
                == crate::runner::resolve_executable_on_path_in_dir(
                    &resolve_python_bin_for_root(&spec.root, &spec.effective_pytest_cmd()),
                    &spec.root,
                )
            && preload_modules
                .iter()
                .all(|module| self.preload_modules.contains(module))
            && !preloaded_modules_shadowed_by_workspace(
                &spec.root,
                pythonpath,
                &self.preload_modules,
                &self.local_py_modules_snapshot,
                &self.parent_loaded_modules,
                &self.parent_loaded_local_stamps,
                &self.parent_loaded_py_module_files,
            )
            && self.pytest_plugins == pytest_plugins.join(",")
            && self.environment_hash == environment_hash
    }

    pub fn run_pytest(
        &mut self,
        args: &[String],
        timeout_secs: u64,
        active_pgid: &AtomicI32,
        capture_dynamic_deps: bool,
        parallel_pgids: Option<&Mutex<BTreeSet<i32>>>,
    ) -> Result<BoundedOutput> {
        let started = Instant::now();
        self.runs = self.runs.saturating_add(1);
        let req = ZygoteRequest {
            args,
            timeout_secs,
            capture_dynamic_deps,
        };
        let payload = serde_json::to_vec(&req)?;
        if payload.len() > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote request too large").into());
        }
        let write_deadline = Instant::now() + ZYGOTE_STARTUP_TIMEOUT;
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&u32::try_from(payload.len())?.to_be_bytes());
        frame.extend_from_slice(&payload);
        write_all_with_deadline(&mut self.stdin, &frame, write_deadline)
            .map_err(|e| invalid(format!("ZYGOTE_PRE_START: stdin write failed: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| invalid(format!("ZYGOTE_PRE_START: stdin flush failed: {e}")))?;

        let mut started_length = [0_u8; 4];
        let start_deadline = Instant::now() + ZYGOTE_STARTUP_TIMEOUT;
        // Once the request has been flushed to zygote stdin, the parent may already have forked the
        // test child; never tag post-send read failures as `ZYGOTE_PRE_START` so pytest is never run twice.
        read_exact_with_deadline(
            &mut self.stdout,
            &mut started_length,
            start_deadline,
            "zygote child-start frame",
        )?;
        let length = u32::from_be_bytes(started_length) as usize;
        if length > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote response too large").into());
        }
        let mut reply_bytes = vec![0_u8; length];
        read_exact_with_deadline(
            &mut self.stdout,
            &mut reply_bytes,
            start_deadline,
            "zygote child-start payload",
        )?;
        let started_frame: serde_json::Value = serde_json::from_slice(&reply_bytes)?;
        if let Some(pre_err) = started_frame["pre_start_error"].as_str() {
            return Err(invalid(format!("ZYGOTE_PRE_START: {pre_err}")).into());
        }
        let pid = started_frame["started_pid"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| invalid("zygote did not return a child pid"))?;
        active_pgid.store(pid, Ordering::Release);
        let _active_guard = ActivePgidGuard(active_pgid);
        let mut already_cancelled = false;
        if let Some(registry) = parallel_pgids {
            let mut pgids = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pgids.contains(&i32::MIN) {
                already_cancelled = true;
            } else {
                pgids.insert(pid);
            }
        }
        let _parallel_guard = ParallelPgidGuard {
            registry: parallel_pgids,
            pgid: pid,
        };
        if already_cancelled {
            unsafe {
                if libc::kill(-pid, libc::SIGKILL) != 0 {
                    let _ = libc::kill(pid, libc::SIGKILL);
                }
            }
        }

        let mut final_length = [0_u8; 4];
        let response_deadline = Instant::now() + Duration::from_secs(timeout_secs.saturating_add(10));
        if let Err(error) = read_exact_with_deadline(
            &mut self.stdout,
            &mut final_length,
            response_deadline,
            "zygote test-result frame",
        ) {
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            return Err(error);
        }
        let length = u32::from_be_bytes(final_length) as usize;
        if length > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote response too large").into());
        }
        let mut reply_bytes = vec![0_u8; length];
        if let Err(error) = read_exact_with_deadline(
            &mut self.stdout,
            &mut reply_bytes,
            response_deadline,
            "zygote test-result payload",
        ) {
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            return Err(error);
        }
        let reply: ZygoteReply = serde_json::from_slice(&reply_bytes)?;
        if let Some(err) = reply.error {
            return Err(invalid(format!("zygote error: {err}")).into());
        }
        let code = reply.exit_code.unwrap_or(1);
        let status = std::process::ExitStatus::from_raw((code & 0xff) << 8);
        let duration_ms = reply.duration_ms.unwrap_or_else(|| {
            u64::try_from(started.elapsed().as_millis())
                .unwrap_or(1)
                .max(1)
        });
        let structured = Some(serde_json::to_vec(&serde_json::json!({
            "reports": reply.reports.unwrap_or_else(|| serde_json::json!([])),
            "collected_counts": reply.collected_counts.unwrap_or_default(),
            "dynamic_deps": reply.dynamic_deps.unwrap_or_default(),
            "shared_dynamic_deps": reply.shared_dynamic_deps.unwrap_or_default(),
            "module_dynamic_deps": reply.module_dynamic_deps.unwrap_or_default(),
            "dynamic_deps_incomplete": reply.dynamic_deps_incomplete.unwrap_or(false),
            "external_module_entries": reply.external_module_entries.unwrap_or_default(),
        }))?);
        self.plugin_errors = reply.plugin_errors.unwrap_or_default();
        Ok(BoundedOutput {
            status,
            stdout: reply.stdout.unwrap_or_default().into_bytes(),
            stderr: reply.stderr.unwrap_or_default().into_bytes(),
            duration_ms,
            timed_out: reply.timed_out.unwrap_or(false),
            structured,
            cancelled: reply.cancelled.unwrap_or(false),
        })
    }

    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for PytestZygote {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct ZygoteManager {
    pub(super) zygote: Option<PytestZygote>,
    pub(super) failures: u32,
    pub(super) disabled_until: Option<Instant>,
    pub(super) last_error: Option<String>,
    pub(super) active_pgid: Arc<AtomicI32>,
    pub(super) last_used: Option<Instant>,
}

impl Default for ZygoteManager {
    fn default() -> Self {
        Self {
            zygote: None,
            failures: 0,
            disabled_until: None,
            last_error: None,
            active_pgid: Arc::new(AtomicI32::new(0)),
            last_used: None,
        }
    }
}

impl ZygoteManager {
    pub fn active_handle(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.active_pgid)
    }
    pub fn ensure(
        &mut self,
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> Result<()> {
        self.ensure_with_env(spec, pythonpath, preload_modules, pytest_plugins, "")
    }

    pub fn ensure_with_env(
        &mut self,
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
        environment_hash: &str,
    ) -> Result<()> {
        if self
            .disabled_until
            .is_some_and(|deadline| deadline > Instant::now())
        {
            return Err(invalid("zygote circuit breaker is open").into());
        }

        let needs_spawn = self.zygote.as_mut().is_none_or(|zygote| {
            !zygote.matches_config_with_env(
                spec,
                &pythonpath,
                preload_modules,
                pytest_plugins,
                environment_hash,
            )
        });
        if needs_spawn {
            match PytestZygote::spawn_with_env(
                spec,
                pythonpath,
                preload_modules,
                pytest_plugins,
                environment_hash,
            ) {
                Ok(parent) => {
                    self.zygote = Some(parent);
                }
                Err(err) => {
                    // Track startup failures in the circuit breaker too so incompatible startup
                    // environments apply bounded backoff instead of retrying every single run.
                    self.failures = self.failures.saturating_add(1);
                    self.last_error = Some(err.to_string());
                    self.zygote = None;
                    if self.failures >= 3 {
                        self.disabled_until = Some(Instant::now() + Duration::from_secs(60));
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        args: &[String],
        timeout_secs: u64,
        capture_dynamic_deps: bool,
        parallel_pgids: Option<&Mutex<BTreeSet<i32>>>,
    ) -> Result<BoundedOutput> {
        let Some(zygote) = self.zygote.as_mut() else {
            return Err(invalid("zygote unavailable").into());
        };
        match zygote.run_pytest(
            args,
            timeout_secs,
            &self.active_pgid,
            capture_dynamic_deps,
            parallel_pgids,
        ) {
            Ok(output) => {
                self.last_used = Some(Instant::now());
                self.failures = 0;
                self.last_error = None;
                Ok(output)
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(error.to_string());
                self.zygote = None;
                if self.failures >= 3 {
                    self.disabled_until = Some(Instant::now() + Duration::from_secs(60));
                }
                Err(error)
            }
        }
    }

    pub fn status(&self) -> serde_json::Value {
        let (pid, rss_bytes) = self
            .zygote
            .as_ref()
            .map(|zygote| {
                let pid = zygote.pid();
                (Some(pid), process_rss_bytes(pid))
            })
            .unwrap_or((None, None));
        serde_json::json!({
            "ready": self.zygote.is_some(),
            "failures": self.failures,
            "circuit_open": self.disabled_until.is_some_and(|deadline| deadline > Instant::now()),
            "last_error": self.last_error,
            "active_pgid": self.active_pgid.load(Ordering::Acquire),
            "plugin_errors": self.zygote.as_ref().map(|zygote| &zygote.plugin_errors),
            "runs": self.zygote.as_ref().map(|zygote| zygote.runs).unwrap_or(0),
            "pid": pid,
            "rss_bytes": rss_bytes,
            "idle_ms": self.last_used.map(|last| u64::try_from(last.elapsed().as_millis()).unwrap_or(u64::MAX)),
        })
    }

    pub fn cancel_active(&self) -> bool {
        let pgid = self.active_pgid.load(Ordering::Acquire);
        if pgid <= 0 {
            return false;
        }
        let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        result == 0
    }

    pub fn shutdown_if_idle(&mut self, timeout: Duration) -> bool {
        if self
            .last_used
            .is_some_and(|last_used| last_used.elapsed() >= timeout)
            && self.active_pgid.load(Ordering::Acquire) == 0
        {
            self.zygote = None;
            self.last_used = None;
            return true;
        }
        false
    }
}

pub(super) fn process_rss_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

pub fn cancel_process_group(active_pgid: &AtomicI32) -> bool {
    let pgid = active_pgid.load(Ordering::Acquire);
    if pgid <= 0 {
        return false;
    }
    unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 }
}

pub(super) fn read_first_line_lossy(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 512];
    let n = std::io::Read::read(&mut f, &mut buf).ok()?;
    // Slice strictly to the first newline BEFORE UTF-8 decoding so a multibyte character split
    // at the buffer boundary on a later line never invalidates line 1's `#!` shebang.
    let end = memchr::memchr(b'\n', &buf[..n]).unwrap_or(n);
    Some(String::from_utf8_lossy(&buf[..end]).trim().to_string())
}

/// Returns `true` if `script_path` has a `#!` shebang passing extra interpreter flags (e.g.
/// `#!/usr/bin/python3 -I` or `#!/usr/bin/env -S python3 -I`) that zygote mode would not honor.
pub fn script_shebang_has_interpreter_flags(root: &Path, program: &str) -> bool {
    let raw_path = Path::new(program);
    let script_path = if !raw_path.is_absolute() && (program.contains('/') || program.contains('\\'))
    {
        root.join(raw_path)
    } else {
        crate::runner::resolve_executable_on_path(raw_path)
    };
    let Some(line) = read_first_line_lossy(&script_path) else {
        return false;
    };
    let Some(rest) = line.strip_prefix("#!").map(str::trim) else {
        return false;
    };
    let mut parts = rest.split_whitespace();
    let Some(prog) = parts.next() else {
        return false;
    };
    let prog_name = Path::new(prog)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if prog_name == "env" {
        let mut saw_python = false;
        for arg in parts {
            if !saw_python {
                if arg == "-S" || arg.starts_with("-S") {
                    continue;
                }
                if arg.starts_with('-') {
                    return true;
                }
                if arg.starts_with("python") || arg.starts_with("pypy") {
                    saw_python = true;
                } else {
                    return false;
                }
            } else {
                // Any argument after the python executable in `env -S python3 -I` is an interpreter flag.
                return true;
            }
        }
        return false;
    }
    if prog_name.starts_with("python") || prog_name.starts_with("pypy") {
        return parts.next().is_some();
    }
    false
}

pub(super) fn python_from_script_shebang(script_path: &Path) -> Option<PathBuf> {
    let line = read_first_line_lossy(script_path)?;
    let rest = line.strip_prefix("#!")?.trim();
    let mut parts = rest.split_whitespace();
    let prog = parts.next()?;
    let prog_name = Path::new(prog).file_name().and_then(|s| s.to_str())?;
    if prog_name == "env" {
        for arg in parts {
            if arg.starts_with('-') {
                continue;
            }
            if arg.starts_with("python") || arg.starts_with("pypy") {
                return Some(PathBuf::from(arg));
            }
            break;
        }
        return None;
    }
    if prog_name.starts_with("python") || prog_name.starts_with("pypy") {
        let pb = PathBuf::from(prog);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

pub fn resolve_python_bin_for_root(root: &Path, pytest_cmd: &[String]) -> PathBuf {
    let Some(first) = pytest_cmd.first() else {
        return PathBuf::from("python3");
    };
    let raw_path = Path::new(first);
    // Resolve relative command paths like `./.venv/bin/pytest` against the workspace root
    // rather than the daemon's working directory.
    let first_path = if !raw_path.is_absolute() && (first.contains('/') || first.contains('\\')) {
        root.join(raw_path)
    } else {
        crate::runner::resolve_executable_on_path_in_dir(raw_path, root)
    };
    if let Some(name) = first_path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with("python") || name.starts_with("pypy") {
            return first_path;
        }
        // For a `pytest` executable script (on PATH or in a venv), honor its `#!` interpreter
        // shebang before falling back to sibling python binaries.
        if first_path.is_file()
            && let Some(shebang_py) = python_from_script_shebang(&first_path)
        {
            return crate::runner::resolve_executable_on_path_in_dir(&shebang_py, root);
        }
        if let Some(parent) = first_path.parent() {
            for candidate in ["python3", "python", "pypy3", "pypy"] {
                let py = parent.join(candidate);
                if py.is_file() {
                    return py;
                }
            }
        }
    }
    PathBuf::from("python3")
}

pub fn resolve_python_bin(pytest_cmd: &[String]) -> PathBuf {
    resolve_python_bin_for_root(Path::new("."), pytest_cmd)
}


