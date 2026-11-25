use super::*;

impl ManagedRoot {
    pub(super) fn remove_watch_dirs_locked(&self, guard: &mut RootState, directories: &[PathBuf]) {
        if directories.is_empty() {
            return;
        }
        let mut watcher = self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(watcher) = watcher.as_mut() else {
            return;
        };
        for directory in directories {
            if let Err(error) = watcher.unwatch(directory) {
                let already_removed = match &error.kind {
                    notify::ErrorKind::WatchNotFound => true,
                    notify::ErrorKind::Io(error) => error.kind() == std::io::ErrorKind::NotFound,
                    _ => false,
                };
                if !already_removed {
                    guard.last_watcher_error =
                        Some(format!("unwatch {}: {error}", directory.display()));
                    continue;
                }
            }
            guard.watched_dirs.remove(directory);
            self.watcher_filter_dirs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(directory);
        }
    }

    pub(super) fn prune_missing_watch_dirs_locked(&self, guard: &mut RootState) {
        let missing: Vec<PathBuf> = guard
            .watched_dirs
            .iter()
            .filter(|directory| !directory.is_dir())
            .cloned()
            .collect();
        self.remove_watch_dirs_locked(guard, &missing);
    }

    pub(super) fn snapshot_invalidation_watermarks(
        guard: &RootState,
        files: &[String],
    ) -> BTreeMap<String, (u64, u64)> {
        let mut out = BTreeMap::new();
        for tf in files {
            let local_gen = guard.invalidation_generation.max(
                guard
                    .file_invalidation_generations
                    .get(tf)
                    .copied()
                    .unwrap_or(0),
            );
            let wal_seq = guard.cache.invalidation_watermark_for(tf);
            out.insert(tf.clone(), (local_gen, wal_seq));
        }
        out
    }

    pub(super) fn was_invalidated_since(
        guard: &RootState,
        test_file: &str,
        start_watermarks: &BTreeMap<String, (u64, u64)>,
    ) -> bool {
        let Some(&(start_local, start_wal)) = start_watermarks.get(test_file) else {
            return false;
        };
        let cur_local = guard.invalidation_generation.max(
            guard
                .file_invalidation_generations
                .get(test_file)
                .copied()
                .unwrap_or(0),
        );
        let cur_wal = guard.cache.invalidation_watermark_for(test_file);
        cur_local > start_local || cur_wal > start_wal
    }

    pub(super) fn is_test_cacheable_in_graph(guard: &RootState, test_file: &str) -> bool {
        if Self::is_graph_incomplete(guard) || guard.graph.untrackable_tests.contains(test_file) {
            return false;
        }
        if guard.fe_env_uncacheable && crate::ast_graph::is_frontend_source_path(test_file) {
            return false;
        }
        if guard.py_env_uncacheable && test_file.ends_with(".py") {
            return false;
        }
        if let Some(node) = guard.graph.files.get(test_file)
            && node
                .parsed
                .markers
                .iter()
                .chain(node.parsed.module_markers.iter())
                .any(|m| {
                    matches!(
                        m.as_str(),
                        "network" | "live" | "external" | "azure" | "docker"
                    )
                })
        {
            return false;
        }
        true
    }

    pub(super) fn is_graph_incomplete(guard: &RootState) -> bool {
        guard.graph.discovery_budget_exceeded
            || guard.required_watches_incomplete
            || guard.inventory_incomplete
            || !guard.unreadable_files.is_empty()
    }

    pub(super) fn test_touches_unreadable_file(guard: &RootState, test_file: &str) -> bool {
        if guard.unreadable_files.is_empty() {
            return false;
        }
        if guard.unreadable_files.contains(test_file) {
            return true;
        }
        if let Some(ids) = guard.graph.transitive_dep_ids_for_test.get(test_file) {
            for &id in ids {
                if let Some(dep_path) = guard.graph.file_index_to_path.get(id as usize)
                    && guard.unreadable_files.contains(dep_path)
                {
                    return true;
                }
            }
        }
        for unread in &guard.unreadable_files {
            if guard.graph.config_hashes.contains_key(unread) {
                return true;
            }
            let fname = Path::new(unread)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if fname == "conftest.py"
                || crate::workspace::PYTEST_CONFIG_FILES.contains(&fname)
                || crate::workspace::RUST_CONFIG_FILES.contains(&fname)
                || crate::workspace::is_cargo_dot_config_rel(Path::new(unread))
                || crate::workspace::is_frontend_config_filename(fname)
                || crate::workspace::POLYGLOT_CONFIG_FILES.contains(&fname)
                || crate::ast_graph::is_frontend_setup_file(unread)
                || fname.ends_with(".csproj")
            {
                return true;
            }
        }
        false
    }

    pub(super) fn refresh_required_watches_flag(guard: &mut RootState) {
        // Retain every outstanding unwatched directory that still exists on disk and is not yet
        // watched, so deleting an unrelated watched directory never clears the fail-closed flag
        // while another directory remains unwatched.
        guard
            .unwatched_dirs
            .retain(|d| d.is_dir() && !guard.watched_dirs.contains(d));
        let missing_required = guard.graph.go_embed_watch_dirs.iter().any(|rel| {
            let full = guard.workspace.root.join(rel);
            full.is_dir() && !guard.watched_dirs.contains(&full)
        });
        guard.required_watches_incomplete = missing_required || !guard.unwatched_dirs.is_empty();
    }

