use super::*;

impl DependencyGraph {
    pub fn build(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let mut graph = Self::build_unindexed(workspace, policy)?;
        let need_save = !graph.has_persisted_edges();
        graph.recompute_edges_or_reuse_persisted();
        if need_save {
            save_persisted_ast_snapshot(&workspace.root, &graph);
        }
        Ok(graph)
    }

    pub fn has_persisted_edges(&self) -> bool {
        self.persisted_edges_valid
            && !self.file_index_to_path.is_empty()
            && self.file_index_to_path.len() == self.files.len()
            && self.persisted_runtime_deps.as_ref() == Some(&self.runtime_deps_for_test)
    }

    pub fn recompute_edges_or_reuse_persisted(&mut self) {
        if self.has_persisted_edges() {
            self.tsconfig_extends_rev = self.compute_tsconfig_extends_rev();
            let frontend_index = FrontendModuleIndex::build(&self.files);
            self.rebuild_barrel_import_maps(&frontend_index);
            self.last_side_effectful_barrels = frontend_index.side_effectful_barrels.clone();
            self.raw_embedded_files = collect_raw_embedded_files(&self.files);
            self.raw_embedded_files
                .extend(frontend_index.collect_raw_imported_files(&self.files));
            self.raw_embedded_files
                .extend(collect_rust_custom_path_mod_files(&self.files));
            self.last_env_ref_digest = self.compute_env_ref_digest();
            self.recompute_all_test_hashes();
            if self.cached_conventions.is_none() {
                self.cached_conventions = Some(self.compute_test_conventions_summary());
            }
        } else {
            self.recompute_edges();
        }
    }

