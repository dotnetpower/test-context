use super::*;

pub(super) struct DaemonHub {
    pub(super) roots: Mutex<BTreeMap<PathBuf, Arc<ManagedRoot>>>,
    pub(super) load_gate: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    pub(super) pending_cancels: Mutex<std::collections::BTreeSet<(PathBuf, String)>>,
    pub(super) in_flight_loads: AtomicUsize,
    pub(super) traces: Mutex<VecDeque<Value>>,
    pub(super) metrics: Mutex<DaemonMetrics>,
    pub(super) started_at: Instant,
    pub(super) local_oneshot: bool,
    pub(super) shutting_down: AtomicBool,
}

pub(super) struct InFlightLoadGuard<'a>(pub(super) &'a AtomicUsize);

impl Drop for InFlightLoadGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl DaemonHub {
    pub(super) fn new() -> Self {
        Self::with_mode(false)
    }

    pub(super) fn with_mode(local_oneshot: bool) -> Self {
        Self {
            roots: Mutex::new(BTreeMap::new()),
            load_gate: Mutex::new(BTreeMap::new()),
            pending_cancels: Mutex::new(std::collections::BTreeSet::new()),
            in_flight_loads: AtomicUsize::new(0),
            traces: Mutex::new(VecDeque::new()),
            metrics: Mutex::new(DaemonMetrics {
                total_requests: 0,
                error_requests: 0,
                test_runs: 0,
                total_saved_ms: 0,
                total_cached_files: 0,
                total_executed_files: 0,
            }),
            started_at: Instant::now(),
            local_oneshot,
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(super) fn lock_roots(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<ManagedRoot>>> {
        self.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn root_load_gate(&self, root: &Path) -> Arc<Mutex<()>> {
        let mut gates = self
            .load_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            gates
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub(super) fn lock_traces(&self) -> std::sync::MutexGuard<'_, VecDeque<Value>> {
        self.traces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
        for root in roots {
            root.cancel_active();
        }
    }

    pub(super) fn shutdown_all(&self) {
        let roots = {
            let mut guard = self.lock_roots();
            std::mem::take(&mut *guard)
                .into_values()
                .collect::<Vec<_>>()
        };
        for root in roots {
            root.shutdown();
        }
    }

    pub(super) fn gc_load_gates(&self) {
        let mut gates = self
            .load_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    }

    pub(super) fn collect_eviction_candidates(&self) -> Vec<(PathBuf, bool, Duration, usize)> {
        let snapshot: Vec<(PathBuf, Arc<ManagedRoot>)> = self
            .lock_roots()
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        snapshot
            .into_iter()
            .map(|(path, root)| {
                let evictable = root.is_evictable();
                let idle = root.last_activity_elapsed();
                let files = root.indexed_file_count();
                (path, evictable, idle, files)
            })
            .collect()
    }

    pub(super) fn try_evict_key_if_still_evictable(
        guard: &mut BTreeMap<PathBuf, Arc<ManagedRoot>>,
        key: &Path,
    ) -> Option<Arc<ManagedRoot>> {
        if guard.get(key).is_some_and(|root| root.is_evictable()) {
            guard.remove(key)
        } else {
            None
        }
    }

    pub(super) fn evict_excess_roots_for_admission(&self, reserved_slots: usize) {
        let max_resident = configured_max_resident_roots();
        let max_mem = configured_max_daemon_rss_bytes();
        let current_mem = daemon_rss_bytes();
        let mut candidates = self.collect_eviction_candidates();
        let mut evicted = Vec::new();
        {
            let mut guard = self.lock_roots();
            while guard.len().saturating_add(reserved_slots) > max_resident {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                let Some(idx) = oldest_idx else {
                    break;
                };
                let (key, _, _, _) = candidates.swap_remove(idx);
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                    evicted.push(old);
                }
            }
            if guard.len() > 1 && current_mem.is_some_and(|rss| rss > max_mem) {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                if let Some(idx) = oldest_idx {
                    let (key, _, _, _) = candidates.swap_remove(idx);
                    if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                        evicted.push(old);
                    }
                }
            }
        }
        let evicted_count = evicted.len();
        for root in evicted {
            root.shutdown();
            drop(root);
        }
        if evicted_count > 0 {
            self.gc_load_gates();
            if !self.local_oneshot {
                eprintln!("[tctx-daemon] event=roots_evicted count={evicted_count}");
            }
            trim_allocator();
        }
    }

    pub(super) fn evict_idle_or_excess_roots(&self) {
        let mut evicted = Vec::new();
        let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
        for root in roots {
            root.reclaim_idle_zygote();
            root.flush_pending_cache();
        }
        let mut candidates = self.collect_eviction_candidates();
        let max_resident = configured_max_resident_roots();
        let max_mem = configured_max_daemon_rss_bytes();
        let mut current_mem = daemon_rss_bytes();
        {
            let mut guard = self.lock_roots();
            let idle_keys: Vec<PathBuf> = candidates
                .iter()
                .filter(|(_, evictable, idle, files)| *evictable && *idle >= root_idle_timeout(*files))
                .map(|(path, _, _, _)| path.clone())
                .collect();
            for k in &idle_keys {
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, k) {
                    evicted.push(old);
                }
            }
            candidates.retain(|(path, _, _, _)| !idle_keys.contains(path));

            while guard.len() > max_resident {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                let Some(idx) = oldest_idx else {
                    break;
                };
                let (key, _, _, _) = candidates.swap_remove(idx);
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                    evicted.push(old);
                }
            }
        }
        let idle_evicted_count = evicted.len();
        for root in evicted.drain(..) {
            root.shutdown();
            drop(root);
        }
        if idle_evicted_count > 0 {
            trim_allocator();
            current_mem = daemon_rss_bytes();
        }
        // Under sustained memory pressure, evict additional idle roots until under budget
        let mut pressure_evicted = 0usize;
        while current_mem.is_some_and(|rss| rss > max_mem) {
            let next_old = {
                let mut guard = self.lock_roots();
                if guard.len() <= 1 {
                    None
                } else {
                    let oldest_idx = candidates
                        .iter()
                        .enumerate()
                        .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                        .max_by_key(|(_, (_, _, idle, _))| *idle)
                        .map(|(idx, _)| idx);
                    oldest_idx.and_then(|idx| {
                        let (key, _, _, _) = candidates.swap_remove(idx);
                        Self::try_evict_key_if_still_evictable(&mut guard, &key)
                    })
                }
            };
            let Some(old) = next_old else {
                break;
            };
            old.shutdown();
            drop(old);
            pressure_evicted += 1;
            trim_allocator();
            current_mem = daemon_rss_bytes();
        }
        if pressure_evicted > 0 && !self.local_oneshot {
            eprintln!("[tctx-daemon] event=roots_evicted_pressure count={pressure_evicted}");
        }
        self.gc_load_gates();
        let high_rss = current_mem.is_some_and(|rss| rss > max_mem / 2);
        if high_rss {
            let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
            for root in roots {
                root.reclaim_idle_zygote();
            }
            trim_allocator();
        }
    }