    pub(super) fn add_watch_dirs_locked(&self, guard: &mut RootState, directories: BTreeSet<PathBuf>) {
        if directories.is_empty() {
            Self::refresh_required_watches_flag(guard);
            return;
        }
        let mut watcher = self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(watcher) = watcher.as_mut() else {
            return;
        };
        for directory in directories {
            if guard.watched_dirs.contains(&directory) {
                guard.unwatched_dirs.remove(&directory);
                continue;
            }
            if guard.watched_dirs.len() >= guard.spec.policy.max_watch_dirs {
                if directory.is_dir() {
                    guard.unwatched_dirs.insert(directory);
                }
                continue;
            }
            match watcher.watch(&directory, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watcher_filter_dirs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(directory.clone());
                    guard.unwatched_dirs.remove(&directory);
                    guard.watched_dirs.insert(directory);
                }
                Err(error) => {
                    if directory.is_dir() {
                        guard.unwatched_dirs.insert(directory.clone());
                    }
                    guard.last_watcher_error =
                        Some(format!("watch {}: {error}", directory.display()));
                }
            }
        }
        Self::refresh_required_watches_flag(guard);
    }

    pub(super) fn drain_fs_events_locked(&self, guard: &mut RootState) {
        let mut raw_paths = BTreeSet::new();
        let mut retired_watch_roots = BTreeSet::new();
        let mut directory_moves = Vec::new();
        let mut overflowed = self.fs_overflow.swap(false, Ordering::AcqRel);
        if let Ok(rx) = self.fs_events_rx.lock() {
            while let Ok(event) = rx.try_recv() {
                guard.watch_events_drained = guard.watch_events_drained.saturating_add(1);
                if event.need_rescan() {
                    overflowed = true;
                }
                if matches!(
                    event.kind,
                    notify::EventKind::Modify(notify::event::ModifyKind::Name(
                        notify::event::RenameMode::Both
                    ))
                ) && let [from, to] = event.paths.as_slice()
                    && guard.watched_dirs.contains(from)
                {
                    directory_moves.push((from.clone(), to.clone()));
                }
                for p in event.paths {
                    if guard.watched_dirs.contains(&p)
                        && (!p.is_dir()
                            || matches!(
                                event.kind,
                                notify::EventKind::Remove(_)
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ))
                    {
                        retired_watch_roots.insert(p.clone());
                    }
                    raw_paths.insert(p);
                }
            }
        }
        if overflowed {
            self.run_overflow_reconcile_locked(guard);
            return;
        }
        if raw_paths.is_empty() {
            return;
        }

        for (from, to) in directory_moves {
            for path in guard.watched_dirs.iter().cloned().chain(
                guard
                    .graph
                    .files
                    .keys()
                    .map(|rel| guard.workspace.root.join(rel)),
            ) {
                if let Ok(suffix) = path.strip_prefix(&from) {
                    raw_paths.insert(to.join(suffix));
                }
            }
        }

        if !retired_watch_roots.is_empty() {
            let retired_directories: Vec<PathBuf> = guard
                .watched_dirs
                .iter()
                .filter(|directory| {
                    retired_watch_roots
                        .iter()
                        .any(|root| directory.starts_with(root))
                })
                .cloned()
                .collect();
            self.remove_watch_dirs_locked(guard, &retired_directories);
            raw_paths.extend(retired_directories);
            raw_paths.extend(
                guard
                    .graph
                    .files
                    .keys()
                    .map(|rel| guard.workspace.root.join(rel))
                    .filter(|path| {
                        retired_watch_roots
                            .iter()
                            .any(|root| path.starts_with(root))
                    }),
            );
        }

        let mut changed_rels = Vec::new();
        let mut structure_changed = false;
        let mut gitignore_changed = false;
        let mut new_watch_dirs = BTreeSet::new();
        let mut missing_candidates: Vec<(PathBuf, String)> = Vec::new();

        for path in raw_paths {
            if path.is_dir() {
                if path.starts_with(&guard.workspace.root) && !guard.watched_dirs.contains(&path) {
                    new_watch_dirs.insert(path);
                }
                continue;
            }
            let Ok(rel) = path.strip_prefix(&guard.workspace.root) else {
                continue;
            };
            let rel_str = normalize_rel_str(rel);
            if rel_str == ".gitignore" {
                gitignore_changed = true;
                continue;
            }
            let is_under_explicit_testpath = rel_str.ends_with(".py")
                && crate::ast_graph::path_matches_testpaths(&rel_str, &guard.graph.testpaths);
            let matches_go_embed = guard.graph.matches_any_go_embed(&rel_str);
            let matches_watched_dir = guard.graph.matches_any_watched_data_dir(&rel_str);
            let matches_missing_ref = guard.graph.matches_any_missing_data_ref(&rel_str);
            let is_excluded_rel = guard.workspace.is_excluded_or_ignored_rel(rel);
            let is_go_native_sibling = is_excluded_rel
                && guard.graph.files.keys().any(|p| p.ends_with(".go"))
                && (rel_str.ends_with(".go")
                    || rel_str == "go.mod"
                    || rel_str.ends_with("/go.mod")
                    || rel_str == "go.work"
                    || rel_str.ends_with("/go.work")
                    || crate::ast_graph::is_cgo_native_source_path(&rel_str)
                    || rel_str.ends_with(".syso")
                    || rel_str.ends_with(".swig")
                    || rel_str.ends_with(".swigcxx"));
            if (matches_go_embed
                || matches_watched_dir
                || matches_missing_ref
                || is_go_native_sibling)
                && is_excluded_rel
            {
                // An excluded/gitignored file inside a Go embed, Go package sibling, referenced
                // build-input directory, or previously absent referenced fixture changed (or was
                // deleted): preserve exclusions and re-run discovery so `discovery_budget_exceeded`
                // stays accurate!
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                continue;
            }
            let is_known_data = guard.graph.files.contains_key(&rel_str)
                || matches_go_embed
                || matches_watched_dir
                || matches_missing_ref
                || guard.graph.matches_any_cgo_include_dir(&rel_str)
                || is_under_explicit_testpath;
            if !guard.workspace.is_permitted_rel(rel) && !is_known_data {
                let is_fe_shadow_candidate = matches!(
                    Path::new(&rel_str).file_name().and_then(|n| n.to_str()),
                    Some("package.json" | "tsconfig.json" | "jsconfig.json")
                ) || rel_str.ends_with(".js")
                    || rel_str.ends_with(".jsx")
                    || rel_str.ends_with(".mjs")
                    || rel_str.ends_with(".cjs")
                    || rel_str.ends_with(".ts")
                    || rel_str.ends_with(".tsx");
                if is_fe_shadow_candidate
                    && guard
                        .graph
                        .files
                        .keys()
                        .any(|p| crate::ast_graph::is_frontend_source_path(p))
                {
                    guard.graph.refresh_untrackable_tests();
                }
                continue;
            }
            let full = guard.workspace.root.join(rel);
            let live_stamp = file_stamp(&full);
            if let Some(stamp) = live_stamp {
                if let Some(parent) = full.parent()
                    && parent.starts_with(&guard.workspace.root)
                    && !guard.watched_dirs.contains(parent)
                {
                    new_watch_dirs.insert(parent.to_path_buf());
                }
                if guard
                    .graph
                    .files
                    .get(&rel_str)
                    .and_then(|n| n.stamp)
                    .is_some_and(|s| s == stamp)
                {
                    continue;
                }
                match guard
                    .workspace
                    .read_known_with_limit(rel, guard.spec.policy.max_file_bytes)
                {
                    Ok(bytes) => {
                        guard.unreadable_files.remove(&rel_str);
                        let new_sha = sha256_hex(&bytes);
                        let unchanged = guard
                            .graph
                            .files
                            .get(&rel_str)
                            .is_some_and(|node| node.sha256 == new_sha);
                        if !unchanged {
                            if guard
                                .graph
                                .insert_file_bytes_with_stamp(&rel_str, &bytes, Some(stamp))
                            {
                                structure_changed = true;
                            }
                            changed_rels.push(rel_str);
                        } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                            node.stamp = Some(stamp);
                        }
                    }
                    Err(_) => {
                        if guard.unreadable_files.insert(rel_str.clone()) {
                            guard.generation = guard.generation.saturating_add(1);
                        }
                        if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                            node.stamp = None;
                        }
                    }
                }
            } else if guard.graph.files.contains_key(&rel_str) {
                missing_candidates.push((rel.to_path_buf(), rel_str));
            } else if guard.unreadable_files.contains(&rel_str)
                && !full.exists()
                && std::fs::symlink_metadata(&full).is_err()
            {
                guard.unreadable_files.remove(&rel_str);
            }
        }

        // Batch-check missing files at most once (max 1 brief yield, never N × 25ms inside lock)
        if !missing_candidates.is_empty() {
            if missing_candidates.len() <= 4 {
                std::thread::sleep(Duration::from_millis(10));
            }
            for (rel, rel_str) in missing_candidates {
                let full = guard.workspace.root.join(&rel);
                if let Some(stamp) = file_stamp(&full) {
                    match guard
                        .workspace
                        .read_known_with_limit(&rel, guard.spec.policy.max_file_bytes)
                    {
                        Ok(bytes) => {
                            guard.unreadable_files.remove(&rel_str);
                            let new_sha = sha256_hex(&bytes);
                            let unchanged = guard
                                .graph
                                .files
                                .get(&rel_str)
                                .is_some_and(|node| node.sha256 == new_sha);
                            if !unchanged {
                                if guard.graph.insert_file_bytes_with_stamp(
                                    &rel_str,
                                    &bytes,
                                    Some(stamp),
                                ) {
                                    structure_changed = true;
                                }
                                changed_rels.push(rel_str);
                            } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                                node.stamp = Some(stamp);
                            }
                        }
                        Err(_) => {
                            if guard.unreadable_files.insert(rel_str.clone()) {
                                guard.generation = guard.generation.saturating_add(1);
                            }
                            if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                                node.stamp = None;
                            }
                        }
                    }
                } else if !full.exists()
                    && std::fs::symlink_metadata(&full).is_err()
                    && guard.graph.remove_file(&rel_str)
                {
                    guard.unreadable_files.remove(&rel_str);
                    guard.ruff_cache.remove(&rel_str);
                    guard.pending_ruff_files.remove(&rel_str);
                    changed_rels.push(rel_str);
                    structure_changed = true;
                } else if std::fs::symlink_metadata(&full).is_ok() {
                    if guard.unreadable_files.insert(rel_str.clone()) {
                        guard.generation = guard.generation.saturating_add(1);
                    }
                    if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                        node.stamp = None;
                    }
                }
            }
        }

        if !new_watch_dirs.is_empty() || !retired_watch_roots.is_empty() {
            invalidate_pythonpath_cache(&guard.workspace.root);
            if guard.graph.reconcile_testpaths_fallback(&guard.workspace.root) {
                guard.graph.recompute_edges();
                guard.generation = guard.generation.saturating_add(1);
            }
        }
        // Register any newly created root embed/cgo directory as required BEFORE attempting
        // `add_watch_dirs_locked` so hitting `max_watch_dirs` immediately flips the fail-closed flag.
        for dir in &new_watch_dirs {
            if let Ok(rel) = dir.strip_prefix(&guard.workspace.root) {
                let rel_s = normalize_rel_str(rel);
                if !rel_s.is_empty()
                    && (guard.graph.dir_matches_any_go_embed(&rel_s)
                        || guard.graph.matches_any_cgo_include_dir(&rel_s)
                        || guard.graph.matches_any_missing_data_ref(&rel_s))
                {
                    guard.graph.go_embed_watch_dirs.insert(rel_s);
                }
            }
        }
        let newly_watched_roots: Vec<PathBuf> = new_watch_dirs.iter().cloned().collect();
        self.add_watch_dirs_locked(guard, new_watch_dirs);
        // After installing watches on newly created directories, scan their existing entries
        // (race-free inotify pattern) so files created between `mkdir` and `watch_dir` (e.g.
        // `.cargo/config.toml` or a new test package) are indexed immediately.
        if !newly_watched_roots.is_empty() {
            let mut scan_stack = newly_watched_roots;
            let mut scanned_entries = 0usize;
            let mut scan_truncated = false;
            while let Some(dir) = scan_stack.pop() {
                if scanned_entries >= 1_024 {
                    scan_truncated = true;
                    break;
                }
                let is_cargo_dir = dir.file_name().and_then(|n| n.to_str()) == Some(".cargo");
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    scanned_entries += 1;
                    if scanned_entries > 1_024 {
                        scan_truncated = true;
                        break;
                    }
                    let Ok(ft) = entry.file_type() else {
                        continue;
                    };
                    if ft.is_symlink() {
                        continue;
                    }
                    let ep = entry.path();
                    let Ok(rel) = ep.strip_prefix(&guard.workspace.root) else {
                        continue;
                    };
                    if ft.is_dir() {
                        if is_cargo_dir {
                            continue;
                        }
                        let dir_rel_str = normalize_rel_str(rel);
                        let is_required_embed_or_cgo_dir = guard
                            .graph
                            .dir_matches_any_go_embed(&dir_rel_str)
                            || guard.graph.matches_any_watched_data_dir(&dir_rel_str)
                            || guard.graph.matches_any_cgo_include_dir(&dir_rel_str)
                            || guard.graph.matches_any_missing_data_ref(&dir_rel_str)
                            || crate::ast_graph::dir_could_match_testpaths(
                                &dir_rel_str,
                                &guard.graph.testpaths,
                            );
                        if guard.graph.matches_any_watched_data_dir(&dir_rel_str)
                            && guard.workspace.is_excluded_or_ignored_rel(rel)
                        {
                            guard.graph.discovery_budget_exceeded = true;
                            continue;
                        }
                        if let Some(name) = ep.file_name().and_then(|n| n.to_str())
                            && guard.workspace.default_excludes
                            && crate::exclusion_policy::is_excluded_dir_name(name)
                            && name != ".cargo"
                            && !is_required_embed_or_cgo_dir
                        {
                            continue;
                        }
                        if is_required_embed_or_cgo_dir && !dir_rel_str.is_empty() {
                            // Register as required BEFORE attempting watch so exceeding
                            // `max_watch_dirs` immediately flips `required_watches_incomplete`.
                            guard.graph.go_embed_watch_dirs.insert(dir_rel_str);
                        }
                        if !guard.watched_dirs.contains(&ep) {
                            // Install watch on each nested directory BEFORE scanning its entries
                            // so files created during traversal cannot be missed.
                            let mut single = BTreeSet::new();
                            single.insert(ep.clone());
                            self.add_watch_dirs_locked(guard, single);
                            scan_stack.push(ep);
                        }
                    } else if ft.is_file() {
                        let rel_str = normalize_rel_str(rel);
                        let is_explicit_tp_py = rel_str.ends_with(".py")
                            && crate::ast_graph::path_matches_testpaths(
                                &rel_str,
                                &guard.graph.testpaths,
                            );
                        let matches_go_embed = guard.graph.matches_any_go_embed(&rel_str);
                        let matches_watched_dir = guard.graph.matches_any_watched_data_dir(&rel_str);
                        let matches_missing_ref = guard.graph.matches_any_missing_data_ref(&rel_str);
                        if (matches_go_embed || matches_watched_dir || matches_missing_ref)
                            && guard.workspace.is_excluded_or_ignored_rel(rel)
                        {
                            // Preserve exclusions during catch-up scans AND fail closed.
                            guard.graph.discovery_budget_exceeded = true;
                            continue;
                        }
                        if !guard.workspace.is_permitted_rel(rel)
                            && !guard.graph.files.contains_key(&rel_str)
                            && !matches_go_embed
                            && !matches_watched_dir
                            && !matches_missing_ref
                            && !guard.graph.matches_any_cgo_include_dir(&rel_str)
                            && !is_explicit_tp_py
                        {
                            continue;
                        }
                        if let Some(stamp) = file_stamp(&ep)
                            && !guard
                                .graph
                                .files
                                .get(&rel_str)
                                .and_then(|n| n.stamp)
                                .is_some_and(|s| s == stamp)
                        {
                            match guard
                                .workspace
                                .read_known_with_limit(rel, guard.spec.policy.max_file_bytes)
                            {
                                Ok(bytes) => {
                                    guard.unreadable_files.remove(&rel_str);
                                    let new_sha = sha256_hex(&bytes);
                                    let unchanged_content = guard
                                        .graph
                                        .files
                                        .get(&rel_str)
                                        .is_some_and(|n| n.sha256 == new_sha);
                                    if !unchanged_content {
                                        if guard.graph.insert_file_bytes_with_stamp(
                                            &rel_str,
                                            &bytes,
                                            Some(stamp),
                                        ) {
                                            structure_changed = true;
                                        }
                                        changed_rels.push(rel_str);
                                    } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                                        node.stamp = Some(stamp);
                                    }
                                }
                                Err(_) => {
                                    if guard.unreadable_files.insert(rel_str.clone()) {
                                        guard.generation = guard.generation.saturating_add(1);
                                    }
                                    if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                                        node.stamp = None;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if scan_truncated {
                // Complete full overflow reconciliation inline before returning so the current
                // request never resolves targets against a truncated directory scan.
                self.run_overflow_reconcile_locked(guard);
                return;
            }
        }

        if gitignore_changed {
            self.reload_gitignore_and_prune_locked(guard);
            self.run_overflow_reconcile_locked(guard);
            return;
        }
        if !changed_rels.is_empty() {
            self.has_go_embeds
                .store(guard.graph.has_go_embed_directives(), Ordering::Release);
            invalidate_git_dirty_cache(&guard.workspace.root);
            let pytest_cfg_touched = changed_rels.iter().any(|rel| {
                matches!(
                    Path::new(rel).file_name().and_then(|n| n.to_str()),
                    Some(
                        "pytest.toml"
                            | ".pytest.toml"
                            | "pytest.ini"
                            | ".pytest.ini"
                            | "pyproject.toml"
                            | "setup.cfg"
                            | "tox.ini"
                    )
                )
            });
            if pytest_cfg_touched && guard.graph.refresh_pytest_config(&guard.workspace) {
                // A live `pytest.ini` / `pyproject.toml` edit may have changed `python_files`
                // or added an existing hidden `testpaths` directory (e.g. `.hidden/tests`):
                // refresh watcher admission, install watches, ALWAYS recompute edges/hashes for
                // the current batch (`changed_rels`), and run overflow reconciliation for newly
                // visible files.
                *self
                    .watcher_testpaths
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = guard.graph.testpaths.clone();
                self.has_go_embeds
                    .store(guard.graph.has_go_embed_directives(), Ordering::Release);
                let tp_watch_dirs: BTreeSet<PathBuf> = collect_testpath_watch_dirs(
                    &guard.workspace.root,
                    &guard.graph.testpaths,
                    &guard.spec.policy,
                )
                .into_iter()
                .filter(|d| !guard.watched_dirs.contains(d))
                .collect();
                if !tp_watch_dirs.is_empty() {
                    self.add_watch_dirs_locked(guard, tp_watch_dirs);
                }
                invalidate_pythonpath_cache(&guard.workspace.root);
                guard.generation = guard.generation.saturating_add(1);
                Self::invalidate_ruff_for_changed_locked(guard, &changed_rels);
                guard.graph.recompute_edges();
                if guard.spec.policy.background_enabled {
                    let affected = guard.graph.affected_tests(&changed_rels);
                    Self::enqueue_affected_locked(guard, affected);
                    self.condvar.notify_all();
                }
                self.run_overflow_reconcile_locked(guard);
                return;
            }
            if guard.graph.reconcile_testpaths_fallback(&guard.workspace.root) {
                structure_changed = true;
            }
            if structure_changed {
                invalidate_pythonpath_cache(&guard.workspace.root);
            }
            guard.generation = guard.generation.saturating_add(1);
            Self::invalidate_ruff_for_changed_locked(guard, &changed_rels);
            if structure_changed {
                let prev_count = guard.graph.files.len();
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                self.has_go_embeds
                    .store(guard.graph.has_go_embed_directives(), Ordering::Release);
                // Install watches for any newly discovered Go embed or cgo header directories
                // BEFORE finalizing edges, then rescan once if new watches were added so files
                // created in the watch-installation race window cannot be missed.
                let new_embed_dirs: BTreeSet<PathBuf> = guard
                    .graph
                    .go_embed_watch_dirs
                    .iter()
                    .map(|d| guard.workspace.root.join(d))
                    .filter(|d| d.is_dir() && !guard.watched_dirs.contains(d))
                    .collect();
                if !new_embed_dirs.is_empty() {
                    self.add_watch_dirs_locked(guard, new_embed_dirs);
                    guard
                        .graph
                        .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                }
                Self::refresh_required_watches_flag(guard);
                if guard.graph.files.len() != prev_count {
                    guard.graph.recompute_edges();
                } else {
                    guard.graph.recompute_structure_incremental(&changed_rels);
                }
            } else {
                guard.graph.recompute_hashes_for_changed(&changed_rels);
            }
            if guard.spec.policy.background_enabled {
                let affected = guard.graph.affected_tests(&changed_rels);
                Self::enqueue_affected_locked(guard, affected);
                self.condvar.notify_all();
            }
        }
    }

    pub(super) fn invalidate_ruff_for_changed_locked(guard: &mut RootState, changed_rels: &[String]) {
        let mut touched_rust_crates = BTreeSet::new();
        for rel_str in changed_rels {
            let file_name = Path::new(rel_str).file_name().and_then(|n| n.to_str());
            if matches!(
                file_name,
                Some(
                    "pyproject.toml"
                        | "ruff.toml"
                        | ".ruff.toml"
                        | "Cargo.toml"
                        | "clippy.toml"
                        | ".clippy.toml"
                        | "biome.json"
                        | "biome.jsonc"
                        | "tsconfig.json"
                )
            ) || file_name
                .is_some_and(|n| n.starts_with("eslint.config.") || n.starts_with(".eslintrc"))
            {
                guard.ruff_cache.clear();
                guard.ruff_config_generation = guard.ruff_config_generation.saturating_add(1);
            } else if rel_str.ends_with(".rs") {
                touched_rust_crates.insert(crate::ast_graph::rust_crate_dir(rel_str));
                guard.ruff_cache.remove(rel_str);
                if guard.graph.files.contains_key(rel_str) {
                    guard.pending_ruff_files.insert(rel_str.clone());
                }
            } else if rel_str.ends_with(".py")
                || crate::ast_graph::is_frontend_source_path(rel_str)
            {
                guard.ruff_cache.remove(rel_str);
                if guard.graph.files.contains_key(rel_str) {
                    guard.pending_ruff_files.insert(rel_str.clone());
                }
            }
        }
        if !touched_rust_crates.is_empty() {
            for crate_dir in &touched_rust_crates {
                let entry = guard
                    .ruff_crate_generations
                    .entry(crate_dir.clone())
                    .or_insert(0);
                *entry = entry.saturating_add(1);
            }
            guard.ruff_cache.retain(|file_part, _| {
                if !file_part.ends_with(".rs") {
                    return true;
                }
                !touched_rust_crates.contains(&crate::ast_graph::rust_crate_dir(file_part))
            });
        }
    }

    pub(super) fn ingest_explicit_paths_locked(guard: &mut RootState, raw_paths: &[String]) {
        let mut added_any = false;
        let root_path = guard.workspace.root.clone();
        for raw in raw_paths {
            let norm_target = normalize_target_for_root(&root_path, raw);
            let file_part = norm_target.split("::").next().unwrap_or(&norm_target);
            let norm = file_part
                .trim()
                .trim_start_matches("./")
                .trim_end_matches('/')
                .replace('\\', "/");
            if norm.is_empty() || norm == "." || guard.graph.files.contains_key(&norm) {
                continue;
            }

            let rel_path = Path::new(&norm);
            if !guard.workspace.is_permitted_rel(rel_path)
                && !guard.workspace.is_permitted_data_rel(rel_path)
            {
                continue;
            }
            let full = guard.workspace.root.join(rel_path);
            if let Some(stamp) = file_stamp(&full)
                && let Ok(bytes) = guard
                    .workspace
                    .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
            {
                guard
                    .graph
                    .insert_file_bytes_with_stamp(&norm, &bytes, Some(stamp));
                added_any = true;
            }
        }
        if added_any {
            guard.generation = guard.generation.saturating_add(1);
            invalidate_pythonpath_cache(&guard.workspace.root);
            guard
                .graph
                .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            guard.graph.recompute_edges();
        }
    }

    pub(super) fn reload_gitignore_and_prune_locked(&self, guard: &mut RootState) {
        let live_gi_stamp = file_stamp(&guard.spec.root.join(".gitignore"));
        let stamp_changed = live_gi_stamp != guard.gitignore_stamp;
        guard.gitignore_stamp = live_gi_stamp;
        if let Ok(rebuilt_ws) = Workspace::new_with_excludes(
            &guard.spec.root,
            guard.spec.scopes.clone(),
            guard.spec.globs.clone(),
            guard.spec.excludes.clone(),
            guard.spec.default_excludes,
        ) {
            guard.workspace = rebuilt_ws.clone();
            *self
                .watcher_workspace
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = rebuilt_ws;
            let rt_deps: BTreeSet<String> = guard
                .graph
                .runtime_deps_for_test
                .values()
                .flatten()
                .cloned()
                .collect();
            let to_prune: Vec<String> = guard
                .graph
                .files
                .keys()
                .filter(|p| {
                    let rel_p = Path::new(p.as_str());
                    !guard.workspace.is_permitted_rel(rel_p)
                        && !guard.workspace.is_permitted_data_rel(rel_p)
                        && !rt_deps.contains(p.as_str())
                        && !guard.graph.raw_embedded_files.contains(p.as_str())
                        && !guard.graph.matches_any_go_embed(p)
                        && !guard.graph.matches_any_cgo_include_dir(p)
                        && !guard.graph.matches_any_missing_data_ref(p)
                })
                .cloned()
                .collect();
            if !to_prune.is_empty() {
                for p in to_prune {
                    guard.graph.remove_file(&p);
                }
                guard.graph.recompute_edges();
                guard.generation = guard.generation.saturating_add(1);
            } else if stamp_changed {
                // Always bump generation when `.gitignore` changed so a concurrent `sync()`
                // cannot commit an obsolete workspace matcher built against the old `.gitignore`.
                guard.generation = guard.generation.saturating_add(1);
            }
        }
    }

    pub(super) fn run_overflow_reconcile_locked(&self, guard: &mut RootState) {
        invalidate_git_dirty_cache(&guard.workspace.root);
        invalidate_pythonpath_cache(&guard.workspace.root);
        // Always refresh `.gitignore` matcher on overflow recovery (cached by `FileStamp`, so
        // essentially free when unchanged) in case an overflow dropped a `.gitignore` event.
        self.reload_gitignore_and_prune_locked(guard);
        self.prune_missing_watch_dirs_locked(guard);
        let mut discovered_rels = Vec::new();
        // Multi-pass race-free overflow recovery: loop directory enumeration & watch installation
        // until a pass adds 0 new directories (closing the empty-dir creation race before
        // `files_limited` scans entries).
        let mut dirs_truncated_in_overflow = false;
        for _ in 0..3 {
            let (enum_dirs, truncated) = guard
                .workspace
                .directories_limited_checked(&guard.spec.policy);
            if truncated {
                dirs_truncated_in_overflow = true;
            }
            let mut all_dirs: BTreeSet<PathBuf> = enum_dirs
                .into_iter()
                .filter(|d| !guard.watched_dirs.contains(d))
                .collect();
            for tp_dir in collect_testpath_watch_dirs(
                &guard.workspace.root,
                &guard.graph.testpaths,
                &guard.spec.policy,
            ) {
                if !guard.watched_dirs.contains(&tp_dir) {
                    all_dirs.insert(tp_dir);
                }
            }
            if all_dirs.is_empty() {
                break;
            }
            let before_watched = guard.watched_dirs.len();
            self.add_watch_dirs_locked(guard, all_dirs);
            if guard.watched_dirs.len() == before_watched {
                break;
            }
        }
        match guard.workspace.files_limited(&guard.spec.policy) {
            Ok(inventory) => {
                for oversized in &inventory.skipped_oversized_paths {
                    if guard.unreadable_files.insert(oversized.clone()) {
                        guard.generation = guard.generation.saturating_add(1);
                    }
                }
                guard.inventory_incomplete = dirs_truncated_in_overflow
                    || inventory.skipped_oversized_files > inventory.skipped_oversized_paths.len()
                    || inventory.skipped_unreadable_dirs > 0;
                let mut extra_dirs = BTreeSet::new();
                for rel_path in &inventory.files {
                    let rel_str = rel_path.to_string_lossy().replace('\\', "/");
                    let full = guard.workspace.root.join(rel_path);
                    let mut cur = full.parent();
                    while let Some(dir) = cur {
                        if !dir.starts_with(&guard.workspace.root) {
                            break;
                        }
                        if !guard.watched_dirs.contains(dir) {
                            extra_dirs.insert(dir.to_path_buf());
                        }
                        if dir == guard.workspace.root {
                            break;
                        }
                        cur = dir.parent();
                    }
                    if !guard.graph.files.contains_key(&rel_str) {
                        if let Some(stamp) = file_stamp(&full) {
                            match guard
                                .workspace
                                .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
                            {
                                Ok(bytes) => {
                                    guard.unreadable_files.remove(&rel_str);
                                    guard.graph.insert_file_bytes_with_stamp(
                                        &rel_str,
                                        &bytes,
                                        Some(stamp),
                                    );
                                    discovered_rels.push(rel_str);
                                }
                                Err(_) => {
                                    if guard.unreadable_files.insert(rel_str) {
                                        guard.generation = guard.generation.saturating_add(1);
                                    }
                                }
                            }
                        } else if std::fs::symlink_metadata(&full).is_ok()
                            && guard.unreadable_files.insert(rel_str)
                        {
                            guard.generation = guard.generation.saturating_add(1);
                        }
                    }
                }
                if !extra_dirs.is_empty() {
                    self.add_watch_dirs_locked(guard, extra_dirs);
                }
            }
            Err(err) => {
                // Keep overflow inventory failure in a dedicated `inventory_incomplete` flag
                // (never cleared by `discover_referenced_data_files`) until inventory succeeds.
                guard.inventory_incomplete = true;
                guard.last_watcher_error = Some(format!("overflow inventory failed: {err}"));
            }
        }
        if !discovered_rels.is_empty() || guard.graph.has_go_embed_directives() {
            if !discovered_rels.is_empty() && guard.graph.refresh_pytest_config(&guard.workspace) {
                *self
                    .watcher_testpaths
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = guard.graph.testpaths.clone();
                self.has_go_embeds
                    .store(guard.graph.has_go_embed_directives(), Ordering::Release);
                let new_tp_dirs: BTreeSet<PathBuf> = collect_testpath_watch_dirs(
                    &guard.workspace.root,
                    &guard.graph.testpaths,
                    &guard.spec.policy,
                )
                .into_iter()
                .filter(|d| !guard.watched_dirs.contains(d))
                .collect();
                if !new_tp_dirs.is_empty() {
                    self.add_watch_dirs_locked(guard, new_tp_dirs);
                    if let Ok(inv_tp) = guard.workspace.files_limited(&guard.spec.policy) {
                        for oversized in &inv_tp.skipped_oversized_paths {
                            if guard.unreadable_files.insert(oversized.clone()) {
                                guard.generation = guard.generation.saturating_add(1);
                            }
                        }
                        if inv_tp.skipped_oversized_files > inv_tp.skipped_oversized_paths.len()
                            || inv_tp.skipped_unreadable_dirs > 0
                        {
                            guard.inventory_incomplete = true;
                        }
                        for rel_path in &inv_tp.files {
                            let rel_str = rel_path.to_string_lossy().replace('\\', "/");
                            let full = guard.workspace.root.join(rel_path);
                            if !guard.graph.files.contains_key(&rel_str)
                                && let Some(stamp) = file_stamp(&full)
                            {
                                match guard.workspace.read_known_with_limit(
                                    rel_path,
                                    guard.spec.policy.max_file_bytes,
                                ) {
                                    Ok(bytes) => {
                                        guard.unreadable_files.remove(&rel_str);
                                        guard.graph.insert_file_bytes_with_stamp(
                                            &rel_str,
                                            &bytes,
                                            Some(stamp),
                                        );
                                        discovered_rels.push(rel_str);
                                    }
                                    Err(_) => {
                                        if guard.unreadable_files.insert(rel_str) {
                                            guard.generation = guard.generation.saturating_add(1);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let prev_keys: BTreeSet<String> = guard.graph.files.keys().cloned().collect();
            guard
                .graph
                .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            self.has_go_embeds
                .store(guard.graph.has_go_embed_directives(), Ordering::Release);
            let embed_watch_dirs: BTreeSet<PathBuf> = guard
                .graph
                .go_embed_watch_dirs
                .iter()
                .map(|d| guard.workspace.root.join(d))
                .filter(|d| d.is_dir() && !guard.watched_dirs.contains(d))
                .collect();
            if !embed_watch_dirs.is_empty() {
                self.add_watch_dirs_locked(guard, embed_watch_dirs);
                // Rescan once after installing watches so files created during the race window
                // are captured.
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            }
            Self::refresh_required_watches_flag(guard);
            for k in guard.graph.files.keys() {
                if !prev_keys.contains(k) {
                    discovered_rels.push(k.clone());
                }
            }
            if !discovered_rels.is_empty() {
                guard.graph.recompute_edges();
                guard.generation = guard.generation.saturating_add(1);
                if guard.spec.policy.background_enabled {
                    let affected = guard.graph.affected_tests(&discovered_rels);
                    Self::enqueue_affected_locked(guard, affected);
                    self.condvar.notify_all();
                }
            }
        }
        let live_struct_changed = Self::reconcile_live_files_locked(guard, None);
        if live_struct_changed {
            // A modified existing file (e.g. Go file gaining `//go:embed dist` or `#cgo -I`, or
            // `pytest.ini` adding a new hidden/empty `testpaths` dir) may have introduced new
            // required directories during reconciliation: discover them, refresh watcher
            // admission, install watches, and rescan once.
            *self
                .watcher_testpaths
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = guard.graph.testpaths.clone();
            let prev_keys: BTreeSet<String> = guard.graph.files.keys().cloned().collect();
            guard
                .graph
                .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            self.has_go_embeds
                .store(guard.graph.has_go_embed_directives(), Ordering::Release);
            let mut late_dirs: BTreeSet<PathBuf> = guard
                .graph
                .go_embed_watch_dirs
                .iter()
                .map(|d| guard.workspace.root.join(d))
                .filter(|d| d.is_dir() && !guard.watched_dirs.contains(d))
                .collect();
            for tp_dir in collect_testpath_watch_dirs(
                &guard.workspace.root,
                &guard.graph.testpaths,
                &guard.spec.policy,
            ) {
                if !guard.watched_dirs.contains(&tp_dir) {
                    late_dirs.insert(tp_dir);
                }
            }
            if !late_dirs.is_empty() {
                self.add_watch_dirs_locked(guard, late_dirs);
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                // Re-scan newly watched `testpaths` directories via `files_limited` so files
                // inside a newly configured hidden `testpaths` directory are indexed immediately.
                if let Ok(inv2) = guard.workspace.files_limited(&guard.spec.policy) {
                    for oversized in &inv2.skipped_oversized_paths {
                        if guard.unreadable_files.insert(oversized.clone()) {
                            guard.generation = guard.generation.saturating_add(1);
                        }
                    }
                    if inv2.skipped_oversized_files > inv2.skipped_oversized_paths.len() {
                        guard.inventory_incomplete = true;
                    }
                    for rel_path in &inv2.files {
                        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
                        let full = guard.workspace.root.join(rel_path);
                        if !guard.graph.files.contains_key(&rel_str)
                            && let Some(stamp) = file_stamp(&full)
                        {
                            match guard
                                .workspace
                                .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
                            {
                                Ok(bytes) => {
                                    guard.unreadable_files.remove(&rel_str);
                                    guard.graph.insert_file_bytes_with_stamp(
                                        &rel_str,
                                        &bytes,
                                        Some(stamp),
                                    );
                                }
                                Err(_) => {
                                    if guard.unreadable_files.insert(rel_str) {
                                        guard.generation = guard.generation.saturating_add(1);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Self::refresh_required_watches_flag(guard);
            if guard.graph.files.len() != prev_keys.len() {
                guard.graph.recompute_edges();
                guard.generation = guard.generation.saturating_add(1);
            }
        }
        guard.last_safety_scan = Instant::now();
    }

    pub(super) fn maybe_safety_reconcile_locked(&self, guard: &mut RootState) {
        if guard.last_safety_scan.elapsed() >= Duration::from_secs(300) {
            self.prune_missing_watch_dirs_locked(guard);
            Self::reconcile_live_files_locked(guard, None);
            guard.last_safety_scan = Instant::now();
        }
    }

    /// Returns all environment variable names statically referenced in the workspace graph plus
    /// whether any file performs dynamic environment lookups (reconciling live edits first so
    /// newly added env references are observed before deciding daemon vs local execution).
    pub fn env_sensitivity_summary(&self) -> (BTreeSet<String>, bool) {
        let mut guard = self.lock_state();
        self.drain_fs_events_locked(&mut guard);
        Self::reconcile_live_files_locked(&mut guard, None);
        let mut vars = BTreeSet::new();
        let mut has_dynamic = !guard.graph.untrackable_tests.is_empty();
        for node in guard.graph.files.values() {
            for v in &node.parsed.env_vars_referenced {
                vars.insert(v.clone());
            }
            if !has_dynamic
                && node.parsed.defined_symbols.iter().any(|s| {
                    s == "py_dynamic_env"
                        || s == "fe_dynamic_env"
                        || s == "go_dynamic_env"
                        || s == "rust_dynamic_env"
                })
            {
                has_dynamic = true;
            }
        }
        (vars, has_dynamic)
    }

    /// Pre-warms external toolchain/runtime version probes (`go version`, `rustc -vV`, `node -v`,
    /// `python` runtime identity) completely OUTSIDE the `RootState` mutex so slow subprocess
    /// probes (up to 5s each) never block concurrent workspace queries or cancellations.
    /// Computes all four environment fingerprints (Rust/Cargo, Frontend/dotenv/npm-link, Go/go.work,
    /// and Python runtime/plugins) completely OUTSIDE the `RootState` mutex, then re-acquires the
    /// lock to compare directly against `guard.graph`'s stored hashes and apply any updates.
    /// This avoids BOTH stale fingerprints from pre-warmed caches AND holding the root mutex
    /// across slow subprocess probes or dotenv/site-packages filesystem scans.
    pub(super) fn warm_environment_probes_unlocked(&self) {
        // Serialize concurrent env probes outside `RootState` mutex so an older probe finishing
        // last can never overwrite a newer probe's freshly observed external config/toolchain hash.
        let _probe_guard = self
            .env_probe_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (
            spec,
            workspace,
            has_rs,
            has_fe,
            has_go,
            has_py,
            fe_dirs,
            go_dirs,
            rs_tc_dirs,
            cargo_cfg_cands,
        ) = {
            let guard = self.lock_state();
            let has_rs = guard.graph.files.keys().any(|p| {
                p.ends_with(".rs") || p == "Cargo.toml" || p.ends_with("/Cargo.toml")
            });
            let has_fe = guard
                .graph
                .files
                .keys()
                .any(|p| crate::ast_graph::is_frontend_source_path(p));
            let has_go = guard.graph.files.keys().any(|p| {
                p.ends_with(".go") || p == "go.mod" || p.ends_with("/go.mod")
            });
            let has_py = !guard.spec.pytest_cmd.is_empty()
                || guard.graph.files.keys().any(|p| p.ends_with(".py"));
            let fe_dirs = if has_fe {
                collect_frontend_env_dirs(&guard.spec, Some(&guard.graph))
            } else {
                BTreeSet::new()
            };
            let go_dirs = if has_go {
                collect_go_module_dirs(&guard.spec, Some(&guard.graph))
            } else {
                BTreeSet::new()
            };
            let rs_tc_dirs = if has_rs {
                rust_toolchain_dirs_for_graph(&guard.spec.root, Some(&guard.graph))
            } else {
                Vec::new()
            };
            let cargo_cfg_cands = cargo_config_candidates_for_spec(&guard.spec, Some(&guard.graph));
            (
                guard.spec.clone(),
                guard.workspace.clone(),
                has_rs,
                has_fe,
                has_go,
                has_py,
                fe_dirs,
                go_dirs,
                rs_tc_dirs,
                cargo_cfg_cands,
            )
        };
        let has_ext_now = cargo_cfg_cands
            .iter()
            .any(|cand| cargo_config_traverses_symlink(&spec.root, cand))
            || !effective_external_cargo_configs(&spec).is_empty();
        let non_py_hash = environment_fingerprint_from_parts(
            &spec,
            false,
            has_rs,
            &rs_tc_dirs,
            &cargo_cfg_cands,
        );
        let env_hash = if has_py {
            environment_fingerprint_from_parts(
                &spec,
                true,
                has_rs,
                &rs_tc_dirs,
                &cargo_cfg_cands,
            )
        } else {
            non_py_hash.clone()
        };
        let (fe_hash, fe_uncacheable) = if has_fe {
            frontend_environment_fingerprint_and_cacheability(&spec, &fe_dirs, Some(&workspace))
        } else {
            (String::new(), false)
        };
        let py_uncacheable = if has_py {
            let (ident, plugs) = python_runtime_identity(&spec);
            ident.contains(PY_ENV_UNCACHEABLE_SENTINEL)
                || plugs.iter().any(|p| p.contains(PY_ENV_UNCACHEABLE_SENTINEL))
        } else {
            false
        };
        let go_hash = if has_go {
            go_environment_fingerprint_for_dirs(&spec, &go_dirs)
        } else {
            String::new()
        };

        let mut guard = self.lock_state();
        let state = &mut *guard;
        state.had_external_cargo_cfg = has_ext_now;
        state.fe_env_uncacheable = fe_uncacheable;
        state.py_env_uncacheable = py_uncacheable;
        let part = state.env_partition.as_deref();
        let env_hash = partition_env_hash(&env_hash, part);
        let non_py_hash = partition_env_hash(&non_py_hash, part);
        let fe_hash = partition_env_hash(&fe_hash, part);
        let go_hash = partition_env_hash(&go_hash, part);
        if non_py_hash != state.graph.non_python_environment_hash
            || env_hash != state.graph.environment_hash
            || fe_hash != state.graph.frontend_environment_hash
            || go_hash != state.graph.go_environment_hash
        {
            let go_changed = go_hash != state.graph.go_environment_hash;
            state
                .graph
                .apply_all_environment_hashes(env_hash, non_py_hash, fe_hash, go_hash);
            if has_go && go_changed {
                state
                    .graph
                    .discover_referenced_data_files(&state.workspace, &state.spec.policy);
                state.graph.recompute_edges();
            }
            state.last_environment_check = Instant::now();
        }
    }

    /// Foreground callers skip 300s full-workspace scans while the background worker handles them,
    /// but still perform an overdue scan if the background worker has been occupied for >= 600s.
    /// Environment fingerprinting is performed unlocked via `warm_environment_probes_unlocked`.
    pub(super) fn maybe_foreground_safety_reconcile_locked(&self, guard: &mut RootState) {
        if !guard.spec.policy.background_enabled
            || guard.last_safety_scan.elapsed() >= Duration::from_secs(600)
            || guard.last_environment_check.elapsed() >= Duration::from_secs(600)
        {
            self.maybe_safety_reconcile_locked(guard);
        }
    }

    /// Reconciles live disk contents for candidate files before serving a test_run or affected query.
    /// Returns `true` if the dependency graph structure changed.
    pub(super) fn reconcile_live_files_locked(guard: &mut RootState, target_tests: Option<&[String]>) -> bool {
        Self::reconcile_live_files_inner_locked(guard, target_tests, false)
    }

    pub(super) fn reconcile_live_files_inner_locked(
        guard: &mut RootState,
        target_tests: Option<&[String]>,
        skip_cargo_lock: bool,
    ) -> bool {
        let mut files_to_check: Vec<String> = if let Some(tests) = target_tests {
            let num_indexed = guard.graph.file_index_to_path.len();
            if num_indexed == guard.graph.files.len() && num_indexed > 0 {
                let mut seen = vec![false; num_indexed];
                let mut extra = BTreeSet::new();
                for t in tests {
                    if let Some(ids) = guard.graph.transitive_dep_ids_for_test.get(t) {
                        for &id in ids {
                            if let Some(slot) = seen.get_mut(id as usize) {
                                *slot = true;
                            }
                        }
                    } else {
                        extra.insert(t.clone());
                    }
                }
                let mut seen_extra: std::collections::HashSet<String> = seen
                    .into_iter()
                    .enumerate()
                    .filter(|&(_, flag)| flag)
                    .map(|(idx, _)| guard.graph.file_index_to_path[idx].clone())
                    .collect();
                for cfg in guard.graph.config_hashes.keys() {
                    seen_extra.insert(cfg.clone());
                }
                for item in extra {
                    seen_extra.insert(item);
                }
                seen_extra.into_iter().collect()
            } else {
                let mut set = BTreeSet::new();
                for t in tests {
                    set.insert(t.clone());
                    for dep in guard.graph.transitive_deps_for(t) {
                        set.insert(dep);
                    }
                }
                for cfg in guard.graph.config_hashes.keys() {
                    set.insert(cfg.clone());
                }
                set.into_iter().collect()
            }
        } else {
            guard.graph.files.keys().cloned().collect()
        };
        if !guard.unreadable_files.is_empty() {
            for unread in &guard.unreadable_files {
                if !files_to_check.iter().any(|p| p == unread) {
                    files_to_check.push(unread.clone());
                }
            }
        }
        // Check whether any previously absent referenced fixture (e.g. `existsSync("fixtures/ready.bin")`)
        // now exists on disk (as a regular file, directory, or symlink): if so, re-run referenced
        // data discovery and edge recomputation so newly created fixtures invalidate cached tests.
        let mut missing_ref_appeared = false;
        if !guard.graph.missing_data_ref_candidates.is_empty() {
            for cand in &guard.graph.missing_data_ref_candidates {
                let full = guard.workspace.root.join(cand);
                if full.exists() || std::fs::symlink_metadata(&full).is_ok() {
                    missing_ref_appeared = true;
                    break;
                }
            }
        }
        if missing_ref_appeared {
            guard
                .graph
                .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            guard.graph.recompute_edges();
            guard.generation = guard.generation.saturating_add(1);
        }
        if skip_cargo_lock {
            files_to_check.retain(|p| p != "Cargo.lock" && !p.ends_with("/Cargo.lock"));
        }

        let mut changed: Vec<String> = Vec::new();
        let mut structure_changed = false;
        let candidates_with_stamps: Vec<(String, Option<crate::ast_graph::FileStamp>)> =
            if files_to_check.len() >= 256 {
            let root_ref = &guard.workspace.root;
            let indexed_ref = &guard.graph.files;
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = files_to_check.len().div_ceil(workers).max(1);
            std::thread::scope(|scope| {
                let handles: Vec<_> = files_to_check
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut diff = Vec::new();
                            for rel_str in chunk {
                                let full = root_ref.join(Path::new(rel_str));
                                let live_stamp = file_stamp(&full);
                                let unchanged = live_stamp.is_some()
                                    && indexed_ref
                                        .get(rel_str)
                                        .and_then(|n| n.stamp)
                                        .is_some_and(|s| Some(s) == live_stamp);
                                if !unchanged {
                                    diff.push((rel_str.clone(), live_stamp));
                                }
                            }
                            diff
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok())
                    .flatten()
                    .collect()
            })
        } else {
            let mut diff = Vec::new();
            for rel_str in files_to_check {
                let full = guard.workspace.root.join(Path::new(&rel_str));
                let live_stamp = file_stamp(&full);
                let unchanged = live_stamp.is_some()
                    && guard
                        .graph
                        .files
                        .get(&rel_str)
                        .and_then(|n| n.stamp)
                        .is_some_and(|s| Some(s) == live_stamp);
                if !unchanged {
                    diff.push((rel_str, live_stamp));
                }
            }
            diff
        };

        for (rel_str, live_stamp_opt) in candidates_with_stamps {
            let rel_path = Path::new(&rel_str);
            let full = guard.workspace.root.join(rel_path);
            if let Some(live_stamp) = live_stamp_opt {
                match guard
                    .workspace
                    .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
                {
                    Ok(bytes) => {
                        guard.unreadable_files.remove(&rel_str);
                        let live_sha = sha256_hex(&bytes);
                        let indexed_sha = guard.graph.files.get(&rel_str).map(|n| n.sha256.as_str());
                        if indexed_sha != Some(live_sha.as_str()) {
                            if guard.graph.insert_file_bytes_with_stamp(
                                &rel_str,
                                &bytes,
                                Some(live_stamp),
                            ) {
                                structure_changed = true;
                            }
                            changed.push(rel_str);
                        } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                            node.stamp = Some(live_stamp);
                        }
                    }
                    Err(_) => {
                        // A modified indexed file grew beyond `max_file_bytes` or became
                        // unreadable: track per-file so its stale old hash can NEVER serve a
                        // pass, and auto-clear as soon as the file is restored within limits.
                        if guard.unreadable_files.insert(rel_str.clone()) {
                            guard.generation = guard.generation.saturating_add(1);
                        }
                        if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                            node.stamp = None;
                        }
                    }
                }
            } else if !full.exists() && std::fs::symlink_metadata(&full).is_err() {
                guard.unreadable_files.remove(&rel_str);
                if guard.graph.remove_file(&rel_str) {
                    guard.ruff_cache.remove(&rel_str);
                    guard.pending_ruff_files.remove(&rel_str);
                    changed.push(rel_str);
                    structure_changed = true;
                }
            } else if std::fs::symlink_metadata(&full).is_ok() {
                // Indexed regular file was replaced with a symlink or non-regular entry: fail
                // closed per-file so its stale pre-replacement content hash can NEVER serve a pass.
                if guard.unreadable_files.insert(rel_str.clone()) {
                    guard.generation = guard.generation.saturating_add(1);
                }
                if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                    node.stamp = None;
                }
            }
        }

        if !changed.is_empty() {
            let pytest_cfg_touched = changed.iter().any(|rel| {
                matches!(
                    Path::new(rel).file_name().and_then(|n| n.to_str()),
                    Some(
                        "pytest.toml"
                            | ".pytest.toml"
                            | "pytest.ini"
                            | ".pytest.ini"
                            | "pyproject.toml"
                            | "setup.cfg"
                            | "tox.ini"
                    )
                )
            });
            if pytest_cfg_touched && guard.graph.refresh_pytest_config(&guard.workspace) {
                structure_changed = true;
            }
            if guard.graph.reconcile_testpaths_fallback(&guard.workspace.root) {
                structure_changed = true;
            }
            if structure_changed {
                invalidate_pythonpath_cache(&guard.workspace.root);
            }
            guard.generation = guard.generation.saturating_add(1);
            Self::invalidate_ruff_for_changed_locked(guard, &changed);
            if structure_changed {
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                guard.graph.recompute_edges();
            } else {
                guard.graph.recompute_hashes_for_changed(&changed);
            }
            if guard.spec.policy.background_enabled {
                let affected = guard.graph.affected_tests(&changed);
                Self::enqueue_affected_locked(guard, affected);
            }
        }
        guard.graph.refresh_referenced_env_hashes();
        if guard
            .graph
            .files
            .keys()
            .any(|p| crate::ast_graph::is_frontend_source_path(p))
        {
            guard.graph.refresh_untrackable_tests();
        }
        structure_changed
    }

    /// Captures a SHA-256 fingerprint of every non-`Cargo.lock` dependency, config file, and
    /// environment hash for each Rust test in `executed_files` so post-execution validation can
    /// distinguish brand-new `Cargo.lock` creation from concurrent edits to source/config/env.
    /// Computes a live fingerprint covering ALL indexed workspace files (Rust sources, Cargo
    /// manifests/locks, configs, AND `include_str!`/`include_bytes!`/referenced fixture files)
    /// plus `raw_embedded_files`, external Cargo/Rust environment hash, and unreadable files so
    /// ANY source or fixture edit during a custom-harness or doctest check triggers
    /// `SOURCE_CHANGED_DURING_TEST`.
    pub(super) fn snapshot_rust_crate_fingerprint(guard: &RootState, crate_dir: &str) -> String {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"crate=");
        buf.extend_from_slice(crate_dir.as_bytes());
        buf.extend_from_slice(b"\0env=");
        buf.extend_from_slice(guard.graph.non_python_environment_hash.as_bytes());
        buf.push(0);
        for (path, node) in &guard.graph.files {
            buf.extend_from_slice(path.as_bytes());
            buf.push(b'=');
            buf.extend_from_slice(node.sha256.as_bytes());
            buf.push(0);
        }
        for path in &guard.graph.raw_embedded_files {
            buf.extend_from_slice(b"raw:");
            buf.extend_from_slice(path.as_bytes());
            buf.push(0);
        }
        for u in &guard.unreadable_files {
            buf.extend_from_slice(b"unreadable:");
            buf.extend_from_slice(u.as_bytes());
            buf.push(0);
        }
        sha256_hex(&buf)
    }

    pub(super) fn snapshot_non_lock_dep_fingerprints(
        guard: &RootState,
        executed_files: &[String],
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for tf in executed_files {
            if !tf.ends_with(".rs") {
                continue;
            }
            let mut buf = Vec::new();
            buf.extend_from_slice(guard.graph.environment_hash.as_bytes());
            buf.push(0);
            buf.extend_from_slice(guard.graph.non_python_environment_hash.as_bytes());
            buf.push(0);
            for (cfg_path, cfg_sha) in &guard.graph.config_hashes {
                if cfg_path == "Cargo.lock" || cfg_path.ends_with("/Cargo.lock") {
                    continue;
                }
                // Only include config files relevant to this Rust test (ignoring unrelated
                // Python/Frontend/Go config edits while a Rust test is running).
                let fname = Path::new(cfg_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if !(crate::workspace::RUST_CONFIG_FILES.contains(&fname)
                    || crate::workspace::is_cargo_dot_config_rel(Path::new(cfg_path)))
                {
                    continue;
                }
                buf.extend_from_slice(cfg_path.as_bytes());
                buf.push(b'=');
                buf.extend_from_slice(cfg_sha.as_bytes());
                buf.push(b'\n');
            }
            if let Some(ids) = guard.graph.transitive_dep_ids_for_test.get(tf) {
                for &id in ids {
                    if let Some(dep_path) = guard.graph.file_index_to_path.get(id as usize) {
                        if dep_path == "Cargo.lock" || dep_path.ends_with("/Cargo.lock") {
                            continue;
                        }
                        if let Some(node) = guard.graph.files.get(dep_path) {
                            buf.extend_from_slice(dep_path.as_bytes());
                            buf.push(b':');
                            buf.extend_from_slice(node.sha256.as_bytes());
                            buf.push(b'\n');
                        }
                    }
                }
            } else if let Some(node) = guard.graph.files.get(tf) {
                buf.extend_from_slice(tf.as_bytes());
                buf.push(b':');
                buf.extend_from_slice(node.sha256.as_bytes());
                buf.push(b'\n');
            }
            out.insert(tf.clone(), sha256_hex(&buf));
        }
        out
    }

    pub(super) fn snapshot_cargo_lock_existence(
        guard: &RootState,
        executed_files: &[String],
    ) -> BTreeMap<String, Option<String>> {
        let mut out = BTreeMap::new();
        if !executed_files.iter().any(|f| f.ends_with(".rs")) {
            return out;
        }
        let mut lock_candidates = BTreeSet::new();
        lock_candidates.insert("Cargo.lock".to_string());
        for tf in executed_files {
            if tf.ends_with(".rs") {
                // Walk ALL ancestor directories from the test's crate dir up to workspace root
                // so intermediate Cargo workspace lockfiles (e.g. `services/api/Cargo.lock`)
                // are also snapshotted.
                let mut cur = crate::ast_graph::rust_crate_dir(tf);
                while !cur.is_empty() {
                    lock_candidates.insert(format!("{cur}/Cargo.lock"));
                    cur = cur.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_string());
                }
            }
        }
        let limit = guard.spec.policy.max_file_bytes.max(8 * 1024 * 1024);
        for lock_rel in lock_candidates {
            // Snapshot ACTUAL on-disk lockfile state (not just `guard.graph.files`) so an
            // ignored or out-of-scope existing `Cargo.lock` on disk is never mistaken for a
            // brand-new lockfile created by `cargo test`.
            let rel_path = Path::new(&lock_rel);
            let full = guard.workspace.root.join(rel_path);
            let disk_sha = if file_stamp(&full).is_some() {
                match guard.workspace.read_known_with_limit(rel_path, limit) {
                    Ok(bytes) => Some(sha256_hex(&bytes)),
                    Err(_) => Some("__tctx_unreadable_lock__".to_string()),
                }
            } else if std::fs::symlink_metadata(&full).is_ok() {
                Some("__tctx_symlink_lock__".to_string())
            } else {
                None
            };
            out.insert(lock_rel, disk_sha);
        }
        out
    }

    pub(super) fn sync_cargo_locks_after_exec_locked(
        guard: &mut RootState,
        executed_files: &[String],
        before_exec_shas: &BTreeMap<String, Option<String>>,
        non_lock_unstable: &BTreeSet<String>,
        records: &mut [CachedTestRecord],
    ) -> bool {
        if !executed_files.iter().any(|f| f.ends_with(".rs")) {
            return false;
        }
        let mut lock_changed = Vec::new();
        let mut created_new_lock = false;
        let mut modified_existing_lock = false;
        let mut structure_changed = false;
        let limit = guard.spec.policy.max_file_bytes.max(8 * 1024 * 1024);
        for (lock_rel, before_sha) in before_exec_shas {
            let rel_path = Path::new(lock_rel);
            let full = guard.workspace.root.join(rel_path);
            let Some(live_stamp) = file_stamp(&full) else {
                if before_sha.is_some() {
                    // An existing `Cargo.lock` was deleted or replaced by a symlink during run!
                    modified_existing_lock = true;
                    if !full.exists() && std::fs::symlink_metadata(&full).is_err() {
                        if guard.graph.remove_file(lock_rel) {
                            structure_changed = true;
                            lock_changed.push(lock_rel.clone());
                        }
                    } else if guard.unreadable_files.insert(lock_rel.clone()) {
                        guard.generation = guard.generation.saturating_add(1);
                    }
                } else if std::fs::symlink_metadata(&full).is_ok() {
                    modified_existing_lock = true;
                    if guard.unreadable_files.insert(lock_rel.clone()) {
                        guard.generation = guard.generation.saturating_add(1);
                    }
                }
                continue;
            };
            let Ok(bytes) = guard.workspace.read_known_with_limit(rel_path, limit) else {
                // `Cargo.lock` became unreadable or oversized during execution: fail closed.
                modified_existing_lock = true;
                if guard.unreadable_files.insert(lock_rel.clone()) {
                    guard.generation = guard.generation.saturating_add(1);
                }
                if let Some(node) = guard.graph.files.get_mut(lock_rel) {
                    node.stamp = None;
                }
                continue;
            };
            guard.unreadable_files.remove(lock_rel);
            let live_sha = sha256_hex(&bytes);
            if before_sha.as_deref() != Some(live_sha.as_str()) {
                if before_sha.is_some() {
                    modified_existing_lock = true;
                } else {
                    created_new_lock = true;
                }
            }
            let indexed_sha = guard.graph.files.get(lock_rel).map(|n| n.sha256.as_str());
            if indexed_sha != Some(live_sha.as_str()) {
                if guard.graph.insert_file_bytes_with_stamp(
                    lock_rel,
                    &bytes,
                    Some(live_stamp),
                ) {
                    structure_changed = true;
                }
                lock_changed.push(lock_rel.clone());
            } else if let Some(node) = guard.graph.files.get_mut(lock_rel) {
                node.stamp = Some(live_stamp);
            }
        }
        if !lock_changed.is_empty() {
            guard.generation = guard.generation.saturating_add(1);
            if structure_changed {
                guard.graph.recompute_edges();
            } else {
                guard.graph.recompute_hashes_for_changed(&lock_changed);
            }
        }
        // Only relabel executed Rust records that had NO non-lock dependency edits when
        // `cargo test` created a brand-new `Cargo.lock` that did not exist before execution.
        if created_new_lock && !modified_existing_lock {
            for rec in records {
                if rec.test_file.ends_with(".rs")
                    && !non_lock_unstable.contains(&rec.test_file)
                    && let Some(dh) = guard.graph.dep_hash_for_test.get(&rec.test_file)
                {
                    rec.dep_hash.clone_from(dh);
                }
            }
        }
        modified_existing_lock
    }

}

