use crate::policy::MemoryPolicy;
use crate::workspace::{DATA_FILE_EXTENSIONS, PYTEST_CONFIG_FILES, Workspace};
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub size: u64,
    pub ino: u64,
}

pub fn file_stamp(full_path: &Path) -> Option<FileStamp> {
    let meta = std::fs::symlink_metadata(full_path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    Some(FileStamp {
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        size: meta.len(),
        ino: meta.ino(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportSpec {
    pub level: usize,
    pub module: String,
    pub symbols: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedPythonFile {
    pub imports: Vec<ImportSpec>,
    pub test_symbols: Vec<String>,
    pub fixtures_defined: Vec<String>,
    pub fixtures_used: Vec<String>,
    pub defined_symbols: Vec<String>,
    pub data_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub sha256: String,
    pub stamp: Option<FileStamp>,
    pub is_test: bool,
    pub is_conftest: bool,
    pub is_config: bool,
    pub parsed: ParsedPythonFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectedTest {
    pub test_file: String,
    pub dep_hash: String,
    pub test_symbols: Vec<String>,
    pub reason_chain: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileGraphInfo {
    pub path: String,
    pub sha256: String,
    pub is_test: bool,
    pub is_conftest: bool,
    pub dep_hash: Option<String>,
    pub direct_imports: Vec<String>,
    pub direct_imported_by: Vec<String>,
    pub transitive_dependencies: Vec<String>,
    pub conftest_chain: Vec<String>,
    pub affected_tests: Vec<String>,
    pub test_symbols: Vec<String>,
    pub fixtures_defined: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DependencyGraph {
    pub files: BTreeMap<String, FileNode>,
    pub direct_imports: BTreeMap<String, BTreeSet<String>>,
    pub reverse_imports: BTreeMap<String, BTreeSet<String>>,
    pub conftest_for_test: BTreeMap<String, Vec<String>>,
    pub file_index_to_path: Vec<String>,
    pub transitive_dep_ids_for_test: BTreeMap<String, Vec<u32>>,
    pub dep_hash_for_test: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            direct_imports: BTreeMap::new(),
            reverse_imports: BTreeMap::new(),
            conftest_for_test: BTreeMap::new(),
            file_index_to_path: Vec::new(),
            transitive_dep_ids_for_test: BTreeMap::new(),
            dep_hash_for_test: BTreeMap::new(),
            config_hashes: BTreeMap::new(),
        }
    }

    pub fn build(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let inventory = workspace.files_limited(policy)?;
        let mut graph = Self::new();
        let mut total_bytes = 0usize;
        for rel_path in &inventory.files {
            let full_path = workspace.root.join(rel_path);
            let stamp = file_stamp(&full_path);
            let bytes = match workspace.read_known_with_limit(rel_path, policy.max_file_bytes) {
                Ok(b) => b,
                Err(_) => continue,
            };
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > policy.max_source_bytes {
                return Err(invalid("workspace exceeds max_source_bytes policy limit").into());
            }
            let rel_str = normalize_rel_str(rel_path);
            graph.insert_file_bytes_with_stamp(&rel_str, &bytes, stamp);
        }
        graph.discover_referenced_data_files(workspace, policy);
        graph.recompute_edges();
        Ok(graph)
    }

    pub fn discover_referenced_data_files(&mut self, workspace: &Workspace, policy: &MemoryPolicy) {
        let mut candidates = BTreeSet::new();
        for (py_path, node) in &self.files {
            if node.parsed.data_refs.is_empty() {
                continue;
            }
            let parent = Path::new(py_path).parent().unwrap_or_else(|| Path::new(""));
            for data_ref in &node.parsed.data_refs {
                let root_cand = Path::new(data_ref).to_path_buf();
                if workspace.is_permitted_data_rel(&root_cand) {
                    candidates.insert(normalize_rel_str(&root_cand));
                }
                if !parent.as_os_str().is_empty() {
                    let rel_cand = parent.join(data_ref);
                    if workspace.is_permitted_data_rel(&rel_cand) {
                        candidates.insert(normalize_rel_str(&rel_cand));
                    }
                }
            }
        }
        for rel_str in candidates {
            if self.files.contains_key(&rel_str) {
                continue;
            }
            let rel_path = Path::new(&rel_str);
            let full_path = workspace.root.join(rel_path);
            if let Some(stamp) = file_stamp(&full_path)
                && let Ok(bytes) = workspace.read_known_with_limit(rel_path, policy.max_file_bytes)
            {
                let sha = sha256_hex(&bytes);
                self.files.insert(
                    rel_str.clone(),
                    FileNode {
                        path: rel_str,
                        sha256: sha,
                        stamp: Some(stamp),
                        is_test: false,
                        is_conftest: false,
                        is_config: false,
                        parsed: ParsedPythonFile::default(),
                    },
                );
            }
        }
    }

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
        let sha = sha256_hex(bytes);
        let file_name = Path::new(rel_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if PYTEST_CONFIG_FILES.contains(&file_name) {
            let is_new = !self.config_hashes.contains_key(rel_str);
            self.config_hashes.insert(rel_str.to_string(), sha.clone());
            self.files.insert(
                rel_str.to_string(),
                FileNode {
                    path: rel_str.to_string(),
                    sha256: sha,
                    stamp,
                    is_test: false,
                    is_conftest: false,
                    is_config: true,
                    parsed: ParsedPythonFile::default(),
                },
            );
            return is_new;
        }
        if !file_name.ends_with(".py") {
            let is_new = !self.files.contains_key(rel_str);
            self.files.insert(
                rel_str.to_string(),
                FileNode {
                    path: rel_str.to_string(),
                    sha256: sha,
                    stamp,
                    is_test: false,
                    is_conftest: false,
                    is_config: false,
                    parsed: ParsedPythonFile::default(),
                },
            );
            return is_new;
        }
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_python_source(&source);
        let is_conftest = file_name == "conftest.py";
        let is_test = !is_conftest && is_test_filename(file_name);
        let structure_changed = match self.files.get(rel_str) {
            Some(existing) => {
                existing.is_test != is_test
                    || existing.is_conftest != is_conftest
                    || existing.parsed != parsed
            }
            None => true,
        };
        self.files.insert(
            rel_str.to_string(),
            FileNode {
                path: rel_str.to_string(),
                sha256: sha,
                stamp,
                is_test,
                is_conftest,
                is_config: false,
                parsed,
            },
        );
        structure_changed
    }

    /// Fast O(affected) Merkle hash update when file contents changed but import/fixture structure did not change.
    pub fn recompute_hashes_for_changed(&mut self, changed_files: &[String]) {
        if self.file_index_to_path.len() != self.files.len() {
            self.recompute_edges();
            return;
        }
        let mut cfg_prefix = Vec::new();
        cfg_prefix.extend_from_slice(b"tctx:merkle:v1\n");
        for (cfg_path, cfg_sha) in &self.config_hashes {
            cfg_prefix.extend_from_slice(b"cfg:");
            cfg_prefix.extend_from_slice(cfg_path.as_bytes());
            cfg_prefix.extend_from_slice(b":");
            cfg_prefix.extend_from_slice(cfg_sha.as_bytes());
            cfg_prefix.extend_from_slice(b"\n");
        }

        let affected: Vec<String> = self
            .affected_tests(changed_files)
            .into_iter()
            .map(|a| a.test_file)
            .collect();

        for test_path in affected {
            let Some(reachable_ids) = self.transitive_dep_ids_for_test.get(&test_path) else {
                continue;
            };
            let mut hasher = Sha256::new();
            hasher.update(&cfg_prefix);
            for &dep_id in reachable_ids {
                if let Some(dep_path) = self.file_index_to_path.get(dep_id as usize)
                    && let Some(dep_node) = self.files.get(dep_path)
                {
                    hasher.update(b"py:");
                    hasher.update(dep_path.as_bytes());
                    hasher.update(b":");
                    hasher.update(dep_node.sha256.as_bytes());
                    hasher.update(b"\n");
                }
            }
            let dep_hash = hex_encode(&hasher.finalize());
            self.dep_hash_for_test.insert(test_path, dep_hash);
        }
    }

    pub fn remove_file(&mut self, rel_str: &str) -> bool {
        let removed_cfg = self.config_hashes.remove(rel_str).is_some();
        let removed_file = self.files.remove(rel_str).is_some();
        removed_cfg || removed_file
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
    pub fn recompute_edges(&mut self) {
        self.direct_imports.clear();
        self.reverse_imports.clear();
        self.conftest_for_test.clear();
        self.file_index_to_path.clear();
        self.transitive_dep_ids_for_test.clear();
        self.dep_hash_for_test.clear();

        let mut path_to_id: BTreeMap<String, u32> = BTreeMap::new();
        for (idx, path) in self.files.keys().enumerate() {
            let id = u32::try_from(idx).unwrap_or(u32::MAX);
            self.file_index_to_path.push(path.clone());
            path_to_id.insert(path.clone(), id);
        }

        let module_index = ModuleIndex::build(&self.files);
        let fixture_providers = build_fixture_provider_index(&self.files);

        for (path, node) in &self.files {
            if node.is_config {
                continue;
            }
            let mut deps = BTreeSet::new();
            for spec in &node.parsed.imports {
                for resolved in module_index.resolve_import(path, spec, &self.files) {
                    if &resolved != path {
                        deps.insert(resolved);
                    }
                }
            }
            for fixture_name in &node.parsed.fixtures_used {
                if let Some(providers) = fixture_providers.get(fixture_name) {
                    for provider in providers {
                        if provider != path && is_provider_applicable(provider, path) {
                            deps.insert(provider.clone());
                        }
                    }
                }
            }
            if !node.parsed.data_refs.is_empty() {
                let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
                for data_ref in &node.parsed.data_refs {
                    if self.files.contains_key(data_ref) && data_ref != path {
                        deps.insert(data_ref.clone());
                    }
                    if !parent.as_os_str().is_empty() {
                        let rel_cand = normalize_rel_str(&parent.join(data_ref));
                        if self.files.contains_key(&rel_cand) && &rel_cand != path {
                            deps.insert(rel_cand);
                        }
                    }
                }
            }
            for dep in &deps {
                self.reverse_imports
                    .entry(dep.clone())
                    .or_default()
                    .insert(path.clone());
            }
            self.direct_imports.insert(path.clone(), deps);
        }

        // Build compact adjacency list in terms of u32 file IDs
        let num_files = self.file_index_to_path.len();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); num_files];
        for (src, targets) in &self.direct_imports {
            if let Some(&src_id) = path_to_id.get(src) {
                adj[src_id as usize] = targets
                    .iter()
                    .filter_map(|t| path_to_id.get(t).copied())
                    .collect();
            }
        }

        let conftests: BTreeSet<String> = self
            .files
            .values()
            .filter(|n| n.is_conftest)
            .map(|n| n.path.clone())
            .collect();

        let test_files: Vec<String> = self
            .files
            .values()
            .filter(|n| n.is_test)
            .map(|n| n.path.clone())
            .collect();

        let mut cfg_prefix = Vec::new();
        cfg_prefix.extend_from_slice(b"tctx:merkle:v1\n");
        for (cfg_path, cfg_sha) in &self.config_hashes {
            cfg_prefix.extend_from_slice(b"cfg:");
            cfg_prefix.extend_from_slice(cfg_path.as_bytes());
            cfg_prefix.extend_from_slice(b":");
            cfg_prefix.extend_from_slice(cfg_sha.as_bytes());
            cfg_prefix.extend_from_slice(b"\n");
        }

        let mut visited_stamp = vec![0u32; num_files];
        let mut epoch = 0u32;
        let mut queue = VecDeque::new();

        for test_path in test_files {
            epoch = epoch.wrapping_add(1);
            if epoch == 0 {
                visited_stamp.fill(0);
                epoch = 1;
            }
            let ancestor_conftests = ancestor_conftests_for(&test_path, &conftests);
            let mut reachable_ids = Vec::new();
            queue.clear();

            if let Some(&test_id) = path_to_id.get(&test_path) {
                visited_stamp[test_id as usize] = epoch;
                reachable_ids.push(test_id);
                queue.push_back(test_id);
            }
            for cf in &ancestor_conftests {
                if let Some(&cf_id) = path_to_id.get(cf)
                    && visited_stamp[cf_id as usize] != epoch
                {
                    visited_stamp[cf_id as usize] = epoch;
                    reachable_ids.push(cf_id);
                    queue.push_back(cf_id);
                }
            }

            while let Some(curr_id) = queue.pop_front() {
                for &next_id in &adj[curr_id as usize] {
                    if visited_stamp[next_id as usize] != epoch {
                        visited_stamp[next_id as usize] = epoch;
                        reachable_ids.push(next_id);
                        queue.push_back(next_id);
                    }
                }
            }

            reachable_ids.sort_unstable();

            let mut hasher = Sha256::new();
            hasher.update(&cfg_prefix);
            for &dep_id in &reachable_ids {
                let dep_path = &self.file_index_to_path[dep_id as usize];
                if let Some(dep_node) = self.files.get(dep_path) {
                    hasher.update(b"py:");
                    hasher.update(dep_path.as_bytes());
                    hasher.update(b":");
                    hasher.update(dep_node.sha256.as_bytes());
                    hasher.update(b"\n");
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

    pub fn test_files(&self) -> Vec<String> {
        self.dep_hash_for_test.keys().cloned().collect()
    }

    /// Returns safe 3rd-party packages imported across the workspace that can be pre-imported in the Warm Pytest Zygote
    /// without touching any workspace-local Python module.
    pub fn external_preload_modules(&self) -> Vec<String> {
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
        for node in self.files.values() {
            for spec in &node.parsed.imports {
                if spec.level > 0 || spec.module.is_empty() {
                    continue;
                }
                let top = spec.module.split('.').next().unwrap_or("");
                if top.is_empty() || local_roots.contains(top) || excluded.contains(&top) {
                    continue;
                }
                externals.insert(top.to_string());
                if matches!(top, "fastapi" | "azure" | "openai" | "pydantic" | "httpx" | "starlette")
                    && spec.module.split('.').count() <= 2
                {
                    externals.insert(spec.module.clone());
                }
            }
        }
        externals.into_iter().take(48).collect()
    }

    /// Computes all transitively affected Python files (both application modules and test files) for a list of modified paths.
    pub fn affected_modules(&self, changed_files: &[String]) -> Vec<String> {
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        for raw in changed_files {
            let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
            if !norm.is_empty() && visited.insert(norm.clone()) {
                queue.push_back(norm);
            }
        }
        while let Some(curr) = queue.pop_front() {
            if let Some(importers) = self.reverse_imports.get(&curr) {
                for importer in importers {
                    if visited.insert(importer.clone()) {
                        queue.push_back(importer.clone());
                    }
                }
            }
        }
        visited.into_iter().collect()
    }

    /// Computes affected test files and shortest dependency chains for a list of modified relative file paths.
    pub fn affected_tests(&self, changed_files: &[String]) -> Vec<AffectedTest> {
        let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
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

            // 1. Pytest global config change affects all tests
            if PYTEST_CONFIG_FILES.contains(&file_name) || self.config_hashes.contains_key(&norm) {
                for (test_path, dep_hash) in &self.dep_hash_for_test {
                    reasons
                        .entry(test_path.clone())
                        .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                    let _ = dep_hash;
                }
                continue;
            }

            // 2. conftest.py change affects all tests under its directory
            if file_name == "conftest.py" {
                let parent_dir = Path::new(&norm)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                for test_path in self.dep_hash_for_test.keys() {
                    if parent_dir.is_empty() || test_path.starts_with(&format!("{parent_dir}/")) {
                        reasons
                            .entry(test_path.clone())
                            .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                    }
                }
            }

            // 3. Reverse import graph traversal
            if visited.insert(norm.clone()) {
                queue.push_back((norm.clone(), vec![norm]));
            }
        }

        while let Some((curr, chain)) = queue.pop_front() {
            if let Some(node) = self.files.get(&curr) {
                if node.is_test {
                    reasons.entry(curr.clone()).or_insert_with(|| chain.clone());
                }
                if node.is_conftest {
                    let parent_dir = Path::new(&curr)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    for test_path in self.dep_hash_for_test.keys() {
                        if parent_dir.is_empty() || test_path.starts_with(&format!("{parent_dir}/"))
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
            if let Some(importers) = self.reverse_imports.get(&curr) {
                for importer in importers {
                    if visited.insert(importer.clone()) {
                        let mut next_chain = chain.clone();
                        next_chain.push(importer.clone());
                        queue.push_back((importer.clone(), next_chain));
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
        let direct_imports = self
            .direct_imports
            .get(&norm)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let direct_imported_by = self
            .reverse_imports
            .get(&norm)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let transitive_dependencies = self.transitive_deps_for(&norm);
        let conftest_chain = self
            .conftest_for_test
            .get(&norm)
            .cloned()
            .unwrap_or_default();
        let affected_tests = self
            .affected_tests(std::slice::from_ref(&norm))
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        Some(FileGraphInfo {
            path: norm.clone(),
            sha256: node.sha256.clone(),
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
        })
    }

    /// Resolves user/agent target arguments (which may be test files, directories, or source files)
    /// into concrete test file paths.
    pub fn resolve_target_test_files(&self, targets: &[String]) -> Vec<String> {
        if targets.is_empty() {
            return self.test_files();
        }
        let mut selected = BTreeSet::new();
        for raw_target in targets {
            // Strip pytest nodeid suffix like `tests/test_a.py::test_foo`
            let file_part = raw_target.split("::").next().unwrap_or(raw_target);
            let norm = file_part
                .trim()
                .trim_start_matches("./")
                .trim_end_matches('/')
                .replace('\\', "/");
            if norm.is_empty() || norm == "." {
                for t in self.dep_hash_for_test.keys() {
                    selected.insert(t.clone());
                }
                continue;
            }
            if self.dep_hash_for_test.contains_key(&norm) {
                selected.insert(norm);
                continue;
            }
            // Directory prefix match for tests under `norm/`
            let dir_prefix = format!("{norm}/");
            let mut matched_dir = false;
            for test_path in self.dep_hash_for_test.keys() {
                if test_path.starts_with(&dir_prefix) {
                    selected.insert(test_path.clone());
                    matched_dir = true;
                }
            }
            // Also check if `norm` is a source file (or source directory) and expand affected tests via AST graph
            if self.files.contains_key(&norm) {
                for affected in self.affected_tests(std::slice::from_ref(&norm)) {
                    selected.insert(affected.test_file);
                }
            } else if !matched_dir {
                // Source directory prefix
                let source_files: Vec<String> = self
                    .files
                    .keys()
                    .filter(|p| p.starts_with(&dir_prefix))
                    .cloned()
                    .collect();
                if !source_files.is_empty() {
                    for affected in self.affected_tests(&source_files) {
                        selected.insert(affected.test_file);
                    }
                }
            }
        }
        selected.into_iter().collect()
    }
}

pub fn is_test_filename(file_name: &str) -> bool {
    (file_name.starts_with("test_") && file_name.ends_with(".py"))
        || (file_name.ends_with("_test.py") && file_name.len() > "_test.py".len())
}

pub fn normalize_rel_str(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches("./")
        .replace('\\', "/")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn ancestor_conftests_for(test_path: &str, conftests: &BTreeSet<String>) -> Vec<String> {
    let mut result = Vec::new();
    if conftests.contains("conftest.py") {
        result.push("conftest.py".to_string());
    }
    let mut prefix = String::new();
    let parent = Path::new(test_path).parent().unwrap_or_else(|| Path::new(""));
    for comp in parent.components() {
        if let std::path::Component::Normal(os) = comp
            && let Some(seg) = os.to_str()
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let candidate = format!("{prefix}/conftest.py");
            if conftests.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    result
}

fn build_fixture_provider_index(
    files: &BTreeMap<String, FileNode>,
) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, node) in files {
        // Only conftest.py files implicitly provide fixtures without an explicit Python import
        if node.is_conftest && !node.parsed.fixtures_defined.is_empty() {
            for fx in &node.parsed.fixtures_defined {
                index.entry(fx.clone()).or_default().push(path.clone());
            }
        }
    }
    index
}

fn is_provider_applicable(provider_path: &str, consumer_path: &str) -> bool {
    let parent = Path::new(provider_path)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default();
    parent.is_empty() || consumer_path.starts_with(&format!("{parent}/"))
}

struct ModuleIndex {
    module_to_files: BTreeMap<String, BTreeSet<String>>,
    reexports: BTreeMap<(String, String), BTreeSet<String>>,
}

impl ModuleIndex {
    fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut module_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in files.keys() {
            if !path.ends_with(".py") {
                continue;
            }
            for dotted in candidate_module_names(path) {
                module_to_files
                    .entry(dotted)
                    .or_default()
                    .insert(path.clone());
            }
        }
        let mut index = Self {
            module_to_files,
            reexports: BTreeMap::new(),
        };
        // Two passes to resolve nested package `__init__.py` re-exports
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            for (path, node) in files {
                if !(path == "__init__.py" || path.ends_with("/__init__.py")) {
                    continue;
                }
                for spec in &node.parsed.imports {
                    if spec.symbols.is_empty() {
                        continue;
                    }
                    let targets = index.resolve_raw_import(path, spec, files);
                    for sym in &spec.symbols {
                        if sym == "*" {
                            continue;
                        }
                        let mut actual_targets = BTreeSet::new();
                        for t in &targets {
                            if (t == "__init__.py" || t.ends_with("/__init__.py"))
                                && let Some(forwarded) = index.reexports.get(&(t.clone(), sym.clone()))
                            {
                                actual_targets.extend(forwarded.iter().cloned());
                            } else {
                                actual_targets.insert(t.clone());
                            }
                        }
                        if !actual_targets.is_empty() {
                            next_reexports
                                .entry((path.clone(), sym.clone()))
                                .or_default()
                                .extend(actual_targets);
                        }
                    }
                }
            }
            index.reexports = next_reexports;
        }
        index
    }

    fn resolve_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw_targets = self.resolve_raw_import(importer_path, spec, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            return raw_targets;
        }

        let mut resolved = BTreeSet::new();
        for target in &raw_targets {
            if target == "__init__.py" || target.ends_with("/__init__.py") {
                let mut all_symbols_accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexported_files) =
                        self.reexports.get(&(target.clone(), sym.clone()))
                    {
                        resolved.extend(reexported_files.iter().cloned());
                    } else if files
                        .get(target)
                        .is_some_and(|n| n.parsed.defined_symbols.contains(sym))
                    {
                        resolved.insert(target.clone());
                    } else {
                        // Check if symbol was already resolved as a submodule file of this package
                        let pkg_dir = target.strip_suffix("/__init__.py").unwrap_or("");
                        let sub_py = if pkg_dir.is_empty() {
                            format!("{sym}.py")
                        } else {
                            format!("{pkg_dir}/{sym}.py")
                        };
                        let sub_init = if pkg_dir.is_empty() {
                            format!("{sym}/__init__.py")
                        } else {
                            format!("{pkg_dir}/{sym}/__init__.py")
                        };
                        if !raw_targets.contains(&sub_py) && !raw_targets.contains(&sub_init) {
                            all_symbols_accounted = false;
                        }
                    }
                }
                if !all_symbols_accounted {
                    resolved.insert(target.clone());
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    fn resolve_raw_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        if spec.level > 0 {
            // Relative import from importer_path
            let mut base_dir = Path::new(importer_path)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            for _ in 1..spec.level {
                base_dir = base_dir
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
            }
            let target_dir = if spec.module.is_empty() {
                base_dir
            } else {
                base_dir.join(spec.module.replace('.', "/"))
            };
            add_path_candidates(&target_dir, files, &mut resolved);
            for sym in &spec.symbols {
                if sym != "*" {
                    add_path_candidates(&target_dir.join(sym), files, &mut resolved);
                }
            }
        } else {
            // Absolute import
            if !spec.module.is_empty() {
                if let Some(matches) = self.module_to_files.get(&spec.module) {
                    resolved.extend(filter_by_package_locality(importer_path, matches));
                }
                // Same-directory fallback (e.g. `import helper` inside `tests/unit/test_foo.py`)
                if let Some(parent) = Path::new(importer_path).parent() {
                    let rel_target = parent.join(spec.module.replace('.', "/"));
                    add_path_candidates(&rel_target, files, &mut resolved);
                }
                for sym in &spec.symbols {
                    if sym != "*" {
                        let sub_mod = format!("{}.{}", spec.module, sym);
                        if let Some(matches) = self.module_to_files.get(&sub_mod) {
                            resolved.extend(filter_by_package_locality(importer_path, matches));
                        }
                        if let Some(parent) = Path::new(importer_path).parent() {
                            let rel_target =
                                parent.join(spec.module.replace('.', "/")).join(sym);
                            add_path_candidates(&rel_target, files, &mut resolved);
                        }
                    }
                }
            }
        }
        resolved
    }
}

fn service_compartment(rel_path: &str) -> Option<&str> {
    let mut parts = rel_path.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if matches!(first, "services" | "extensions" | "benchmarks" | "apps")
        || (first == "src" && second.contains('-'))
    {
        let len = first.len() + 1 + second.len();
        Some(&rel_path[..len])
    } else {
        None
    }
}

fn filter_by_package_locality(
    importer_path: &str,
    candidates: &BTreeSet<String>,
) -> BTreeSet<String> {
    if candidates.len() <= 1 {
        return candidates.clone();
    }
    if let Some(importer_comp) = service_compartment(importer_path) {
        let same_comp: BTreeSet<String> = candidates
            .iter()
            .filter(|c| service_compartment(c) == Some(importer_comp))
            .cloned()
            .collect();
        if !same_comp.is_empty() {
            return same_comp;
        }
        let non_foreign_service: BTreeSet<String> = candidates
            .iter()
            .filter(|c| match service_compartment(c) {
                Some(comp) => comp == importer_comp,
                None => true,
            })
            .cloned()
            .collect();
        if !non_foreign_service.is_empty() {
            return non_foreign_service;
        }
    }
    candidates.clone()
}

fn add_path_candidates(
    target_no_ext: &Path,
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let norm = normalize_rel_str(target_no_ext);
    let file_candidate = if norm.is_empty() {
        String::new()
    } else {
        format!("{norm}.py")
    };
    if !file_candidate.is_empty() && files.contains_key(&file_candidate) {
        resolved.insert(file_candidate);
    }
    let init_candidate = if norm.is_empty() {
        "__init__.py".to_string()
    } else {
        format!("{norm}/__init__.py")
    };
    if files.contains_key(&init_candidate) {
        resolved.insert(init_candidate);
    }
}

fn candidate_module_names(rel_py_path: &str) -> Vec<String> {
    let stripped = if let Some(s) = rel_py_path.strip_suffix("/__init__.py") {
        s
    } else if rel_py_path == "__init__.py" {
        return Vec::new();
    } else if let Some(s) = rel_py_path.strip_suffix(".py") {
        s
    } else {
        return Vec::new();
    };

    let parts: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let mut names = BTreeSet::new();
    // 1. Full path from repo root (when all segments are valid Python identifiers)
    if parts.iter().all(|s| is_valid_dotted_module(s)) {
        names.insert(parts.join("."));
    }

    // 2. If there is a `src` or `python` segment (e.g. `src/pkg/mod.py`, `services/foo/src/pkg/mod.py`, `packages/bar/src/pkg/mod.py`),
    // register suffix after `src` or `python`.
    for (idx, &seg) in parts.iter().enumerate() {
        if (seg == "src" || seg == "python") && idx + 1 < parts.len() {
            let tail = &parts[idx + 1..];
            if tail.iter().all(|s| is_valid_dotted_module(s)) {
                names.insert(tail.join("."));
            }
        }
    }

    // 3. Support polyglot service roots like `src/ai-service/routers/description_generator.py` or `services/order-svc/app/main.py`
    // where a parent directory segment is a hyphenated service folder or a top-level service container (`src/<svc>/...`, `services/<svc>/...`).
    for (idx, &seg) in parts.iter().enumerate() {
        if !is_valid_dotted_module(seg) && idx + 1 < parts.len() {
            let tail = &parts[idx + 1..];
            if tail.iter().all(|s| is_valid_dotted_module(s)) {
                names.insert(tail.join("."));
            }
        }
    }
    if parts.len() >= 2
        && matches!(parts[0], "src" | "services" | "apps" | "packages" | "extensions")
        && parts.get(2) != Some(&"src")
    {
        let tail = &parts[2..];
        if !tail.is_empty() && tail.iter().all(|s| is_valid_dotted_module(s)) {
            names.insert(tail.join("."));
        }
    }

    names.into_iter().collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PyToken {
    Ident(String),
    StringLit(String),
    Dot,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Assign,
    At,
    Star,
    Newline,
    Other,
}

fn tokenize_python(source: &str) -> Vec<PyToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::new();

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            b'\\' => {
                // Line continuation `\<newline>`
                i += 1;
                if i < len && bytes[i] == b'\r' {
                    i += 1;
                }
                if i < len && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                tokens.push(PyToken::Newline);
                i += 1;
            }
            b';' => {
                tokens.push(PyToken::Newline);
                i += 1;
            }
            b'.' => {
                tokens.push(PyToken::Dot);
                i += 1;
            }
            b',' => {
                tokens.push(PyToken::Comma);
                i += 1;
            }
            b'(' => {
                tokens.push(PyToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(PyToken::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(PyToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(PyToken::RBracket);
                i += 1;
            }
            b':' => {
                tokens.push(PyToken::Colon);
                i += 1;
            }
            b'=' => {
                if i + 1 < len && bytes[i + 1] == b'=' {
                    tokens.push(PyToken::Other);
                    i += 2;
                } else {
                    tokens.push(PyToken::Assign);
                    i += 1;
                }
            }
            b'@' => {
                tokens.push(PyToken::At);
                i += 1;
            }
            b'*' => {
                tokens.push(PyToken::Star);
                i += 1;
            }
            b'\'' | b'"' => {
                let (lit, next_i) = scan_string_literal(source, i, false);
                tokens.push(PyToken::StringLit(lit));
                i = next_i;
            }
            _ if is_ident_start(b) => {
                let start = i;
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let word = &source[start..i];
                // Check for string prefix like r"...", f"...", b"...", rf"..."
                if i < len
                    && (bytes[i] == b'\'' || bytes[i] == b'"')
                    && word.len() <= 2
                    && word
                        .bytes()
                        .all(|c| matches!(c, b'r' | b'R' | b'f' | b'F' | b'b' | b'B' | b'u' | b'U'))
                {
                    let is_fstring = word.bytes().any(|c| c == b'f' || c == b'F');
                    let (lit, next_i) = scan_string_literal(source, i, is_fstring);
                    tokens.push(PyToken::StringLit(lit));
                    i = next_i;
                } else {
                    tokens.push(PyToken::Ident(word.to_string()));
                }
            }
            _ => {
                tokens.push(PyToken::Other);
                i += 1;
            }
        }
    }
    tokens
}

fn scan_string_literal(source: &str, start_quote_idx: usize, is_fstring: bool) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let quote = bytes[start_quote_idx];
    let is_triple = start_quote_idx + 2 < len
        && bytes[start_quote_idx + 1] == quote
        && bytes[start_quote_idx + 2] == quote;
    let content_start = if is_triple {
        start_quote_idx + 3
    } else {
        start_quote_idx + 1
    };
    let mut i = content_start;
    let mut brace_depth = 0usize;
    while i < len {
        if bytes[i] == b'\\' {
            i = (i + 2).min(len);
            continue;
        }
        if is_fstring {
            if bytes[i] == b'{' {
                if i + 1 < len && bytes[i + 1] == b'{' {
                    i += 2;
                    continue;
                }
                brace_depth += 1;
                i += 1;
                continue;
            } else if bytes[i] == b'}' {
                if i + 1 < len && bytes[i + 1] == b'}' {
                    i += 2;
                    continue;
                }
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
                continue;
            }
        }
        if is_triple {
            if brace_depth == 0
                && i + 2 < len
                && bytes[i] == quote
                && bytes[i + 1] == quote
                && bytes[i + 2] == quote
            {
                let lit = source[content_start..i].to_string();
                return (lit, i + 3);
            }
            i += 1;
        } else {
            if (brace_depth == 0 && bytes[i] == quote) || bytes[i] == b'\n' {
                let lit = source[content_start..i].to_string();
                return (lit, (i + 1).min(len));
            }
            i += 1;
        }
    }
    (source[content_start..len].to_string(), len)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parses a Python source file to extract imports, test symbols, pytest fixtures, defined symbols, and data file references.
pub fn parse_python_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_python(source);
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut fixtures_defined = BTreeSet::new();
    let mut fixtures_used = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();

    for tok in &tokens {
        if let PyToken::StringLit(lit) = tok
            && let Some(clean_ref) = normalize_data_ref(lit)
        {
            data_refs.insert(clean_ref);
        }
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut at_statement_start = true;
    let mut pending_fixture_decorator = false;
    let mut pending_fixture_custom_name: Option<String> = None;

    while i < n {
        match &tokens[i] {
            PyToken::Newline => {
                at_statement_start = true;
                i += 1;
            }
            PyToken::At if at_statement_start => {
                // Decorator: check if `@pytest.fixture` / `@fixture` or `@pytest.mark.usefixtures(...)`
                i += 1;
                let mut deco_parts = Vec::new();
                while i < n {
                    match &tokens[i] {
                        PyToken::Ident(name) => {
                            deco_parts.push(name.as_str());
                            i += 1;
                        }
                        PyToken::Dot => {
                            i += 1;
                        }
                        _ => break,
                    }
                }
                let is_fixture_deco = deco_parts.last().copied() == Some("fixture");
                let is_usefixtures_deco = deco_parts.last().copied() == Some("usefixtures");
                if is_fixture_deco {
                    pending_fixture_decorator = true;
                }
                // Parse decorator arguments (including balanced parens)
                let mut paren_depth = 0usize;
                while i < n {
                    match &tokens[i] {
                        PyToken::LParen => {
                            paren_depth += 1;
                            i += 1;
                        }
                        PyToken::RParen => {
                            paren_depth = paren_depth.saturating_sub(1);
                            i += 1;
                        }
                        PyToken::Ident(arg_name)
                            if is_fixture_deco
                                && paren_depth == 1
                                && arg_name == "name"
                                && matches!(tokens.get(i + 1), Some(PyToken::Assign)) =>
                        {
                            if let Some(PyToken::StringLit(custom)) = tokens.get(i + 2)
                                && !custom.is_empty()
                            {
                                pending_fixture_custom_name = Some(custom.clone());
                            }
                            i += 2;
                        }
                        PyToken::StringLit(fx_name)
                            if is_usefixtures_deco && paren_depth == 1 && !fx_name.is_empty() =>
                        {
                            fixtures_used.insert(fx_name.clone());
                            i += 1;
                        }
                        PyToken::Newline if paren_depth == 0 => {
                            at_statement_start = true;
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            PyToken::Ident(kw) if at_statement_start && kw == "import" => {
                i += 1;
                let (parsed_specs, next_i) = parse_import_statement(&tokens, i);
                for spec in parsed_specs {
                    imports.insert(spec);
                }
                i = next_i;
                at_statement_start = true;
            }
            PyToken::Ident(kw) if at_statement_start && kw == "from" => {
                i += 1;
                if let Some((spec, next_i)) = parse_from_import_statement(&tokens, i) {
                    imports.insert(spec);
                    i = next_i;
                }
                at_statement_start = true;
            }
            PyToken::Ident(kw)
                if at_statement_start && (kw == "def" || kw == "async" || kw == "class") =>
            {
                let is_class = kw == "class";
                if kw == "async" {
                    i += 1;
                    if !matches!(tokens.get(i), Some(PyToken::Ident(k)) if k == "def") {
                        at_statement_start = false;
                        continue;
                    }
                }
                i += 1;
                if let Some(PyToken::Ident(name)) = tokens.get(i) {
                    let sym_name = name.clone();
                    defined_symbols.insert(sym_name.clone());
                    i += 1;
                    if is_class {
                        if sym_name.starts_with("Test") {
                            test_symbols.insert(sym_name);
                        }
                        pending_fixture_decorator = false;
                        pending_fixture_custom_name = None;
                    } else {
                        let is_test_fn =
                            sym_name.starts_with("test_") || sym_name.ends_with("_test");
                        if is_test_fn {
                            test_symbols.insert(sym_name.clone());
                        }
                        if pending_fixture_decorator {
                            let fx_name =
                                pending_fixture_custom_name.take().unwrap_or(sym_name);
                            fixtures_defined.insert(fx_name);
                        }
                        if (is_test_fn || pending_fixture_decorator)
                            && matches!(tokens.get(i), Some(PyToken::LParen))
                        {
                            let (params, next_i) = parse_function_params(&tokens, i + 1);
                            for p in params {
                                if p != "self" && p != "cls" {
                                    fixtures_used.insert(p);
                                }
                            }
                            i = next_i;
                        }
                        pending_fixture_decorator = false;
                        pending_fixture_custom_name = None;
                    }
                }
                at_statement_start = false;
            }
            PyToken::Ident(ident) if ident == "usefixtures" => {
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::LParen)) {
                    i += 1;
                    let mut depth = 1usize;
                    while i < n && depth > 0 {
                        match &tokens[i] {
                            PyToken::LParen => {
                                depth += 1;
                                i += 1;
                            }
                            PyToken::RParen => {
                                depth -= 1;
                                i += 1;
                            }
                            PyToken::StringLit(fx) if depth == 1 && !fx.is_empty() => {
                                fixtures_used.insert(fx.clone());
                                i += 1;
                            }
                            _ => i += 1,
                        }
                    }
                }
                at_statement_start = false;
            }
            PyToken::Ident(ident) if at_statement_start && ident == "pytest_plugins" => {
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::Assign)) {
                    i += 1;
                    // Collect string literals on this assignment
                    let mut bracket_depth = 0usize;
                    while i < n {
                        match &tokens[i] {
                            PyToken::LBracket | PyToken::LParen => {
                                bracket_depth += 1;
                                i += 1;
                            }
                            PyToken::RBracket | PyToken::RParen => {
                                bracket_depth = bracket_depth.saturating_sub(1);
                                i += 1;
                            }
                            PyToken::StringLit(s) => {
                                if is_valid_dotted_module(s) {
                                    imports.insert(ImportSpec {
                                        level: 0,
                                        module: s.clone(),
                                        symbols: Vec::new(),
                                    });
                                }
                                i += 1;
                            }
                            PyToken::Newline if bracket_depth == 0 => break,
                            _ => i += 1,
                        }
                    }
                }
                at_statement_start = false;
            }
            PyToken::Ident(ident) if ident == "import_module" || ident == "importorskip" => {
                // Dynamic call: `importlib.import_module("pkg.mod")` or `pytest.importorskip("pkg.mod")`
                if matches!(tokens.get(i + 1), Some(PyToken::LParen))
                    && let Some(PyToken::StringLit(mod_name)) = tokens.get(i + 2)
                    && is_valid_dotted_module(mod_name)
                {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: mod_name.clone(),
                        symbols: Vec::new(),
                    });
                }
                at_statement_start = false;
                i += 1;
            }
            PyToken::Ident(ident) if at_statement_start => {
                if matches!(tokens.get(i + 1), Some(PyToken::Assign | PyToken::Colon)) {
                    defined_symbols.insert(ident.clone());
                }
                at_statement_start = false;
                i += 1;
            }
            _ => {
                at_statement_start = false;
                i += 1;
            }
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: fixtures_defined.into_iter().collect(),
        fixtures_used: fixtures_used.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
    }
}

fn normalize_data_ref(lit: &str) -> Option<String> {
    let trimmed = lit.trim().trim_start_matches("./").replace('\\', "/");
    if trimmed.len() < 3
        || trimmed.len() > 160
        || trimmed.starts_with('/')
        || trimmed.contains("..")
        || trimmed.contains(['\n', '\r', '\0', ' ', '{', '}', '*', '?'])
    {
        return None;
    }
    if DATA_FILE_EXTENSIONS
        .iter()
        .any(|ext| trimmed.ends_with(ext))
    {
        Some(trimmed)
    } else {
        None
    }
}

fn is_valid_dotted_module(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|part| !part.is_empty() && part.bytes().all(is_ident_continue))
}

fn parse_import_statement(tokens: &[PyToken], mut i: usize) -> (Vec<ImportSpec>, usize) {
    let n = tokens.len();
    let mut specs = Vec::new();
    while i < n {
        if matches!(tokens[i], PyToken::Newline) {
            i += 1;
            break;
        }
        let (dotted, next_i) = read_dotted_name(tokens, i);
        i = next_i;
        if !dotted.is_empty() {
            specs.push(ImportSpec {
                level: 0,
                module: dotted,
                symbols: Vec::new(),
            });
        }
        // Optional `as alias`
        if matches!(tokens.get(i), Some(PyToken::Ident(kw)) if kw == "as") {
            i += 1;
            if matches!(tokens.get(i), Some(PyToken::Ident(_))) {
                i += 1;
            }
        }
        if matches!(tokens.get(i), Some(PyToken::Comma)) {
            i += 1;
        } else {
            while i < n && !matches!(tokens[i], PyToken::Newline) {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            break;
        }
    }
    (specs, i)
}

fn parse_from_import_statement(tokens: &[PyToken], mut i: usize) -> Option<(ImportSpec, usize)> {
    let n = tokens.len();
    let mut level = 0usize;
    while i < n && matches!(tokens[i], PyToken::Dot) {
        level += 1;
        i += 1;
    }
    let (module, next_i) = if matches!(tokens.get(i), Some(PyToken::Ident(kw)) if kw != "import") {
        read_dotted_name(tokens, i)
    } else {
        (String::new(), i)
    };
    i = next_i;
    if !matches!(tokens.get(i), Some(PyToken::Ident(kw)) if kw == "import") {
        return None;
    }
    i += 1;
    let mut in_parens = false;
    if matches!(tokens.get(i), Some(PyToken::LParen)) {
        in_parens = true;
        i += 1;
    }
    let mut symbols = Vec::new();
    while i < n {
        match &tokens[i] {
            PyToken::Newline if !in_parens => {
                i += 1;
                break;
            }
            PyToken::Newline | PyToken::Comma => {
                i += 1;
            }
            PyToken::RParen if in_parens => {
                i += 1;
                break;
            }
            PyToken::Star => {
                symbols.push("*".to_string());
                i += 1;
            }
            PyToken::Ident(sym) => {
                symbols.push(sym.clone());
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::Ident(kw)) if kw == "as") {
                    i += 1;
                    if matches!(tokens.get(i), Some(PyToken::Ident(_))) {
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    Some((
        ImportSpec {
            level,
            module,
            symbols,
        },
        i,
    ))
}

fn read_dotted_name(tokens: &[PyToken], mut i: usize) -> (String, usize) {
    let n = tokens.len();
    let mut parts = Vec::new();
    while i < n {
        if let PyToken::Ident(ident) = &tokens[i] {
            if ident == "import" || ident == "as" {
                break;
            }
            parts.push(ident.as_str());
            i += 1;
            if matches!(tokens.get(i), Some(PyToken::Dot)) {
                i += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    (parts.join("."), i)
}

fn parse_function_params(tokens: &[PyToken], mut i: usize) -> (Vec<String>, usize) {
    let n = tokens.len();
    let mut params = Vec::new();
    let mut depth = 1usize;
    let mut expect_param_name = true;
    let mut skip_vararg_ident = false;

    while i < n && depth > 0 {
        match &tokens[i] {
            PyToken::LParen | PyToken::LBracket => {
                depth += 1;
                i += 1;
            }
            PyToken::RParen | PyToken::RBracket => {
                depth -= 1;
                i += 1;
            }
            PyToken::Star if depth == 1 && expect_param_name => {
                skip_vararg_ident = true;
                i += 1;
            }
            PyToken::Comma if depth == 1 => {
                expect_param_name = true;
                skip_vararg_ident = false;
                i += 1;
            }
            PyToken::Colon | PyToken::Assign if depth == 1 => {
                expect_param_name = false;
                skip_vararg_ident = false;
                i += 1;
            }
            PyToken::Ident(name) if depth == 1 && expect_param_name => {
                if !skip_vararg_ident {
                    params.push(name.clone());
                }
                expect_param_name = false;
                skip_vararg_ident = false;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    (params, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imports_fixtures_and_test_symbols() {
        let src = r#"
import os, sys as system
from pkg.sub import (
    alpha,
    beta as b,
)
from ..relative_mod import helper

# from fake.comment import ignored
DOC = """
import fake.in_docstring
def test_fake():
    pass
"""

@pytest.fixture(scope="module")
def sample_db(tmp_path):
    from lazy.mod import connect
    return connect(tmp_path)

async def test_create_user(sample_db, monkeypatch: object = None):
    mod = importlib.import_module("dynamic.plugin")
    assert sample_db is not None
"#;
        let parsed = parse_python_source(src);
        assert!(parsed.test_symbols.contains(&"test_create_user".to_string()));
        assert!(!parsed.test_symbols.contains(&"test_fake".to_string()));
        assert_eq!(parsed.fixtures_defined, vec!["sample_db".to_string()]);
        assert!(parsed.fixtures_used.contains(&"sample_db".to_string()));
        assert!(parsed.fixtures_used.contains(&"tmp_path".to_string()));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 0,
            module: "pkg.sub".to_string(),
            symbols: vec!["alpha".to_string(), "beta".to_string()],
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 2,
            module: "relative_mod".to_string(),
            symbols: vec!["helper".to_string()],
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 0,
            module: "lazy.mod".to_string(),
            symbols: vec!["connect".to_string()],
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 0,
            module: "dynamic.plugin".to_string(),
            symbols: Vec::new(),
        }));
    }

    #[test]
    fn parses_named_fixtures_usefixtures_varargs_nested_fstrings_and_data_refs() {
        let src = r#"
pytestmark = pytest.mark.usefixtures("global_fx")

@pytest.fixture(scope="session", name="aliased_db")
def _internal_db_builder(*args, seed_fx, **kwargs):
    msg = f"nested {inner("import fake_inside_fstring")} done"
    schema = "config/schema.json"
    return msg, schema

@pytest.mark.usefixtures("mark_fx1", "mark_fx2")
def test_with_varargs(first_fx, *args, kw_fx: int = 1, **kwargs):
    pass
"#;
        let parsed = parse_python_source(src);
        assert_eq!(parsed.fixtures_defined, vec!["aliased_db".to_string()]);
        assert!(parsed.fixtures_used.contains(&"global_fx".to_string()));
        assert!(parsed.fixtures_used.contains(&"mark_fx1".to_string()));
        assert!(parsed.fixtures_used.contains(&"mark_fx2".to_string()));
        assert!(parsed.fixtures_used.contains(&"seed_fx".to_string()));
        assert!(parsed.fixtures_used.contains(&"first_fx".to_string()));
        assert!(parsed.fixtures_used.contains(&"kw_fx".to_string()));
        assert!(!parsed.fixtures_used.contains(&"args".to_string()));
        assert!(!parsed.fixtures_used.contains(&"kwargs".to_string()));
        assert!(parsed.data_refs.contains(&"config/schema.json".to_string()));
    }

    #[test]
    fn isolates_monorepo_service_compartments_and_supports_hash_only_updates() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "services/svc_a/src/app/config.py",
            b"VALUE = 1\n",
        );
        graph.insert_file_bytes(
            "services/svc_b/src/app/config.py",
            b"VALUE = 2\n",
        );
        graph.insert_file_bytes(
            "services/svc_a/tests/test_a.py",
            b"from app.config import VALUE\ndef test_a(): assert VALUE == 1\n",
        );
        graph.insert_file_bytes(
            "services/svc_b/tests/test_b.py",
            b"from app.config import VALUE\ndef test_b(): assert VALUE == 2\n",
        );
        graph.recompute_edges();

        // Changing svc_a's config must only affect svc_a's test, never svc_b's test!
        let affected_a = graph.affected_tests(&["services/svc_a/src/app/config.py".to_string()]);
        assert_eq!(affected_a.len(), 1);
        assert_eq!(affected_a[0].test_file, "services/svc_a/tests/test_a.py");

        let hash_a_before = graph.dep_hash_for_test["services/svc_a/tests/test_a.py"].clone();
        let hash_b_before = graph.dep_hash_for_test["services/svc_b/tests/test_b.py"].clone();

        // Body-only change returns structure_changed == false and updates only affected dep_hash
        let structure_changed = graph.insert_file_bytes(
            "services/svc_a/src/app/config.py",
            b"VALUE = 100\n",
        );
        assert!(!structure_changed);
        graph.recompute_hashes_for_changed(&["services/svc_a/src/app/config.py".to_string()]);

        assert_ne!(
            graph.dep_hash_for_test["services/svc_a/tests/test_a.py"],
            hash_a_before
        );
        assert_eq!(
            graph.dep_hash_for_test["services/svc_b/tests/test_b.py"],
            hash_b_before
        );
    }

    #[test]
    fn resolves_aks_store_demo_polyglot_hyphenated_subservice_imports() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "src/ai-service/routers/__init__.py",
            b"",
        );
        graph.insert_file_bytes(
            "src/ai-service/routers/description_generator.py",
            b"def generate_description(): return 'desc'\n",
        );
        graph.insert_file_bytes(
            "src/ai-service/routers/image_generator.py",
            b"def generate_image(): return 'img'\n",
        );
        graph.insert_file_bytes(
            "src/ai-service/main.py",
            b"from routers.description_generator import generate_description\nfrom routers.image_generator import generate_image\n",
        );
        graph.insert_file_bytes(
            "src/ai-service/tests/test_description.py",
            b"from routers.description_generator import generate_description\ndef test_desc(): assert generate_description() == 'desc'\n",
        );
        graph.insert_file_bytes(
            "src/ai-service/tests/test_image.py",
            b"from routers.image_generator import generate_image\ndef test_img(): assert generate_image() == 'img'\n",
        );
        graph.insert_file_bytes(
            "src/ai-service/tests/test_main.py",
            b"from main import generate_description\ndef test_app(): pass\n",
        );
        graph.recompute_edges();

        let affected = graph.affected_tests(&[
            "src/ai-service/routers/description_generator.py".to_string(),
        ]);
        let test_files: Vec<String> = affected.into_iter().map(|a| a.test_file).collect();
        assert_eq!(
            test_files,
            vec![
                "src/ai-service/tests/test_description.py".to_string(),
                "src/ai-service/tests/test_main.py".to_string(),
            ]
        );
        let modules = graph.affected_modules(&[
            "src/ai-service/routers/description_generator.py".to_string(),
        ]);
        assert!(modules.contains(&"src/ai-service/main.py".to_string()));
    }
}
