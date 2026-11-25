use super::*;

impl ManagedRoot {
    pub fn sync(&self, wait_background: bool) -> Result<serde_json::Value> {
        // Retry if a concurrent request updated/discovered files while we built `new_graph`
        // unlocked, so `sync` never overwrites a newer graph with a stale snapshot.
        for attempt in 0..3 {
            let (workspace, spec, start_gen, env_partition) = {
                let mut guard = self.lock_state();
                guard.last_activity = Instant::now();
                self.drain_fs_events_locked(&mut guard);
                (
                    guard.workspace.clone(),
                    guard.spec.clone(),
                    guard.generation,
                    guard.env_partition.clone(),
                )
            };
            let mut new_graph = DependencyGraph::build(&workspace, &spec.policy)?;
            let part = env_partition.as_deref();
            let environment_hash =
                partition_env_hash(&environment_fingerprint_for_graph(&spec, &new_graph), part);
            let non_py_hash = partition_env_hash(
                &non_python_environment_fingerprint_for_graph(&spec, Some(&new_graph)),
                part,
            );
            let fe_hash = partition_env_hash(
                &frontend_environment_fingerprint_for_graph(&spec, Some(&new_graph)),
                part,
            );
            new_graph.apply_all_environment_hashes(environment_hash, non_py_hash, fe_hash);
            let mut watch_dirs = workspace
                .watch_directories_for(new_graph.files.keys().map(Path::new), &spec.policy)?;
            let mut sync_watch_set: BTreeSet<PathBuf> = watch_dirs.iter().cloned().collect();
            for rel_dir in &new_graph.required_watch_dirs {
                let full_dir = workspace.root.join(rel_dir);
                if full_dir.is_dir()
                    && !sync_watch_set.contains(&full_dir)
                    && sync_watch_set.len() < spec.policy.max_watch_dirs
                    && sync_watch_set.insert(full_dir.clone())
                {
                    watch_dirs.push(full_dir);
                }
            }
            for tp_dir in
                collect_testpath_watch_dirs(&workspace.root, &new_graph.testpaths, &spec.policy)
            {
                if !sync_watch_set.contains(&tp_dir)
                    && sync_watch_set.len() < spec.policy.max_watch_dirs
                    && sync_watch_set.insert(tp_dir.clone())
                {
                    watch_dirs.push(tp_dir);
                }
            }
            let (sync_enum_dirs, sync_dirs_truncated) =
                workspace.directories_limited_checked(&spec.policy);
            for dir in sync_enum_dirs {
                if sync_watch_set.len() >= spec.policy.max_watch_dirs {
                    break;
                }
                if sync_watch_set.insert(dir.clone()) {
                    watch_dirs.push(dir);
                }
            }
            let mut guard = self.lock_state();
            guard.last_activity = Instant::now();
            self.drain_fs_events_locked(&mut guard);
            if guard.generation != start_gen {
                if attempt < 2 {
                    continue;
                }
                // Never commit an obsolete matcher/graph built before a concurrent `.gitignore`
                // or filesystem mutation; fail explicitly so the caller retries cleanly.
                return Err(invalid(
                    "Workspace changed concurrently during test_sync; retry test_sync",
                )
                .into());
            }
            // Preserve learned runtime dependencies from both the live graph and cache records
            // across `sync()` so dynamically observed data/fixture edges are never discarded.
            let mut preserved_runtime_deps: BTreeMap<String, BTreeSet<String>> =
                guard.graph.runtime_deps_for_test.clone();
            for rec in guard.cache.latest_by_file.values() {
                if !rec.dynamic_dependencies.is_empty() {
                    preserved_runtime_deps
                        .entry(rec.test_file.clone())
                        .or_default()
                        .extend(rec.dynamic_dependencies.iter().cloned());
                }
            }
            let mut applied_runtime_deps = false;
            for (tf, deps) in preserved_runtime_deps {
                let dep_vec: Vec<String> = deps.into_iter().collect();
                if new_graph.apply_runtime_dependencies(
                    &guard.workspace,
                    &guard.spec.policy,
                    &tf,
                    &dep_vec,
                ) {
                    applied_runtime_deps = true;
                }
            }
            let need_reconcile = guard.generation != start_gen;
            if need_reconcile {
                // Merge any newly ingested/created files from `guard.graph` that aren't in `new_graph`,
                // then reconcile live disk state (edits & deletions) after installing `new_graph`.
                let mut merged_any = false;
                for (rel_str, old_node) in &guard.graph.files {
                    if !new_graph.files.contains_key(rel_str) {
                        let full = guard.workspace.root.join(rel_str);
                        if let Some(stamp) = file_stamp(&full)
                            && let Ok(bytes) = guard.workspace.read_known_with_limit(
                                Path::new(rel_str),
                                guard.spec.policy.max_file_bytes,
                            )
                        {
                            new_graph.insert_file_bytes_with_stamp(rel_str, &bytes, Some(stamp));
                            merged_any = true;
                        } else if old_node.stamp.is_some() && full.is_file() {
                            new_graph.files.insert(rel_str.clone(), old_node.clone());
                            merged_any = true;
                        }
                    }
                }
                if merged_any || applied_runtime_deps {
                    new_graph.recompute_edges();
                }
            } else if applied_runtime_deps {
                new_graph.recompute_edges();
            }
            guard.workspace = workspace.clone();
            *self
                .watcher_workspace
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = workspace;
            guard.gitignore_stamp = file_stamp(&guard.spec.root.join(".gitignore"));
            guard.inventory_incomplete =
                sync_dirs_truncated || new_graph.skipped_oversized_inventory;
            guard.unreadable_files = new_graph.unreadable_inventory_files.clone();
            guard.graph = new_graph;
            *self
                .watcher_testpaths
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = guard.graph.testpaths.clone();
            self.has_raw_or_data_watches
                .store(guard.graph.has_raw_or_data_watch_directives(), Ordering::Release);
            if need_reconcile {
                Self::reconcile_live_files_locked(&mut guard, None);
            }
            self.prune_missing_watch_dirs_locked(&mut guard);
            self.add_watch_dirs_locked(&mut guard, watch_dirs.into_iter().collect());
            Self::refresh_required_watches_flag(&mut guard);
            guard.generation = guard.generation.saturating_add(1);
            if guard.spec.policy.background_enabled {
                let candidates: Vec<(String, String)> = guard
                    .graph
                    .dep_hash_for_test
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (test_file, dep_hash) in candidates {
                    if guard.cache.get_matching(&test_file, &dep_hash).is_none()
                        && background_eligible_with_cache(&guard, &test_file)
                        && !guard.pending_queue.contains(&test_file)
                    {
                        guard.pending_queue.push_back(test_file);
                    }
                }
                self.condvar.notify_all();
            }
            break;
        }

        if wait_background {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut guard = self.lock_state();
            while (!guard.pending_queue.is_empty()
                || !guard.in_flight.is_empty()
                || !guard.pending_ruff_files.is_empty())
                && Instant::now() < deadline
            {
                guard = self.wait_state_timeout(guard, Duration::from_millis(100));
            }
        }

        Ok(self.status())
    }