    pub fn build_unindexed(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let inventory = workspace.files_limited(policy)?;
        let mut graph = Self::new();
        for oversized in &inventory.skipped_oversized_paths {
            graph.unreadable_inventory_files.insert(oversized.clone());
        }
        // Only set the global `skipped_oversized_inventory` flag when oversized files exceed
        // our per-file tracking sample cap; otherwise `unreadable_inventory_files` tracks each
        // file individually so caching fails closed AND auto-recovers when the file shrinks/is deleted.
        graph.skipped_oversized_inventory =
            inventory.skipped_oversized_files > inventory.skipped_oversized_paths.len()
                || inventory.skipped_unreadable_dirs > 0;
        graph.test_file_patterns = workspace.pytest_file_patterns();
        graph.test_function_patterns = workspace.pytest_function_patterns();
        graph.testpaths = workspace.pytest_testpaths();
        graph.testpaths_fallback_active = !graph.testpaths.is_empty()
            && !testpaths_match_any_on_disk(&workspace.root, &graph.testpaths);
        let effective_testpaths: &[String] = if graph.testpaths_fallback_active {
            &[]
        } else {
            &graph.testpaths
        };
        let mut total_bytes = 0usize;

        let (persisted, persisted_edges) = load_persisted_ast_nodes(
            &workspace.root,
            &graph.test_file_patterns,
            &graph.test_function_patterns,
            &graph.testpaths,
            graph.testpaths_fallback_active,
        );
        let mut cache_reused = 0usize;
        let mut parsed_fresh = 0usize;

        if inventory.files.len() >= 64 {
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = inventory.files.len().div_ceil(workers);
            let patterns = &graph.test_file_patterns;
            let fn_patterns = &graph.test_function_patterns;
            let testpaths = effective_testpaths;
            let persisted_ref = &persisted;
            let stamps_ref = &inventory.file_stamps;
            let max_file_bytes = policy.max_file_bytes;
            let max_source_bytes = policy.max_source_bytes;
            let shared_source_bytes = std::sync::atomic::AtomicUsize::new(0);
            let source_limit_hit = std::sync::atomic::AtomicBool::new(false);
            let shared_bytes_ref = &shared_source_bytes;
            let limit_hit_ref = &source_limit_hit;
            std::thread::scope(|scope| -> Result<()> {
                let handles: Vec<_> = inventory
                    .files
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            use std::sync::atomic::Ordering;
                            let mut out = Vec::with_capacity(chunk.len());
                            let mut failed_rels = Vec::new();
                            for rel_path in chunk {
                                if limit_hit_ref.load(Ordering::Acquire) {
                                    break;
                                }
                                let rel_str = normalize_rel_str(rel_path);
                                let stamp_opt = stamps_ref
                                    .get(rel_path)
                                    .copied()
                                    .or_else(|| file_stamp(&workspace.root.join(rel_path)));
                                let Some(stamp) = stamp_opt else {
                                    if std::fs::symlink_metadata(workspace.root.join(rel_path))
                                        .is_ok()
                                    {
                                        failed_rels.push(rel_str);
                                    }
                                    continue;
                                };
                                let expected_len = usize::try_from(stamp.size).unwrap_or(usize::MAX);
                                if expected_len > max_file_bytes {
                                    failed_rels.push(rel_str);
                                    continue;
                                }
                                if let Some(cached_node) = persisted_ref.get(&rel_str)
                                    && cached_node.stamp == Some(stamp)
                                {
                                    let next_total = shared_bytes_ref
                                        .fetch_add(expected_len, Ordering::AcqRel)
                                        .saturating_add(expected_len);
                                    if next_total > max_source_bytes {
                                        limit_hit_ref.store(true, Ordering::Release);
                                        break;
                                    }
                                    out.push((cached_node.clone(), expected_len, true));
                                    continue;
                                }
                                let Ok(bytes) =
                                    workspace.read_known_with_limit(rel_path, max_file_bytes)
                                else {
                                    failed_rels.push(rel_str);
                                    continue;
                                };
                                let actual_len = bytes.len();
                                let next_total = shared_bytes_ref
                                    .fetch_add(actual_len, Ordering::AcqRel)
                                    .saturating_add(actual_len);
                                if next_total > max_source_bytes {
                                    limit_hit_ref.store(true, Ordering::Release);
                                    break;
                                }
                                let node = build_file_node_from_bytes(
                                    &rel_str,
                                    &bytes,
                                    Some(stamp),
                                    patterns,
                                    fn_patterns,
                                    testpaths,
                                );
                                out.push((node, actual_len, false));
                            }
                            (out, failed_rels)
                        })
                    })
                    .collect();
                for handle in handles {
                    let (chunk, failed_rels) = handle
                        .join()
                        .map_err(|_| invalid("AST build worker thread panicked"))?;
                    for failed in failed_rels {
                        graph.unreadable_inventory_files.insert(failed);
                    }
                    if limit_hit_ref.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(
                            invalid("workspace exceeds max_source_bytes policy limit").into()
                        );
                    }
                    for (node, byte_len, was_cached) in chunk {
                        total_bytes = total_bytes.saturating_add(byte_len);
                        if total_bytes > policy.max_source_bytes {
                            return Err(
                                invalid("workspace exceeds max_source_bytes policy limit").into()
                            );
                        }
                        if was_cached {
                            cache_reused += 1;
                        } else {
                            parsed_fresh += 1;
                        }
                        if node.is_config {
                            graph
                                .config_hashes
                                .insert(node.path.clone(), node.sha256.clone());
                        }
                        graph.files.insert(node.path.clone(), node);
                    }
                }
                Ok(())
            })?;
        } else {
            for rel_path in &inventory.files {
                let stamp = inventory
                    .file_stamps
                    .get(rel_path)
                    .copied()
                    .or_else(|| file_stamp(&workspace.root.join(rel_path)));
                let rel_str = normalize_rel_str(rel_path);
                if let Some(st) = stamp
                    && let Some(cached_node) = persisted.get(&rel_str)
                    && cached_node.stamp == Some(st)
                    && usize::try_from(st.size).unwrap_or(usize::MAX) <= policy.max_file_bytes
                {
                    let byte_len = usize::try_from(st.size).unwrap_or(0);
                    total_bytes = total_bytes.saturating_add(byte_len);
                    if total_bytes > policy.max_source_bytes {
                        return Err(
                            invalid("workspace exceeds max_source_bytes policy limit").into()
                        );
                    }
                    if cached_node.is_config {
                        graph
                            .config_hashes
                            .insert(cached_node.path.clone(), cached_node.sha256.clone());
                    }
                    graph.files.insert(rel_str, cached_node.clone());
                    cache_reused += 1;
                    continue;
                }
                let bytes = match workspace.read_known_with_limit(rel_path, policy.max_file_bytes) {
                    Ok(b) => b,
                    Err(_) => {
                        graph.unreadable_inventory_files.insert(rel_str);
                        continue;
                    }
                };
                total_bytes = total_bytes.saturating_add(bytes.len());
                if total_bytes > policy.max_source_bytes {
                    return Err(invalid("workspace exceeds max_source_bytes policy limit").into());
                }
                graph.insert_file_bytes_with_stamp(&rel_str, &bytes, stamp);
                parsed_fresh += 1;
            }
        }
        let data_fresh = graph.discover_referenced_data_files_with_cache(workspace, policy, &persisted);
        if data_fresh == 0
            && let Some(edges) = persisted_edges
            && edges.file_count == persisted.len()
        {
            if parsed_fresh == 0
                && graph.files.len() == persisted.len()
                && edges.forward_adj.len() == graph.files.len()
                && edges.reverse_adj.len() == graph.files.len()
            {
                graph.file_index_to_path.reserve(graph.files.len());
                graph.path_to_id.reserve(graph.files.len());
                for (idx, (path, node)) in graph.files.iter().enumerate() {
                    let id = u32::try_from(idx).unwrap_or(u32::MAX);
                    graph.file_index_to_path.push(path.clone());
                    graph.path_to_id.insert(path.clone(), id);
                    if node.is_conftest {
                        graph.conftest_paths.insert(path.clone());
                    }
                    if node.is_test {
                        for sym in &node.parsed.test_symbols {
                            graph
                                .symbol_to_test_files
                                .entry(sym.clone())
                                .or_default()
                                .push(path.clone());
                        }
                    }
                }
                graph.forward_adj = edges.forward_adj;
                graph.reverse_adj = edges.reverse_adj;
                graph.conftest_for_test = edges.conftest_for_test;
                graph.transitive_dep_ids_for_test = edges.transitive_dep_ids_for_test;
                graph.local_python_roots = edges.local_python_roots;
                graph.persisted_runtime_deps = Some(edges.runtime_deps_for_test);
                graph.persisted_edges_valid = true;
            } else if parsed_fresh <= 16 && graph.files.len() == persisted.len() {
                let mut same_structure = true;
                let mut changed_paths = Vec::new();
                for (path, node) in &graph.files {
                    let Some(old) = persisted.get(path) else {
                        same_structure = false;
                        break;
                    };
                    if old.is_test != node.is_test
                        || old.is_conftest != node.is_conftest
                        || old.is_config != node.is_config
                        || old.parsed != node.parsed
                    {
                        same_structure = false;
                        break;
                    }
                    if old.sha256 != node.sha256 {
                        changed_paths.push(path.clone());
                    }
                }
                if same_structure
                    && edges.forward_adj.len() == graph.files.len()
                    && edges.reverse_adj.len() == graph.files.len()
                {
                    graph.file_index_to_path.reserve(graph.files.len());
                    graph.path_to_id.reserve(graph.files.len());
                    for (idx, (path, node)) in graph.files.iter().enumerate() {
                        let id = u32::try_from(idx).unwrap_or(u32::MAX);
                        graph.file_index_to_path.push(path.clone());
                        graph.path_to_id.insert(path.clone(), id);
                        if node.is_conftest {
                            graph.conftest_paths.insert(path.clone());
                        }
                        if node.is_test {
                            for sym in &node.parsed.test_symbols {
                                graph
                                    .symbol_to_test_files
                                    .entry(sym.clone())
                                    .or_default()
                                    .push(path.clone());
                            }
                        }
                    }
                    graph.forward_adj = edges.forward_adj;
                    graph.reverse_adj = edges.reverse_adj;
                    graph.conftest_for_test = edges.conftest_for_test;
                    graph.transitive_dep_ids_for_test = edges.transitive_dep_ids_for_test;
                    graph.local_python_roots = edges.local_python_roots;
                    graph.persisted_runtime_deps = Some(edges.runtime_deps_for_test);
                    graph.persisted_edges_valid = true;
                    let _ = changed_paths;
                }
            }
        }
        let _ = cache_reused;
        Ok(graph)
    }

}

