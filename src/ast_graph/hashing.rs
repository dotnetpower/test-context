use super::*;

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            conftest_for_test: BTreeMap::new(),
            file_index_to_path: Vec::new(),
            path_to_id: std::collections::HashMap::new(),
            forward_adj: Vec::new(),
            reverse_adj: Vec::new(),
            conftest_paths: BTreeSet::new(),
            symbol_to_test_files: BTreeMap::new(),
            transitive_dep_ids_for_test: BTreeMap::new(),
            dep_hash_for_test: BTreeMap::new(),
            config_hashes: BTreeMap::new(),
            test_file_patterns: vec!["test_*.py".to_string(), "*_test.py".to_string()],
            test_function_patterns: vec!["test_*".to_string()],
            testpaths: Vec::new(),
            testpaths_fallback_active: false,
            environment_hash: String::new(),
            non_python_environment_hash: String::new(),
            frontend_environment_hash: String::new(),
            referenced_data_dirs: BTreeSet::new(),
            runtime_deps_for_test: BTreeMap::new(),
            raw_embedded_files: BTreeSet::new(),
            required_watch_dirs: BTreeSet::new(),
            workspace_root: None,
            missing_data_ref_candidates: BTreeSet::new(),
            tsconfig_extends_rev: BTreeMap::new(),
            barrel_imports_by_importer: BTreeMap::new(),
            barrel_to_importers: BTreeMap::new(),
            last_side_effectful_barrels: BTreeSet::new(),
            discovery_budget_exceeded: false,
            untrackable_tests: BTreeSet::new(),
            skipped_oversized_inventory: false,
            unreadable_inventory_files: BTreeSet::new(),
            last_env_ref_digest: String::new(),
            local_python_roots: BTreeSet::new(),
            cached_conventions: None,
            persisted_edges_valid: false,
            persisted_runtime_deps: None,
            provider_structure_dirty: false,
            test_membership_dirty: false,
        }
    }

    pub(super) fn build_base_hasher(&self) -> Sha256 {
        let mut hasher = Sha256::new();
        hasher.update(b"tctx:merkle:v19\nenv:");
        hasher.update(self.environment_hash.as_bytes());
        hasher.update(b"\n");
        hasher
    }

    pub(super) fn build_non_python_base_hasher(&self) -> Sha256 {
        if self.non_python_environment_hash.is_empty() {
            return self.build_base_hasher();
        }
        let mut hasher = Sha256::new();
        hasher.update(b"tctx:merkle:v19\nenv:");
        hasher.update(self.non_python_environment_hash.as_bytes());
        hasher.update(b"\n");
        hasher
    }

    pub(super) fn compute_tsconfig_extends_rev(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut pkg_to_dir: BTreeMap<String, String> = BTreeMap::new();
        for (path, node) in &self.files {
            if !node.is_config {
                continue;
            }
            let dir = Path::new(path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            for sym in &node.parsed.defined_symbols {
                if let Some(pkg_name) = sym.strip_prefix("fe_pkg:") {
                    pkg_to_dir.insert(pkg_name.to_string(), dir.clone());
                }
            }
        }
        let mut direct: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (path, node) in &self.files {
            if !node.is_config && !path.ends_with(".json") && !path.ends_with(".jsonc") {
                continue;
            }
            for sym in &node.parsed.defined_symbols {
                if let Some(ext_rel) = strip_fe_extends_symbol(sym)
                    && let Some(target) =
                        resolve_tsconfig_extends_target(path, ext_rel, &self.files, &pkg_to_dir)
                {
                    direct.entry(target).or_default().insert(path.clone());
                }
            }
        }
        // Transitive closure up to 16 hops: base -> all configs that transitively extend base.
        let mut rev = direct.clone();
        for _ in 0..16 {
            let mut changed = false;
            let snapshot = rev.clone();
            for (base, children) in &snapshot {
                let mut extra = Vec::new();
                for child in children {
                    if let Some(grand_children) = snapshot.get(child) {
                        for gc in grand_children {
                            if !children.contains(gc) {
                                extra.push(gc.clone());
                            }
                        }
                    }
                }
                if !extra.is_empty() {
                    let entry = rev.entry(base.clone()).or_default();
                    for gc in extra {
                        if entry.insert(gc) {
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        rev
    }

    /// Hash runtime-read dependencies (recorded by the Python audit hook) AND Go `//go:embed`
    /// embedded files by raw `sha256` bytes so comment/whitespace edits to files read or embedded
    /// as data (e.g. `open("src/foo.py").read()`, `//go:embed assets/app.js`) always invalidate cache.
    pub(super) fn update_runtime_dep_raw_hashes(
        &self,
        hasher: &mut Sha256,
        test_path: &str,
        reachable_ids: &[u32],
    ) {
        if let Some(rt_deps) = self.runtime_deps_for_test.get(test_path) {
            for dep_path in rt_deps {
                if let Some(node) = self.files.get(dep_path) {
                    hasher.update(b"runtime_raw:");
                    hasher.update(dep_path.as_bytes());
                    hasher.update(b":");
                    hasher.update(node.sha256.as_bytes());
                    hasher.update(b"\n");
                }
            }
        }
        if !self.raw_embedded_files.is_empty() {
            for &dep_id in reachable_ids {
                if let Some(dep_path) = self.file_index_to_path.get(dep_id as usize)
                    && self.raw_embedded_files.contains(dep_path)
                    && let Some(node) = self.files.get(dep_path)
                {
                    hasher.update(b"embed_raw:");
                    hasher.update(dep_path.as_bytes());
                    hasher.update(b":");
                    hasher.update(node.sha256.as_bytes());
                    hasher.update(b"\n");
                }
            }
        }
    }

    pub fn has_raw_or_data_watch_directives(&self) -> bool {
        !self.required_watch_dirs.is_empty()
            || !self.referenced_data_dirs.is_empty()
            || !self.raw_embedded_files.is_empty()
            || self
                .testpaths
                .iter()
                .any(|tp| tp.split('/').any(|seg| seg.starts_with('.')))
            || self.files.iter().any(|(_, node)| {
                node.parsed.defined_symbols.iter().any(|s| {
                    s.starts_with("rust_raw_embed:")
                        || s.starts_with("rust_raw_embed_crate:")
                        || s.starts_with("rust_raw_fs:")
                        || s.starts_with("py_raw_fs:")
                        || s.starts_with("fe_raw_fs:")
                        || s.starts_with("fe_raw_url_fs:")
                        || s.starts_with("fe_raw_import:")
                })
            })
    }

    pub fn matches_any_missing_data_ref(&self, rel_str: &str) -> bool {
        if self.missing_data_ref_candidates.contains(rel_str) {
            return true;
        }
        if !rel_str.is_empty() && !self.missing_data_ref_candidates.is_empty() {
            let dir_pfx = format!("{rel_str}/");
            if self
                .missing_data_ref_candidates
                .range(dir_pfx.clone()..)
                .next()
                .is_some_and(|cand| cand.starts_with(&dir_pfx))
            {
                return true;
            }
            // Also match files created inside a previously absent referenced directory
            // (e.g. `readdirSync("fixtures/out")` -> creating `fixtures/out/a.bin`).
            if self
                .missing_data_ref_candidates
                .iter()
                .any(|cand| !cand.is_empty() && rel_str.starts_with(&format!("{cand}/")))
            {
                return true;
            }
        }
        false
    }

    pub fn matches_any_watched_data_dir(&self, rel_str: &str) -> bool {
        !self.referenced_data_dirs.is_empty()
            && self.referenced_data_dirs.iter().any(|dir| {
                !dir.is_empty()
                    && (rel_str == dir || rel_str.starts_with(&format!("{dir}/")))
            })
    }

    pub(super) fn base_hasher_for_test(
        &self,
        test_path: &str,
        reachable_ids: &[u32],
        default_base: &Sha256,
        non_py_base: &Sha256,
    ) -> Sha256 {
        let mut hasher = if (test_path.ends_with(".rs") || is_frontend_source_path(test_path))
            && !self.non_python_environment_hash.is_empty()
            && !reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| p.ends_with(".py"))
            })
        {
            non_py_base.clone()
        } else {
            default_base.clone()
        };
        if is_frontend_source_path(test_path) && !self.frontend_environment_hash.is_empty() {
            hasher.update(b"fe_env:");
            hasher.update(self.frontend_environment_hash.as_bytes());
            hasher.update(b"\n");
        }
        hasher
    }

    pub(super) fn is_pyproject_relevant_to_non_py(
        &self,
        cfg_path: &str,
        is_rs_test: bool,
        reachable_ids: &[u32],
    ) -> bool {
        if !is_rs_test {
            return false;
        }
        let cfg_is_maturin_or_pyo3 = self.files.get(cfg_path).is_some_and(|node| {
            node.parsed.module_markers.iter().any(|m| m == "maturin")
                || node
                    .parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s.starts_with("pyo3:"))
        });
        if !cfg_is_maturin_or_pyo3 {
            return false;
        }
        reachable_ids.iter().any(|&id| {
            self.file_index_to_path
                .get(id as usize)
                .and_then(|p| self.files.get(p))
                .is_some_and(|n| {
                    n.path.ends_with(".py")
                        || n.parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s.starts_with("pyo3:"))
                })
        })
    }

    pub fn is_config_relevant_to_test_ids(
        &self,
        cfg_path: &str,
        test_path: &str,
        reachable_ids: &[u32],
    ) -> bool {
        let file_name = Path::new(cfg_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let is_cargo_dot_cfg =
            cfg_path == ".cargo/config.toml" || cfg_path == ".cargo/config" || cfg_path.ends_with("/.cargo/config.toml") || cfg_path.ends_with("/.cargo/config");
        let is_rust_only_cfg = RUST_CONFIG_FILES.contains(&file_name) || is_cargo_dot_cfg;
        let is_dotenv_cfg = crate::workspace::is_dotenv_filename(file_name);
        let is_fe_only_cfg = (crate::workspace::is_frontend_config_filename(file_name)
            && !is_dotenv_cfg)
            || self.tsconfig_extends_rev.contains_key(cfg_path);
        let is_py_only_cfg =
            PYTEST_CONFIG_FILES.contains(&file_name) && file_name != "pyproject.toml";
        let is_csharp_only_cfg = matches!(
            file_name,
            "Directory.Build.props" | "Directory.Build.targets" | "global.json"
        ) || file_name.ends_with(".csproj")
            || file_name.ends_with(".sln");
        let is_cpp_only_cfg = file_name == "CMakeLists.txt";

        let is_rs_test = test_path.ends_with(".rs");
        let is_fe_test = is_frontend_source_path(test_path);
        let is_py_test = test_path.ends_with(".py");
        let is_cs_test = test_path.ends_with(".cs");
        let is_cpp_test = is_c_cpp_source_path(test_path);

        if is_dotenv_cfg && !is_fe_test && !is_py_test {
            return false;
        }

        if is_py_only_cfg && (is_rs_test || is_fe_test || is_cs_test || is_cpp_test) {
            return false;
        }
        if file_name == "pyproject.toml"
            && (is_rs_test || is_fe_test || is_cs_test || is_cpp_test)
            && !self.is_pyproject_relevant_to_non_py(cfg_path, is_rs_test, reachable_ids)
        {
            return false;
        }
        if is_fe_only_cfg && !is_fe_test {
            return false;
        }
        if is_csharp_only_cfg && !is_cs_test {
            let depends_on_cs = reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| p.ends_with(".cs"))
            });
            if !depends_on_cs {
                return false;
            }
        }
        if is_cpp_only_cfg && !is_cpp_test {
            let depends_on_cpp = reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| is_c_cpp_source_path(p))
            });
            if !depends_on_cpp {
                return false;
            }
        }
        if is_rust_only_cfg && !is_rs_test {
            let depends_on_rs = reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| p.ends_with(".rs"))
            });
            if !depends_on_rs {
                return false;
            }
        }

        let raw_parent = Path::new(cfg_path)
            .parent()
            .map(normalize_rel_str)
            .unwrap_or_default();
        let scope_dir = if raw_parent == ".cargo" {
            ""
        } else if let Some(stripped) = raw_parent.strip_suffix("/.cargo") {
            stripped
        } else {
            raw_parent.as_str()
        };
        if scope_dir.is_empty() {
            return true;
        }
        let prefix = format!("{scope_dir}/");
        if test_path.starts_with(&prefix)
            || reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| p.starts_with(&prefix))
            })
        {
            return true;
        }
        // If another tsconfig extends this base config (e.g. `apps/web/tsconfig.json` extends
        // `configs/tsconfig.base.json`), propagate relevance through the extending child's scope.
        if let Some(extenders) = self.tsconfig_extends_rev.get(cfg_path) {
            for child_cfg in extenders {
                let child_scope = Path::new(child_cfg)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                if child_scope.is_empty() {
                    return true;
                }
                let child_prefix = format!("{child_scope}/");
                if test_path.starts_with(&child_prefix)
                    || reachable_ids.iter().any(|&id| {
                        self.file_index_to_path
                            .get(id as usize)
                            .is_some_and(|p| p.starts_with(&child_prefix))
                    })
                {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn update_relevant_config_hashes(
        &self,
        hasher: &mut Sha256,
        test_path: &str,
        reachable_ids: &[u32],
    ) {
        let is_fe_test_top = is_frontend_source_path(test_path);
        // When a Rust test (or any Rust dependency it uses) contains a location macro
        // (`line!()` / `column!()` / `file!()`), `#[track_caller]`, or `Location::caller()`,
        // raw-hash EVERY reachable `.rs` file so blank/comment line shifts at macro/caller
        // invocation sites always invalidate the test cache.
        if test_path.ends_with(".rs") {
            let any_rs_caller_sensitive = reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .and_then(|p| self.files.get(p))
                    .is_some_and(|n| {
                        n.parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s == "rust_caller_sensitive")
                    })
            });
            if any_rs_caller_sensitive {
                for &id in reachable_ids {
                    if let Some(p) = self.file_index_to_path.get(id as usize)
                        && p.ends_with(".rs")
                        && let Some(n) = self.files.get(p)
                    {
                        hasher.update(b"rs_raw_caller:");
                        hasher.update(p.as_bytes());
                        hasher.update(b":");
                        hasher.update(n.sha256.as_bytes());
                        hasher.update(b"\n");
                    }
                }
            }
        }
        // When a Python test (or any Python helper it calls) inspects call-stack line numbers or
        // source text via `inspect.currentframe()`/`stack()`/`getsource()`/`sys._getframe()`,
        // raw-hash EVERY reachable `.py` file so blank/comment line shifts invalidate the cache.
        if test_path.ends_with(".py") {
            let any_py_loc_sensitive = reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .and_then(|p| self.files.get(p))
                    .is_some_and(|n| {
                        n.parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s == "py_location_sensitive")
                    })
            });
            if any_py_loc_sensitive {
                for &id in reachable_ids {
                    if let Some(p) = self.file_index_to_path.get(id as usize)
                        && p.ends_with(".py")
                        && let Some(n) = self.files.get(p)
                    {
                        hasher.update(b"py_raw_caller:");
                        hasher.update(p.as_bytes());
                        hasher.update(b":");
                        hasher.update(n.sha256.as_bytes());
                        hasher.update(b"\n");
                    }
                }
            }
        }
        // Hash ONLY the exact pure-barrel `(barrel, sym)` re-export mappings actually traversed
        // by files reachable from `test_path` (O(barrels used), independent of `config_hashes`).
        if is_fe_test_top && !self.barrel_imports_by_importer.is_empty() {
            let mut seen_pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
            for &id in reachable_ids {
                if let Some(pairs) = self.barrel_imports_by_importer.get(&id) {
                    for (barrel_path, sym) in pairs {
                        if !seen_pairs.insert((barrel_path.as_str(), sym.as_str())) {
                            continue;
                        }
                        if let Some(bnode) = self.files.get(barrel_path) {
                            let prefix = format!("reexport:{sym}->");
                            for sym_def in &bnode.parsed.defined_symbols {
                                if sym_def.starts_with(&prefix) {
                                    hasher.update(b"barrel_reexport:");
                                    hasher.update(barrel_path.as_bytes());
                                    hasher.update(b":");
                                    hasher.update(sym_def.as_bytes());
                                    hasher.update(b"\n");
                                }
                            }
                        }
                    }
                }
            }
        }
        if self.config_hashes.is_empty()
            && !(is_fe_test_top && !self.tsconfig_extends_rev.is_empty())
        {
            return;
        }
        let mut reachable_dirs = BTreeSet::new();
        let mut curr = Path::new(test_path).parent();
        while let Some(dir) = curr {
            let s = normalize_rel_str(dir);
            if s.is_empty() || !reachable_dirs.insert(s) {
                break;
            }
            curr = dir.parent();
        }
        let mut depends_on_rs = test_path.ends_with(".rs");
        for &id in reachable_ids {
            if let Some(p) = self.file_index_to_path.get(id as usize) {
                if p.ends_with(".rs") {
                    depends_on_rs = true;
                }
                let mut d = Path::new(p.as_str()).parent();
                while let Some(dir) = d {
                    let s = normalize_rel_str(dir);
                    if s.is_empty() || !reachable_dirs.insert(s) {
                        break;
                    }
                    d = dir.parent();
                }
            }
        }
        let is_rs_test = test_path.ends_with(".rs");
        let is_fe_test = is_frontend_source_path(test_path);
        let is_py_test = test_path.ends_with(".py");
        let is_cs_test = test_path.ends_with(".cs");
        let is_cpp_test = is_c_cpp_source_path(test_path);
        let depends_on_cs = !is_cs_test
            && reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| p.ends_with(".cs"))
            });
        let depends_on_cpp = !is_cpp_test
            && reachable_ids.iter().any(|&id| {
                self.file_index_to_path
                    .get(id as usize)
                    .is_some_and(|p| is_c_cpp_source_path(p))
            });

        for (cfg_path, cfg_sha) in &self.config_hashes {
            let file_name = Path::new(cfg_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let is_cargo_dot_cfg = cfg_path == ".cargo/config.toml"
                || cfg_path == ".cargo/config"
                || cfg_path.ends_with("/.cargo/config.toml")
                || cfg_path.ends_with("/.cargo/config");
            let is_rust_only_cfg = RUST_CONFIG_FILES.contains(&file_name) || is_cargo_dot_cfg;
            let is_dotenv_cfg = crate::workspace::is_dotenv_filename(file_name);
            let is_fe_only_cfg =
                crate::workspace::is_frontend_config_filename(file_name) && !is_dotenv_cfg;
            let is_py_only_cfg =
                PYTEST_CONFIG_FILES.contains(&file_name) && file_name != "pyproject.toml";
            let is_csharp_only_cfg = matches!(
                file_name,
                "Directory.Build.props" | "Directory.Build.targets" | "global.json"
            ) || file_name.ends_with(".csproj")
                || file_name.ends_with(".sln");
            let is_cpp_only_cfg = file_name == "CMakeLists.txt";

            if is_dotenv_cfg && !is_fe_test && !is_py_test {
                continue;
            }

            if is_py_only_cfg && (is_rs_test || is_fe_test || is_cs_test || is_cpp_test) {
                continue;
            }
            if file_name == "pyproject.toml"
                && (is_rs_test || is_fe_test || is_cs_test || is_cpp_test)
                && !self.is_pyproject_relevant_to_non_py(cfg_path, is_rs_test, reachable_ids)
            {
                continue;
            }
            if is_fe_only_cfg && !is_fe_test {
                continue;
            }
            if is_csharp_only_cfg && !is_cs_test && !depends_on_cs {
                continue;
            }
            if is_cpp_only_cfg && !is_cpp_test && !depends_on_cpp {
                continue;
            }
            if is_rust_only_cfg && !is_rs_test && !depends_on_rs {
                continue;
            }
            let raw_parent = Path::new(cfg_path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            let scope_dir = if raw_parent == ".cargo" {
                ""
            } else if let Some(stripped) = raw_parent.strip_suffix("/.cargo") {
                stripped
            } else {
                raw_parent.as_str()
            };
            let matches_scope = scope_dir.is_empty()
                || reachable_dirs.contains(scope_dir)
                || self.tsconfig_extends_rev.get(cfg_path).is_some_and(|extenders| {
                    extenders.iter().any(|child_cfg| {
                        let child_dir = Path::new(child_cfg)
                            .parent()
                            .map(normalize_rel_str)
                            .unwrap_or_default();
                        child_dir.is_empty() || reachable_dirs.contains(&child_dir)
                    })
                });
            if !matches_scope {
                continue;
            }
            hasher.update(b"cfg:");
            hasher.update(cfg_path.as_bytes());
            hasher.update(b":");
            hasher.update(cfg_sha.as_bytes());
            hasher.update(b"\n");
            // Also hash transitive JS/TS helper modules imported by a config file (e.g.
            // `vite.config.ts` -> `./settings.ts` -> `./shared.ts`) by exact raw bytes so
            // editing an imported config helper always invalidates affected frontend tests.
            if let Some(&cfg_id) = self.path_to_id.get(cfg_path)
                && !self.forward_adj.is_empty()
                && let Some(direct_deps) = self.forward_adj.get(cfg_id as usize)
                && !direct_deps.is_empty()
            {
                let mut seen_cfg_deps: BTreeSet<u32> = BTreeSet::new();
                let mut q: VecDeque<u32> = VecDeque::new();
                seen_cfg_deps.insert(cfg_id);
                for &d in direct_deps {
                    if seen_cfg_deps.insert(d) {
                        q.push_back(d);
                    }
                }
                let mut ordered_dep_paths: BTreeSet<&str> = BTreeSet::new();
                while let Some(cur) = q.pop_front() {
                    if let Some(p) = self.file_index_to_path.get(cur as usize) {
                        ordered_dep_paths.insert(p.as_str());
                    }
                    if let Some(nexts) = self.forward_adj.get(cur as usize) {
                        for &nx in nexts {
                            if seen_cfg_deps.insert(nx) {
                                q.push_back(nx);
                            }
                        }
                    }
                }
                for dep_p in ordered_dep_paths {
                    if let Some(dep_node) = self.files.get(dep_p) {
                        hasher.update(b"cfg_dep:");
                        hasher.update(dep_p.as_bytes());
                        hasher.update(b":");
                        hasher.update(dep_node.sha256.as_bytes());
                        hasher.update(b"\n");
                    }
                }
            }
        }
        // Also hash arbitrarily named base configs extended by a frontend tsconfig (e.g. `configs/base.json`)
        // that are indexed as `.json` files rather than listed in `config_hashes`.
        if is_fe_test {
            for (base_cfg, extenders) in &self.tsconfig_extends_rev {
                if self.config_hashes.contains_key(base_cfg) {
                    continue;
                }
                let matches_extender = extenders.iter().any(|child_cfg| {
                    let child_dir = Path::new(child_cfg)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    child_dir.is_empty() || reachable_dirs.contains(&child_dir)
                });
                if matches_extender
                    && let Some(node) = self.files.get(base_cfg)
                {
                    hasher.update(b"cfg:");
                    hasher.update(base_cfg.as_bytes());
                    hasher.update(b":");
                    hasher.update(node.sha256.as_bytes());
                    hasher.update(b"\n");
                }
            }
        }
    }

    pub(super) fn compute_env_ref_digest(&self) -> String {
        let mut hasher = Sha256::new();
        let mut keys = BTreeSet::new();
        for node in self.files.values() {
            for k in &node.parsed.env_vars_referenced {
                keys.insert(k.as_str());
            }
        }
        if keys.is_empty() {
            return String::new();
        }
        for key in keys {
            hasher.update(key.as_bytes());
            if let Some(val) = std::env::var_os(key) {
                hasher.update(b"=S:");
                hasher.update(val.as_encoded_bytes());
            } else {
                hasher.update(b"=U");
            }
            hasher.update(b"\0");
        }
        hex_encode(&hasher.finalize())
    }

    pub fn apply_environment_hash(&mut self, environment_hash: String) {
        let non_py = self.non_python_environment_hash.clone();
        self.apply_environment_hashes(environment_hash, non_py);
    }

    pub fn apply_environment_hashes(
        &mut self,
        environment_hash: String,
        non_python_environment_hash: String,
    ) {
        let fe_hash = self.frontend_environment_hash.clone();
        self.apply_all_environment_hashes(
            environment_hash,
            non_python_environment_hash,
            fe_hash,
        );
    }

    pub fn apply_all_environment_hashes(
        &mut self,
        environment_hash: String,
        non_python_environment_hash: String,
        frontend_environment_hash: String,
    ) {
        if self.environment_hash == environment_hash
            && self.non_python_environment_hash == non_python_environment_hash
            && self.frontend_environment_hash == frontend_environment_hash
        {
            return;
        }
        self.environment_hash = environment_hash;
        self.non_python_environment_hash = non_python_environment_hash;
        self.frontend_environment_hash = frontend_environment_hash;
        if self.file_index_to_path.len() != self.files.len() {
            self.recompute_edges();
        } else {
            self.recompute_all_test_hashes();
        }
    }

    pub(super) fn build_all_node_hash_bytes(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.file_index_to_path
            .iter()
            .map(|dep_path| {
                self.files
                    .get(dep_path)
                    .map(|dep_node| {
                        (
                            build_node_hash_bytes(dep_path, dep_node, true),
                            build_node_hash_bytes(dep_path, dep_node, false),
                        )
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Recomputes `untrackable_tests` across all indexed tests: marks any test whose
    /// transitive dependencies OR applicable frontend config files (`vite.config.*`, etc.)
    /// contain dynamic/untrackable imports or environment accesses (`fe_dynamic_import` /
    /// `fe_dynamic_env`). Called at the end of EVERY full, incremental, and hash-only update.
    pub fn refresh_untrackable_tests(&mut self) {
        self.untrackable_tests.clear();
        // Build `FrontendModuleIndex` lazily when ANY frontend config, frontend source, OR
        // stylesheet file has imports so we can fail closed on unresolved relative imports,
        // unresolved configured aliases (e.g. `@gen/*` -> gitignored file or `../shared`),
        // unresolved stylesheet partials, AND unresolved binary `?raw`/`?inline`/`?url` assets!
        let has_fe_with_imports = self.files.iter().any(|(p, n)| {
            (is_frontend_source_path(p)
                || is_stylesheet_path(p)
                || (n.is_config && is_js_or_ts_file_ext(p)))
                && !n.parsed.imports.is_empty()
        });
        let fe_idx_for_cfg = if has_fe_with_imports {
            Some(FrontendModuleIndex::build(&self.files))
        } else {
            None
        };
        let is_unresolved_code_rel_import = |mod_path: &str, mod_node: &FileNode| -> bool {
            let Some(ref fe_idx) = fe_idx_for_cfg else {
                return false;
            };
            // Scope alias matching ONLY to config directories that actually govern `mod_path`
            // (its ancestor dirs up to workspace root), so an alias like `"react"` in service A
            // never marks service B's ordinary external `react` import as unresolved/uncacheable!
            let mut ancestor_cfg_dirs: Vec<String> = Vec::new();
            let mut cur_dir = Path::new(mod_path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            loop {
                ancestor_cfg_dirs.push(cur_dir.clone());
                if cur_dir.is_empty() {
                    break;
                }
                cur_dir = Path::new(&cur_dir)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
            }
            let matches_configured_alias = |spec: &str| -> bool {
                let matches_pat = |pat: &str| -> bool {
                    if let Some((pfx, sfx)) = pat.split_once('*') {
                        spec.starts_with(pfx)
                            && spec.ends_with(sfx)
                            && spec.len() >= pfx.len() + sfx.len()
                    } else if pat.ends_with('/') {
                        spec.starts_with(pat)
                    } else {
                        spec == pat || spec.starts_with(&format!("{pat}/"))
                    }
                };
                spec.starts_with('#')
                    || spec.starts_with("@/")
                    || spec.starts_with("~/")
                    || ancestor_cfg_dirs.iter().any(|ds| {
                        fe_idx
                            .vite_aliases_by_dir
                            .get(ds)
                            .is_some_and(|aliases| aliases.iter().any(|(pat, _, _)| matches_pat(pat)))
                            || fe_idx
                                .aliases_by_dir
                                .get(ds)
                                .is_some_and(|aliases| aliases.iter().any(|(pat, _, _)| matches_pat(pat)))
                    })
            };
            let matches_ambiguous_cwd_alias = |spec: &str| -> bool {
                ancestor_cfg_dirs.iter().any(|ds| {
                    fe_idx.ambiguous_cwd_aliases_by_dir.get(ds).is_some_and(|pats| {
                        pats.iter().any(|pat| {
                            if let Some((pfx, sfx)) = pat.split_once('*') {
                                spec.starts_with(pfx)
                                    && spec.ends_with(sfx)
                                    && spec.len() >= pfx.len() + sfx.len()
                            } else if pat.ends_with('/') {
                                spec.starts_with(pat.as_str())
                            } else {
                                spec == pat || spec.starts_with(&format!("{pat}/"))
                            }
                        })
                    })
                })
            };
            let nearest_vite_root_is_ambiguous = ancestor_cfg_dirs
                .iter()
                .find(|ds| fe_idx.vite_root_by_dir.contains_key(ds.as_str()))
                .is_some_and(|ds| fe_idx.ambiguous_vite_root_dirs.contains(ds.as_str()));
            let has_unindexed_ts = self.workspace_root.as_deref().is_some_and(|ws_root| {
                fe_idx.has_unindexed_nearest_tsconfig(mod_path, &self.files, ws_root)
            });
            mod_node.parsed.imports.iter().any(|imp| {
                let is_rel = imp.module.starts_with("./") || imp.module.starts_with("../");
                let is_root_rel = imp.module.starts_with('/') && !imp.module.starts_with("//");
                let is_alias = !is_rel && !is_root_rel && matches_configured_alias(&imp.module);
                if !is_rel && !is_root_rel && !is_alias {
                    // An unindexed/gitignored nested `tsconfig.json`/`jsconfig.json` can override
                    // `paths` or `baseUrl` for any bare import: fail closed!
                    if has_unindexed_ts {
                        return true;
                    }
                    // Check governing `baseUrl` FIRST before workspace packages so an ignored
                    // `src/utils.ts` under `baseUrl: "./src"` can never be masked by a tracked
                    // workspace package named `utils`!
                    if let Some(ws_root) = self.workspace_root.as_deref()
                        && fe_idx.bare_import_masked_by_unindexed_baseurl_file(
                            mod_path,
                            &imp.module,
                            &self.files,
                            ws_root,
                        )
                    {
                        return true;
                    }
                    // Bare workspace-package import (`@scope/ui` or `@scope/ui/Button`): verify
                    // ALL potentially active conditional `exports` targets resolve to indexed
                    // non-declaration files (and never let `src/index.ts` mask an ignored export!).
                    if let Some(pkg_ok) = fe_idx.resolve_workspace_pkg_for_cache_check(
                        mod_path,
                        &imp.module,
                        &self.files,
                        self.workspace_root.as_deref(),
                    ) {
                        return !pkg_ok;
                    }
                    return false;
                }
                if is_alias && !imp.module.starts_with('#') && has_unindexed_ts {
                    return true;
                }
                if is_root_rel && nearest_vite_root_is_ambiguous {
                    // Nested relative Vite `root: './client'` depends on runner cwd (workspace
                    // root vs package dir); fail closed so an indexed file under the other root
                    // can never mask a gitignored file under the actual runner cwd!
                    return true;
                }
                if is_alias && matches_ambiguous_cwd_alias(&imp.module) {
                    // Cwd-relative Vite alias (`path.resolve('./generated')` without `__dirname`)
                    // depends on runner cwd; fail closed so a tracked target under one cwd can
                    // never mask an ignored target under the actual runner cwd!
                    return true;
                }
                // Check Vite aliases BEFORE `#imports` (matching Vite's `resolve.alias` >
                // `package.json` `#imports` precedence so Vite `#db -> actual.js` wins over
                // `package.json` `"#db": "./fallback.js"`!).
                if is_alias
                    && let Some(vite_ok) = fe_idx.resolve_vite_alias_for_cache_check(
                        mod_path,
                        &imp.module,
                        &self.files,
                        self.workspace_root.as_deref(),
                    )
                {
                    return !vite_ok;
                }
                if imp.module.starts_with('#') {
                    if let Some(ws_root) = self.workspace_root.as_deref()
                        && fe_idx.has_unindexed_nearest_package_json(mod_path, &self.files, ws_root)
                    {
                        // An unindexed/gitignored nested `package.json` sits closer to `mod_path`
                        // than any indexed `package.json` and may define `#imports`: fail closed!
                        return true;
                    }
                    // For `#imports` (including conditional objects like `{ "node": "./gen.js",
                    // "default": "./src.js" }`), fail closed if ANY potentially active conditional
                    // target of the winning pattern is unindexed!
                    return !fe_idx.resolve_subpath_for_cache_check(
                        mod_path,
                        &imp.module,
                        &self.files,
                        self.workspace_root.as_deref(),
                    );
                }
                if is_alias
                    && let Some(alias_ok) = fe_idx.resolve_alias_for_cache_check(
                        mod_path,
                        &imp.module,
                        &self.files,
                        self.workspace_root.as_deref(),
                    )
                {
                    return !alias_ok;
                }
                // Any relative, Vite root-relative (`/src/logo.png`), or alias import (including
                // images/fonts/media/wasm assets) that does not resolve to a non-declaration
                // indexed workspace file cannot be content-hashed, so fail closed. Also fail
                // closed if an explicit `.js`/`.jsx`/`.mjs`/`.cjs` specifier remapped to `.ts`/
                // `.tsx`/`.mts`/`.cts` while an unindexed/gitignored `.js` file ACTUALLY EXISTS
                // on disk at that exact path (since Vite/Node loads the `.js` file first!).
                let targets = fe_idx.resolve_raw_specifier(mod_path, &imp.module, &self.files);
                if targets.is_empty()
                    || targets
                        .iter()
                        .all(|t| t.ends_with(".d.ts") || t.ends_with(".d.mts") || t.ends_with(".d.cts"))
                {
                    return true;
                }
                if let Some(ws_root) = self.workspace_root.as_deref()
                    && fe_idx.remapped_js_masked_by_unindexed_disk_file(
                        mod_path,
                        &imp.module,
                        &targets,
                        &self.files,
                        ws_root,
                    )
                {
                    return true;
                }
                false
            })
        };
        let dynamic_fe_configs: Vec<(String, String)> = self
            .files
            .iter()
            .filter(|(p, n)| {
                if !n.is_config {
                    return false;
                }
                let self_dynamic = n
                    .parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s == "fe_dynamic_import" || s == "fe_dynamic_env");
                if self_dynamic {
                    return true;
                }
                // Validate EVERY relative, root-relative, and configured-alias import (e.g.
                // `#settings`, `@/config`) of the config file AND every transitive helper module
                // in its dependency closure: if ANY fails to resolve to an indexed file OR any
                // transitive helper is dynamic, fail closed for governed frontend tests.
                if is_unresolved_code_rel_import(p, n) {
                    return true;
                }
                if let Some(&cfg_id) = self.path_to_id.get(p.as_str())
                    && let Some(direct) = self.forward_adj.get(cfg_id as usize)
                {
                    let mut seen: BTreeSet<u32> = BTreeSet::new();
                    let mut q: VecDeque<u32> = VecDeque::new();
                    seen.insert(cfg_id);
                    for &d in direct {
                        if seen.insert(d) {
                            q.push_back(d);
                        }
                    }
                    while let Some(cur) = q.pop_front() {
                        if let Some(cp) = self.file_index_to_path.get(cur as usize)
                            && let Some(cn) = self.files.get(cp)
                            && (cn
                                .parsed
                                .defined_symbols
                                .iter()
                                .any(|s| s == "fe_dynamic_import" || s == "fe_dynamic_env")
                                || is_unresolved_code_rel_import(cp, cn))
                        {
                            return true;
                        }
                        if let Some(nexts) = self.forward_adj.get(cur as usize) {
                            for &nx in nexts {
                                if seen.insert(nx) {
                                    q.push_back(nx);
                                }
                            }
                        }
                    }
                }
                false
            })
            .map(|(p, _)| {
                let dir = Path::new(p)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                (p.clone(), dir)
            })
            .collect();
        for (test_path, reachable_ids) in &self.transitive_dep_ids_for_test {
            let has_untrackable_dep = reachable_ids.iter().any(|&dep_id| {
                self.file_index_to_path
                    .get(dep_id as usize)
                    .and_then(|p| self.files.get(p).map(|n| (p, n)))
                    .is_some_and(|(p, n)| {
                        n.parsed.defined_symbols.iter().any(|s| {
                            s == "fe_dynamic_import"
                                || s == "fe_dynamic_env"
                                || s == "py_dynamic_env"
                                || s == "py_dynamic_import"
                                || s == "rust_dynamic_io"
                                || s == "rust_dynamic_env"
                                || s == "rust_dynamic_include"
                        }) || ((is_frontend_source_path(p) || is_stylesheet_path(p))
                            && is_unresolved_code_rel_import(p, n))
                    })
            });
            let has_untrackable_cfg = !dynamic_fe_configs.is_empty()
                && is_frontend_source_path(test_path)
                && dynamic_fe_configs.iter().any(|(_, cfg_dir)| {
                    cfg_dir.is_empty()
                        || test_path.starts_with(&format!("{cfg_dir}/"))
                        || matches!(cfg_dir.as_str(), "config" | "configs" | ".config")
                        || cfg_dir.ends_with("/config")
                        || cfg_dir.ends_with("/configs")
                });
            if has_untrackable_dep || has_untrackable_cfg {
                self.untrackable_tests.insert(test_path.clone());
            }
        }
    }

    pub(super) fn recompute_all_test_hashes(&mut self) {
        self.refresh_untrackable_tests();
        let base_hasher = self.build_base_hasher();
        let non_py_base_hasher = self.build_non_python_base_hasher();
        let mut digest_cache: std::collections::HashMap<(u32, bool), Vec<u8>> =
            std::collections::HashMap::new();
        for (test_path, reachable_ids) in &self.transitive_dep_ids_for_test {
            let self_id = self.path_to_id.get(test_path).copied();
            let mut hasher = self.base_hasher_for_test(
                test_path,
                reachable_ids,
                &base_hasher,
                &non_py_base_hasher,
            );
            self.update_relevant_config_hashes(&mut hasher, test_path, reachable_ids);
            self.update_runtime_dep_raw_hashes(&mut hasher, test_path, reachable_ids);
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
            self.dep_hash_for_test.insert(test_path.clone(), dep_hash);
        }
    }

    pub fn apply_runtime_dependencies(
        &mut self,
        workspace: &Workspace,
        policy: &MemoryPolicy,
        test_file: &str,
        dependencies: &[String],
    ) -> bool {
        let mut normalized = self
            .runtime_deps_for_test
            .get(test_file)
            .cloned()
            .unwrap_or_default();
        for dependency in dependencies {
            let rel = Path::new(dependency);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                continue;
            }
            let rel_string = normalize_rel_str(rel);
            if rel_string == test_file {
                continue;
            }
            if !self.files.contains_key(&rel_string) {
                let full = workspace.root.join(rel);
                let Some(stamp) = file_stamp(&full) else {
                    continue;
                };
                let Ok(bytes) = workspace.read_known_with_limit(rel, policy.max_file_bytes) else {
                    continue;
                };
                self.insert_file_bytes_with_stamp(&rel_string, &bytes, Some(stamp));
            }
            normalized.insert(rel_string);
        }
        let changed = self.runtime_deps_for_test.get(test_file) != Some(&normalized);
        if changed {
            self.runtime_deps_for_test
                .insert(test_file.to_string(), normalized);
        }
        changed
    }

}

