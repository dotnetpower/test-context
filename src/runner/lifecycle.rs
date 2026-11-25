use super::*;

impl ManagedRoot {
    pub fn load(spec: Spec) -> Result<Arc<Self>> {
        // Capture `.gitignore` stamp BEFORE constructing `Workspace` so any edit between
        // matcher construction and watcher startup is detected and reconciled post-watch.
        let initial_gitignore_stamp = file_stamp(&spec.root.join(".gitignore"));
        let workspace = spec.workspace()?;
        let bg_enabled = spec.policy.background_enabled;
        let (graph_res, cache, dirty) = std::thread::scope(|scope| {
            let ws_ref = &workspace;
            let pol_ref = &spec.policy;
            let spec_ref = &spec;
            let graph_handle = scope.spawn(move || DependencyGraph::build_unindexed(ws_ref, pol_ref));
            let cache_handle = scope.spawn(move || {
                RootCache::load_or_new_with_limits(
                    &ws_ref.root,
                    pol_ref.max_cached_results,
                    pol_ref.max_cache_bytes,
                )
            });
            let dirty_handle = scope.spawn(move || {
                if bg_enabled {
                    git_dirty_files(&ws_ref.root).unwrap_or_default()
                } else {
                    Vec::new()
                }
            });
            let py_handle = scope.spawn(move || {
                let maybe_has_py = !spec_ref.pytest_cmd.is_empty()
                    || ws_ref.root.join("pyproject.toml").is_file()
                    || ws_ref.root.join("pytest.ini").is_file()
                    || ws_ref.root.join("setup.cfg").is_file()
                    || ws_ref.root.join("conftest.py").is_file();
                if maybe_has_py {
                    let _ = python_runtime_identity(spec_ref);
                }
            });
            let _ = py_handle.join();
            (
                graph_handle
                    .join()
                    .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                        invalid("parallel AST build panicked").into()
                    })
                    .and_then(|res| res),
                cache_handle
                    .join()
                    .unwrap_or_else(|_| RootCache::new(workspace.root.clone())),
                dirty_handle.join().unwrap_or_default(),
            )
        });
        let mut graph = graph_res?;
        for record in cache.latest_by_file.values() {
            let _ = graph.apply_runtime_dependencies(
                &workspace,
                &spec.policy,
                &record.test_file,
                &record.dynamic_dependencies,
            );
        }
        graph.environment_hash = environment_fingerprint_for_graph(&spec, &graph);
        graph.non_python_environment_hash =
            non_python_environment_fingerprint_for_graph(&spec, Some(&graph));
        graph.frontend_environment_hash =
            frontend_environment_fingerprint_for_graph(&spec, Some(&graph));
        graph.go_environment_hash = go_environment_fingerprint_for_graph(&spec, Some(&graph));
        let save_snapshot = !graph.has_persisted_edges();
        graph.recompute_edges_or_reuse_persisted();
        let ast_save_handle = if save_snapshot {
            let root_for_snap = workspace.root.clone();
            let graph_for_snap = graph.clone();
            std::thread::Builder::new()
                .name("tctx-ast-save".to_string())
                .spawn(move || {
                    crate::ast_graph::save_persisted_ast_snapshot(&root_for_snap, &graph_for_snap);
                })
                .ok()
        } else {
            None
        };

        // Queue background execution for dirty git files (or all uncached tests when suite <= 32 files)
        let mut pending_queue = VecDeque::new();
        let mut pending_ruff_files = BTreeSet::new();
        if spec.policy.background_enabled {
            let dirty = expand_dirty_paths_for_graph(&graph, &dirty);
            let py_files: Vec<String> = graph
                .files
                .keys()
                .filter(|k| k.ends_with(".py"))
                .cloned()
                .collect();
            if py_files.len() <= 64 {
                pending_ruff_files.extend(py_files);
            }
            for f in &dirty {
                let norm = f.trim().trim_start_matches("./").replace('\\', "/");
                if (norm.ends_with(".py")
                    || norm.ends_with(".rs")
                    || crate::ast_graph::is_frontend_source_path(&norm))
                    && graph.files.contains_key(&norm)
                {
                    pending_ruff_files.insert(norm);
                }
            }
            if !dirty.is_empty() {
                for item in graph.affected_tests(&dirty) {
                    if cache
                        .get_matching(&item.test_file, &item.dep_hash)
                        .is_none()
                        && background_eligible_from_parts(&spec, &graph, &cache, &item.test_file)
                        && !pending_queue.contains(&item.test_file)
                    {
                        pending_queue.push_back(item.test_file);
                    }
                }
            }
        }

        let mut watch_dirs =
            workspace.watch_directories_for(graph.files.keys().map(Path::new), &spec.policy)?;
        // Prioritize dependency-critical Go embed/cgo directories BEFORE filling remaining
        // `max_watch_dirs` capacity with empty ordinary directories; fail closed via
        // `required_watches_incomplete` if any required embed/cgo directory cannot be watched.
        let mut watch_set: BTreeSet<PathBuf> = watch_dirs.iter().cloned().collect();
        let mut unwatched_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for rel_dir in &graph.go_embed_watch_dirs {
            let full_dir = workspace.root.join(rel_dir);
            if full_dir.is_dir() && !watch_set.contains(&full_dir) {
                if watch_set.len() >= spec.policy.max_watch_dirs {
                    unwatched_dirs.insert(full_dir);
                } else if watch_set.insert(full_dir.clone()) {
                    watch_dirs.push(full_dir);
                }
            }
        }
        // Also watch configured pytest `testpaths` directories (including empty hidden testpaths
        // like `.hidden/tests` and their existing ancestors).
        for tp_dir in collect_testpath_watch_dirs(&workspace.root, &graph.testpaths, &spec.policy) {
            if !watch_set.contains(&tp_dir) {
                if watch_set.len() >= spec.policy.max_watch_dirs {
                    unwatched_dirs.insert(tp_dir);
                } else if watch_set.insert(tp_dir.clone()) {
                    watch_dirs.push(tp_dir);
                }
            }
        }
        let (enum_dirs, dirs_truncated) = workspace.directories_limited_checked(&spec.policy);
        for dir in enum_dirs {
            if watch_set.contains(&dir) {
                continue;
            }
            if watch_set.len() >= spec.policy.max_watch_dirs {
                unwatched_dirs.insert(dir);
            } else if watch_set.insert(dir.clone()) {
                watch_dirs.push(dir);
            }
        }
        let required_watches_incomplete = !unwatched_dirs.is_empty() || dirs_truncated;
        let watched_dirs_set: BTreeSet<PathBuf> = watch_set;
        let watcher_filter_dirs = Arc::new(Mutex::new(watched_dirs_set.clone()));
        let watcher_filter_for_callback = Arc::clone(&watcher_filter_dirs);
        let has_go_embeds = Arc::new(AtomicBool::new(graph.has_go_embed_directives()));
        let has_go_embeds_for_cb = Arc::clone(&has_go_embeds);
        let watcher_workspace = Arc::new(Mutex::new(workspace.clone()));
        let watcher_workspace_for_cb = Arc::clone(&watcher_workspace);
        let watcher_testpaths = Arc::new(Mutex::new(graph.testpaths.clone()));
        let watcher_testpaths_for_cb = Arc::clone(&watcher_testpaths);
        let initial_inventory_incomplete = graph.skipped_oversized_inventory;
        let initial_unreadable_files = graph.unreadable_inventory_files.clone();

        let had_external_cargo_cfg = has_symlinked_cargo_config(&spec, Some(&graph))
            || !effective_external_cargo_configs(&spec).is_empty();
        let initial_fe_uncacheable =
            frontend_env_is_uncacheable(&spec, Some(&graph), Some(&workspace));
        let initial_py_uncacheable = python_env_is_uncacheable(&spec, &graph);
        let state = RootState {
            spec: spec.clone(),
            workspace,
            graph,
            cache,
            pending_queue,
            in_flight: BTreeSet::new(),
            watched_dirs: watched_dirs_set,
            last_activity: Instant::now(),
            background_running: false,
            last_background_error: None,
            last_watcher_error: None,
            background_resources: BTreeSet::new(),
            foreground_resources: BTreeMap::new(),
            background_runs_deferred_load: 0,
            last_safety_scan: Instant::now(),
            generation: 1,
            watch_events_drained: 0,
            background_batches: 0,
            last_environment_check: Instant::now(),
            env_partition: None,
            had_external_cargo_cfg,
            prediction_enqueues: 0,
            ruff_cache: std::collections::HashMap::new(),
            ruff_cache_tick: 0,
            ruff_config_generation: 0,
            ruff_crate_generations: std::collections::HashMap::new(),
            pending_ruff_files,
            required_watches_incomplete,
            unwatched_dirs,
            inventory_incomplete: initial_inventory_incomplete,
            unreadable_files: initial_unreadable_files,
            next_invalidation_seq: 0,
            invalidation_generation: 0,
            file_invalidation_generations: BTreeMap::new(),
            gitignore_stamp: initial_gitignore_stamp,
            fe_env_uncacheable: initial_fe_uncacheable,
            py_env_uncacheable: initial_py_uncacheable,
        };

        let (fs_tx, fs_rx) = sync_channel(4096);
        let fs_overflow = Arc::new(AtomicBool::new(false));
        let overflow_for_cb = Arc::clone(&fs_overflow);

        let arc = Arc::new({
            let zygote_manager = ZygoteManager::default();
            let bg_zygote_manager = ZygoteManager::default();
            let active_pgid = zygote_manager.active_handle();
            let bg_active_pgid = bg_zygote_manager.active_handle();
            Self {
                state: Mutex::new(state),
                cache_io_lock: Mutex::new(()),
                env_probe_lock: Mutex::new(()),
                condvar: Condvar::new(),
                stopped: AtomicBool::new(false),
                active_requests: std::sync::atomic::AtomicUsize::new(0),
                fs_events_rx: Mutex::new(fs_rx),
                fs_overflow,
                has_go_embeds,
                watcher: Mutex::new(None),
                watcher_filter_dirs,
                watcher_workspace,
                watcher_testpaths,
                background_worker: Mutex::new(None),
                ast_save_worker: Mutex::new(ast_save_handle),
                zygote: Mutex::new(zygote_manager),
                bg_zygote: Mutex::new(bg_zygote_manager),
                active_pgid,
                bg_active_pgid,
                active_pgids: Mutex::new(BTreeSet::new()),
                scoped_cancels: Mutex::new(ScopedCancelState::default()),
                cancel_requested: AtomicBool::new(false),
            }
        });
        let weak_for_watcher = Arc::downgrade(&arc);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let event = match res {
                Ok(event) => event,
                Err(_) => {
                    overflow_for_cb.store(true, Ordering::Release);
                    if let Some(strong) = weak_for_watcher.upgrade() {
                        strong.condvar.notify_all();
                    }
                    return;
                }
            };
            // Never let our own file reads or access-time updates trigger an inotify feedback storm
            if matches!(
                event.kind,
                notify::EventKind::Access(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
                        notify::event::MetadataKind::AccessTime
                    ))
            ) {
                return;
            }
            if !event.need_rescan()
                && !event.paths.is_empty()
                && event.paths.iter().all(|path| {
                    if watcher_filter_for_callback
                        .lock()
                        .map(|directories| directories.contains(path))
                        .unwrap_or(false)
                    {
                        return path.is_dir()
                            && !matches!(
                                event.kind,
                                notify::EventKind::Remove(_)
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                            );
                    }
                    if path.is_dir() {
                        return false;
                    }
                    let ws_guard = watcher_workspace_for_cb
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    path.strip_prefix(&ws_guard.root)
                        .ok()
                        .is_none_or(|relative| {
                            // Always admit `.gitignore` so live ignore-rule edits hot-reload
                            // the workspace matcher and reconcile newly unignored files/dirs.
                            if relative == Path::new(".gitignore") {
                                return false;
                            }
                            // Always admit `.py` files matching configured pytest `testpaths`
                            // (including hidden/globbed directories like `.hidden/tests` or `*/tests`).
                            if relative.extension().and_then(|e| e.to_str()) == Some("py")
                                && watcher_testpaths_for_cb.lock().ok().is_some_and(|tps| {
                                    !tps.is_empty()
                                        && crate::ast_graph::path_matches_testpaths(
                                            &relative.to_string_lossy().replace('\\', "/"),
                                            &tps,
                                        )
                                })
                            {
                                return false;
                            }
                            // When the workspace has Go `//go:embed`, cgo `-I` dirs, or tracked
                            // C/cgo `#include` headers, admit non-VCS file events so
                            // `drain_fs_events_locked` can match them accurately.
                            if has_go_embeds_for_cb.load(Ordering::Acquire) {
                                return relative.components().any(|c| {
                                    matches!(
                                        c.as_os_str().to_str(),
                                        Some(".git" | ".hg" | ".svn")
                                    )
                                });
                            }
                            !ws_guard.is_permitted_rel(relative)
                                && !ws_guard.is_permitted_data_rel(relative)
                        })
                })
            {
                return;
            }
            if event.need_rescan() {
                overflow_for_cb.store(true, Ordering::Release);
            }
            if let Err(err) = fs_tx.try_send(event)
                && matches!(err, TrySendError::Full(_))
            {
                overflow_for_cb.store(true, Ordering::Release);
            }
            if let Some(strong) = weak_for_watcher.upgrade() {
                strong.condvar.notify_all();
            }
        })?;
        for dir in &watch_dirs {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }
        *arc.watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(watcher);

        // Close the startup race window between `DependencyGraph::build` and `watcher.watch`:
        // reload `.gitignore` & prune newly ignored files if it changed during load, watch any
        // newly created directories, discover embeds/cgo headers, and reconcile edits/deletions.
        {
            let mut guard = arc.lock_state();
            if file_stamp(&guard.workspace.root.join(".gitignore")) != initial_gitignore_stamp {
                arc.reload_gitignore_and_prune_locked(&mut guard);
            }
            arc.run_overflow_reconcile_locked(&mut guard);
        }

        if spec.policy.background_enabled {
            Self::spawn_background_worker(&arc)?;
        }

        Ok(arc)
    }

    pub(super) fn lock_state(&self) -> std::sync::MutexGuard<'_, RootState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn wait_state_timeout<'a>(
        &'a self,
        guard: std::sync::MutexGuard<'a, RootState>,
        dur: Duration,
    ) -> std::sync::MutexGuard<'a, RootState> {
        match self.condvar.wait_timeout(guard, dur) {
            Ok((next_guard, _)) => next_guard,
            Err(poisoned) => poisoned.into_inner().0,
        }
    }

    pub fn last_activity_elapsed(&self) -> Duration {
        self.lock_state().last_activity.elapsed()
    }

    pub fn indexed_file_count(&self) -> usize {
        self.lock_state().graph.files.len()
    }

    pub fn acquire_lease(self: &Arc<Self>) -> RootLease {
        self.active_requests.fetch_add(1, Ordering::AcqRel);
        RootLease {
            root: Arc::clone(self),
        }
    }

    pub fn is_evictable(&self) -> bool {
        self.active_requests.load(Ordering::Acquire) == 0
            && self.active_pgid.load(Ordering::Acquire) <= 0
            && self.bg_active_pgid.load(Ordering::Acquire) <= 0
            && self
                .active_pgids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
            && {
                let guard = self.lock_state();
                !guard.background_running
                    && guard.in_flight.is_empty()
                    && guard.pending_queue.is_empty()
            }
    }

    pub fn reclaim_idle_zygote(&self) -> bool {
        let timeout_secs = std::env::var("TCTX_ZYGOTE_IDLE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| (1..=3_600).contains(value))
            .unwrap_or(60);
        let dur = Duration::from_secs(timeout_secs);
        // Use try_lock so daemon maintenance never blocks while a test is actively using a zygote.
        let fg = self
            .zygote
            .try_lock()
            .is_ok_and(|mut z| z.shutdown_if_idle(dur));
        let bg = self
            .bg_zygote
            .try_lock()
            .is_ok_and(|mut z| z.shutdown_if_idle(dur));
        fg || bg
    }

    /// Reconciles cross-process on-disk WAL/snapshot updates (such as `tctx invalidate` from
    /// another CLI process) before serving cache hits, preserving `cache_io_lock -> state` lock
    /// ordering. Steady-state cost is two `stat` calls when unchanged.
    pub(super) fn refresh_cache_from_disk_if_changed(&self) {
        let needs_refresh = {
            let guard = self.lock_state();
            guard.cache.disk_changed_since_sync()
        };
        if !needs_refresh {
            return;
        }
        let _io_guard = self
            .cache_io_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut guard = self.lock_state();
        let policy = guard.spec.policy.clone();
        guard.cache.refresh_from_disk_if_changed(&policy);
    }

    pub(super) fn flush_cache_detached_checked(&self) -> Result<()> {
        let _io_guard = self
            .cache_io_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pending = {
            let mut guard = self.lock_state();
            let policy = guard.spec.policy.clone();
            guard.cache.take_pending_events_for_flush(&policy)
        };
        let Some(pending) = pending else {
            return Ok(());
        };
        match pending.flush() {
            Ok(outcome) => {
                let mut guard = self.lock_state();
                guard.cache.apply_flush_outcome(outcome);
                Ok(())
            }
            Err(error) => {
                let msg = error.to_string();
                let mut guard = self.lock_state();
                guard.cache.restore_failed_flush(pending, msg.clone());
                Err(invalid(format!("CACHE_PERSISTENCE_FAILED: {msg}")).into())
            }
        }
    }

    pub(super) fn flush_cache_detached(&self) {
        let _ = self.flush_cache_detached_checked();
    }

    pub fn flush_pending_cache(&self) {
        self.flush_cache_detached();
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cancel_active();
        self.condvar.notify_all();
        *self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        if let Some(handle) = self
            .background_worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        // Join any initial async AST snapshot writer before writing the final shutdown snapshot
        // so both cannot race on rename ordering.
        if let Some(handle) = self
            .ast_save_worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        {
            let guard = self.lock_state();
            if guard.generation > 1 {
                crate::ast_graph::save_persisted_ast_snapshot(&guard.workspace.root, &guard.graph);
            }
        }
        self.flush_cache_detached();
        *self
            .zygote
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ZygoteManager::default();
        *self
            .bg_zygote
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ZygoteManager::default();
    }

    pub(super) fn enqueue_affected_locked(guard: &mut RootState, affected: Vec<AffectedTest>) {
        let affected_files: Vec<String> =
            affected.iter().map(|item| item.test_file.clone()).collect();
        let predicted = guard.cache.predicted_affected_tests(&affected_files);
        guard.prediction_enqueues = guard
            .prediction_enqueues
            .saturating_add(u64::try_from(predicted.len()).unwrap_or(u64::MAX));
        let predicted_set: BTreeSet<String> = predicted.iter().cloned().collect();
        let mut ordered = predicted;
        ordered.extend(
            affected_files
                .into_iter()
                .filter(|test_file| !predicted_set.contains(test_file)),
        );
        for test_file in ordered.into_iter().rev() {
            let Some(dep_hash) = guard.graph.dep_hash_for_test.get(&test_file) else {
                continue;
            };
            if guard.cache.get_matching(&test_file, dep_hash).is_none()
                && background_eligible_with_cache(guard, &test_file)
            {
                guard.pending_queue.retain(|queued| queued != &test_file);
                guard.pending_queue.push_front(test_file);
            }
        }
    }

    pub fn cancel_active(&self) -> bool {
        self.cancel_active_scoped(None)
    }

    pub fn cancel_active_scoped(&self, cancel_id: Option<&str>) -> bool {
        if let Some(id) = cancel_id {
            let mut cancelled = false;
            let handle_opt = {
                let mut sc = self
                    .scoped_cancels
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(h) = sc.active.get(id) {
                    Some(h.clone())
                } else {
                    // Atomically record pre-cancellation under the same lock as registration so
                    // an arriving `run_tests_once` cannot race between pre-cancel check and insert.
                    if sc.pre_cancelled.len() >= 128 {
                        sc.pre_cancelled.clear();
                    }
                    sc.pre_cancelled.insert(id.to_string());
                    None
                }
            };
            if let Some(handle) = handle_opt {
                handle.cancelled.store(true, Ordering::SeqCst);
                let pgids_snapshot: Vec<i32> = {
                    let mut pgids = handle
                        .pgids
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    // Sentinel `i32::MIN` tells any concurrently spawning child under the same
                    // `pgids` lock that this run is already cancelled so it kills itself immediately.
                    pgids.insert(i32::MIN);
                    pgids.iter().copied().filter(|&p| p > 0).collect()
                };
                for pgid in pgids_snapshot {
                    let killed = unsafe {
                        libc::kill(-pgid, libc::SIGKILL) == 0
                            || libc::kill(pgid, libc::SIGKILL) == 0
                    };
                    cancelled |= killed;
                }
                self.condvar.notify_all();
                return cancelled || handle.cancelled.load(Ordering::Acquire);
            }
            return false;
        }
        let mut unscoped_cancelled = cancel_process_group(&self.active_pgid);
        let bg_cancelled = cancel_process_group(&self.bg_active_pgid);
        unscoped_cancelled |= bg_cancelled;
        let pgids_snapshot: Vec<i32> = {
            let pgids = self
                .active_pgids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pgids.iter().copied().filter(|&p| p > 0).collect()
        };
        for pgid in pgids_snapshot {
            unscoped_cancelled |= unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 };
        }
        let (scoped_handles, unscoped_handles): (Vec<ScopedRunHandle>, Vec<ScopedRunHandle>) = {
            let sc = self
                .scoped_cancels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                sc.active.values().cloned().collect(),
                sc.unscoped_active.values().cloned().collect(),
            )
        };
        for handle in unscoped_handles {
            handle.cancelled.store(true, Ordering::SeqCst);
            unscoped_cancelled = true;
            let pgids: Vec<i32> = {
                let mut guard = handle
                    .pgids
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.insert(i32::MIN);
                guard.iter().copied().filter(|&p| p > 0).collect()
            };
            for pgid in pgids {
                let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 };
            }
        }
        let mut scoped_cancelled_any = false;
        for handle in scoped_handles {
            handle.cancelled.store(true, Ordering::SeqCst);
            scoped_cancelled_any = true;
            let pgids: Vec<i32> = {
                let mut guard = handle
                    .pgids
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.insert(i32::MIN);
                guard.iter().copied().filter(|&p| p > 0).collect()
            };
            for pgid in pgids {
                let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 };
            }
        }
        // Only set `cancel_requested` for an interrupted background worker (foreground runs — both
        // scoped and unscoped — use their own per-request `ScopedRunHandle.cancelled` flag so a
        // cancelled run NEVER leaks a stale flag into a subsequent unrelated run!).
        if bg_cancelled {
            self.cancel_requested.store(true, Ordering::Release);
        }
        if unscoped_cancelled || scoped_cancelled_any {
            self.condvar.notify_all();
        }
        unscoped_cancelled || scoped_cancelled_any
    }

    pub(super) fn spawn_background_worker(this: &Arc<Self>) -> Result<()> {
        let worker_root = Arc::clone(this);
        let handle = std::thread::Builder::new()
            .name("tctx-bg-runner".to_string())
            .spawn(move || {
                worker_root.background_loop();
            })?;
        let mut slot = this
            .background_worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(handle);
        Ok(())
    }

    pub(super) fn background_loop(&self) {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.background_step();
            }));
            if step.is_err() {
                let mut guard = self.lock_state();
                guard.in_flight.clear();
                guard.background_running = false;
                guard.background_resources.clear();
                guard.last_background_error =
                    Some("background worker recovered from an internal panic".to_string());
                self.condvar.notify_all();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    pub(super) fn background_step(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        // Always keep safety & environment reconciliation running on schedule even when
        // background test execution is paused or deferred due to CPU load.
        let max_load_ratio = {
            let mut guard = self.lock_state();
            self.drain_fs_events_locked(&mut guard);
            self.maybe_safety_reconcile_locked(&mut guard);
            guard.spec.policy.background_max_load_ratio_percent
        };
        if !background_capacity_available(max_load_ratio) {
            let mut guard = self.lock_state();
            guard.background_runs_deferred_load =
                guard.background_runs_deferred_load.saturating_add(1);
            drop(self.wait_state_timeout(guard, Duration::from_millis(200)));
            return;
        }
        let mut batch: Vec<(String, String)>;
        let spec: Spec;
        let preload_modules: Vec<String>;
        let environment_hash: String;
        let capture_dynamic_dependencies: bool;
        let _bg_permit: GlobalBackgroundPermit;
        {
            let mut guard = self.lock_state();
            self.drain_fs_events_locked(&mut guard);
            // Perform periodic safety reconcile here on the background worker instead of blocking foreground queries
            self.maybe_safety_reconcile_locked(&mut guard);
            while guard.pending_queue.is_empty()
                && guard.pending_ruff_files.is_empty()
                && !self.stopped.load(Ordering::SeqCst)
            {
                guard.background_running = false;
                let prev_drained = guard.watch_events_drained;
                guard = self.wait_state_timeout(guard, Duration::from_secs(2));
                if !self.stopped.load(Ordering::SeqCst) {
                    self.drain_fs_events_locked(&mut guard);
                    if guard.watch_events_drained > prev_drained {
                        // Only debounce when a real burst of FS events arrived
                        drop(guard);
                        std::thread::sleep(Duration::from_millis(25));
                        guard = self.lock_state();
                        self.drain_fs_events_locked(&mut guard);
                    }
                    // Recheck 300s periodic safety & environment scan even while idle
                    self.maybe_safety_reconcile_locked(&mut guard);
                }
            }
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            let Some(permit) = GlobalBackgroundPermit::try_acquire() else {
                guard.background_runs_deferred_load =
                    guard.background_runs_deferred_load.saturating_add(1);
                drop(self.wait_state_timeout(guard, Duration::from_millis(150)));
                return;
            };
            _bg_permit = permit;
            if !guard.pending_ruff_files.is_empty() {
                let ruff_files: Vec<String> =
                    std::mem::take(&mut guard.pending_ruff_files).into_iter().collect();
                drop(guard);
                self.ensure_ruff_cached_unlocked(&ruff_files);
                self.condvar.notify_all();
                guard = self.lock_state();
                if guard.pending_queue.is_empty() {
                    guard.background_running = false;
                    return;
                }
            }
            let max_batch = guard.spec.policy.background_batch_size.max(1);
            // Partial selection when queue is large to avoid sorting thousands of entries
            let mut sorted: Vec<String> = guard.pending_queue.drain(..).collect();
            let prediction_rank: std::collections::HashMap<String, usize> = guard
                .cache
                .predicted_affected_tests(&sorted)
                .into_iter()
                .enumerate()
                .map(|(rank, test_file)| (test_file, rank))
                .collect();
            let reverse_dur = guard.background_batches % 4 == 3;
            let key_fn = |test_file: &String| -> (usize, u64) {
                let rank = prediction_rank
                    .get(test_file)
                    .copied()
                    .unwrap_or(usize::MAX);
                let dur = guard
                    .cache
                    .latest_by_file
                    .get(test_file)
                    .map(|record| record.duration_ms)
                    .unwrap_or(0);
                if reverse_dur {
                    (rank, u64::MAX - dur)
                } else {
                    (rank, dur)
                }
            };
            let window = (max_batch * 4).min(sorted.len());
            if sorted.len() > window && window > 0 {
                sorted.select_nth_unstable_by_key(window, key_fn);
                sorted[..window].sort_unstable_by_key(key_fn);
            } else {
                sorted.sort_unstable_by_key(key_fn);
            }
            guard.pending_queue.extend(sorted);
            let mut selected = Vec::new();
            let mut deferred_busy = Vec::new();
            const SLOW_TEST_THRESHOLD_MS: u64 = 5_000;
            while selected.len() < max_batch {
                let Some(test_file) = guard.pending_queue.pop_front() else {
                    break;
                };
                let Some(dep_hash) = guard.graph.dep_hash_for_test.get(&test_file).cloned()
                else {
                    continue;
                };
                if Self::test_touches_unreadable_file(&guard, &test_file) {
                    // Defer tests blocked by persistently unreadable/oversized dependencies
                    // until those files recover or are deleted (avoiding unbounded background loops).
                    continue;
                }
                if guard.cache.get_matching(&test_file, &dep_hash).is_some() {
                    continue;
                }
                let is_slow = guard
                    .cache
                    .latest_by_file
                    .get(&test_file)
                    .is_some_and(|r| r.duration_ms >= SLOW_TEST_THRESHOLD_MS);
                if is_slow && !selected.is_empty() {
                    // Isolate slow test into its own batch so fast tests commit to cache immediately
                    guard.pending_queue.push_front(test_file);
                    break;
                }
                let test_res_keys = test_resource_keys(&guard.graph, &test_file);
                if test_res_keys
                    .iter()
                    .any(|rk| guard.foreground_resources.contains_key(rk))
                {
                    // Bidirectional resource exclusion: do not start a background test whose
                    // resource key is currently held by an active foreground test run.
                    deferred_busy.push(test_file);
                    continue;
                }
                let flight_key = flight_key(&test_file, &dep_hash);
                if guard.in_flight.insert(flight_key) {
                    selected.push((test_file, dep_hash));
                    if is_slow {
                        break;
                    }
                } else {
                    // Retain files currently in flight (e.g. from a foreground run that was just
                    // invalidated) so they are re-executed once the active run finishes.
                    deferred_busy.push(test_file);
                }
            }
            let had_deferred_busy = !deferred_busy.is_empty();
            for busy_tf in deferred_busy {
                if !guard.pending_queue.contains(&busy_tf) {
                    guard.pending_queue.push_back(busy_tf);
                }
            }
            if selected.is_empty() {
                guard.background_running = false;
                if had_deferred_busy {
                    // Wait briefly on condvar while active foreground/background runs finish
                    // instead of spinning in a hot loop contending on the global permit.
                    drop(self.wait_state_timeout(guard, Duration::from_millis(50)));
                }
                return;
            }
            guard.background_running = true;
            guard.background_batches = guard.background_batches.saturating_add(1);
            guard.background_resources = selected
                .iter()
                .flat_map(|(test_file, _)| {
                    test_resource_keys(&guard.graph, test_file).into_iter()
                })
                .collect();
            spec = guard.spec.clone();
            let has_py = selected.iter().any(|(tf, _)| tf.ends_with(".py"));
            preload_modules = if has_py {
                guard.graph.external_preload_modules_for_tests(
                    &selected
                        .iter()
                        .map(|(test_file, _)| test_file.clone())
                        .collect::<Vec<_>>(),
                )
            } else {
                Vec::new()
            };
            batch = selected;
            environment_hash = guard.graph.environment_hash.clone();
            capture_dynamic_dependencies = batch.iter().any(|(test_file, dep_hash)| {
                guard
                    .cache
                    .get_matching(test_file, dep_hash)
                    .is_none_or(|record| !record.dynamic_dependencies_complete)
            });
        }

        let files_only: Vec<String> = batch.iter().map(|(f, _)| f.clone()).collect();
        let batch_map: BTreeMap<String, String> = batch.iter().cloned().collect();
        let (had_cargo_lock_before_exec, before_non_lock_fp, start_inv_watermarks) = {
            let guard = self.lock_state();
            (
                Self::snapshot_cargo_lock_existence(&guard, &files_only),
                Self::snapshot_non_lock_dep_fingerprints(&guard, &files_only),
                Self::snapshot_invalidation_watermarks(&guard, &files_only),
            )
        };
        let plugins = pytest_plugin_manifest_for_targets(&spec, &files_only);
        let exec_res = execute_pytest_batch(
            &spec,
            &files_only,
            &batch_map,
            None,
            None,
            &[],
            "background",
            Some(&self.bg_zygote),
            None,
            Some(&self.active_pgids),
            &preload_modules,
            &environment_hash,
            &plugins,
            capture_dynamic_dependencies,
        );
        if self.cancel_requested.swap(false, Ordering::AcqRel) {
            let mut guard = self.lock_state();
            for (test_file, dep_hash) in &batch {
                guard.in_flight.remove(&flight_key(test_file, dep_hash));
            }
            guard.last_background_error =
                Some("TEST_CANCELLED: background pytest was terminated".to_string());
            guard.background_running = !guard.pending_queue.is_empty();
            guard.background_resources.clear();
            self.condvar.notify_all();
            return;
        }
        let exec_res = exec_res.map(|(mut records, raw)| {
            records.shrink_to_fit();
            (records, raw)
        });
        self.warm_environment_probes_unlocked();

        {
            let mut guard = self.lock_state();
            for (test_file, dep_hash) in &batch {
                guard.in_flight.remove(&flight_key(test_file, dep_hash));
            }
            if guard.cache.disk_changed_since_sync() {
                drop(guard);
                self.refresh_cache_from_disk_if_changed();
                guard = self.lock_state();
            }
            self.drain_fs_events_locked(&mut guard);
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            let has_rs_batch = files_only.iter().any(|f| f.ends_with(".rs"));
            Self::reconcile_live_files_inner_locked(&mut guard, Some(&files_only), has_rs_batch);
            // Compute non-lock instability BEFORE syncing newly created `Cargo.lock` by comparing
            // per-test non-lock dependency fingerprints (unaffected by brand-new `Cargo.lock`
            // creation, but catching every source, config, env, or graph edge edit).
            let after_non_lock_fp =
                Self::snapshot_non_lock_dep_fingerprints(&guard, &files_only);
            let mut unstable: BTreeSet<String> = batch
                .iter()
                .filter(|(test_file, expected_hash)| {
                    if test_file.ends_with(".rs") {
                        before_non_lock_fp.get(test_file) != after_non_lock_fp.get(test_file)
                            || Self::test_touches_unreadable_file(&guard, test_file)
                    } else {
                        guard.graph.dep_hash_for_test.get(test_file) != Some(expected_hash)
                            || Self::test_touches_unreadable_file(&guard, test_file)
                    }
                })
                .map(|(test_file, _)| test_file.clone())
                .collect();
            let mut records_opt = exec_res;
            let existing_lock_modified = if let Ok((ref mut recs, _)) = records_opt {
                let mod_existing = Self::sync_cargo_locks_after_exec_locked(
                    &mut guard,
                    &files_only,
                    &had_cargo_lock_before_exec,
                    &unstable,
                    recs,
                );
                if !mod_existing && has_rs_batch {
                    for rec in recs.iter() {
                        if rec.test_file.ends_with(".rs") && !unstable.contains(&rec.test_file) {
                            for (tf, dh) in &mut batch {
                                if tf == &rec.test_file {
                                    dh.clone_from(&rec.dep_hash);
                                }
                            }
                        }
                    }
                }
                mod_existing
            } else {
                false
            };
            for (test_file, expected_hash) in &batch {
                if guard.graph.dep_hash_for_test.get(test_file) != Some(expected_hash)
                    || Self::test_touches_unreadable_file(&guard, test_file)
                    || (existing_lock_modified && test_file.ends_with(".rs"))
                {
                    unstable.insert(test_file.clone());
                }
            }
            for test_file in unstable.iter().rev() {
                if background_eligible_with_cache(&guard, test_file)
                    && !Self::test_touches_unreadable_file(&guard, test_file)
                {
                    guard.pending_queue.retain(|queued| queued != test_file);
                    guard.pending_queue.push_front(test_file.clone());
                }
            }
            match records_opt {
                Ok((mut records, _raw_out)) => {
                    let workspace = guard.workspace.clone();
                    let policy_snapshot = guard.spec.policy.clone();
                    let mut runtime_changed = false;
                    let mut runtime_dep_modified = BTreeSet::new();
                    let mut runtime_dep_untrackable = BTreeSet::new();
                    for record in &mut records {
                        resolve_module_attributed_dynamic_deps(&guard.graph, record);
                        let prev_known: BTreeSet<String> = guard
                            .graph
                            .runtime_deps_for_test
                            .get(&record.test_file)
                            .cloned()
                            .unwrap_or_default();
                        let changed_this = guard.graph.apply_runtime_dependencies(
                            &workspace,
                            &policy_snapshot,
                            &record.test_file,
                            &record.dynamic_dependencies,
                        );
                        if changed_this {
                            runtime_changed = true;
                        }
                        let (modified_during_run, has_untrackable) =
                            classify_runtime_deps_stability(&guard, record, &prev_known);
                        if modified_during_run {
                            runtime_dep_modified.insert(record.test_file.clone());
                        } else if has_untrackable {
                            runtime_dep_untrackable.insert(record.test_file.clone());
                        }
                    }
                    if runtime_changed {
                        guard.graph.recompute_edges();
                        let environment_hash = guard.graph.environment_hash.clone();
                        guard.graph.apply_environment_hash(environment_hash);
                        for record in &mut records {
                            if let Some(dep_hash) =
                                guard.graph.dep_hash_for_test.get(&record.test_file)
                            {
                                record.dep_hash.clone_from(dep_hash);
                            }
                        }
                    }
                    let policy = guard.spec.policy.clone();
                    for rec in records {
                        if runtime_dep_modified.contains(&rec.test_file) {
                            if background_eligible_with_cache(&guard, &rec.test_file) {
                                guard.pending_queue.retain(|queued| queued != &rec.test_file);
                                guard.pending_queue.push_front(rec.test_file.clone());
                            }
                            continue;
                        }
                        // Leave untrackable-dep tests (e.g. probing a permanently missing file)
                        // uncached WITHOUT requeuing so background does not spin indefinitely.
                        if !unstable.contains(&rec.test_file)
                            && Self::is_test_cacheable_in_graph(&guard, &rec.test_file)
                            && !runtime_dep_untrackable.contains(&rec.test_file)
                            && rec.dynamic_dependencies_complete
                            && !Self::was_invalidated_since(
                                &guard,
                                &rec.test_file,
                                &start_inv_watermarks,
                            )
                        {
                            guard.cache.insert(rec, &policy);
                        }
                    }
                    guard.cache.total_background_runs =
                        guard.cache.total_background_runs.saturating_add(1);
                    guard.last_background_error = None;
                }
                Err(err) => {
                    guard.last_background_error = Some(err.to_string());
                }
            }
            guard.background_running = !guard.pending_queue.is_empty();
            guard.background_resources.clear();
            self.condvar.notify_all();
        }
        self.flush_cache_detached();
    }

}