    pub fn invalidate(&self, files: &[String]) -> Result<serde_json::Value> {
        let mut guard = self.lock_state();
        guard.last_activity = Instant::now();
        let norm_files = normalize_targets_for_root(&guard.workspace.root, files);
        let removed = if norm_files.is_empty() {
            guard.next_invalidation_seq = guard.next_invalidation_seq.saturating_add(1);
            guard.invalidation_generation = guard.next_invalidation_seq;
            let count = guard.cache.invalidate_all();
            if guard.spec.policy.background_enabled {
                let all_tests: Vec<String> =
                    guard.graph.dep_hash_for_test.keys().cloned().collect();
                for test_file in all_tests {
                    if !background_eligible_with_cache(&guard, &test_file) {
                        continue;
                    }
                    if !guard.pending_queue.contains(&test_file) {
                        guard.pending_queue.push_back(test_file);
                    }
                }
                self.condvar.notify_all();
            }
            count
        } else {
            let resolved = guard.graph.resolve_target_test_files(&norm_files);
            let mut count = 0usize;
            for test_file in resolved {
                guard.next_invalidation_seq = guard.next_invalidation_seq.saturating_add(1);
                let seq = guard.next_invalidation_seq;
                guard
                    .file_invalidation_generations
                    .insert(test_file.clone(), seq);
                count += guard.cache.invalidate_file(&test_file);
                if guard.spec.policy.background_enabled
                    && background_eligible_with_cache(&guard, &test_file)
                    && !guard.pending_queue.contains(&test_file)
                {
                    guard.pending_queue.push_back(test_file);
                }
            }
            self.condvar.notify_all();
            count
        };
        let root_path = guard.workspace.root.clone();
        let pending_bg_len = guard.pending_queue.len();
        let existing_persist_err = guard.cache.persistence_error.clone();
        drop(guard);
        if let Some(err_msg) = existing_persist_err {
            return Err(invalid(format!("CACHE_PERSISTENCE_FAILED: {err_msg}")).into());
        }
        self.flush_cache_detached_checked()?;
        Ok(serde_json::json!({
            "root": root_path,
            "invalidated_entries": removed,
            "pending_background_queue": pending_bg_len,
        }))
    }