    pub(super) fn get_or_load_root(&self, requested_root: &Path) -> Result<crate::runner::RootLease> {
        let canonical = requested_root.canonicalize()?;
        // Resolve the most-specific registered root first (from zero-clone Arc cache)
        // so a loaded parent repo never shadows a registered nested child repo.
        let specs = catalog::load_arc()?;
        if let Some(best_registered) = specs
            .iter()
            .filter(|s| canonical.starts_with(&s.root))
            .max_by_key(|s| s.root.components().count())
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&best_registered.root) {
                return Ok(existing.acquire_lease());
            }
        }
        let mut spec = registration::prepare_or_resolve_spec_canonical(&canonical)?;
        if self.local_oneshot {
            spec.policy.background_enabled = false;
        }
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(existing.acquire_lease());
            }
        }
        // Serialize cold loads per root so concurrent callers for the same root do not duplicate work,
        // while allowing independent workspaces to load concurrently without head-of-line blocking.
        let root_gate = self.root_load_gate(&spec.root);
        let _load_guard = root_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(existing.acquire_lease());
            }
        }
        let reserved_slots = self.in_flight_loads.fetch_add(1, Ordering::AcqRel) + 1;
        let _in_flight_guard = InFlightLoadGuard(&self.in_flight_loads);
        if !self.local_oneshot {
            self.evict_excess_roots_for_admission(reserved_slots);
        }
        let load_started = Instant::now();
        let spec_for_load = spec.clone();
        let loaded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            ManagedRoot::load(spec_for_load)
        })) {
            Ok(res) => res?,
            Err(payload) => {
                return Err(invalid(format!(
                    "failed to load workspace root {} (internal panic: {})",
                    spec.root.display(),
                    panic_payload_message(payload.as_ref())
                ))
                .into());
            }
        };
        if !self.local_oneshot {
            eprintln!(
                "[tctx-daemon] event=root_loaded root={} files={} load_ms={:.1}",
                spec.root.display(),
                loaded.indexed_file_count(),
                load_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        let lease = loaded.acquire_lease();
        let mut guard = self.lock_roots();
        // Atomically hand off any pending cold-root cancellations while holding `roots` lock
        // so `test_cancel` can never miss a root being published concurrently.
        let pending_ids: Vec<String> = {
            let mut pending = self
                .pending_cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let matched: Vec<(PathBuf, String)> = pending
                .iter()
                .filter(|(r, _)| r == &spec.root || r.starts_with(&spec.root))
                .cloned()
                .collect();
            let mut ids = Vec::with_capacity(matched.len());
            for key in matched {
                pending.remove(&key);
                ids.push(key.1);
            }
            ids
        };
        for cid in pending_ids {
            loaded.cancel_active_scoped(Some(&cid));
        }
        if let Some(old) = guard.insert(spec.root, Arc::clone(&loaded)) {
            old.shutdown();
        }
        Ok(lease)
    }

    pub(super) fn loaded_root(&self, requested_root: &Path) -> Result<Option<crate::runner::RootLease>> {
        let requested = requested_root.canonicalize()?;
        let guard = self.lock_roots();
        Ok(guard
            .iter()
            .filter(|(root, _)| requested.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, managed)| managed.acquire_lease()))
    }

    pub(super) fn record_trace(&self, mut trace: Value) {
        if let Some(obj) = trace.as_object_mut() {
            obj.remove("arguments");
            obj.remove("value");
        }
        let mut guard = self.lock_traces();
        if guard.len() >= 64 {
            guard.pop_front();
        }
        guard.push_back(trace);
    }

    pub(super) fn capabilities() -> Value {
        json!({
            "schema": 1,
            "test_run": true,
            "test_affected": true,
            "test_graph": true,
            "test_status": true,
            "test_sync": true,
            "test_invalidate": true,
            "test_cancel": true,
            "test_ruff": true,
            "test_explain": true,
            "test_cache_export": true,
            "test_cache_import": true,
            "authorized_worktrees": true,
            "request_tracing": true,
            "trace_clock": "unix_ms",
            "client_trace_outcomes": true,
        })
    }

    pub(super) fn execute(&self, mut req: Request) -> std::result::Result<Value, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("daemon is shutting down".to_string());
        }
        let started = Instant::now();
        let trace_id = extract_trace_id(&mut req.arguments);
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_inner(&req)
        })) {
            Ok(res) => res,
            Err(payload) => Err(invalid(format!(
                "internal daemon panic: {}",
                panic_payload_message(payload.as_ref())
            ))
            .into()),
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let now_ms = crate::cache::now_unix_ms();

        match outcome {
            Ok(value) => {
                if !self.local_oneshot {
                    let mut m = self
                        .metrics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    m.total_requests = m.total_requests.saturating_add(1);
                    if req.name == "test_run" {
                        m.test_runs = m.test_runs.saturating_add(1);
                        let saved = value["saved_ms"].as_u64().unwrap_or(0);
                        let cached = value["cached_files_count"].as_u64().unwrap_or(0);
                        let executed = value["executed_files_count"].as_u64().unwrap_or(0);
                        m.total_saved_ms = m.total_saved_ms.saturating_add(saved);
                        m.total_cached_files = m.total_cached_files.saturating_add(cached);
                        m.total_executed_files = m.total_executed_files.saturating_add(executed);
                        drop(m);
                        let status_s = value["status"].as_str().unwrap_or("unknown");
                        let mode_s = value["mode"].as_str().unwrap_or("unknown");
                        eprintln!(
                            "[tctx-daemon] op=test_run status={status_s} mode={mode_s} cached={cached} executed={executed} saved_ms={saved} duration_ms={elapsed_ms:.1} root={}",
                            req.root
                                .as_deref()
                                .map_or_else(|| "-".to_string(), |p| p.display().to_string())
                        );
                        append_metrics_jsonl(&json!({
                            "unix_ms": now_ms,
                            "op": "test_run",
                            "root": req.root,
                            "status": status_s,
                            "mode": mode_s,
                            "cached_files": cached,
                            "executed_files": executed,
                            "saved_ms": saved,
                            "duration_ms": elapsed_ms,
                        }));
                    } else {
                        drop(m);
                        if matches!(
                            req.name.as_str(),
                            "test_affected" | "test_graph" | "test_sync" | "test_invalidate" | "test_ruff"
                        ) {
                            append_metrics_jsonl(&json!({
                                "unix_ms": now_ms,
                                "op": req.name,
                                "root": req.root,
                                "ok": true,
                                "duration_ms": elapsed_ms,
                            }));
                        }
                        if elapsed_ms >= 200.0 && req.name != "test_trace" {
                            eprintln!(
                                "[tctx-daemon] op={} outcome=slow duration_ms={elapsed_ms:.1} root={}",
                                req.name,
                                req.root
                                    .as_deref()
                                    .map_or_else(|| "-".to_string(), |p| p.display().to_string())
                            );
                        }
                    }
                }
                if let Some(tid) = trace_id {
                    self.record_trace(json!({
                        "trace_id": tid,
                        "operation": req.name,
                        "root": req.root,
                        "execute_ms": elapsed_ms,
                        "daemon_unix_ms": now_ms,
                        "outcome": "success",
                    }));
                    Ok(json!({
                        "_tctx_trace": {
                            "trace_id": tid,
                            "execute_ms": elapsed_ms,
                            "daemon_unix_ms": now_ms,
                        },
                        "value": value,
                    }))
                } else {
                    Ok(value)
                }
            }
            Err(err) => {
                let err_msg = err.to_string();
                if !self.local_oneshot {
                    let mut m = self
                        .metrics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    m.total_requests = m.total_requests.saturating_add(1);
                    m.error_requests = m.error_requests.saturating_add(1);
                    drop(m);
                    eprintln!(
                        "[tctx-daemon] op={} outcome=error duration_ms={elapsed_ms:.1} root={} err={}",
                        req.name,
                        req.root
                            .as_deref()
                            .map_or_else(|| "-".to_string(), |p| p.display().to_string()),
                        err_msg.lines().next().unwrap_or(&err_msg)
                    );
                    append_metrics_jsonl(&json!({
                        "unix_ms": now_ms,
                        "op": req.name,
                        "root": req.root,
                        "ok": false,
                        "duration_ms": elapsed_ms,
                        "error": err_msg.lines().next().unwrap_or(&err_msg),
                    }));
                }
                Err(err_msg)
            }
        }
    }

    pub(super) fn execute_inner(&self, req: &Request) -> Result<Value> {
        match req.name.as_str() {
            "ping" => Ok(json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "uptime_secs": self.started_at.elapsed().as_secs(),
                "capabilities": Self::capabilities(),
            })),
            "test_workspaces" => {
                let specs = catalog::load_arc()?;
                Ok(serde_json::to_value(specs.as_ref())?)
            }
            "test_trace" => {
                self.record_trace(req.arguments.clone());
                Ok(json!({"recorded": true}))
            }
            "test_status" => {
                let root_status = if let Some(root_path) = &req.root {
                    if let Some(managed) = self.loaded_root(root_path)? {
                        Some(managed.status())
                    } else if req
                        .arguments
                        .get("load")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Some(self.get_or_load_root(root_path)?.status())
                    } else {
                        let spec = catalog::resolve_spec(root_path)?;
                        Some(json!({
                            "root": spec.root,
                            "loaded": false,
                            "pytest_cmd": spec.effective_pytest_cmd(),
                            "state": "registered_unloaded",
                        }))
                    }
                } else {
                    None
                };
                let registered = catalog::load_arc()?;
                let loaded_roots: Vec<PathBuf> = self.lock_roots().keys().cloned().collect();
                let metrics = self
                    .metrics
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .to_json();
                Ok(json!({
                    "daemon": {
                        "ok": true,
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                        "uptime_secs": self.started_at.elapsed().as_secs(),
                        "registered_roots": registered.len(),
                        "loaded_roots": loaded_roots,
                        "metrics": metrics,
                    },
                    "capabilities": Self::capabilities(),
                    "workspace": root_status,
                }))
            }
            "test_sync" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_sync requires a workspace root"))?;
                let wait = req
                    .arguments
                    .get("wait")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let managed = self.get_or_load_root(root_path)?;
                managed.sync(wait)
            }
            "test_invalidate" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_invalidate requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let managed = self.get_or_load_root(root_path)?;
                managed.invalidate(&files)
            }
            "test_cancel" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cancel requires a workspace root"))?;
                let cancel_id = req.arguments.get("cancel_id").and_then(Value::as_str);
                let spec_root = catalog::resolve_spec(root_path).map(|s| s.root).ok();
                let canonical = std::fs::canonicalize(root_path).unwrap_or_else(|_| PathBuf::from(root_path));
                // Check `roots` and record `pending_cancels` atomically under `roots` lock so a
                // concurrently completing `get_or_load_root` always observes the cancellation.
                let loaded_opt = {
                    let guard = self.lock_roots();
                    let matched = spec_root
                        .as_ref()
                        .and_then(|sr| guard.get(sr).cloned())
                        .or_else(|| guard.get(&canonical).cloned())
                        .or_else(|| {
                            guard
                                .iter()
                                .filter(|(root, _)| canonical.starts_with(root))
                                .max_by_key(|(root, _)| root.components().count())
                                .map(|(_, m)| Arc::clone(m))
                        });
                    if matched.is_none()
                        && let Some(cid) = cancel_id
                    {
                        let key_root = spec_root.clone().unwrap_or_else(|| canonical.clone());
                        let mut pending = self
                            .pending_cancels
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        if pending.len() >= 256 {
                            pending.clear();
                        }
                        pending.insert((key_root, cid.to_string()));
                    }
                    matched
                };
                let cancelled = loaded_opt.is_some_and(|managed| managed.cancel_active_scoped(cancel_id));
                Ok(json!({"cancelled": cancelled}))
            }
            "test_graph" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_graph requires a workspace root"))?;
                let file = req
                    .arguments
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("test_graph requires 'file' argument"))?;
                let managed = self.get_or_load_root(root_path)?;
                let info = managed.graph_info(file)?;
                Ok(serde_json::to_value(info)?)
            }
            "test_affected" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_affected requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let git_diff = req
                    .arguments
                    .get("git_diff")
                    .and_then(Value::as_bool)
                    .unwrap_or(files.is_empty());
                let managed = self.get_or_load_root(root_path)?;
                managed.affected(&files, git_diff)
            }
            "test_run" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_run requires a workspace root"))?;
                let mut args = req.arguments.clone();
                let caller_env: Option<BTreeMap<String, String>> = args
                    .as_object_mut()
                    .and_then(|obj| {
                        obj.remove("root");
                        obj.remove("_tctx_caller_env")
                    })
                    .and_then(|v| serde_json::from_value(v).ok());
                let run_req: TestRunRequest = if args.is_null() {
                    TestRunRequest::default()
                } else {
                    serde_json::from_value(args)?
                };
                if !self.local_oneshot && run_req.env_partition.is_some() {
                    return Err(invalid(
                        "CALLER_ENV_MISMATCH: env_partition is only valid in local isolated execution",
                    )
                    .into());
                }
                if !self.local_oneshot
                    && let Some(ref cenv) = caller_env
                {
                    let daemon_env = capture_caller_env_for_daemon();
                    // Compare the COMPLETE captured CLI environment against the daemon's environment
                    // so even unindexed plugin variables (e.g. `DJANGO_SETTINGS_MODULE`, custom
                    // pytest plugin config vars) isolate cleanly to the caller's process!
                    if cenv != &daemon_env {
                        return Err(invalid(
                            "CALLER_ENV_MISMATCH: caller environment differs from daemon environment",
                        )
                        .into());
                    }
                }
                let canon_req = std::fs::canonicalize(root_path)
                    .unwrap_or_else(|_| PathBuf::from(root_path));
                let canon_root = catalog::resolve_spec(root_path).map(|s| s.root).ok();
                if let Some(cid) = run_req.cancel_id.as_deref() {
                    let mut pending = self
                        .pending_cancels
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let already = pending.remove(&(canon_req.clone(), cid.to_string()))
                        || canon_root
                            .as_ref()
                            .is_some_and(|c| pending.remove(&(c.clone(), cid.to_string())));
                    if already {
                        return Err(invalid(
                            "TEST_CANCELLED: test_run cancelled while workspace was loading",
                        )
                        .into());
                    }
                }
                let managed = self.get_or_load_root(root_path)?;
                if let Some(cid) = run_req.cancel_id.as_deref() {
                    let mut pending = self
                        .pending_cancels
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let already = pending.remove(&(canon_req, cid.to_string()))
                        || canon_root
                            .as_ref()
                            .is_some_and(|c| pending.remove(&(c.clone(), cid.to_string())));
                    if already {
                        return Err(invalid(
                            "TEST_CANCELLED: test_run cancelled while workspace was loading",
                        )
                        .into());
                    }
                }
                let response = managed.run_tests(&run_req)?;
                Ok(serde_json::to_value(response)?)
            }
            "test_ruff" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_ruff requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let fix = req
                    .arguments
                    .get("fix")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let managed = self.get_or_load_root(root_path)?;
                managed.check_ruff(&files, fix)
            }
            "test_explain" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_explain requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.explain_failures()
            }
            "test_cache_export" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cache_export requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.export_cache_bundle()
            }
            "test_cache_import" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cache_import requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.import_cache_bundle(&req.arguments)
            }
            other => Err(invalid(format!("unknown daemon operation: {other}")).into()),
        }
    }
}

pub(super) fn extract_trace_id(arguments: &mut Value) -> Option<String> {
    let obj = arguments.as_object_mut()?;
    let val = obj.remove(TRACE_ARGUMENT)?;
    let s = val.as_str()?;
    if s.is_empty()
        || s.len() > 96
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return None;
    }
    Some(s.to_owned())
}


