use super::*;

impl DependencyGraph {
    pub fn test_files(&self) -> Vec<String> {
        self.dep_hash_for_test.keys().cloned().collect()
    }

    /// Returns safe 3rd-party packages imported across the workspace that can be pre-imported in the Warm Pytest Zygote
    /// without touching any workspace-local Python module.
    pub fn external_preload_modules(&self) -> Vec<String> {
        self.external_preload_modules_for_tests(&self.test_files())
    }

    pub(super) fn compute_local_python_roots(&self) -> BTreeSet<String> {
        let mut local_roots = BTreeSet::new();
        for path in self.files.keys() {
            if !path.ends_with(".py") {
                continue;
            }
            for cand in candidate_module_names(path) {
                if let Some(first) = cand.split('.').next() {
                    local_roots.insert(first.to_string());
                }
            }
            if let Some(stem) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
                local_roots.insert(stem.to_string());
            }
        }
        local_roots
    }

    pub fn external_preload_modules_for_tests(&self, test_files: &[String]) -> Vec<String> {
        if !test_files.iter().any(|tf| tf.ends_with(".py")) {
            return Vec::new();
        }
        let computed_roots;
        let local_roots = if !self.local_python_roots.is_empty() {
            &self.local_python_roots
        } else {
            computed_roots = self.compute_local_python_roots();
            &computed_roots
        };
        let excluded = [
            "pytest",
            "_pytest",
            "conftest",
            "anyio",
            "trio",
            "hypothesis",
            "pytest_asyncio",
            "pytest_trio",
            "pytest_cov",
        ];
        let mut externals = BTreeSet::new();
        let num_indexed = self.file_index_to_path.len();
        let mut relevant_paths = BTreeSet::new();
        if num_indexed == self.files.len() && num_indexed > 0 {
            let mut seen = vec![false; num_indexed];
            for test_file in test_files {
                relevant_paths.insert(test_file.as_str());
                if let Some(ids) = self.transitive_dep_ids_for_test.get(test_file) {
                    for &id in ids {
                        if let Some(slot) = seen.get_mut(id as usize) {
                            *slot = true;
                        }
                    }
                }
            }
            for (idx, flag) in seen.into_iter().enumerate() {
                if flag {
                    relevant_paths.insert(self.file_index_to_path[idx].as_str());
                }
            }
        } else {
            for test_file in test_files {
                relevant_paths.insert(test_file.as_str());
            }
        }
        for path in relevant_paths {
            let Some(node) = self.files.get(path) else {
                continue;
            };
            for spec in &node.parsed.imports {
                if spec.level > 0 || spec.module.is_empty() {
                    continue;
                }
                let top = spec.module.split('.').next().unwrap_or("");
                if top.is_empty() || local_roots.contains(top) || excluded.contains(&top) {
                    continue;
                }
                externals.insert(top.to_string());
                if matches!(
                    top,
                    "fastapi" | "azure" | "openai" | "pydantic" | "httpx" | "starlette"
                ) && spec.module.split('.').count() <= 2
                {
                    externals.insert(spec.module.clone());
                }
            }
        }
        externals.into_iter().take(48).collect()
    }

    /// Computes all transitively affected Python files (both application modules and test files) for a list of modified paths.
    pub fn affected_modules(&self, changed_files: &[String]) -> Vec<String> {
        let num_files = self.file_index_to_path.len();
        if num_files > 0 && self.reverse_adj.len() == num_files {
            let mut visited = vec![false; num_files];
            let mut queue: VecDeque<u32> = VecDeque::new();
            let mut extra: BTreeSet<String> = BTreeSet::new();
            for raw in changed_files {
                let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
                if norm.is_empty() {
                    continue;
                }
                if let Some(&id) = self.path_to_id.get(&norm) {
                    if !visited[id as usize] {
                        visited[id as usize] = true;
                        queue.push_back(id);
                    }
                } else {
                    extra.insert(norm);
                }
            }
            while let Some(curr_id) = queue.pop_front() {
                for &imp_id in &self.reverse_adj[curr_id as usize] {
                    if !visited[imp_id as usize] {
                        visited[imp_id as usize] = true;
                        queue.push_back(imp_id);
                    }
                }
            }
            for (idx, &flag) in visited.iter().enumerate() {
                if flag {
                    extra.insert(self.file_index_to_path[idx].clone());
                }
            }
            return extra.into_iter().collect();
        }
        // Fallback when adjacency index is not populated yet
        let mut visited: BTreeSet<String> = BTreeSet::new();
        for raw in changed_files {
            let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
            if !norm.is_empty() {
                visited.insert(norm);
            }
        }
        visited.into_iter().collect()
    }

    /// Lightweight variant of `affected_tests` returning only test file paths
    /// without reconstructing reason chains or cloning `test_symbols` / `dep_hash`.
    pub fn affected_test_names(&self, changed_files: &[String]) -> BTreeSet<String> {
        let mut tests = BTreeSet::new();
        let num_files = self.file_index_to_path.len();
        let use_fast_adj = num_files > 0
            && self.reverse_adj.len() == num_files
            && self.path_to_id.len() == num_files;
        let mut visited_ids = vec![false; if use_fast_adj { num_files } else { 0 }];
        let mut id_queue: VecDeque<u32> = VecDeque::new();

        for raw in changed_files {
            let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
            if norm.is_empty() {
                continue;
            }
            let file_name = Path::new(&norm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if PYTEST_CONFIG_FILES.contains(&file_name)
                || RUST_CONFIG_FILES.contains(&file_name)
                || crate::workspace::is_cargo_dot_config_rel(Path::new(&norm))
                || crate::workspace::is_frontend_config_filename(file_name)
                || POLYGLOT_CONFIG_FILES.contains(&file_name)
                || file_name.ends_with(".csproj")
                || file_name.ends_with(".sln")
                || self.config_hashes.contains_key(&norm)
                || self.tsconfig_extends_rev.contains_key(&norm)
            {
                let empty_ids: &[u32] = &[];
                for test_path in self.dep_hash_for_test.keys() {
                    let ids = self
                        .transitive_dep_ids_for_test
                        .get(test_path)
                        .map_or(empty_ids, Vec::as_slice);
                    if self.is_config_relevant_to_test_ids(&norm, test_path, ids) {
                        tests.insert(test_path.clone());
                    }
                }
                // Do not `continue` here: also traverse reverse graph edges so any test reading
                // a cross-language config file at runtime (e.g. a Python test reading `Cargo.toml`)
                // is still invalidated and recomputed.
            }
            if file_name == "conftest.py"
                || is_rust_common_test_helper(&norm)
                || is_frontend_setup_file(&norm)
            {
                let scope_dir = conftest_scope_dir(&norm);
                let helper_lang = file_language_family(&norm);
                if scope_dir.is_empty() {
                    for test_path in self.dep_hash_for_test.keys() {
                        if file_language_family(test_path) == helper_lang
                            && self.is_conftest_used_by_test(&norm, test_path)
                        {
                            tests.insert(test_path.clone());
                        }
                    }
                } else {
                    let prefix = format!("{scope_dir}/");
                    for (test_path, _) in self
                        .dep_hash_for_test
                        .range(prefix.clone()..)
                        .take_while(|(p, _)| p.starts_with(&prefix))
                    {
                        if file_language_family(test_path) == helper_lang
                            && self.is_conftest_used_by_test(&norm, test_path)
                        {
                            tests.insert(test_path.clone());
                        }
                    }
                }
            }
            if use_fast_adj
                && let Some(&id) = self.path_to_id.get(&norm)
                && !visited_ids[id as usize]
            {
                visited_ids[id as usize] = true;
                id_queue.push_back(id);
            }
            // When a pure barrel file itself is edited (e.g. changing `export { a as foo }` to
            // `export { b as foo }`), seed its direct consumers without coupling sibling exports.
            if use_fast_adj
                && let Some(importers) = self.barrel_to_importers.get(&norm)
            {
                for &imp_id in importers {
                    if !visited_ids[imp_id as usize] {
                        visited_ids[imp_id as usize] = true;
                        id_queue.push_back(imp_id);
                    }
                }
            }
        }

        while let Some(curr_id) = id_queue.pop_front() {
            let curr_path = &self.file_index_to_path[curr_id as usize];
            if let Some(node) = self.files.get(curr_path) {
                if node.is_test {
                    tests.insert(curr_path.clone());
                }
                if node.is_conftest {
                    let scope_dir = conftest_scope_dir(curr_path);
                    let helper_lang = file_language_family(curr_path);
                    if scope_dir.is_empty() {
                        for test_path in self.dep_hash_for_test.keys() {
                            if !tests.contains(test_path)
                                && file_language_family(test_path) == helper_lang
                                && self.is_conftest_used_by_test(curr_path, test_path)
                            {
                                tests.insert(test_path.clone());
                            }
                        }
                    } else {
                        let prefix = format!("{scope_dir}/");
                        for (test_path, _) in self
                            .dep_hash_for_test
                            .range(prefix.clone()..)
                            .take_while(|(p, _)| p.starts_with(&prefix))
                        {
                            if !tests.contains(test_path)
                                && file_language_family(test_path) == helper_lang
                                && self.is_conftest_used_by_test(curr_path, test_path)
                            {
                                tests.insert(test_path.clone());
                            }
                        }
                    }
                }
            }
            for &imp_id in &self.reverse_adj[curr_id as usize] {
                if !visited_ids[imp_id as usize] {
                    visited_ids[imp_id as usize] = true;
                    id_queue.push_back(imp_id);
                }
            }
        }
        tests
    }

    pub(super) fn reconstruct_chain(&self, parent_id: &[u32], leaf_id: u32) -> Vec<String> {
        let mut ids = Vec::new();
        let mut curr = leaf_id;
        while curr != u32::MAX {
            ids.push(curr);
            let next = parent_id[curr as usize];
            if next == curr {
                break;
            }
            curr = next;
        }
        ids.reverse();
        ids.into_iter()
            .filter_map(|id| self.file_index_to_path.get(id as usize).cloned())
            .collect()
    }

    /// Computes affected test files and shortest dependency chains for a list of modified relative file paths.
    pub fn affected_tests(&self, changed_files: &[String]) -> Vec<AffectedTest> {
        let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let num_files = self.file_index_to_path.len();
        let use_fast_adj = num_files > 0
            && self.reverse_adj.len() == num_files
            && self.path_to_id.len() == num_files;
        let mut visited_ids = vec![false; if use_fast_adj { num_files } else { 0 }];
        let mut parent_id = vec![u32::MAX; if use_fast_adj { num_files } else { 0 }];
        let mut id_queue: VecDeque<u32> = VecDeque::new();
        let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
        let mut visited: BTreeSet<String> = BTreeSet::new();

        for raw in changed_files {
            let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
            if norm.is_empty() {
                continue;
            }
            let file_name = Path::new(&norm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            // 1. Scoped config change affects matching language tests within its scope or downstream importers
            if PYTEST_CONFIG_FILES.contains(&file_name)
                || RUST_CONFIG_FILES.contains(&file_name)
                || crate::workspace::is_cargo_dot_config_rel(Path::new(&norm))
                || crate::workspace::is_frontend_config_filename(file_name)
                || POLYGLOT_CONFIG_FILES.contains(&file_name)
                || file_name.ends_with(".csproj")
                || file_name.ends_with(".sln")
                || self.config_hashes.contains_key(&norm)
                || self.tsconfig_extends_rev.contains_key(&norm)
            {
                let empty_ids: &[u32] = &[];
                for test_path in self.dep_hash_for_test.keys() {
                    let ids = self
                        .transitive_dep_ids_for_test
                        .get(test_path)
                        .map_or(empty_ids, Vec::as_slice);
                    if !self.is_config_relevant_to_test_ids(&norm, test_path, ids) {
                        continue;
                    }
                    reasons
                        .entry(test_path.clone())
                        .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                }
                // Fall through to reverse-edge traversal so explicit static/runtime readers of a
                // cross-language config file (e.g. a Python test reading `Cargo.toml`) are included.
            }

            // 2. conftest.py, Rust tests/common helper, or frontend setupTests change affects tests under its scope directory
            if file_name == "conftest.py"
                || is_rust_common_test_helper(&norm)
                || is_frontend_setup_file(&norm)
            {
                let scope_dir = conftest_scope_dir(&norm);
                let helper_lang = file_language_family(&norm);
                if scope_dir.is_empty() {
                    for test_path in self.dep_hash_for_test.keys() {
                        if file_language_family(test_path) == helper_lang
                            && self.is_conftest_used_by_test(&norm, test_path)
                        {
                            reasons
                                .entry(test_path.clone())
                                .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                        }
                    }
                } else {
                    let prefix = format!("{scope_dir}/");
                    for (test_path, _) in self
                        .dep_hash_for_test
                        .range(prefix.clone()..)
                        .take_while(|(p, _)| p.starts_with(&prefix))
                    {
                        if file_language_family(test_path) == helper_lang
                            && self.is_conftest_used_by_test(&norm, test_path)
                        {
                            reasons
                                .entry(test_path.clone())
                                .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                        }
                    }
                }
            }

            // 3. Reverse import graph traversal
            if use_fast_adj {
                let self_id_opt = self.path_to_id.get(&norm).copied();
                if let Some(id) = self_id_opt
                    && !visited_ids[id as usize]
                {
                    visited_ids[id as usize] = true;
                    parent_id[id as usize] = u32::MAX;
                    id_queue.push_back(id);
                }
                if let Some(importers) = self.barrel_to_importers.get(&norm) {
                    for &imp_id in importers {
                        if !visited_ids[imp_id as usize] {
                            visited_ids[imp_id as usize] = true;
                            parent_id[imp_id as usize] = self_id_opt.unwrap_or(u32::MAX);
                            id_queue.push_back(imp_id);
                        }
                    }
                }
            } else if visited.insert(norm.clone()) {
                queue.push_back((norm.clone(), vec![norm]));
            }
        }

        if use_fast_adj {
            while let Some(curr_id) = id_queue.pop_front() {
                let curr_path = &self.file_index_to_path[curr_id as usize];
                if let Some(node) = self.files.get(curr_path) {
                    if node.is_test && !reasons.contains_key(curr_path) {
                        reasons.insert(
                            curr_path.clone(),
                            self.reconstruct_chain(&parent_id, curr_id),
                        );
                    }
                    if node.is_config {
                        let empty_ids: &[u32] = &[];
                        let mut cfg_chain: Option<Vec<String>> = None;
                        for test_path in self.dep_hash_for_test.keys() {
                            if reasons.contains_key(test_path) {
                                continue;
                            }
                            let ids = self
                                .transitive_dep_ids_for_test
                                .get(test_path)
                                .map_or(empty_ids, Vec::as_slice);
                            if !self.is_config_relevant_to_test_ids(curr_path, test_path, ids) {
                                continue;
                            }
                            let base = cfg_chain.get_or_insert_with(|| {
                                self.reconstruct_chain(&parent_id, curr_id)
                            });
                            let mut c = base.clone();
                            if c.last() != Some(test_path) {
                                c.push(test_path.clone());
                            }
                            reasons.insert(test_path.clone(), c);
                        }
                    }
                    if node.is_conftest {
                        let scope_dir = conftest_scope_dir(curr_path);
                        let helper_lang = file_language_family(curr_path);
                        let mut cf_chain: Option<Vec<String>> = None;
                        if scope_dir.is_empty() {
                            for test_path in self.dep_hash_for_test.keys() {
                                if !reasons.contains_key(test_path)
                                    && file_language_family(test_path) == helper_lang
                                    && self.is_conftest_used_by_test(curr_path, test_path)
                                {
                                    let base = cf_chain.get_or_insert_with(|| {
                                        self.reconstruct_chain(&parent_id, curr_id)
                                    });
                                    let mut c = base.clone();
                                    if c.last() != Some(test_path) {
                                        c.push(test_path.clone());
                                    }
                                    reasons.insert(test_path.clone(), c);
                                }
                            }
                        } else {
                            let prefix = format!("{scope_dir}/");
                            for (test_path, _) in self
                                .dep_hash_for_test
                                .range(prefix.clone()..)
                                .take_while(|(p, _)| p.starts_with(&prefix))
                            {
                                if !reasons.contains_key(test_path)
                                    && file_language_family(test_path) == helper_lang
                                    && self.is_conftest_used_by_test(curr_path, test_path)
                                {
                                    let base = cf_chain.get_or_insert_with(|| {
                                        self.reconstruct_chain(&parent_id, curr_id)
                                    });
                                    let mut c = base.clone();
                                    if c.last() != Some(test_path) {
                                        c.push(test_path.clone());
                                    }
                                    reasons.insert(test_path.clone(), c);
                                }
                            }
                        }
                    }
                }
                for &imp_id in &self.reverse_adj[curr_id as usize] {
                    if !visited_ids[imp_id as usize] {
                        visited_ids[imp_id as usize] = true;
                        parent_id[imp_id as usize] = curr_id;
                        id_queue.push_back(imp_id);
                    }
                }
            }
        } else {
            while let Some((curr, chain)) = queue.pop_front() {
                if let Some(node) = self.files.get(&curr) {
                    if node.is_test {
                        reasons.entry(curr.clone()).or_insert_with(|| chain.clone());
                    }
                    if node.is_conftest {
                        let scope_dir = conftest_scope_dir(&curr);
                        let helper_lang = file_language_family(&curr);
                        if scope_dir.is_empty() {
                            for test_path in self.dep_hash_for_test.keys() {
                                if file_language_family(test_path) == helper_lang
                                    && self.is_conftest_used_by_test(&curr, test_path)
                                {
                                    reasons.entry(test_path.clone()).or_insert_with(|| {
                                        let mut c = chain.clone();
                                        if c.last() != Some(test_path) {
                                            c.push(test_path.clone());
                                        }
                                        c
                                    });
                                }
                            }
                        } else {
                            let prefix = format!("{scope_dir}/");
                            for (test_path, _) in self
                                .dep_hash_for_test
                                .range(prefix.clone()..)
                                .take_while(|(p, _)| p.starts_with(&prefix))
                            {
                                if file_language_family(test_path) == helper_lang
                                    && self.is_conftest_used_by_test(&curr, test_path)
                                {
                                    reasons.entry(test_path.clone()).or_insert_with(|| {
                                        let mut c = chain.clone();
                                        if c.last() != Some(test_path) {
                                            c.push(test_path.clone());
                                        }
                                        c
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut results = Vec::new();
        for (test_file, reason_chain) in reasons {
            if let Some(dep_hash) = self.dep_hash_for_test.get(&test_file) {
                let test_symbols = self
                    .files
                    .get(&test_file)
                    .map(|n| n.parsed.test_symbols.clone())
                    .unwrap_or_default();
                results.push(AffectedTest {
                    test_file,
                    dep_hash: dep_hash.clone(),
                    test_symbols,
                    reason_chain,
                });
            }
        }
        results
    }

    pub fn file_graph_info(&self, rel_path: &str) -> Option<FileGraphInfo> {
        let norm = rel_path.trim().trim_start_matches("./").replace('\\', "/");
        let node = self.files.get(&norm)?;
        let node_id = self.path_to_id.get(&norm).copied();
        let direct_imports: Vec<String> = node_id
            .and_then(|id| self.forward_adj.get(id as usize))
            .map(|ids| {
                let mut v: Vec<String> = ids
                    .iter()
                    .filter_map(|&dst| self.file_index_to_path.get(dst as usize).cloned())
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        let mut direct_imported_by: Vec<String> = node_id
            .and_then(|id| self.reverse_adj.get(id as usize))
            .map(|ids| {
                let mut v: Vec<String> = ids
                    .iter()
                    .filter_map(|&src| self.file_index_to_path.get(src as usize).cloned())
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        direct_imported_by.truncate(50);
        let mut transitive_dependencies = self.transitive_deps_for(&norm);
        transitive_dependencies.truncate(50);
        let conftest_chain = self
            .conftest_for_test
            .get(&norm)
            .cloned()
            .unwrap_or_default();
        let mut affected_raw = self.affected_tests(std::slice::from_ref(&norm));
        if affected_raw.len() > 50 {
            affected_raw.sort_by(|a, b| {
                a.reason_chain
                    .len()
                    .cmp(&b.reason_chain.len())
                    .then_with(|| a.test_file.cmp(&b.test_file))
            });
            affected_raw.truncate(50);
        }
        let affected_tests = affected_raw.into_iter().map(|a| a.test_file).collect();
        let language = if norm.ends_with(".rs") {
            "rust"
        } else if norm.ends_with(".py") {
            "python"
        } else if norm.ends_with(".tsx") {
            "react_tsx"
        } else if norm.ends_with(".jsx") {
            "react_jsx"
        } else if norm.ends_with(".ts") || norm.ends_with(".mts") || norm.ends_with(".cts") {
            "typescript"
        } else if norm.ends_with(".js") || norm.ends_with(".mjs") || norm.ends_with(".cjs") {
            "javascript"
        } else if norm.ends_with(".vue") {
            "vue"
        } else if norm.ends_with(".svelte") {
            "svelte"
        } else if norm.ends_with(".css")
            || norm.ends_with(".scss")
            || norm.ends_with(".sass")
            || norm.ends_with(".less")
            || norm.ends_with(".styl")
            || norm.ends_with(".stylus")
        {
            "css"
        } else if norm.ends_with(".go") {
            "go"
        } else if norm.ends_with(".cs") {
            "csharp"
        } else if norm.ends_with(".java") {
            "java"
        } else if norm.ends_with(".kt") {
            "kotlin"
        } else if norm.ends_with(".c")
            || norm.ends_with(".cpp")
            || norm.ends_with(".cc")
            || norm.ends_with(".h")
            || norm.ends_with(".hpp")
        {
            "cpp"
        } else if norm.ends_with(".proto") {
            "protobuf"
        } else if norm.ends_with(".graphql") || norm.ends_with(".gql") {
            "graphql"
        } else if node.is_config {
            "config"
        } else {
            "data"
        }
        .to_string();
        Some(FileGraphInfo {
            path: norm.clone(),
            language,
            sha256: node.sha256.clone(),
            semantic_sha256: if node.semantic_sha256.is_empty() {
                node.sha256.clone()
            } else {
                node.semantic_sha256.clone()
            },
            is_test: node.is_test,
            is_conftest: node.is_conftest,
            dep_hash: self.dep_hash_for_test.get(&norm).cloned(),
            direct_imports,
            direct_imported_by,
            transitive_dependencies,
            conftest_chain,
            affected_tests,
            test_symbols: node.parsed.test_symbols.clone(),
            fixtures_defined: node.parsed.fixtures_defined.clone(),
            env_vars_referenced: node.parsed.env_vars_referenced.clone(),
        })
    }

    pub fn is_conftest_used_by_test(&self, conftest_path: &str, test_path: &str) -> bool {
        if !conftest_path.ends_with("conftest.py") {
            return true;
        }
        let Some(cf_node) = self.files.get(conftest_path) else {
            return true;
        };
        if cf_node.parsed.fixtures_defined.is_empty()
            || cf_node
                .parsed
                .module_markers
                .iter()
                .any(|m| m == "autouse" || m == "pytest_hook")
            || cf_node
                .parsed
                .defined_symbols
                .iter()
                .any(|s| s.starts_with("pytest_"))
        {
            return true;
        }
        if let Some(test_node) = self.files.get(test_path)
            && (test_node
                .parsed
                .module_markers
                .iter()
                .any(|m| m == "py_dynamic_fixtures")
                || test_node
                    .parsed
                    .fixtures_used
                    .iter()
                    .any(|fx| cf_node.parsed.fixtures_defined.binary_search(fx).is_ok()))
        {
            return true;
        }
        if let (Some(&test_id), Some(&cf_id)) = (
            self.path_to_id.get(test_path),
            self.path_to_id.get(conftest_path),
        ) && let Some(row) = self.forward_adj.get(test_id as usize)
            && row.contains(&cf_id)
        {
            return true;
        }
        false
    }

    pub(super) fn has_any_affected_test(&self, norm: &str) -> bool {
        let num_files = self.file_index_to_path.len();
        if num_files > 0
            && self.reverse_adj.len() == num_files
            && let Some(&start_id) = self.path_to_id.get(norm)
        {
            let mut visited = vec![false; num_files];
            let mut queue: VecDeque<u32> = VecDeque::new();
            visited[start_id as usize] = true;
            queue.push_back(start_id);
            while let Some(curr_id) = queue.pop_front() {
                let curr = &self.file_index_to_path[curr_id as usize];
                if let Some(node) = self.files.get(curr) {
                    if node.is_test && self.dep_hash_for_test.contains_key(curr) {
                        return true;
                    }
                    if node.is_conftest {
                        let scope_dir = conftest_scope_dir(curr);
                        let helper_lang = file_language_family(curr);
                        if scope_dir.is_empty() {
                            if self.dep_hash_for_test.keys().any(|tp| {
                                file_language_family(tp) == helper_lang
                                    && self.is_conftest_used_by_test(curr, tp)
                            }) {
                                return true;
                            }
                        } else {
                            let prefix = format!("{scope_dir}/");
                            if self
                                .dep_hash_for_test
                                .range(prefix.clone()..)
                                .take_while(|(p, _)| p.starts_with(&prefix))
                                .any(|(tp, _)| {
                                    file_language_family(tp) == helper_lang
                                        && self.is_conftest_used_by_test(curr, tp)
                                })
                            {
                                return true;
                            }
                        }
                    }
                }
                for &imp_id in &self.reverse_adj[curr_id as usize] {
                    if !visited[imp_id as usize] {
                        visited[imp_id as usize] = true;
                        queue.push_back(imp_id);
                    }
                }
            }
            return false;
        }
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        visited.insert(norm);
        queue.push_back(norm);
        while let Some(curr) = queue.pop_front() {
            if let Some(node) = self.files.get(curr) {
                if node.is_test && self.dep_hash_for_test.contains_key(curr) {
                    return true;
                }
                if node.is_conftest {
                    let scope_dir = conftest_scope_dir(curr);
                    let helper_lang = file_language_family(curr);
                    if scope_dir.is_empty() {
                        if self.dep_hash_for_test.keys().any(|tp| {
                            file_language_family(tp) == helper_lang
                                && self.is_conftest_used_by_test(curr, tp)
                        }) {
                            return true;
                        }
                    } else {
                        let prefix = format!("{scope_dir}/");
                        if self
                            .dep_hash_for_test
                            .range(prefix.clone()..)
                            .take_while(|(p, _)| p.starts_with(&prefix))
                            .any(|(tp, _)| {
                                file_language_family(tp) == helper_lang
                                    && self.is_conftest_used_by_test(curr, tp)
                            })
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Identifies modified source files that have zero reverse test dependents in the AST graph.
    pub fn untested_modified_files(&self, changed_files: &[String]) -> Vec<String> {
        let mut untested = Vec::new();
        for raw in changed_files {
            let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
            let Some(node) = self.files.get(&norm) else {
                continue;
            };
            if node.is_test || node.is_conftest || node.is_config {
                continue;
            }
            if !self.has_any_affected_test(&norm) {
                untested.push(norm);
            }
        }
        untested
    }

    /// Suggests candidate test files by symbol/stem similarity when static imports are absent (e.g., dynamic dispatch).
    pub fn suggest_tests_for_untested(&self, source_files: &[String]) -> Vec<String> {
        if source_files.is_empty() {
            return Vec::new();
        }
        let mut suggestions = BTreeSet::new();
        for src in source_files {
            let stem = Path::new(src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let defined: Vec<String> = self
                .files
                .get(src)
                .map(|n| {
                    n.parsed
                        .defined_symbols
                        .iter()
                        .filter(|s| !s.contains(':') && s.len() >= 4)
                        .map(|s| s.to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            for test_path in self.dep_hash_for_test.keys() {
                if stem.len() >= 3 && contains_ascii_case_insensitive(test_path, stem.as_bytes()) {
                    suggestions.insert(test_path.clone());
                    continue;
                }
                if !defined.is_empty()
                    && let Some(t_node) = self.files.get(test_path)
                {
                    for t_sym in &t_node.parsed.test_symbols {
                        if defined
                            .iter()
                            .any(|d| contains_ascii_case_insensitive(t_sym, d.as_bytes()))
                        {
                            suggestions.insert(test_path.clone());
                            break;
                        }
                    }
                }
            }
        }
        suggestions.into_iter().take(10).collect()
    }

    /// Summarizes repository test conventions (frameworks, fixtures, markers) for AI coding agents.
    pub fn test_conventions_summary(&self) -> serde_json::Value {
        if let Some(ref cached) = self.cached_conventions {
            return cached.clone();
        }
        self.compute_test_conventions_summary()
    }

    pub(super) fn compute_test_conventions_summary(&self) -> serde_json::Value {
        let mut frameworks = BTreeSet::new();
        let mut fixtures = BTreeSet::new();
        let mut markers = BTreeSet::new();
        for (path, node) in &self.files {
            if node.is_test {
                if path.ends_with(".py") {
                    frameworks.insert("pytest");
                } else if path.ends_with(".rs") {
                    frameworks.insert("cargo_test");
                } else if is_storybook_story_path(path) {
                    frameworks.insert("storybook_play");
                } else if is_playwright_e2e_path(path) {
                    frameworks.insert("playwright");
                } else if is_cypress_test_path(path) {
                    frameworks.insert("cypress");
                } else if is_frontend_source_path(path) {
                    frameworks.insert("vitest_or_jest");
                } else if path.ends_with(".cs") {
                    frameworks.insert("dotnet_csharp");
                } else if is_c_cpp_source_path(path) {
                    frameworks.insert("cpp_gtest");
                }
            }
            for fx in &node.parsed.fixtures_defined {
                fixtures.insert(fx.clone());
            }
            for mk in &node.parsed.markers {
                markers.insert(mk.clone());
            }
        }
        serde_json::json!({
            "frameworks": frameworks.into_iter().collect::<Vec<_>>(),
            "fixtures_defined": fixtures.into_iter().take(32).collect::<Vec<_>>(),
            "markers": markers.into_iter().take(32).collect::<Vec<_>>(),
        })
    }

    /// Returns true if `rel_path` is a Python file that matches `python_files` or defines test symbols
    /// even when it lies outside `testpaths` (matching pytest's explicit-target collection behavior).
    pub fn is_explicit_python_test(&self, rel_path: &str) -> bool {
        if !rel_path.ends_with(".py") {
            return false;
        }
        let Some(node) = self.files.get(rel_path) else {
            return false;
        };
        if node.is_config || node.is_conftest {
            return false;
        }
        let file_name = Path::new(rel_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        matches_test_filename(file_name, &self.test_file_patterns)
            || !node.parsed.test_symbols.is_empty()
            || node
                .parsed
                .defined_symbols
                .iter()
                .any(|sym| matches_pytest_function_name(sym, &self.test_function_patterns))
    }

    pub(super) fn is_collectable_explicit_python_file(&self, rel_path: &str) -> bool {
        if !rel_path.ends_with(".py") {
            return false;
        }
        self.files
            .get(rel_path)
            .is_some_and(|node| !node.is_config && !node.is_conftest)
    }

    /// Returns the Merkle `dep_hash` for a test file, computing it on demand when an explicit Python
    /// test target outside `testpaths` or with custom `python_functions` is requested.
    pub fn dep_hash_for_test_or_explicit(&self, test_path: &str) -> Option<String> {
        if let Some(dh) = self.dep_hash_for_test.get(test_path) {
            return Some(dh.clone());
        }
        if !self.is_collectable_explicit_python_file(test_path) {
            return None;
        }
        let &self_id = self.path_to_id.get(test_path)?;
        let ancestor_conftests = ancestor_conftests_for(test_path, &self.conftest_paths);
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(self_id);
        queue.push_back(self_id);
        for cf in &ancestor_conftests {
            if let Some(&cf_id) = self.path_to_id.get(cf)
                && self.is_conftest_used_by_test(cf, test_path)
                && visited.insert(cf_id)
            {
                queue.push_back(cf_id);
            }
        }
        while let Some(curr_id) = queue.pop_front() {
            if let Some(adj) = self.forward_adj.get(curr_id as usize) {
                for &next_id in adj {
                    if visited.insert(next_id) {
                        queue.push_back(next_id);
                    }
                }
            }
        }
        let reachable_ids: Vec<u32> = visited.into_iter().collect();
        let base_hasher = self.build_base_hasher();
        let non_py_base_hasher = self.build_non_python_base_hasher();
        let mut hasher =
            self.base_hasher_for_test(test_path, &reachable_ids, &base_hasher, &non_py_base_hasher);
        self.update_relevant_config_hashes(&mut hasher, test_path, &reachable_ids);
        self.update_runtime_dep_raw_hashes(&mut hasher, test_path, &reachable_ids);
        for &dep_id in &reachable_ids {
            if let Some(dep_path) = self.file_index_to_path.get(dep_id as usize)
                && let Some(node) = self.files.get(dep_path)
            {
                let bytes = build_node_hash_bytes(dep_path, node, dep_id == self_id);
                hasher.update(&bytes);
            }
        }
        Some(hex_encode(&hasher.finalize()))
    }

    /// Resolves user/agent target arguments (which may be test files, directories, or source files)
    /// into concrete test file paths.
    pub fn resolve_target_test_files(&self, targets: &[String]) -> Vec<String> {
        if targets.is_empty() {
            return self.test_files();
        }
        let mut selected = BTreeSet::new();
        for raw_target in targets {
            let (norm, is_nodeid) = split_target_file_and_node_in_files(raw_target, &self.files);
            if norm.is_empty() || norm == "." {
                for t in self.dep_hash_for_test.keys() {
                    selected.insert(t.clone());
                }
                continue;
            }
            if self.dep_hash_for_test.contains_key(&norm)
                || self.is_explicit_python_test(&norm)
                || (is_nodeid && self.is_collectable_explicit_python_file(&norm))
            {
                selected.insert(norm);
                continue;
            }
            // Directory prefix match for tests under `norm/` (including explicit Python tests outside `testpaths`)
            let dir_prefix = format!("{norm}/");
            for (test_path, _) in self
                .dep_hash_for_test
                .range(dir_prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&dir_prefix))
            {
                selected.insert(test_path.clone());
            }
            for (path, node) in self
                .files
                .range(dir_prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&dir_prefix))
            {
                if path.ends_with(".py") && !node.is_config && !node.is_conftest {
                    let file_name = Path::new(path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if matches_test_filename(file_name, &self.test_file_patterns) {
                        selected.insert(path.clone());
                    }
                }
            }
            // Also check if `norm` is a source file (or source directory) and expand affected tests via AST graph
            if self.files.contains_key(&norm) {
                let affected = self.affected_tests(std::slice::from_ref(&norm));
                let crate_dir = if norm.ends_with(".rs") || norm.ends_with("/Cargo.toml") {
                    rust_crate_dir(&norm)
                } else {
                    String::new()
                };
                if !crate_dir.is_empty() {
                    let crate_prefix = format!("{crate_dir}/");
                    let same_crate: Vec<String> = affected
                        .iter()
                        .filter(|a| a.test_file.starts_with(&crate_prefix))
                        .map(|a| a.test_file.clone())
                        .collect();
                    if !same_crate.is_empty() {
                        selected.extend(same_crate);
                    } else if affected.is_empty() {
                        for (test_path, _) in self
                            .dep_hash_for_test
                            .range(crate_prefix.clone()..)
                            .take_while(|(p, _)| p.starts_with(&crate_prefix))
                        {
                            selected.insert(test_path.clone());
                        }
                    } else {
                        for a in affected {
                            selected.insert(a.test_file);
                        }
                    }
                } else {
                    for a in affected {
                        selected.insert(a.test_file);
                    }
                }
            } else {
                // Always expand affected tests for non-test source files under `norm/` even when
                // `norm/` also contains some test files (e.g. `src/test_util.py` + `src/feature.py`).
                let source_files: Vec<String> = self
                    .files
                    .range(dir_prefix.clone()..)
                    .take_while(|(p, _)| p.starts_with(&dir_prefix))
                    .map(|(p, _)| p.clone())
                    .collect();
                if !source_files.is_empty() {
                    let affected = self.affected_tests(&source_files);
                    let has_nested_cargo = self.files.contains_key(&format!("{norm}/Cargo.toml"));
                    if has_nested_cargo {
                        let same_crate: Vec<String> = affected
                            .iter()
                            .filter(|a| a.test_file.starts_with(&dir_prefix))
                            .map(|a| a.test_file.clone())
                            .collect();
                        if !same_crate.is_empty() {
                            selected.extend(same_crate);
                        } else {
                            for a in affected {
                                selected.insert(a.test_file);
                            }
                        }
                    } else {
                        for a in affected {
                            selected.insert(a.test_file);
                        }
                    }
                } else if !norm.contains('/')
                    && !norm.ends_with(".py")
                    && !norm.ends_with(".rs")
                    && !is_frontend_source_path(&norm)
                {
                    // Exact test symbol match (e.g. `test_add_unit` or `test_create_user`)
                    if let Some(matched_files) = self.symbol_to_test_files.get(&norm) {
                        for tf in matched_files {
                            selected.insert(tf.clone());
                        }
                    } else {
                        for test_path in self.dep_hash_for_test.keys() {
                            if self
                                .files
                                .get(test_path)
                                .is_some_and(|n| n.parsed.test_symbols.binary_search(&norm).is_ok())
                            {
                                selected.insert(test_path.clone());
                            }
                        }
                    }
                }
            }
        }
        selected.into_iter().collect()
    }

    pub fn suggest_closest_targets(&self, raw_targets: &[String]) -> Vec<String> {
        let include_symbols = raw_targets
            .iter()
            .any(|r| r.contains("::") || !r.trim_start_matches("./").contains('/'));
        let mut candidates = BTreeSet::new();
        for test_path in self.dep_hash_for_test.keys() {
            candidates.insert(test_path.clone());
            if include_symbols
                && let Some(node) = self.files.get(test_path)
            {
                for sym in &node.parsed.test_symbols {
                    candidates.insert(format!("{test_path}::{sym}"));
                }
            }
        }
        let mut scored: Vec<(usize, String)> = Vec::new();
        for raw in raw_targets {
            let query = raw.trim().trim_start_matches("./").replace('\\', "/");
            if query.is_empty() {
                continue;
            }
            let query_base = Path::new(&query)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&query);
            for cand in &candidates {
                let cand_base = Path::new(cand.split("::").next().unwrap_or(cand))
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(cand);
                let dist = edit_distance(query_base, cand_base).min(edit_distance(&query, cand));
                let bonus = usize::from(cand.contains(query_base) || query_base.contains(cand_base));
                let effective = dist.saturating_sub(bonus * 2);
                if effective <= 6 {
                    scored.push((effective, cand.clone()));
                }
            }
        }
        scored.sort();
        let mut suggestions = Vec::new();
        for (_, cand) in scored {
            if !suggestions.contains(&cand) {
                suggestions.push(cand);
                if suggestions.len() >= 5 {
                    break;
                }
            }
        }
        suggestions
    }
}


/// Computes iterative Tarjan Strongly Connected Components (in reverse topological order)
/// and propagates reachability bitsets bottom-up across the condensed DAG.
pub(super) fn compute_scc_reachability_bitsets(adj: &[Vec<u32>]) -> (Vec<u32>, Vec<Vec<u64>>) {
    let n = adj.len();
    let mut index = vec![u32::MAX; n];
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut tarjan_stack: Vec<u32> = Vec::with_capacity(n.min(1024));
    let mut node_to_scc = vec![u32::MAX; n];
    let mut scc_members: Vec<Vec<u32>> = Vec::new();
    let mut next_index = 0u32;
    let mut dfs_stack: Vec<(u32, usize)> = Vec::with_capacity(n.min(1024));

    for start in 0..n {
        if index[start] != u32::MAX {
            continue;
        }
        let s = start as u32;
        index[start] = next_index;
        lowlink[start] = next_index;
        next_index += 1;
        tarjan_stack.push(s);
        on_stack[start] = true;
        dfs_stack.push((s, 0));

        while let Some((v, edge_idx)) = dfs_stack.last_mut() {
            let v_u = *v as usize;
            if *edge_idx < adj[v_u].len() {
                let w = adj[v_u][*edge_idx];
                *edge_idx += 1;
                let w_u = w as usize;
                if index[w_u] == u32::MAX {
                    index[w_u] = next_index;
                    lowlink[w_u] = next_index;
                    next_index += 1;
                    tarjan_stack.push(w);
                    on_stack[w_u] = true;
                    dfs_stack.push((w, 0));
                } else if on_stack[w_u] && index[w_u] < lowlink[v_u] {
                    lowlink[v_u] = index[w_u];
                }
            } else {
                let finished = *v;
                let finished_u = finished as usize;
                dfs_stack.pop();
                if let Some(&(parent, _)) = dfs_stack.last() {
                    let p_u = parent as usize;
                    if lowlink[finished_u] < lowlink[p_u] {
                        lowlink[p_u] = lowlink[finished_u];
                    }
                }
                if lowlink[finished_u] == index[finished_u] {
                    let scc_id = scc_members.len() as u32;
                    let mut members = Vec::new();
                    while let Some(w) = tarjan_stack.pop() {
                        let w_u = w as usize;
                        on_stack[w_u] = false;
                        node_to_scc[w_u] = scc_id;
                        members.push(w);
                        if w == finished {
                            break;
                        }
                    }
                    scc_members.push(members);
                }
            }
        }
    }

    // Tarjan emits SCCs in reverse topological order: every outgoing edge goes to a lower SCC ID.
    let words = n.div_ceil(64);
    let mut scc_bits: Vec<Vec<u64>> = Vec::with_capacity(scc_members.len());
    for (scc_id, members) in scc_members.iter().enumerate() {
        let mut bits = vec![0u64; words];
        for &u in members {
            bits[(u as usize) / 64] |= 1u64 << ((u as usize) % 64);
        }
        for &u in members {
            for &w in &adj[u as usize] {
                let child_scc = node_to_scc[w as usize] as usize;
                if child_scc < scc_id {
                    let child_bits = &scc_bits[child_scc];
                    for (dst, &src) in bits.iter_mut().zip(child_bits.iter()) {
                        *dst |= src;
                    }
                }
            }
        }
        scc_bits.push(bits);
    }
    (node_to_scc, scc_bits)
}

pub(super) fn extract_sorted_ids_from_bitset(bits: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for (word_idx, &word) in bits.iter().enumerate() {
        let mut w = word;
        let base = (word_idx * 64) as u32;
        while w != 0 {
            let tz = w.trailing_zeros();
            out.push(base + tz);
            w &= w - 1;
        }
    }
    out
}

pub(super) fn contains_ascii_case_insensitive(haystack: &str, needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    if hay.len() < needle_lower.len() {
        return false;
    }
    hay.windows(needle_lower.len()).any(|win| {
        win.iter()
            .zip(needle_lower.iter())
            .all(|(&a, &b)| a.to_ascii_lowercase() == b)
    })
}

pub(super) fn edit_distance(a: &str, b: &str) -> usize {
    const MAX_LEN: usize = 128;
    const CUTOFF: usize = 8;
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() > MAX_LEN
        || b_bytes.len() > MAX_LEN
        || a_bytes.len().abs_diff(b_bytes.len()) > CUTOFF
    {
        return 99;
    }
    let m = b_bytes.len();
    let mut prev = [0usize; MAX_LEN + 1];
    let mut curr = [0usize; MAX_LEN + 1];
    for (j, slot) in prev.iter_mut().enumerate().take(m + 1) {
        *slot = j;
    }
    for (i, &ca) in a_bytes.iter().enumerate() {
        curr[0] = i + 1;
        let mut row_min = curr[0];
        for (j, &cb) in b_bytes.iter().enumerate() {
            let cost = usize::from(!ca.eq_ignore_ascii_case(&cb));
            let val = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
            curr[j + 1] = val;
            if val < row_min {
                row_min = val;
            }
        }
        if row_min > CUTOFF {
            return 99;
        }
        prev[..=m].copy_from_slice(&curr[..=m]);
    }
    prev[m]
}