impl DependencyGraph {
    pub fn insert_file_bytes(&mut self, rel_str: &str, bytes: &[u8]) -> bool {
        self.insert_file_bytes_with_stamp(rel_str, bytes, None)
    }

    /// Inserts or updates a file node and returns `true` if the dependency structure (AST imports/fixtures/data_refs) changed,
    /// or `false` if only the file's content hash changed.
    pub fn insert_file_bytes_with_stamp(
        &mut self,
        rel_str: &str,
        bytes: &[u8],
        stamp: Option<FileStamp>,
    ) -> bool {
        let effective_testpaths: &[String] = if self.testpaths_fallback_active {
            &[]
        } else {
            &self.testpaths
        };
        let node = build_file_node_from_bytes(
            rel_str,
            bytes,
            stamp,
            &self.test_file_patterns,
            &self.test_function_patterns,
            effective_testpaths,
        );
        self.persisted_edges_valid = false;
        let prev_had_provider = self
            .files
            .get(rel_str)
            .is_some_and(node_has_provider_or_fixture);
        if prev_had_provider || node_has_provider_or_fixture(&node) {
            self.provider_structure_dirty = true;
        }
        let prev_is_test = self.files.get(rel_str).is_some_and(|n| n.is_test);
        if prev_is_test != node.is_test {
            self.test_membership_dirty = true;
        }
        if node.is_config {
            let is_new = !self.config_hashes.contains_key(rel_str);
            self.config_hashes
                .insert(rel_str.to_string(), node.sha256.clone());
            let structure_changed = match self.files.get(rel_str) {
                Some(existing) => existing.parsed != node.parsed,
                None => true,
            };
            self.files.insert(rel_str.to_string(), node);
            return is_new || structure_changed;
        }
        let structure_changed = match self.files.get(rel_str) {
            Some(existing) => {
                existing.is_test != node.is_test
                    || existing.is_conftest != node.is_conftest
                    || existing.parsed != node.parsed
            }
            None => true,
        };
        self.files.insert(rel_str.to_string(), node);
        structure_changed
    }

    /// Fast O(affected) Merkle hash update when file contents changed but import/fixture structure did not change.
    pub fn recompute_hashes_for_changed(&mut self, changed_files: &[String]) {
        if self.file_index_to_path.len() != self.files.len() {
            self.recompute_edges();
            return;
        }
        let base_hasher = self.build_base_hasher();
        let non_py_base_hasher = self.build_non_python_base_hasher();

        let affected = self.affected_test_names(changed_files);

        let mut digest_cache: std::collections::HashMap<(u32, bool), Vec<u8>> =
            std::collections::HashMap::new();
        for test_path in affected {
            let Some(reachable_ids) = self.transitive_dep_ids_for_test.get(&test_path) else {
                continue;
            };
            let self_id = self.path_to_id.get(&test_path).copied();
            let mut hasher = self.base_hasher_for_test(
                &test_path,
                reachable_ids,
                &base_hasher,
                &non_py_base_hasher,
            );
            self.update_relevant_config_hashes(&mut hasher, &test_path, reachable_ids);
            self.update_runtime_dep_raw_hashes(&mut hasher, &test_path, reachable_ids);
            for &dep_id in reachable_ids {
                let is_self = Some(dep_id) == self_id;
                let bytes = digest_cache.entry((dep_id, is_self)).or_insert_with(|| {
                    self.file_index_to_path
                        .get(dep_id as usize)
                        .and_then(|dep_path| {
                            self.files
                                .get(dep_path)
                                .map(|dep_node| build_node_hash_bytes(dep_path, dep_node, is_self))
                        })
                        .unwrap_or_default()
                });
                hasher.update(bytes);
            }
            let dep_hash = hex_encode(&hasher.finalize());
            self.dep_hash_for_test.insert(test_path, dep_hash);
        }
    }

    pub fn refresh_referenced_env_hashes(&mut self) {
        let digest = self.compute_env_ref_digest();
        if digest == self.last_env_ref_digest {
            return;
        }
        self.last_env_ref_digest = digest;
        let env_files: Vec<String> = self
            .files
            .iter()
            .filter(|(_, node)| !node.parsed.env_vars_referenced.is_empty())
            .map(|(path, _)| path.clone())
            .collect();
        if !env_files.is_empty() {
            self.recompute_hashes_for_changed(&env_files);
        }
    }

    pub fn remove_file(&mut self, rel_str: &str) -> bool {
        self.persisted_edges_valid = false;
        let removed_cfg = self.config_hashes.remove(rel_str).is_some();
        let removed_node = self.files.remove(rel_str);
        if removed_node
            .as_ref()
            .is_some_and(node_has_provider_or_fixture)
        {
            self.provider_structure_dirty = true;
        }
        if removed_node.as_ref().is_some_and(|n| n.is_test) {
            self.test_membership_dirty = true;
        }
        removed_cfg || removed_node.is_some()
    }