    pub fn status(&self) -> serde_json::Value {
        let mut guard = self.lock_state();
        self.drain_fs_events_locked(&mut guard);
        let total_tests = guard.graph.dep_hash_for_test.len();
        let mut cached_passed = 0usize;
        let mut cached_failed = 0usize;
        let mut uncached = 0usize;
        for (test_file, dep_hash) in &guard.graph.dep_hash_for_test {
            match guard.cache.get_matching(test_file, dep_hash) {
                Some(rec) if rec.outcome.is_green() => cached_passed += 1,
                Some(_) => cached_failed += 1,
                None => uncached += 1,
            }
        }
        let root = guard.workspace.root.clone();
        let pytest_cmd = guard.spec.effective_pytest_cmd();
        let cargo_cmd = guard.spec.effective_cargo_cmd();
        let frontend_test_cmd = guard.spec.effective_frontend_test_cmd();
        let indexed_rust_files = guard
            .graph
            .files
            .keys()
            .filter(|k| k.ends_with(".rs"))
            .count();
        let indexed_frontend_files = guard
            .graph
            .files
            .keys()
            .filter(|k| crate::ast_graph::is_frontend_source_path(k))
            .count();
        let total_rust_test_files = guard
            .graph
            .dep_hash_for_test
            .keys()
            .filter(|k| k.ends_with(".rs"))
            .count();
        let total_frontend_test_files = guard
            .graph
            .dep_hash_for_test
            .keys()
            .filter(|k| crate::ast_graph::is_frontend_source_path(k))
            .count();
        let total_python_test_files = guard
            .graph
            .dep_hash_for_test
            .keys()
            .filter(|k| k.ends_with(".py"))
            .count();
        let background_safe_only = guard.spec.policy.background_safe_only;
        let background_deferred = guard.background_runs_deferred_load;
        let generation = guard.generation;
        let watch_events_drained = guard.watch_events_drained;
        let background_batches = guard.background_batches;
        let prediction_enqueues = guard.prediction_enqueues;
        let environment_check_age_ms =
            u64::try_from(guard.last_environment_check.elapsed().as_millis()).unwrap_or(u64::MAX);
        let safety_scan_age_ms =
            u64::try_from(guard.last_safety_scan.elapsed().as_millis()).unwrap_or(u64::MAX);
        let watched_directories = guard.watched_dirs.len();
        let watcher_overflow = self.fs_overflow.load(Ordering::Acquire);
        let cache_storage = guard.cache.storage_metrics();
        let indexed_python_files = guard
            .graph
            .files
            .keys()
            .filter(|k| k.ends_with(".py"))
            .count();
        let indexed_total_files = guard.graph.files.len();
        let ruff_cached_files = guard.ruff_cache.len();
        let ruff_diagnostics_count: usize = guard
            .graph
            .files
            .iter()
            .filter(|(p, _)| {
                p.ends_with(".py")
                    || p.ends_with(".rs")
                    || crate::ast_graph::is_frontend_source_path(p)
            })
            .filter_map(|(p, n)| {
                guard
                    .ruff_cache
                    .get(p.as_str())
                    .filter(|e| e.sha256 == n.sha256)
            })
            .map(|e| e.diagnostics.len())
            .sum();
        let payload = serde_json::json!({
            "root": root,
            "pytest_cmd": pytest_cmd,
            "cargo_cmd": cargo_cmd,
            "frontend_test_cmd": frontend_test_cmd,
            "indexed_total_files": indexed_total_files,
            "indexed_python_files": indexed_python_files,
            "indexed_rust_files": indexed_rust_files,
            "indexed_frontend_files": indexed_frontend_files,
            "total_test_files": total_tests,
            "total_python_test_files": total_python_test_files,
            "total_rust_test_files": total_rust_test_files,
            "total_frontend_test_files": total_frontend_test_files,
            "cached_passed_files": cached_passed,
            "cached_failed_files": cached_failed,
            "uncached_or_stale_files": uncached,
            "ruff_cached_files": ruff_cached_files,
            "ruff_diagnostics_count": ruff_diagnostics_count,
            "background_pending_queue": guard.pending_queue.len(),
            "background_in_flight": guard.in_flight.len(),
            "background_running": guard.background_running,
            "last_background_error": guard.last_background_error,
            "last_watcher_error": guard.last_watcher_error,
            "total_saved_ms": guard.cache.total_saved_ms,
            "total_cache_hits": guard.cache.total_cache_hits,
            "total_delta_runs": guard.cache.total_delta_runs,
            "total_background_runs": guard.cache.total_background_runs,
            "background_safe_only": background_safe_only,
            "background_runs_deferred_load": background_deferred,
            "watcher_overflow_pending": watcher_overflow,
            "generation": generation,
            "watch_events_drained": watch_events_drained,
            "background_batches": background_batches,
            "prediction_enqueues": prediction_enqueues,
            "environment_check_age_ms": environment_check_age_ms,
            "safety_scan_age_ms": safety_scan_age_ms,
            "watched_directories": watched_directories,
            "cache_quarantined_keys": guard.cache.quarantined_keys.len(),
            "cache_persistence_error": guard.cache.persistence_error,
            "cache_storage": cache_storage,
        });
        drop(guard);
        let zygote = match self.zygote.try_lock() {
            Ok(z) => z.status(),
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().status(),
            Err(std::sync::TryLockError::WouldBlock) => serde_json::json!({
                "running": true,
                "busy": true,
            }),
        };
        let mut payload = payload;
        payload["zygote"] = zygote;
        payload
    }