    pub(super) fn reclassify_python_test_membership(&mut self) {
        let effective_testpaths: &[String] = if self.testpaths_fallback_active {
            &[]
        } else {
            &self.testpaths
        };
        for (rel_str, node) in &mut self.files {
            if !rel_str.ends_with(".py") || node.is_config {
                continue;
            }
            let file_name = Path::new(rel_str)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            node.is_test = !node.is_conftest
                && matches_test_filename(file_name, &self.test_file_patterns)
                && matches_test_path(rel_str, effective_testpaths);
        }
    }

    /// Re-evaluates whether pytest's `testpaths` empty-match fallback should be active on disk
    /// (e.g. when a configured testpath directory is created or deleted live).
    pub fn reconcile_testpaths_fallback(&mut self, root: &Path) -> bool {
        if self.testpaths.is_empty() {
            return false;
        }
        let next_fallback = !testpaths_match_any_on_disk(root, &self.testpaths);
        if next_fallback == self.testpaths_fallback_active {
            return false;
        }
        self.testpaths_fallback_active = next_fallback;
        self.persisted_edges_valid = false;
        self.test_membership_dirty = true;
        self.reclassify_python_test_membership();
        true
    }

    /// Reloads pytest `python_files`, `python_functions`, and `testpaths` from disk when a pytest
    /// config file changes, reclassifying `.py` file nodes and marking test membership dirty if changed.
    pub fn refresh_pytest_config(&mut self, workspace: &Workspace) -> bool {
        let new_patterns = workspace.pytest_file_patterns();
        let new_fn_patterns = workspace.pytest_function_patterns();
        let new_testpaths = workspace.pytest_testpaths();
        let new_fallback =
            !new_testpaths.is_empty() && !testpaths_match_any_on_disk(&workspace.root, &new_testpaths);
        if new_patterns == self.test_file_patterns
            && new_fn_patterns == self.test_function_patterns
            && new_testpaths == self.testpaths
            && new_fallback == self.testpaths_fallback_active
        {
            return false;
        }
        let fn_patterns_changed = new_fn_patterns != self.test_function_patterns;
        self.test_file_patterns = new_patterns;
        self.test_function_patterns = new_fn_patterns;
        self.testpaths = new_testpaths;
        self.testpaths_fallback_active = new_fallback;
        self.persisted_edges_valid = false;
        self.test_membership_dirty = true;
        if fn_patterns_changed {
            // Re-parse Python files so custom `python_functions` test symbols & fixture params update
            let py_paths: Vec<String> = self
                .files
                .keys()
                .filter(|k| k.ends_with(".py"))
                .cloned()
                .collect();
            for rel_str in py_paths {
                let full = workspace.root.join(&rel_str);
                if let Ok(bytes) = std::fs::read(&full) {
                    let stamp = self.files.get(&rel_str).and_then(|n| n.stamp);
                    self.insert_file_bytes_with_stamp(&rel_str, &bytes, stamp);
                }
            }
        } else {
            self.reclassify_python_test_membership();
        }
        true
    }

    pub fn transitive_deps_for(&self, test_file: &str) -> Vec<String> {
        let Some(ids) = self.transitive_dep_ids_for_test.get(test_file) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|&id| self.file_index_to_path.get(id as usize).cloned())
            .collect()
    }

    /// Recomputes module resolution, forward/reverse import edges, conftest chains, and Merkle dep_hashes.
    pub(super) fn rebuild_barrel_import_maps(&mut self, frontend_index: &FrontendModuleIndex) {
        let mut by_importer: BTreeMap<u32, Vec<(String, String)>> = BTreeMap::new();
        let mut by_barrel: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (path, node) in &self.files {
            if !is_frontend_source_path(path) || node.parsed.imports.is_empty() {
                continue;
            }
            let Some(&importer_id) = self.path_to_id.get(path) else {
                continue;
            };
            let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
            for spec in &node.parsed.imports {
                if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
                    continue;
                }
                let raw_targets = frontend_index.resolve_raw_specifier(path, &spec.module, &self.files);
                for target in raw_targets {
                    if is_frontend_barrel_file(&target) {
                        for sym in &spec.symbols {
                            pairs.insert((target.clone(), sym.clone()));
                        }
                        let imps = by_barrel.entry(target).or_default();
                        if !imps.contains(&importer_id) {
                            imps.push(importer_id);
                        }
                    }
                }
            }
            if !pairs.is_empty() {
                by_importer.insert(importer_id, pairs.into_iter().collect());
            }
        }
        self.barrel_imports_by_importer = by_importer;
        self.barrel_to_importers = by_barrel;
    }

    pub fn recompute_edges(&mut self) {
        self.conftest_for_test.clear();
        self.file_index_to_path.clear();
        self.path_to_id.clear();
        self.forward_adj.clear();
        self.reverse_adj.clear();
        self.conftest_paths.clear();
        self.symbol_to_test_files.clear();
        self.transitive_dep_ids_for_test.clear();
        self.dep_hash_for_test.clear();

        self.file_index_to_path.reserve(self.files.len());
        self.path_to_id.reserve(self.files.len());
        let mut has_py_or_proto = false;
        let mut has_rs = false;
        let mut has_fe = false;
        for (idx, (path, node)) in self.files.iter().enumerate() {
            let id = u32::try_from(idx).unwrap_or(u32::MAX);
            self.file_index_to_path.push(path.clone());
            self.path_to_id.insert(path.clone(), id);
            if node.is_conftest {
                self.conftest_paths.insert(path.clone());
            }
            if node.is_test {
                for sym in &node.parsed.test_symbols {
                    self.symbol_to_test_files
                        .entry(sym.clone())
                        .or_default()
                        .push(path.clone());
                }
            }
            if path.ends_with(".py") || path.ends_with(".proto") {
                has_py_or_proto = true;
            } else if path.ends_with(".rs") {
                has_rs = true;
            } else if is_frontend_source_path(path) || is_stylesheet_path(path) {
                has_fe = true;
            }
        }
        self.file_index_to_path.shrink_to_fit();

        let module_index = if has_py_or_proto {
            ModuleIndex::build(&self.files)
        } else {
            ModuleIndex::default()
        };
        let fixture_providers = if has_py_or_proto {
            build_fixture_provider_index(&self.files)
        } else {
            BTreeMap::new()
        };
        let rust_index = if has_rs {
            RustModuleIndex::build(&self.files)
        } else {
            RustModuleIndex::default()
        };
        let frontend_index = if has_fe {
            FrontendModuleIndex::build(&self.files)
        } else {
            FrontendModuleIndex::default()
        };
        let bridge_index = BridgeProviderIndex::build(&self.files);
        self.raw_embedded_files = collect_raw_embedded_files(&self.files);
        self.raw_embedded_files
            .extend(frontend_index.collect_raw_imported_files(&self.files));
        self.raw_embedded_files
            .extend(collect_rust_custom_path_mod_files(&self.files));

        let num_files = self.file_index_to_path.len();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); num_files];
        let mut rev_adj: Vec<Vec<u32>> = vec![Vec::new(); num_files];

        if self.files.len() >= 128 {
            let entries: Vec<(u32, &String, &FileNode)> = self
                .files
                .iter()
                .enumerate()
                .filter(|(_, (_, node))| {
                    !node.is_config
                        || !node.parsed.imports.is_empty()
                        || !node.parsed.data_refs.is_empty()
                })
                .map(|(idx, (path, node))| (u32::try_from(idx).unwrap_or(u32::MAX), path, node))
                .collect();
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = entries.len().div_ceil(workers).max(1);
            let files_ref = &self.files;
            let mod_ref = &module_index;
            let rs_ref = &rust_index;
            let fe_ref = &frontend_index;
            let fix_ref = &fixture_providers;
            let br_ref = &bridge_index;
            let rt_ref = &self.runtime_deps_for_test;
            let p2id_ref = &self.path_to_id;
            std::thread::scope(|scope| {
                let handles: Vec<_> = entries
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut local = Vec::with_capacity(chunk.len());
                            for &(src_id, path, node) in chunk {
                                let deps = compute_direct_dependencies(
                                    path, node, files_ref, mod_ref, rs_ref, fe_ref, fix_ref,
                                    br_ref, rt_ref,
                                );
                                let mut row = Vec::with_capacity(deps.len());
                                for dep in deps {
                                    if let Some(&dst_id) = p2id_ref.get(&dep) {
                                        row.push(dst_id);
                                    }
                                }
                                local.push((src_id, row));
                            }
                            local
                        })
                    })
                    .collect();
                for handle in handles {
                    let chunk = handle
                        .join()
                        .expect("AST edge resolution worker panicked");
                    for (src_id, row) in chunk {
                        for &dst_id in &row {
                            rev_adj[dst_id as usize].push(src_id);
                        }
                        adj[src_id as usize] = row;
                    }
                }
            });
        } else {
            for (idx, (path, node)) in self.files.iter().enumerate() {
                if node.is_config
                    && node.parsed.imports.is_empty()
                    && node.parsed.data_refs.is_empty()
                {
                    continue;
                }
                let src_id = u32::try_from(idx).unwrap_or(u32::MAX);
                let deps = compute_direct_dependencies(
                    path,
                    node,
                    &self.files,
                    &module_index,
                    &rust_index,
                    &frontend_index,
                    &fixture_providers,
                    &bridge_index,
                    &self.runtime_deps_for_test,
                );
                let mut row = Vec::with_capacity(deps.len());
                for dep in deps {
                    if let Some(&dst_id) = self.path_to_id.get(&dep) {
                        row.push(dst_id);
                        rev_adj[dst_id as usize].push(src_id);
                    }
                }
                adj[src_id as usize] = row;
            }
        }
        for r in &mut rev_adj {
            r.shrink_to_fit();
        }
        self.forward_adj = adj;
        self.reverse_adj = rev_adj;

        let conftests = self.conftest_paths.clone();
        let test_files: Vec<String> = self
            .files
            .values()
            .filter(|n| n.is_test)
            .map(|n| n.path.clone())
            .collect();

        self.tsconfig_extends_rev = self.compute_tsconfig_extends_rev();
        self.rebuild_barrel_import_maps(&frontend_index);
        self.last_side_effectful_barrels = frontend_index.side_effectful_barrels.clone();
        let base_hasher = self.build_base_hasher();
        let non_py_base_hasher = self.build_non_python_base_hasher();
        self.last_env_ref_digest = self.compute_env_ref_digest();
        let node_digests = self.build_all_node_hash_bytes();

        // For graphs up to 4K files with >= 32 test files (<= 2 MiB temporary bitsets),
        // compute SCC-condensed DAG bitset reachability once instead of O(T * (V + E)) BFS + sorts.
        let scc_reach = if num_files > 0 && num_files <= 4_096 && test_files.len() >= 32 {
            Some(compute_scc_reachability_bitsets(&self.forward_adj))
        } else {
            None
        };

        if test_files.len() >= 64 {
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = test_files.len().div_ceil(workers).max(1);
            let this_ref: &Self = self;
            let cf_ref = &conftests;
            let digests_ref = &node_digests;
            let base_ref = &base_hasher;
            let non_py_base_ref = &non_py_base_hasher;
            let scc_ref = scc_reach.as_ref();
            type TestEdgeOutcome = (String, Vec<String>, Vec<u32>, String);
            let chunk_outcomes: Vec<Vec<TestEdgeOutcome>> = std::thread::scope(|scope| {
                let handles: Vec<_> = test_files
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut visited_stamp = if scc_ref.is_none() {
                                vec![0u32; num_files]
                            } else {
                                Vec::new()
                            };
                            let mut epoch = 0u32;
                            let mut queue = VecDeque::new();
                            let words = num_files.div_ceil(64);
                            let mut scratch_bits = if scc_ref.is_some() {
                                vec![0u64; words]
                            } else {
                                Vec::new()
                            };
                            let mut out = Vec::with_capacity(chunk.len());
                            for test_path in chunk {
                                let ancestor_conftests = ancestor_conftests_for(test_path, cf_ref);
                                let self_id = this_ref.path_to_id.get(test_path).copied();
                                let reachable_ids = if let Some((node_to_scc, scc_bits)) = scc_ref {
                                    scratch_bits.fill(0);
                                    if let Some(test_id) = self_id {
                                        let bits = &scc_bits[node_to_scc[test_id as usize] as usize];
                                        for (dst, &src) in scratch_bits.iter_mut().zip(bits.iter()) {
                                            *dst |= src;
                                        }
                                    }
                                    for cf in &ancestor_conftests {
                                        if let Some(&cf_id) = this_ref.path_to_id.get(cf)
                                            && this_ref.is_conftest_used_by_test(cf, test_path)
                                        {
                                            let bits =
                                                &scc_bits[node_to_scc[cf_id as usize] as usize];
                                            for (dst, &src) in
                                                scratch_bits.iter_mut().zip(bits.iter())
                                            {
                                                *dst |= src;
                                            }
                                        }
                                    }
                                    extract_sorted_ids_from_bitset(&scratch_bits)
                                } else {
                                    epoch = epoch.wrapping_add(1);
                                    if epoch == 0 {
                                        visited_stamp.fill(0);
                                        epoch = 1;
                                    }
                                    let mut ids = Vec::new();
                                    queue.clear();
                                    if let Some(test_id) = self_id {
                                        visited_stamp[test_id as usize] = epoch;
                                        ids.push(test_id);
                                        queue.push_back(test_id);
                                    }
                                    for cf in &ancestor_conftests {
                                        if let Some(&cf_id) = this_ref.path_to_id.get(cf)
                                            && visited_stamp[cf_id as usize] != epoch
                                            && this_ref.is_conftest_used_by_test(cf, test_path)
                                        {
                                            visited_stamp[cf_id as usize] = epoch;
                                            ids.push(cf_id);
                                            queue.push_back(cf_id);
                                        }
                                    }
                                    while let Some(curr_id) = queue.pop_front() {
                                        for &next_id in &this_ref.forward_adj[curr_id as usize] {
                                            if visited_stamp[next_id as usize] != epoch {
                                                visited_stamp[next_id as usize] = epoch;
                                                ids.push(next_id);
                                                queue.push_back(next_id);
                                            }
                                        }
                                    }
                                    ids.sort_unstable();
                                    ids
                                };
                                let mut hasher = this_ref.base_hasher_for_test(
                                    test_path,
                                    &reachable_ids,
                                    base_ref,
                                    non_py_base_ref,
                                );
                                this_ref.update_relevant_config_hashes(
                                    &mut hasher,
                                    test_path,
                                    &reachable_ids,
                                );
                                this_ref.update_runtime_dep_raw_hashes(
                                    &mut hasher,
                                    test_path,
                                    &reachable_ids,
                                );
                                for &dep_id in &reachable_ids {
                                    if let Some((self_bytes, api_bytes)) =
                                        digests_ref.get(dep_id as usize)
                                    {
                                        if Some(dep_id) == self_id {
                                            hasher.update(self_bytes);
                                        } else {
                                            hasher.update(api_bytes);
                                        }
                                    }
                                }
                                let dep_hash = hex_encode(&hasher.finalize());
                                out.push((
                                    test_path.clone(),
                                    ancestor_conftests,
                                    reachable_ids,
                                    dep_hash,
                                ));
                            }
                            out
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("AST test hash worker panicked"))
                    .collect()
            });
            for chunk in chunk_outcomes {
                for (test_path, ancestor_conftests, reachable_ids, dep_hash) in chunk {
                    self.conftest_for_test
                        .insert(test_path.clone(), ancestor_conftests);
                    self.transitive_dep_ids_for_test
                        .insert(test_path.clone(), reachable_ids);
                    self.dep_hash_for_test.insert(test_path, dep_hash);
                }
            }
        } else {
            let mut visited_stamp = if scc_reach.is_none() {
                vec![0u32; num_files]
            } else {
                Vec::new()
            };
            let mut epoch = 0u32;
            let mut queue = VecDeque::new();
            let words = num_files.div_ceil(64);
            let mut scratch_bits = if scc_reach.is_some() {
                vec![0u64; words]
            } else {
                Vec::new()
            };

            for test_path in test_files {
                let ancestor_conftests = ancestor_conftests_for(&test_path, &conftests);
                let self_id = self.path_to_id.get(&test_path).copied();
                let reachable_ids = if let Some((ref node_to_scc, ref scc_bits)) = scc_reach {
                    scratch_bits.fill(0);
                    if let Some(test_id) = self_id {
                        let bits = &scc_bits[node_to_scc[test_id as usize] as usize];
                        for (dst, &src) in scratch_bits.iter_mut().zip(bits.iter()) {
                            *dst |= src;
                        }
                    }
                    for cf in &ancestor_conftests {
                        if let Some(&cf_id) = self.path_to_id.get(cf)
                            && self.is_conftest_used_by_test(cf, &test_path)
                        {
                            let bits = &scc_bits[node_to_scc[cf_id as usize] as usize];
                            for (dst, &src) in scratch_bits.iter_mut().zip(bits.iter()) {
                                *dst |= src;
                            }
                        }
                    }
                    extract_sorted_ids_from_bitset(&scratch_bits)
                } else {
                    epoch = epoch.wrapping_add(1);
                    if epoch == 0 {
                        visited_stamp.fill(0);
                        epoch = 1;
                    }
                    let mut ids = Vec::new();
                    queue.clear();

                    if let Some(test_id) = self_id {
                        visited_stamp[test_id as usize] = epoch;
                        ids.push(test_id);
                        queue.push_back(test_id);
                    }
                    for cf in &ancestor_conftests {
                        if let Some(&cf_id) = self.path_to_id.get(cf)
                            && visited_stamp[cf_id as usize] != epoch
                            && self.is_conftest_used_by_test(cf, &test_path)
                        {
                            visited_stamp[cf_id as usize] = epoch;
                            ids.push(cf_id);
                            queue.push_back(cf_id);
                        }
                    }

                    while let Some(curr_id) = queue.pop_front() {
                        for &next_id in &self.forward_adj[curr_id as usize] {
                            if visited_stamp[next_id as usize] != epoch {
                                visited_stamp[next_id as usize] = epoch;
                                ids.push(next_id);
                                queue.push_back(next_id);
                            }
                        }
                    }
                    ids.sort_unstable();
                    ids
                };

                let mut hasher = self.base_hasher_for_test(
                    &test_path,
                    &reachable_ids,
                    &base_hasher,
                    &non_py_base_hasher,
                );
                self.update_relevant_config_hashes(&mut hasher, &test_path, &reachable_ids);
                self.update_runtime_dep_raw_hashes(&mut hasher, &test_path, &reachable_ids);
                for &dep_id in &reachable_ids {
                    if let Some((self_bytes, api_bytes)) = node_digests.get(dep_id as usize) {
                        if Some(dep_id) == self_id {
                            hasher.update(self_bytes);
                        } else {
                            hasher.update(api_bytes);
                        }
                    }
                }
                let dep_hash = hex_encode(&hasher.finalize());
                self.conftest_for_test
                    .insert(test_path.clone(), ancestor_conftests);
                self.transitive_dep_ids_for_test
                    .insert(test_path.clone(), reachable_ids);
                self.dep_hash_for_test.insert(test_path, dep_hash);
            }
        }
        self.local_python_roots = self.compute_local_python_roots();
        self.refresh_untrackable_tests();
        self.cached_conventions = Some(self.compute_test_conventions_summary());
        self.provider_structure_dirty = false;
        self.test_membership_dirty = false;
    }

    pub fn recompute_structure_incremental(&mut self, changed_files: &[String]) {
        if self.provider_structure_dirty
            || self.test_membership_dirty
            || self.file_index_to_path.len() != self.files.len()
            || changed_files.iter().any(|path| {
                path.ends_with("/__init__.py")
                    || path == "__init__.py"
                    || path.ends_with("conftest.py")
                    || path.ends_with("/mod.rs")
                    || path == "mod.rs"
                    || path.ends_with("/lib.rs")
                    || path == "src/lib.rs"
                    || path.ends_with("build.rs")
                    || is_rust_common_test_helper(path)
                    || is_frontend_setup_file(path)
                    || is_frontend_barrel_file(path)
                    || self
                        .files
                        .get(path)
                        .is_some_and(node_has_provider_or_fixture)
            })
        {
            self.recompute_edges();
            return;
        }
        let mut affected = self.affected_test_names(changed_files);
        let need_py = changed_files
            .iter()
            .any(|p| p.ends_with(".py") || p.ends_with(".proto"));
        let has_any_rs = self.files.keys().any(|k| k.ends_with(".rs"));
        let need_rs = has_any_rs
            && changed_files
                .iter()
                .any(|p| p.ends_with(".rs") || p.ends_with(".py"));
        let need_fe = changed_files
            .iter()
            .any(|p| is_frontend_source_path(p) || is_stylesheet_path(p));
        let module_index = if need_py {
            ModuleIndex::build(&self.files)
        } else {
            ModuleIndex::default()
        };
        let rust_index = if need_rs {
            RustModuleIndex::build(&self.files)
        } else {
            RustModuleIndex::default()
        };
        let frontend_index = if need_fe {
            FrontendModuleIndex::build(&self.files)
        } else {
            FrontendModuleIndex::default()
        };
        let fixture_providers = if need_py {
            build_fixture_provider_index(&self.files)
        } else {
            BTreeMap::new()
        };
        let need_bridge = changed_files.iter().any(|p| {
            self.files.get(p).is_some_and(|n| {
                n.parsed.defined_symbols.iter().any(|s| {
                    s.starts_with("proto:")
                        || s.starts_with("proto_use:")
                        || s.starts_with("gql:")
                        || s.starts_with("gql_use:")
                        || s.starts_with("openapi:")
                        || s.starts_with("pyo3:")
                        || s.starts_with("wasm:")
                        || s.starts_with("wasm_use:")
                        || s.starts_with("api_route:")
                        || s.starts_with("api_call:")
                        || s.starts_with("ctx_def:")
                        || s.starts_with("ctx_use:")
                        || s.starts_with("css_class:")
                }) || n.parsed.imports.iter().any(|imp| {
                    imp.module.contains("_pb2") || imp.module.ends_with(".proto")
                })
            })
        });
        if need_fe && frontend_index.side_effectful_barrels != self.last_side_effectful_barrels {
            // Propagated barrel side-effect status changed (e.g. a non-barrel file started
            // importing a side-effectful module); recompute all edges so existing barrel consumers
            // update their edges and hashes immediately.
            self.recompute_edges();
            return;
        }
        let bridge_index = if need_bridge {
            BridgeProviderIndex::build(&self.files)
        } else {
            BridgeProviderIndex::default()
        };
        if need_py || need_fe || need_rs {
            // Rebuild `raw_embedded_files` cleanly so removed Python/Frontend/Rust raw reads
            // do not remain stuck in the raw-hashed set after incremental edits.
            self.raw_embedded_files = collect_raw_embedded_files(&self.files);
            if need_fe {
                self.raw_embedded_files
                    .extend(frontend_index.collect_raw_imported_files(&self.files));
            } else if self.files.values().any(|n| {
                n.parsed.defined_symbols.iter().any(|s| {
                    s.starts_with("fe_raw_import:")
                        || s.starts_with("fe_raw_fs:")
                        || s.starts_with("fe_raw_url_fs:")
                })
            }) {
                let fe_idx = FrontendModuleIndex::build(&self.files);
                self.raw_embedded_files
                    .extend(fe_idx.collect_raw_imported_files(&self.files));
            }
            self.raw_embedded_files
                .extend(collect_rust_custom_path_mod_files(&self.files));
        }
        self.cached_conventions = None;
        if need_py {
            self.local_python_roots = self.compute_local_python_roots();
        }
        let mut replacements = BTreeMap::new();
        for path in changed_files {
            let Some(node) = self.files.get(path) else {
                self.recompute_edges();
                return;
            };
            replacements.insert(
                path.clone(),
                compute_direct_dependencies(
                    path,
                    node,
                    &self.files,
                    &module_index,
                    &rust_index,
                    &frontend_index,
                    &fixture_providers,
                    &bridge_index,
                    &self.runtime_deps_for_test,
                ),
            );
        }
        for (path, new_deps) in replacements {
            let Some(&s_id) = self.path_to_id.get(&path) else {
                continue;
            };
            let old_row = std::mem::take(&mut self.forward_adj[s_id as usize]);
            for old_d in old_row {
                if let Some(rev_row) = self.reverse_adj.get_mut(old_d as usize) {
                    rev_row.retain(|&x| x != s_id);
                }
            }
            let mut new_row = Vec::with_capacity(new_deps.len());
            for dep in &new_deps {
                if let Some(&d_id) = self.path_to_id.get(dep) {
                    new_row.push(d_id);
                    if let Some(rev_row) = self.reverse_adj.get_mut(d_id as usize)
                        && !rev_row.contains(&s_id)
                    {
                        rev_row.push(s_id);
                    }
                }
            }
            self.forward_adj[s_id as usize] = new_row;
        }
        // Keep `symbol_to_test_files` synchronized for changed test files
        if !changed_files.is_empty() {
            let changed_set: std::collections::HashSet<&str> =
                changed_files.iter().map(String::as_str).collect();
            self.symbol_to_test_files.retain(|_, files| {
                files.retain(|f| !changed_set.contains(f.as_str()));
                !files.is_empty()
            });
            for path in changed_files {
                if let Some(node) = self.files.get(path)
                    && node.is_test
                {
                    for sym in &node.parsed.test_symbols {
                        let entry = self.symbol_to_test_files.entry(sym.clone()).or_default();
                        if !entry.iter().any(|existing| existing == path) {
                            entry.push(path.clone());
                        }
                    }
                }
            }
        }
        affected.extend(self.affected_test_names(changed_files));
        self.recompute_selected_tests(&affected);
    }

    pub(super) fn recompute_selected_tests(&mut self, tests: &BTreeSet<String>) {
        if tests.is_empty() {
            return;
        }
        if self.path_to_id.len() != self.file_index_to_path.len()
            || self.forward_adj.len() != self.file_index_to_path.len()
        {
            self.recompute_edges();
            return;
        }
        let conftests = self.conftest_paths.clone();
        let base_hasher = self.build_base_hasher();
        let non_py_base_hasher = self.build_non_python_base_hasher();
        let mut digest_cache: BTreeMap<(u32, bool), Vec<u8>> = BTreeMap::new();
        let mut visited_stamp = vec![0u32; self.file_index_to_path.len()];
        let mut epoch = 0u32;
        let mut queue = VecDeque::new();
        for test_path in tests {
            let Some(&test_id) = self.path_to_id.get(test_path) else {
                continue;
            };
            epoch = epoch.wrapping_add(1);
            if epoch == 0 {
                visited_stamp.fill(0);
                epoch = 1;
            }
            let conftest_chain = ancestor_conftests_for(test_path, &conftests);
            let mut reachable = vec![test_id];
            queue.clear();
            queue.push_back(test_id);
            visited_stamp[test_id as usize] = epoch;
            for conftest in &conftest_chain {
                if let Some(&id) = self.path_to_id.get(conftest)
                    && visited_stamp[id as usize] != epoch
                    && self.is_conftest_used_by_test(conftest, test_path)
                {
                    visited_stamp[id as usize] = epoch;
                    reachable.push(id);
                    queue.push_back(id);
                }
            }
            while let Some(id) = queue.pop_front() {
                for &next in &self.forward_adj[id as usize] {
                    if visited_stamp[next as usize] != epoch {
                        visited_stamp[next as usize] = epoch;
                        reachable.push(next);
                        queue.push_back(next);
                    }
                }
            }
            reachable.sort_unstable();
            let mut hasher =
                self.base_hasher_for_test(test_path, &reachable, &base_hasher, &non_py_base_hasher);
            self.update_relevant_config_hashes(&mut hasher, test_path, &reachable);
            self.update_runtime_dep_raw_hashes(&mut hasher, test_path, &reachable);
            for &id in &reachable {
                let is_self = id == test_id;
                let bytes = digest_cache.entry((id, is_self)).or_insert_with(|| {
                    let path = &self.file_index_to_path[id as usize];
                    self.files
                        .get(path)
                        .map(|node| build_node_hash_bytes(path, node, is_self))
                        .unwrap_or_default()
                });
                hasher.update(bytes);
            }
            self.conftest_for_test
                .insert(test_path.clone(), conftest_chain);
            self.transitive_dep_ids_for_test
                .insert(test_path.clone(), reachable);
            self.dep_hash_for_test
                .insert(test_path.clone(), hex_encode(&hasher.finalize()));
        }
        self.refresh_untrackable_tests();
    }

}