    pub fn graph_info(&self, file: &str) -> Result<FileGraphInfo> {
        let mut guard = self.lock_state();
        guard.last_activity = Instant::now();
        self.drain_fs_events_locked(&mut guard);
        let norm_file = normalize_target_for_root(&guard.workspace.root, file);
        Self::ingest_explicit_paths_locked(&mut guard, std::slice::from_ref(&norm_file));
        self.maybe_safety_reconcile_locked(&mut guard);
        guard.graph.file_graph_info(&norm_file).ok_or_else(|| {
            let suggestions = guard
                .graph
                .suggest_closest_targets(std::slice::from_ref(&norm_file));
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!("; did_you_mean=[{}]", suggestions.join(", "))
            };
            invalid(format!(
                "file is not indexed in the TCTX polyglot AST graph: {norm_file}{hint}"
            ))
            .into()
        })
    }

    pub fn affected(&self, files: &[String], use_git_diff: bool) -> Result<serde_json::Value> {
        self.refresh_cache_from_disk_if_changed();
        self.warm_environment_probes_unlocked();
        let workspace_root = {
            let mut guard = self.lock_state();
            guard.last_activity = Instant::now();
            self.drain_fs_events_locked(&mut guard);
            Self::ingest_explicit_paths_locked(&mut guard, files);
            self.maybe_foreground_safety_reconcile_locked(&mut guard);
            guard.workspace.root.clone()
        };

        let changed_files = if !files.is_empty() {
            normalize_targets_for_root(&workspace_root, files)
        } else if use_git_diff {
            let raw_dirty =
                normalize_targets_for_root(&workspace_root, &git_dirty_files(&workspace_root)?);
            let guard = self.lock_state();
            expand_dirty_paths_for_graph(&guard.graph, &raw_dirty)
        } else {
            Vec::new()
        };

        let mut fast_lint_changed: Vec<String> = Vec::new();
        let mut rs_lint_changed: Vec<String> = Vec::new();
        let mut all_lint_changed: Vec<String> = Vec::new();
        for f in &changed_files {
            if f.ends_with(".py") || crate::ast_graph::is_frontend_source_path(f) {
                fast_lint_changed.push(f.clone());
                all_lint_changed.push(f.clone());
            } else if f.ends_with(".rs") {
                rs_lint_changed.push(f.clone());
                all_lint_changed.push(f.clone());
            }
        }
        if !fast_lint_changed.is_empty() {
            self.ensure_ruff_cached_unlocked(&fast_lint_changed);
        }
        let mut rust_lint_status: Option<&'static str> = None;
        if !rs_lint_changed.is_empty() {
            // Avoid blocking read-only test_affected on heavy `cargo clippy --all-targets`;
            // enqueue uncached Rust files for background clippy when background execution is enabled.
            let mut guard = self.lock_state();
            let bg_enabled = guard.spec.policy.background_enabled;
            let mut uncached_rs = false;
            for f in &rs_lint_changed {
                if let Some(node) = guard.graph.files.get(f)
                    && !guard
                        .ruff_cache
                        .get(f.as_str())
                        .is_some_and(|e| e.sha256 == node.sha256)
                {
                    uncached_rs = true;
                    if bg_enabled {
                        guard.pending_ruff_files.insert(f.clone());
                    }
                }
            }
            if uncached_rs {
                if bg_enabled {
                    rust_lint_status = Some("queued_in_background");
                    self.condvar.notify_all();
                } else {
                    rust_lint_status = Some("unavailable_background_disabled");
                }
            }
        }
        let mut guard = self.lock_state();
        let ruff_diagnostics = Self::collect_ruff_diagnostics_locked(&mut guard, &all_lint_changed);

        let mut affected_modules = guard.graph.affected_modules(&changed_files);
        let mut untested_modified_files = guard.graph.untested_modified_files(&changed_files);
        let suggested_tests = guard
            .graph
            .suggest_tests_for_untested(&untested_modified_files);
        let test_conventions = guard.graph.test_conventions_summary();
        let affected_raw: Vec<AffectedTest> = guard.graph.affected_tests(&changed_files);
        let total_affected = affected_raw.len();
        let mut items = Vec::with_capacity(total_affected);
        for a in affected_raw {
            let f_key = flight_key(&a.test_file, &a.dep_hash);
            let (cache_state, last_duration_ms) = if guard.in_flight.contains(&f_key) {
                ("running_in_background".to_string(), None)
            } else if !Self::is_graph_incomplete(&guard)
                && let Some(rec) = guard.cache.get_matching(&a.test_file, &a.dep_hash)
                && rec.dynamic_dependencies_complete
            {
                (
                    format!("cached_{}", rec.outcome.as_str()),
                    Some(rec.duration_ms),
                )
            } else if guard.pending_queue.contains(&a.test_file) {
                ("queued_in_background".to_string(), None)
            } else {
                ("uncached".to_string(), None)
            };
            items.push(AffectedTestStatus {
                test_file: a.test_file,
                dep_hash: a.dep_hash,
                cache_state,
                last_duration_ms,
                test_symbols: a.test_symbols,
                reason_chain: a.reason_chain,
            });
        }
        if items.len() > 30 {
            items.sort_by(|a, b| {
                a.reason_chain
                    .len()
                    .cmp(&b.reason_chain.len())
                    .then_with(|| a.test_file.cmp(&b.test_file))
            });
            items.truncate(30);
        }
        affected_modules.truncate(30);
        untested_modified_files.truncate(30);

        let mut payload = serde_json::json!({
            "root": guard.workspace.root,
            "changed_files": changed_files,
            "affected_modules": affected_modules,
            "total_affected_test_files": total_affected,
            "affected_tests": items,
            "untested_modified_files": untested_modified_files,
            "suggested_tests": suggested_tests,
            "test_conventions": test_conventions,
            "ruff_errors_count": ruff_diagnostics.len(),
            "ruff_diagnostics": ruff_diagnostics,
        });
        if let Some(status) = rust_lint_status
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("rust_lint_status".into(), serde_json::Value::String(status.to_string()));
        }
        Ok(payload)
    }

    pub fn check_ruff(&self, files: &[String], fix: bool) -> Result<serde_json::Value> {
        let (workspace_root, spec_snapshot, target_py_files, authorized_crates, all_rs_files) = {
            let mut guard = self.lock_state();
            guard.last_activity = Instant::now();
            self.drain_fs_events_locked(&mut guard);
            Self::ingest_explicit_paths_locked(&mut guard, files);
            self.maybe_safety_reconcile_locked(&mut guard);
            let root_path = guard.workspace.root.clone();
            let norm_files = normalize_targets_for_root(&root_path, files);
            let authorized_crates = if norm_files.is_empty() {
                guard
                    .graph
                    .files
                    .keys()
                    .filter(|k| k.ends_with(".rs"))
                    .map(|k| crate::ast_graph::rust_crate_dir(k))
                    .collect::<BTreeSet<String>>()
            } else {
                authorized_clippy_fix_crates(&root_path, &norm_files, &guard.graph.files)
            };

            let target_py_files: Vec<String> = if !norm_files.is_empty() {
                let mut set = BTreeSet::new();
                for raw in &norm_files {
                    let raw_trimmed = raw
                        .split("::")
                        .next()
                        .unwrap_or(raw)
                        .trim()
                        .replace('\\', "/");
                    let is_root_equiv = raw_trimmed == "." || raw_trimmed == "./";
                    let norm = raw_trimmed
                        .trim_start_matches("./")
                        .trim_end_matches('/');
                    if norm.ends_with(".py")
                        || norm.ends_with(".rs")
                        || crate::ast_graph::is_frontend_source_path(norm)
                    {
                        set.insert(norm.to_string());
                    } else {
                        let prefix = if is_root_equiv || norm.is_empty() || norm == "." {
                            String::new()
                        } else {
                            format!("{norm}/")
                        };
                        for k in guard.graph.files.keys() {
                            if (k.ends_with(".py")
                                || k.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(k))
                                && (prefix.is_empty() || k.starts_with(&prefix))
                            {
                                set.insert(k.clone());
                            }
                        }
                    }
                }
                set.into_iter().collect()
            } else {
                let raw_dirty = git_dirty_files(&root_path).unwrap_or_default();
                let dirty: Vec<String> = expand_dirty_paths_for_graph(&guard.graph, &raw_dirty)
                    .into_iter()
                    .filter(|f| {
                        (f.ends_with(".py")
                            || f.ends_with(".rs")
                            || crate::ast_graph::is_frontend_source_path(f))
                            && guard.graph.files.contains_key(f)
                    })
                    .collect();
                if !dirty.is_empty() {
                    dirty
                } else {
                    guard
                        .graph
                        .files
                        .keys()
                        .filter(|k| {
                            k.ends_with(".py")
                                || k.ends_with(".rs")
                                || crate::ast_graph::is_frontend_source_path(k)
                        })
                        .cloned()
                        .collect()
                }
            };
            let all_rs_files: Vec<String> = guard
                .graph
                .files
                .keys()
                .filter(|k| k.ends_with(".rs"))
                .cloned()
                .collect();
            (
                root_path,
                guard.spec.clone(),
                target_py_files,
                authorized_crates,
                all_rs_files,
            )
        };

        if fix && !target_py_files.is_empty() {
            let fix_files: Vec<String> = target_py_files
                .iter()
                .filter(|f| {
                    !f.ends_with(".rs")
                        || authorized_crates.contains(&crate::ast_graph::rust_crate_dir(f))
                })
                .cloned()
                .collect();
            if !fix_files.is_empty() {
                let touched_crates: BTreeSet<String> = fix_files
                    .iter()
                    .filter(|f| f.ends_with(".rs"))
                    .map(|f| crate::ast_graph::rust_crate_dir(f))
                    .collect();
                let mut reconcile_targets: BTreeSet<String> = fix_files.iter().cloned().collect();
                if !touched_crates.is_empty() {
                    for rs in &all_rs_files {
                        if touched_crates.contains(&crate::ast_graph::rust_crate_dir(rs)) {
                            reconcile_targets.insert(rs.clone());
                        }
                    }
                }
                let reconcile_vec: Vec<String> = reconcile_targets.into_iter().collect();
                let _ = execute_ruff_check(&spec_snapshot, &fix_files, true);
                let mut guard = self.lock_state();
                Self::reconcile_live_files_locked(&mut guard, Some(&reconcile_vec));
                for f in &reconcile_vec {
                    guard.ruff_cache.remove(f);
                }
            }
        }
        self.ensure_ruff_cached_unlocked(&target_py_files);
        let mut guard = self.lock_state();
        let diagnostics = Self::collect_ruff_diagnostics_locked(&mut guard, &target_py_files);
        let unchecked_files: Vec<String> = target_py_files
            .iter()
            .filter(|f| {
                // Missing/unindexed explicit targets must be reported as unchecked (status=error),
                // never silently treated as passing lint.
                guard.graph.files.get(f.as_str()).is_none_or(|node| {
                    !guard
                        .ruff_cache
                        .get(f.as_str())
                        .is_some_and(|entry| entry.sha256 == node.sha256)
                })
            })
            .cloned()
            .collect();
        let status = if !unchecked_files.is_empty() {
            "error"
        } else if diagnostics.is_empty() {
            "passed"
        } else {
            "failed"
        };
        let mut payload = serde_json::json!({
            "root": workspace_root,
            "status": status,
            "fixed": fix,
            "checked_files_count": target_py_files.len().saturating_sub(unchecked_files.len()),
            "ruff_errors_count": diagnostics.len(),
            "ruff_diagnostics": diagnostics,
        });
        if !unchecked_files.is_empty()
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("unchecked_files".into(), serde_json::json!(unchecked_files));
        }
        Ok(payload)
    }

    pub fn explain_failures(&self) -> Result<serde_json::Value> {
        let mut guard = self.lock_state();
        guard.last_activity = Instant::now();
        self.drain_fs_events_locked(&mut guard);
        let raw_dirty = git_dirty_files(&guard.workspace.root).unwrap_or_default();
        let dirty = expand_dirty_paths_for_graph(&guard.graph, &raw_dirty);
        let mut failing_files = Vec::new();
        let mut all_failures = Vec::new();
        let mut culprits = BTreeSet::new();
        for (test_file, rec) in &guard.cache.latest_by_file {
            if !rec.outcome.is_green() {
                failing_files.push(test_file.clone());
                all_failures.extend(extract_structured_failures(
                    test_file,
                    &rec.failed_nodeids,
                    &rec.output_snippet,
                ));
                let deps = guard.graph.transitive_deps_for(test_file);
                for d in &dirty {
                    let norm = d.trim().trim_start_matches("./").replace('\\', "/");
                    if deps.contains(&norm) || norm == *test_file {
                        culprits.insert(norm);
                    }
                }
            }
        }
        Ok(serde_json::json!({
            "root": guard.workspace.root,
            "failing_test_files": failing_files,
            "structured_failures": all_failures,
            "suspected_culprit_files": culprits.into_iter().collect::<Vec<_>>(),
            "flaky_tests": guard.cache.flaky_test_files(),
        }))
    }

    pub fn export_cache_bundle(&self) -> Result<serde_json::Value> {
        let guard = self.lock_state();
        Ok(guard.cache.export_bundle())
    }

    pub fn import_cache_bundle(&self, bundle: &serde_json::Value) -> Result<serde_json::Value> {
        let (root_path, imported) = {
            let mut guard = self.lock_state();
            let policy = guard.spec.policy.clone();
            let imported = guard.cache.import_bundle(bundle, &policy);
            (guard.workspace.root.clone(), imported)
        };
        self.flush_cache_detached();
        Ok(serde_json::json!({
            "root": root_path,
            "imported_records": imported,
        }))
    }

    pub(super) fn ensure_ruff_cached_unlocked(&self, py_files: &[String]) {
        self.ensure_ruff_cached_unlocked_tracked(py_files, None);
    }

    pub(super) fn ensure_ruff_cached_unlocked_tracked(
        &self,
        py_files: &[String],
        active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    ) {
        for _attempt in 0..3 {
            let (spec, uncached, expected_shas, start_cfg_gen, start_crate_gens) = {
                let guard = self.lock_state();
                let mut uncached = Vec::new();
                let mut expected_shas = BTreeMap::new();
                let mut crate_gens: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for f in py_files {
                    if !f.ends_with(".py")
                        && !f.ends_with(".rs")
                        && !crate::ast_graph::is_frontend_source_path(f)
                    {
                        continue;
                    }
                    let Some(node) = guard.graph.files.get(f) else {
                        continue;
                    };
                    if !guard
                        .ruff_cache
                        .get(f.as_str())
                        .is_some_and(|entry| entry.sha256 == node.sha256)
                    {
                        uncached.push(f.clone());
                        expected_shas.insert(f.clone(), node.sha256.clone());
                        if f.ends_with(".rs") {
                            let crate_dir = crate::ast_graph::rust_crate_dir(f);
                            let cur_gen = guard
                                .ruff_crate_generations
                                .get(&crate_dir)
                                .copied()
                                .unwrap_or(0);
                            crate_gens.insert(crate_dir, cur_gen);
                        }
                    }
                }
                (
                    guard.spec.clone(),
                    uncached,
                    expected_shas,
                    guard.ruff_config_generation,
                    crate_gens,
                )
            };
            if uncached.is_empty() {
                return;
            }
            let (by_file, checked_files) =
                execute_ruff_check_tracked(&spec, &uncached, false, active_pgids);
            let mut guard = self.lock_state();
            // Drain watcher events before validating hashes/generations so edits made during
            // the lint subprocess are immediately reflected and retried.
            self.drain_fs_events_locked(&mut guard);
            if guard.ruff_config_generation != start_cfg_gen {
                // Lint config changed while subprocess was in flight; retry against new config.
                continue;
            }
            let batch_size = expected_shas.len();
            let protected: std::collections::HashSet<&str> =
                py_files.iter().map(String::as_str).collect();
            let mut any_stale = false;
            for (file, sha) in expected_shas {
                // Never cache files whose linter invocation failed to spawn, timed out, or emitted invalid JSON.
                if !checked_files.contains(&file) {
                    continue;
                }
                // Guard against out-of-order concurrent lint jobs and Rust crate-wide invalidations
                let still_current = guard
                    .graph
                    .files
                    .get(&file)
                    .is_some_and(|node| node.sha256 == sha);
                let crate_still_current = if file.ends_with(".rs") {
                    let crate_dir = crate::ast_graph::rust_crate_dir(&file);
                    let cur_gen = guard
                        .ruff_crate_generations
                        .get(&crate_dir)
                        .copied()
                        .unwrap_or(0);
                    start_crate_gens.get(&crate_dir).copied().unwrap_or(0) == cur_gen
                } else {
                    true
                };
                if !still_current || !crate_still_current {
                    any_stale = true;
                    continue;
                }
                let mut diags = by_file.get(&file).cloned().unwrap_or_default();
                diags.truncate(100);
                guard.ruff_cache_tick = guard.ruff_cache_tick.saturating_add(1);
                let tick = guard.ruff_cache_tick;
                guard.ruff_cache.insert(
                    file,
                    RuffCacheEntry {
                        sha256: sha,
                        diagnostics: diags,
                        access_tick: tick,
                    },
                );
            }
            let max_entries = 4096usize.max(batch_size);
            if guard.ruff_cache.len() > max_entries {
                let excess = guard
                    .ruff_cache
                    .len()
                    .saturating_sub(max_entries.saturating_sub(512).max(batch_size));
                let mut entries: Vec<(u64, String)> = guard
                    .ruff_cache
                    .iter()
                    .filter(|(k, _)| !protected.contains(k.as_str()))
                    .map(|(k, v)| (v.access_tick, k.clone()))
                    .collect();
                if excess < entries.len() {
                    entries.select_nth_unstable_by_key(excess, |(tick, _)| *tick);
                    entries.truncate(excess);
                }
                for (_, k) in entries {
                    guard.ruff_cache.remove(&k);
                }
            }
            if !any_stale {
                return;
            }
        }
    }

    pub(super) fn collect_ruff_diagnostics_locked(
        guard: &mut RootState,
        py_files: &[String],
    ) -> Vec<RuffDiagnostic> {
        let mut out = Vec::new();
        for f in py_files {
            if let Some(sha) = guard.graph.files.get(f).map(|n| n.sha256.clone()) {
                guard.ruff_cache_tick = guard.ruff_cache_tick.saturating_add(1);
                let tick = guard.ruff_cache_tick;
                if let Some(entry) = guard.ruff_cache.get_mut(f.as_str())
                    && entry.sha256 == sha
                {
                    entry.access_tick = tick;
                    out.extend(entry.diagnostics.iter().cloned());
                }
            }
        }
        out
    }

}

