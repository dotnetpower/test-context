use crate::policy::MemoryPolicy;
use crate::workspace::{
    DATA_FILE_EXTENSIONS, FRONTEND_CONFIG_FILES, FRONTEND_SOURCE_EXTENSIONS,
    POLYGLOT_CONFIG_FILES, POLYGLOT_SOURCE_EXTENSIONS, PYTEST_CONFIG_FILES, RUST_CONFIG_FILES,
    Workspace,
};
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
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
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
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportSpec {
    pub level: usize,
    pub module: String,
    pub symbols: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RustModDecl {
    pub name: String,
    pub custom_path: Option<String>,
    pub is_inline: bool,
    pub is_cfg_test: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedPythonFile {
    pub imports: Vec<ImportSpec>,
    pub test_symbols: Vec<String>,
    pub fixtures_defined: Vec<String>,
    pub fixtures_used: Vec<String>,
    pub defined_symbols: Vec<String>,
    pub data_refs: Vec<String>,
    pub markers: Vec<String>,
    pub module_markers: Vec<String>,
    #[serde(default)]
    pub rust_mods: Vec<RustModDecl>,
    #[serde(default)]
    pub type_checking_imports: Vec<ImportSpec>,
    #[serde(default)]
    pub env_vars_referenced: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub semantic_sha256: String,
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
    #[serde(default)]
    pub language: String,
    pub sha256: String,
    #[serde(default)]
    pub semantic_sha256: String,
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
    #[serde(default)]
    pub env_vars_referenced: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DependencyGraph {
    pub files: BTreeMap<String, FileNode>,
    pub direct_imports: BTreeMap<String, BTreeSet<String>>,
    pub reverse_imports: BTreeMap<String, BTreeSet<String>>,
    pub conftest_for_test: BTreeMap<String, Vec<String>>,
    pub file_index_to_path: Vec<String>,
    pub path_to_id: BTreeMap<String, u32>,
    pub transitive_dep_ids_for_test: BTreeMap<String, Vec<u32>>,
    pub dep_hash_for_test: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
    pub test_file_patterns: Vec<String>,
    pub testpaths: Vec<String>,
    pub environment_hash: String,
    pub runtime_deps_for_test: BTreeMap<String, BTreeSet<String>>,
    pub last_env_ref_digest: String,
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
            path_to_id: BTreeMap::new(),
            transitive_dep_ids_for_test: BTreeMap::new(),
            dep_hash_for_test: BTreeMap::new(),
            config_hashes: BTreeMap::new(),
            test_file_patterns: vec!["test_*.py".to_string(), "*_test.py".to_string()],
            testpaths: Vec::new(),
            environment_hash: String::new(),
            runtime_deps_for_test: BTreeMap::new(),
            last_env_ref_digest: String::new(),
        }
    }

    fn build_base_hasher(&self) -> Sha256 {
        let mut hasher = Sha256::new();
        hasher.update(b"tctx:merkle:v1\nenv:");
        hasher.update(self.environment_hash.as_bytes());
        hasher.update(b"\n");
        for (cfg_path, cfg_sha) in &self.config_hashes {
            hasher.update(b"cfg:");
            hasher.update(cfg_path.as_bytes());
            hasher.update(b":");
            hasher.update(cfg_sha.as_bytes());
            hasher.update(b"\n");
        }
        hasher
    }

    fn compute_env_ref_digest(&self) -> String {
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
            hasher.update(b"=");
            if let Some(val) = std::env::var_os(key) {
                hasher.update(val.to_string_lossy().as_bytes());
            }
            hasher.update(b"\n");
        }
        hex_encode(&hasher.finalize())
    }

    pub fn apply_environment_hash(&mut self, environment_hash: String) {
        if self.environment_hash == environment_hash {
            return;
        }
        self.environment_hash = environment_hash;
        if self.file_index_to_path.len() != self.files.len() {
            self.recompute_edges();
        } else {
            self.recompute_all_test_hashes();
        }
    }

    fn build_all_node_hash_bytes(&self) -> Vec<Vec<u8>> {
        self.file_index_to_path
            .iter()
            .map(|dep_path| {
                self.files
                    .get(dep_path)
                    .map(|dep_node| build_node_hash_bytes(dep_path, dep_node))
                    .unwrap_or_default()
            })
            .collect()
    }

    fn recompute_all_test_hashes(&mut self) {
        let base_hasher = self.build_base_hasher();
        let node_digests = self.build_all_node_hash_bytes();
        for (test_path, reachable_ids) in &self.transitive_dep_ids_for_test {
            let mut hasher = base_hasher.clone();
            for &dep_id in reachable_ids {
                if let Some(bytes) = node_digests.get(dep_id as usize) {
                    hasher.update(bytes);
                }
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

    pub fn build(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let mut graph = Self::build_unindexed(workspace, policy)?;
        graph.recompute_edges();
        Ok(graph)
    }

    pub fn build_unindexed(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let inventory = workspace.files_limited(policy)?;
        let mut graph = Self::new();
        graph.test_file_patterns = workspace.pytest_file_patterns();
        graph.testpaths = workspace.pytest_testpaths();
        let mut total_bytes = 0usize;

        if inventory.files.len() >= 64 {
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = inventory.files.len().div_ceil(workers);
            let patterns = &graph.test_file_patterns;
            let testpaths = &graph.testpaths;
            let max_file_bytes = policy.max_file_bytes;
            let chunk_results: Vec<Vec<(FileNode, usize)>> = std::thread::scope(|scope| {
                let handles: Vec<_> = inventory
                    .files
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut out = Vec::with_capacity(chunk.len());
                            for rel_path in chunk {
                                let full_path = workspace.root.join(rel_path);
                                let stamp = file_stamp(&full_path);
                                let Ok(bytes) =
                                    workspace.read_known_with_limit(rel_path, max_file_bytes)
                                else {
                                    continue;
                                };
                                let rel_str = normalize_rel_str(rel_path);
                                let node = build_file_node_from_bytes(
                                    &rel_str,
                                    &bytes,
                                    stamp,
                                    patterns,
                                    testpaths,
                                );
                                out.push((node, bytes.len()));
                            }
                            out
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok())
                    .collect()
            });
            for chunk in chunk_results {
                for (node, byte_len) in chunk {
                    total_bytes = total_bytes.saturating_add(byte_len);
                    if total_bytes > policy.max_source_bytes {
                        return Err(
                            invalid("workspace exceeds max_source_bytes policy limit").into()
                        );
                    }
                    if node.is_config {
                        graph
                            .config_hashes
                            .insert(node.path.clone(), node.sha256.clone());
                    }
                    graph.files.insert(node.path.clone(), node);
                }
            }
        } else {
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
        }
        graph.discover_referenced_data_files(workspace, policy);
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
                if let Some(norm_root) = normalize_lexical_rel(Path::new(data_ref)) {
                    let root_cand = Path::new(&norm_root);
                    if workspace.is_permitted_data_rel(root_cand)
                        || workspace.is_permitted_rel(root_cand)
                    {
                        candidates.insert(norm_root);
                    }
                }
                if !parent.as_os_str().is_empty()
                    && let Some(norm_rel) = normalize_lexical_rel(&parent.join(data_ref))
                {
                    let rel_cand = Path::new(&norm_rel);
                    if workspace.is_permitted_data_rel(rel_cand)
                        || workspace.is_permitted_rel(rel_cand)
                    {
                        candidates.insert(norm_rel);
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
                self.insert_file_bytes_with_stamp(&rel_str, &bytes, Some(stamp));
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
        let node = build_file_node_from_bytes(
            rel_str,
            bytes,
            stamp,
            &self.test_file_patterns,
            &self.testpaths,
        );
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

        let affected: Vec<String> = self
            .affected_tests(changed_files)
            .into_iter()
            .map(|a| a.test_file)
            .collect();

        for test_path in affected {
            let Some(reachable_ids) = self.transitive_dep_ids_for_test.get(&test_path) else {
                continue;
            };
            let mut hasher = base_hasher.clone();
            for &dep_id in reachable_ids {
                if let Some(dep_path) = self.file_index_to_path.get(dep_id as usize)
                    && let Some(dep_node) = self.files.get(dep_path)
                {
                    hash_dep_node(&mut hasher, dep_path, dep_node);
                }
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
        self.path_to_id.clear();
        self.transitive_dep_ids_for_test.clear();
        self.dep_hash_for_test.clear();

        for (idx, path) in self.files.keys().enumerate() {
            let id = u32::try_from(idx).unwrap_or(u32::MAX);
            self.file_index_to_path.push(path.clone());
            self.path_to_id.insert(path.clone(), id);
        }

        let module_index = ModuleIndex::build(&self.files);
        let fixture_providers = build_fixture_provider_index(&self.files);
        let rust_index = RustModuleIndex::build(&self.files);
        let frontend_index = FrontendModuleIndex::build(&self.files);

        if self.files.len() >= 128 {
            let entries: Vec<(&String, &FileNode)> = self
                .files
                .iter()
                .filter(|(_, node)| !node.is_config)
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
            let rt_ref = &self.runtime_deps_for_test;
            let chunk_deps: Vec<Vec<(String, BTreeSet<String>)>> = std::thread::scope(|scope| {
                let handles: Vec<_> = entries
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|&(path, node)| {
                                    let deps = compute_direct_dependencies(
                                        path, node, files_ref, mod_ref, rs_ref, fe_ref, fix_ref,
                                        rt_ref,
                                    );
                                    (path.clone(), deps)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok())
                    .collect()
            });
            for chunk in chunk_deps {
                for (path, deps) in chunk {
                    for dep in &deps {
                        self.reverse_imports
                            .entry(dep.clone())
                            .or_default()
                            .insert(path.clone());
                    }
                    self.direct_imports.insert(path, deps);
                }
            }
        } else {
            for (path, node) in &self.files {
                if node.is_config {
                    continue;
                }
                let deps = compute_direct_dependencies(
                    path,
                    node,
                    &self.files,
                    &module_index,
                    &rust_index,
                    &frontend_index,
                    &fixture_providers,
                    &self.runtime_deps_for_test,
                );
                for dep in &deps {
                    self.reverse_imports
                        .entry(dep.clone())
                        .or_default()
                        .insert(path.clone());
                }
                self.direct_imports.insert(path.clone(), deps);
            }
        }

        // Build compact adjacency list in terms of u32 file IDs
        let num_files = self.file_index_to_path.len();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); num_files];
        for (src, targets) in &self.direct_imports {
            if let Some(&src_id) = self.path_to_id.get(src) {
                adj[src_id as usize] = targets
                    .iter()
                    .filter_map(|t| self.path_to_id.get(t).copied())
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

        let base_hasher = self.build_base_hasher();
        self.last_env_ref_digest = self.compute_env_ref_digest();
        let node_digests = self.build_all_node_hash_bytes();

        if test_files.len() >= 64 {
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(2, 8);
            let chunk_size = test_files.len().div_ceil(workers).max(1);
            let this_ref: &Self = self;
            let adj_ref = &adj;
            let cf_ref = &conftests;
            let digests_ref = &node_digests;
            let base_ref = &base_hasher;
            type TestEdgeOutcome = (String, Vec<String>, Vec<u32>, String);
            let chunk_outcomes: Vec<Vec<TestEdgeOutcome>> = std::thread::scope(|scope| {
                let handles: Vec<_> = test_files
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut visited_stamp = vec![0u32; num_files];
                            let mut epoch = 0u32;
                            let mut queue = VecDeque::new();
                            let mut out = Vec::with_capacity(chunk.len());
                            for test_path in chunk {
                                epoch = epoch.wrapping_add(1);
                                if epoch == 0 {
                                    visited_stamp.fill(0);
                                    epoch = 1;
                                }
                                let ancestor_conftests = ancestor_conftests_for(test_path, cf_ref);
                                let mut reachable_ids = Vec::new();
                                queue.clear();
                                if let Some(&test_id) = this_ref.path_to_id.get(test_path) {
                                    visited_stamp[test_id as usize] = epoch;
                                    reachable_ids.push(test_id);
                                    queue.push_back(test_id);
                                }
                                for cf in &ancestor_conftests {
                                    if let Some(&cf_id) = this_ref.path_to_id.get(cf)
                                        && visited_stamp[cf_id as usize] != epoch
                                        && this_ref.is_conftest_used_by_test(cf, test_path)
                                    {
                                        visited_stamp[cf_id as usize] = epoch;
                                        reachable_ids.push(cf_id);
                                        queue.push_back(cf_id);
                                    }
                                }
                                while let Some(curr_id) = queue.pop_front() {
                                    for &next_id in &adj_ref[curr_id as usize] {
                                        if visited_stamp[next_id as usize] != epoch {
                                            visited_stamp[next_id as usize] = epoch;
                                            reachable_ids.push(next_id);
                                            queue.push_back(next_id);
                                        }
                                    }
                                }
                                reachable_ids.sort_unstable();
                                let mut hasher = base_ref.clone();
                                for &dep_id in &reachable_ids {
                                    if let Some(bytes) = digests_ref.get(dep_id as usize) {
                                        hasher.update(bytes);
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
                    .filter_map(|h| h.join().ok())
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

                if let Some(&test_id) = self.path_to_id.get(&test_path) {
                    visited_stamp[test_id as usize] = epoch;
                    reachable_ids.push(test_id);
                    queue.push_back(test_id);
                }
                for cf in &ancestor_conftests {
                    if let Some(&cf_id) = self.path_to_id.get(cf)
                        && visited_stamp[cf_id as usize] != epoch
                        && self.is_conftest_used_by_test(cf, &test_path)
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

                let mut hasher = base_hasher.clone();
                for &dep_id in &reachable_ids {
                    if let Some(bytes) = node_digests.get(dep_id as usize) {
                        hasher.update(bytes);
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
    }

    pub fn recompute_structure_incremental(&mut self, changed_files: &[String]) {
        if self.file_index_to_path.len() != self.files.len()
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
                    || self.files.get(path).is_some_and(|node| node.is_config)
            })
        {
            self.recompute_edges();
            return;
        }
        let mut affected: BTreeSet<String> = self
            .affected_tests(changed_files)
            .into_iter()
            .map(|item| item.test_file)
            .collect();
        let module_index = ModuleIndex::build(&self.files);
        let rust_index = RustModuleIndex::build(&self.files);
        let frontend_index = FrontendModuleIndex::build(&self.files);
        let fixture_providers = build_fixture_provider_index(&self.files);
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
                    &self.runtime_deps_for_test,
                ),
            );
        }
        for (path, new_deps) in replacements {
            if let Some(old_deps) = self.direct_imports.insert(path.clone(), new_deps.clone()) {
                for dep in old_deps {
                    if let Some(importers) = self.reverse_imports.get_mut(&dep) {
                        importers.remove(&path);
                    }
                }
            }
            for dep in new_deps {
                self.reverse_imports
                    .entry(dep)
                    .or_default()
                    .insert(path.clone());
            }
        }
        self.reverse_imports
            .retain(|_, importers| !importers.is_empty());
        affected.extend(
            self.affected_tests(changed_files)
                .into_iter()
                .map(|item| item.test_file),
        );
        self.recompute_selected_tests(&affected);
    }

    fn recompute_selected_tests(&mut self, tests: &BTreeSet<String>) {
        if tests.is_empty() {
            return;
        }
        if self.path_to_id.len() != self.file_index_to_path.len() {
            self.path_to_id.clear();
            for (idx, path) in self.file_index_to_path.iter().enumerate() {
                self.path_to_id
                    .insert(path.clone(), u32::try_from(idx).unwrap_or(u32::MAX));
            }
        }
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); self.file_index_to_path.len()];
        for (source, deps) in &self.direct_imports {
            if let Some(&source_id) = self.path_to_id.get(source) {
                adj[source_id as usize] = deps
                    .iter()
                    .filter_map(|dep| self.path_to_id.get(dep).copied())
                    .collect();
            }
        }
        let conftests: BTreeSet<String> = self
            .files
            .values()
            .filter(|node| node.is_conftest)
            .map(|node| node.path.clone())
            .collect();
        let base_hasher = self.build_base_hasher();
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
                for &next in &adj[id as usize] {
                    if visited_stamp[next as usize] != epoch {
                        visited_stamp[next as usize] = epoch;
                        reachable.push(next);
                        queue.push_back(next);
                    }
                }
            }
            reachable.sort_unstable();
            let mut hasher = base_hasher.clone();
            for &id in &reachable {
                let path = &self.file_index_to_path[id as usize];
                if let Some(node) = self.files.get(path) {
                    hash_dep_node(&mut hasher, path, node);
                }
            }
            self.conftest_for_test
                .insert(test_path.clone(), conftest_chain);
            self.transitive_dep_ids_for_test
                .insert(test_path.clone(), reachable);
            self.dep_hash_for_test
                .insert(test_path.clone(), hex_encode(&hasher.finalize()));
        }
    }

    pub fn test_files(&self) -> Vec<String> {
        self.dep_hash_for_test.keys().cloned().collect()
    }

    /// Returns safe 3rd-party packages imported across the workspace that can be pre-imported in the Warm Pytest Zygote
    /// without touching any workspace-local Python module.
    pub fn external_preload_modules(&self) -> Vec<String> {
        self.external_preload_modules_for_tests(&self.test_files())
    }

    pub fn external_preload_modules_for_tests(&self, test_files: &[String]) -> Vec<String> {
        if !test_files.iter().any(|tf| tf.ends_with(".py")) {
            return Vec::new();
        }
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

            // 1. Global config change affects matching language tests
            if PYTEST_CONFIG_FILES.contains(&file_name)
                || RUST_CONFIG_FILES.contains(&file_name)
                || FRONTEND_CONFIG_FILES.contains(&file_name)
                || self.config_hashes.contains_key(&norm)
            {
                let is_rust_only_cfg = RUST_CONFIG_FILES.contains(&file_name);
                let is_fe_only_cfg = FRONTEND_CONFIG_FILES.contains(&file_name);
                let is_py_only_cfg =
                    PYTEST_CONFIG_FILES.contains(&file_name) && file_name != "pyproject.toml";
                for (test_path, dep_hash) in &self.dep_hash_for_test {
                    let is_rs_test = test_path.ends_with(".rs");
                    let is_fe_test = is_frontend_source_path(test_path);
                    if is_py_only_cfg && (is_rs_test || is_fe_test) {
                        continue;
                    }
                    if is_fe_only_cfg && !is_fe_test {
                        continue;
                    }
                    if is_rust_only_cfg && !is_rs_test {
                        // Also check if Python test transitively depends on any .rs file (PyO3 hybrid)
                        let depends_on_rs = self
                            .transitive_deps_for(test_path)
                            .iter()
                            .any(|d| d.ends_with(".rs"));
                        if !depends_on_rs {
                            continue;
                        }
                    }
                    reasons
                        .entry(test_path.clone())
                        .or_insert_with(|| vec![norm.clone(), test_path.clone()]);
                    let _ = dep_hash;
                }
                continue;
            }

            // 2. conftest.py, Rust tests/common helper, or frontend setupTests change affects tests under its scope directory
            if file_name == "conftest.py"
                || is_rust_common_test_helper(&norm)
                || is_frontend_setup_file(&norm)
            {
                let scope_dir = conftest_scope_dir(&norm);
                let helper_lang = file_language_family(&norm);
                for test_path in self.dep_hash_for_test.keys() {
                    if file_language_family(test_path) != helper_lang {
                        continue;
                    }
                    if (scope_dir.is_empty() || test_path.starts_with(&format!("{scope_dir}/")))
                        && self.is_conftest_used_by_test(&norm, test_path)
                    {
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
                    let scope_dir = conftest_scope_dir(&curr);
                    let helper_lang = file_language_family(&curr);
                    for test_path in self.dep_hash_for_test.keys() {
                        if file_language_family(test_path) != helper_lang {
                            continue;
                        }
                        if (scope_dir.is_empty() || test_path.starts_with(&format!("{scope_dir}/")))
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
            semantic_sha256: node.semantic_sha256.clone(),
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
            && test_node
                .parsed
                .fixtures_used
                .iter()
                .any(|fx| cf_node.parsed.fixtures_defined.contains(fx))
        {
            return true;
        }
        if let Some(direct) = self.direct_imports.get(test_path)
            && direct.contains(conftest_path)
        {
            return true;
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
            if self.affected_tests(std::slice::from_ref(&norm)).is_empty() {
                untested.push(norm);
            }
        }
        untested
    }

    /// Suggests candidate test files by symbol/stem similarity when static imports are absent (e.g., dynamic dispatch).
    pub fn suggest_tests_for_untested(&self, source_files: &[String]) -> Vec<String> {
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
                        .filter(|s| !s.contains(':'))
                        .map(|s| s.to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            for test_path in self.dep_hash_for_test.keys() {
                let test_lower = test_path.to_ascii_lowercase();
                if !stem.is_empty() && stem.len() >= 3 && test_lower.contains(&stem) {
                    suggestions.insert(test_path.clone());
                    continue;
                }
                if let Some(t_node) = self.files.get(test_path) {
                    for t_sym in &t_node.parsed.test_symbols {
                        let t_sym_low = t_sym.to_ascii_lowercase();
                        if defined.iter().any(|d| d.len() >= 4 && t_sym_low.contains(d)) {
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
                } else if path.ends_with("_test.go") {
                    frameworks.insert("go_test");
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
                } else if !norm.contains('/')
                    && !norm.ends_with(".py")
                    && !norm.ends_with(".rs")
                    && !is_frontend_source_path(&norm)
                {
                    // Exact test symbol match (e.g. `test_add_unit` or `test_create_user`)
                    for test_path in self.dep_hash_for_test.keys() {
                        if self
                            .files
                            .get(test_path)
                            .is_some_and(|n| n.parsed.test_symbols.contains(&norm))
                        {
                            selected.insert(test_path.clone());
                        }
                    }
                }
            }
        }
        selected.into_iter().collect()
    }

    pub fn suggest_closest_targets(&self, raw_targets: &[String]) -> Vec<String> {
        let mut candidates = BTreeSet::new();
        for test_path in self.dep_hash_for_test.keys() {
            candidates.insert(test_path.clone());
            if let Some(node) = self.files.get(test_path) {
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

fn edit_distance(a: &str, b: &str) -> usize {
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

#[allow(clippy::too_many_arguments)]
fn compute_direct_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
    module_index: &ModuleIndex,
    rust_index: &RustModuleIndex,
    frontend_index: &FrontendModuleIndex,
    fixture_providers: &BTreeMap<String, Vec<String>>,
    runtime_deps_for_test: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    if path.ends_with(".rs") {
        deps.extend(rust_index.resolve_file_dependencies(path, node, files));
    } else if is_frontend_source_path(path) || is_stylesheet_path(path) {
        deps.extend(frontend_index.resolve_file_dependencies(path, node, files));
    } else if is_polyglot_source_path(path) {
        deps.extend(resolve_polyglot_dependencies(path, node, files));
    } else {
        for spec in &node.parsed.imports {
            for resolved in module_index.resolve_import(path, spec, files) {
                if resolved != path {
                    deps.insert(resolved);
                }
            }
            for resolved in rust_index.resolve_pyo3_import(spec, files) {
                if resolved != path {
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
    }
    // Cross-language & symbol-level bridges:
    // 1) WebAssembly (`#[wasm_bindgen]` Rust -> Frontend TS/JS)
    // 2) Full-Stack API routes (`api_route:/api/...` -> `api_call:/api/...`)
    // 3) React Context (`ctx_def:<Name>` -> `ctx_use:<Name>`)
    // 4) Protobuf (`.proto` -> generated `_pb2.py`, `.pb.go`, `_pb.ts`, Rust prost modules)
    for sym in &node.parsed.defined_symbols {
        if let Some(api_path) = sym.strip_prefix("api_call:") {
            let target_tag = format!("api_route:{api_path}");
            for (other_path, other_node) in files {
                if other_path != path && other_node.parsed.defined_symbols.contains(&target_tag) {
                    deps.insert(other_path.clone());
                }
            }
        } else if let Some(ctx_name) = sym.strip_prefix("ctx_use:") {
            let target_tag = format!("ctx_def:{ctx_name}");
            for (other_path, other_node) in files {
                if other_path != path && other_node.parsed.defined_symbols.contains(&target_tag) {
                    deps.insert(other_path.clone());
                }
            }
        } else if let Some(wasm_sym) = sym.strip_prefix("wasm_use:") {
            let target_tag = format!("wasm:{wasm_sym}");
            for (other_path, other_node) in files {
                if other_path != path && other_node.parsed.defined_symbols.contains(&target_tag) {
                    deps.insert(other_path.clone());
                }
            }
        } else if let Some(proto_stem) = sym.strip_prefix("proto_use:") {
            let target_tag = format!("proto:{proto_stem}");
            for (other_path, other_node) in files {
                if other_path != path && other_node.parsed.defined_symbols.contains(&target_tag) {
                    deps.insert(other_path.clone());
                }
            }
        }
    }
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    for data_ref in &node.parsed.data_refs {
        if let Some(norm_root) = normalize_lexical_rel(Path::new(data_ref))
            && files.contains_key(&norm_root)
            && norm_root != path
        {
            deps.insert(norm_root);
        }
        if !parent.as_os_str().is_empty()
            && let Some(relative) = normalize_lexical_rel(&parent.join(data_ref))
            && files.contains_key(&relative)
            && relative != path
        {
            deps.insert(relative);
        }
    }
    if let Some(runtime_deps) = runtime_deps_for_test.get(path) {
        deps.extend(
            runtime_deps
                .iter()
                .filter(|dependency| files.contains_key(*dependency))
                .cloned(),
        );
    }
    deps
}

pub fn normalize_lexical_rel(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(os) => {
                let s = os.to_str()?;
                if !s.is_empty() {
                    parts.push(s);
                }
            }
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub fn is_frontend_source_path(rel_path: &str) -> bool {
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if FRONTEND_CONFIG_FILES.contains(&file_name) {
        return false;
    }
    FRONTEND_SOURCE_EXTENSIONS
        .iter()
        .any(|ext| file_name.ends_with(ext))
}

pub fn is_frontend_setup_file(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let stem = file_name.split('.').next().unwrap_or("");
    if matches!(
        stem,
        "setupTests" | "vitest.setup" | "jest.setup" | "test-setup" | "setup-tests"
    ) || file_name.starts_with("vitest.setup.")
        || file_name.starts_with("jest.setup.")
    {
        return true;
    }
    rel_path.ends_with("/test/setup.ts")
        || rel_path.ends_with("/test/setup.tsx")
        || rel_path.ends_with("/test/setup.js")
        || rel_path.ends_with("/tests/setup.ts")
        || rel_path.ends_with("/tests/setup.tsx")
        || rel_path.ends_with("/tests/setup.js")
        || rel_path == "test/setup.ts"
        || rel_path == "test/setup.tsx"
        || rel_path == "test/setup.js"
        || rel_path == "src/test/setup.ts"
        || rel_path == "src/test/setup.tsx"
}

pub fn is_frontend_test_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) || is_frontend_setup_file(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".test")
                || stem.ends_with(".spec")
                || stem.ends_with(".e2e")
                || stem.ends_with(".cy")
                || stem.ends_with("_test")
                || stem.ends_with("_spec"))
        {
            return true;
        }
    }
    (rel_path.starts_with("__tests__/")
        || rel_path.contains("/__tests__/")
        || rel_path.starts_with("e2e/")
        || rel_path.contains("/e2e/")
        || rel_path.starts_with("cypress/")
        || rel_path.contains("/cypress/"))
        && !file_name.starts_with("setup")
        && !file_name.starts_with("helper")
}

pub fn is_cypress_test_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext) && stem.ends_with(".cy") {
            return true;
        }
    }
    rel_path.starts_with("cypress/") || rel_path.contains("/cypress/")
}

pub fn is_polyglot_source_path(rel_path: &str) -> bool {
    POLYGLOT_SOURCE_EXTENSIONS
        .iter()
        .any(|ext| rel_path.ends_with(ext))
        || rel_path.ends_with(".graphql")
        || rel_path.ends_with(".gql")
}

pub fn is_polyglot_test_path(rel_path: &str, parsed: &ParsedPythonFile) -> bool {
    if rel_path.ends_with("_test.go") {
        return true;
    }
    if (rel_path.ends_with(".cs")
        || rel_path.ends_with(".java")
        || rel_path.ends_with(".kt")
        || rel_path.ends_with(".cpp")
        || rel_path.ends_with(".cc")
        || rel_path.ends_with(".c"))
        && !parsed.test_symbols.is_empty()
    {
        return true;
    }
    false
}

pub fn is_frontend_barrel_file(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    file_name.starts_with("index.")
}

pub fn is_stylesheet_path(rel_path: &str) -> bool {
    rel_path.ends_with(".css")
        || rel_path.ends_with(".scss")
        || rel_path.ends_with(".sass")
        || rel_path.ends_with(".less")
}

pub fn is_storybook_story_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".stories") || stem.ends_with(".story"))
        {
            return true;
        }
    }
    false
}

pub fn is_playwright_e2e_path(rel_path: &str) -> bool {
    if !is_frontend_source_path(rel_path) {
        return false;
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for ext in FRONTEND_SOURCE_EXTENSIONS {
        if let Some(stem) = file_name.strip_suffix(ext)
            && (stem.ends_with(".e2e") || stem.ends_with(".e2e.spec") || stem.ends_with(".e2e.test"))
        {
            return true;
        }
    }
    rel_path.starts_with("e2e/") || rel_path.contains("/e2e/")
}

pub fn file_language_family(rel_path: &str) -> &'static str {
    if rel_path.ends_with(".rs") {
        "rust"
    } else if is_frontend_source_path(rel_path) || is_stylesheet_path(rel_path) {
        "frontend"
    } else if rel_path.ends_with(".go") {
        "go"
    } else if rel_path.ends_with(".cs") {
        "csharp"
    } else if rel_path.ends_with(".java") || rel_path.ends_with(".kt") {
        "jvm"
    } else if rel_path.ends_with(".c")
        || rel_path.ends_with(".cpp")
        || rel_path.ends_with(".cc")
        || rel_path.ends_with(".h")
        || rel_path.ends_with(".hpp")
    {
        "cpp"
    } else {
        "python"
    }
}

pub fn frontend_pkg_dir(rel_path: &str, files: &BTreeMap<String, FileNode>) -> String {
    let mut cur = Path::new(rel_path).parent();
    while let Some(dir) = cur {
        let dir_str = normalize_rel_str(dir);
        if dir_str.is_empty() {
            break;
        }
        if files.contains_key(&format!("{dir_str}/package.json"))
            || files.contains_key(&format!("{dir_str}/tsconfig.json"))
            || files.contains_key(&format!("{dir_str}/vite.config.ts"))
            || files.contains_key(&format!("{dir_str}/vitest.config.ts"))
        {
            return dir_str;
        }
        cur = dir.parent();
    }
    if let Some((first, rest)) = rel_path.split_once('/')
        && matches!(first, "packages" | "apps" | "services")
        && let Some((second, _)) = rest.split_once('/')
    {
        return format!("{first}/{second}");
    }
    if let Some((first, _)) = rel_path.split_once('/')
        && matches!(
            first,
            "frontend" | "web" | "client" | "ui" | "vscode-extension"
        )
    {
        return first.to_string();
    }
    String::new()
}

pub fn is_rust_common_test_helper(rel_path: &str) -> bool {
    rel_path == "tests/common/mod.rs"
        || rel_path == "tests/common.rs"
        || rel_path.ends_with("/tests/common/mod.rs")
        || rel_path.ends_with("/tests/common.rs")
}

pub fn is_rust_integration_test_path(rel_path: &str) -> bool {
    if !rel_path.ends_with(".rs") || is_rust_common_test_helper(rel_path) {
        return false;
    }
    if let Some(rest) = rel_path.strip_prefix("tests/") {
        return !rest.contains('/');
    }
    if let Some((_, rest)) = rel_path.rsplit_once("/tests/") {
        return !rest.contains('/');
    }
    false
}

pub fn conftest_scope_dir(path: &str) -> String {
    if path == "conftest.py" {
        return String::new();
    }
    if let Some(prefix) = path.strip_suffix("/conftest.py") {
        return prefix.to_string();
    }
    if path == "tests/common/mod.rs" || path == "tests/common.rs" {
        return "tests".to_string();
    }
    if let Some(prefix) = path.strip_suffix("/common/mod.rs") {
        return prefix.to_string();
    }
    if let Some(prefix) = path.strip_suffix("/common.rs") {
        return prefix.to_string();
    }
    if is_frontend_setup_file(path) {
        for suffix in [
            "/src/test/setup.ts",
            "/src/test/setup.tsx",
            "/src/test/setup.js",
            "/src/setupTests.ts",
            "/src/setupTests.tsx",
            "/src/setupTests.js",
            "/test/setup.ts",
            "/test/setup.tsx",
            "/test/setup.js",
        ] {
            if let Some(prefix) = path.strip_suffix(suffix) {
                return prefix.to_string();
            }
        }
        if path.starts_with("src/") || path.starts_with("test/") || path.starts_with("tests/") {
            return String::new();
        }
    }
    Path::new(path)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default()
}

pub fn is_test_filename(file_name: &str) -> bool {
    (file_name.starts_with("test_") && file_name.ends_with(".py"))
        || (file_name.ends_with("_test.py") && file_name.len() > "_test.py".len())
        || (file_name.starts_with("test_") && file_name.ends_with(".rs"))
        || (file_name.ends_with("_test.rs") && file_name.len() > "_test.rs".len())
        || is_frontend_test_path(file_name)
}

fn matches_single_pattern_fast(file_name: &str, pattern: &str) -> Option<bool> {
    if pattern.contains(['?', '[', ']', '{', '}', '/']) {
        return None;
    }
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return Some(file_name == pattern);
    };
    if suffix.contains('*') {
        return None;
    }
    Some(
        file_name.len() >= prefix.len() + suffix.len()
            && file_name.starts_with(prefix)
            && file_name.ends_with(suffix),
    )
}

pub fn matches_test_filename(file_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if let Some(matched) = matches_single_pattern_fast(file_name, pattern) {
            return matched;
        }
        globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .ok()
            .is_some_and(|glob| glob.compile_matcher().is_match(file_name))
    })
}

fn matches_test_path(rel_path: &str, testpaths: &[String]) -> bool {
    testpaths.is_empty()
        || testpaths
            .iter()
            .any(|testpath| rel_path == testpath || rel_path.starts_with(&format!("{testpath}/")))
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

fn build_node_hash_bytes(dep_path: &str, dep_node: &FileNode) -> Vec<u8> {
    let effective_sha = if dep_node.semantic_sha256.is_empty() {
        &dep_node.sha256
    } else {
        &dep_node.semantic_sha256
    };
    let mut out = Vec::with_capacity(
        5 + dep_path.len()
            + effective_sha.len()
            + dep_node.parsed.env_vars_referenced.len() * 80,
    );
    out.extend_from_slice(b"py:");
    out.extend_from_slice(dep_path.as_bytes());
    out.push(b':');
    out.extend_from_slice(effective_sha.as_bytes());
    out.push(b'\n');
    for env_key in &dep_node.parsed.env_vars_referenced {
        out.extend_from_slice(b"env_ref:");
        out.extend_from_slice(env_key.as_bytes());
        out.push(b'=');
        if let Some(val) = std::env::var_os(env_key) {
            out.extend_from_slice(sha256_hex(val.to_string_lossy().as_bytes()).as_bytes());
        }
        out.push(b'\n');
    }
    out
}

fn hash_dep_node(hasher: &mut Sha256, dep_path: &str, dep_node: &FileNode) {
    hasher.update(build_node_hash_bytes(dep_path, dep_node));
}

fn build_file_node_from_bytes(
    rel_str: &str,
    bytes: &[u8],
    stamp: Option<FileStamp>,
    test_file_patterns: &[String],
    testpaths: &[String],
) -> FileNode {
    let sha = sha256_hex(bytes);
    let semantic_sha = semantic_sha256_for_source(rel_str, bytes);
    let file_name = Path::new(rel_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if PYTEST_CONFIG_FILES.contains(&file_name)
        || RUST_CONFIG_FILES.contains(&file_name)
        || FRONTEND_CONFIG_FILES.contains(&file_name)
        || POLYGLOT_CONFIG_FILES.contains(&file_name)
        || file_name.ends_with(".csproj")
    {
        let parsed = if matches!(
            file_name,
            "Cargo.toml"
                | "pyproject.toml"
                | "package.json"
                | "tsconfig.json"
                | "tsconfig.app.json"
                | "tsconfig.node.json"
                | "jsconfig.json"
                | "go.mod"
        ) || file_name.starts_with("vite.config.")
            || file_name.starts_with("vitest.config.")
            || file_name.starts_with("vitest.workspace.")
        {
            parse_manifest_metadata(&String::from_utf8_lossy(bytes), file_name)
        } else {
            ParsedPythonFile::default()
        };
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha.clone(),
            semantic_sha256: sha,
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: true,
            parsed,
        };
    }
    if file_name.ends_with(".rs") {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_rust_source(&source);
        let is_conftest = is_rust_common_test_helper(rel_str);
        let is_test = !is_conftest
            && (!parsed.test_symbols.is_empty() || is_rust_integration_test_path(rel_str));
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: semantic_sha,
            stamp,
            is_test,
            is_conftest,
            is_config: false,
            parsed,
        };
    }
    if is_frontend_source_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let mut parsed = parse_frontend_source(&source);
        if is_playwright_e2e_path(rel_str) {
            if !parsed.markers.iter().any(|m| m == "playwright") {
                parsed.markers.push("playwright".to_string());
            }
            if !parsed.module_markers.iter().any(|m| m == "e2e") {
                parsed.module_markers.push("e2e".to_string());
            }
        } else if is_cypress_test_path(rel_str) {
            if !parsed.markers.iter().any(|m| m == "cypress") {
                parsed.markers.push("cypress".to_string());
            }
            if !parsed.module_markers.iter().any(|m| m == "e2e") {
                parsed.module_markers.push("e2e".to_string());
            }
        }
        let is_conftest = is_frontend_setup_file(rel_str);
        let is_test = !is_conftest
            && (is_frontend_test_path(rel_str)
                || parsed.markers.iter().any(|m| m == "in_source_vitest")
                || (is_storybook_story_path(rel_str)
                    && parsed.markers.iter().any(|m| m == "storybook_play")));
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: semantic_sha,
            stamp,
            is_test,
            is_conftest,
            is_config: false,
            parsed,
        };
    }
    if is_stylesheet_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_css_source(&source);
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: semantic_sha,
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: false,
            parsed,
        };
    }
    if is_polyglot_source_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_polyglot_source(rel_str, &source);
        let is_test = is_polyglot_test_path(rel_str, &parsed);
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: semantic_sha,
            stamp,
            is_test,
            is_conftest: false,
            is_config: false,
            parsed,
        };
    }
    if !file_name.ends_with(".py") {
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha.clone(),
            semantic_sha256: sha,
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: false,
            parsed: ParsedPythonFile::default(),
        };
    }
    let source = String::from_utf8_lossy(bytes);
    let parsed = parse_python_source(&source);
    let is_conftest = file_name == "conftest.py";
    let is_test = !is_conftest
        && matches_test_filename(file_name, test_file_patterns)
        && matches_test_path(rel_str, testpaths);
    FileNode {
        path: rel_str.to_string(),
        sha256: sha,
        semantic_sha256: semantic_sha,
        stamp,
        is_test,
        is_conftest,
        is_config: false,
        parsed,
    }
}

/// Computes a comment- and non-semantic-whitespace-normalized SHA-256 hash for source files
/// so comment-only or blank-line formatting edits do not trigger false test cache invalidations.
/// Preserves pragma/directive comments (`noqa`, `type:`, `pragma`, `eslint`, `@ts-`, `leaf_delta_nonce`, `bg_spec_nonce`, Rust doctest fences).
pub fn semantic_sha256_for_source(rel_path: &str, bytes: &[u8]) -> String {
    let is_py = rel_path.ends_with(".py");
    let is_c_style = rel_path.ends_with(".rs")
        || is_frontend_source_path(rel_path)
        || is_stylesheet_path(rel_path)
        || rel_path.ends_with(".go")
        || rel_path.ends_with(".cs")
        || rel_path.ends_with(".java")
        || rel_path.ends_with(".kt")
        || rel_path.ends_with(".c")
        || rel_path.ends_with(".cpp")
        || rel_path.ends_with(".cc")
        || rel_path.ends_with(".h")
        || rel_path.ends_with(".hpp");
    if !is_py && !is_c_style {
        return sha256_hex(bytes);
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return sha256_hex(bytes);
    };
    let normalized = if is_py {
        normalize_python_semantic(source)
    } else {
        normalize_c_style_semantic(source)
    };
    sha256_hex(normalized.as_bytes())
}

fn is_preserved_comment_directive(comment: &str) -> bool {
    let c = comment.trim();
    c.contains("noqa")
        || c.contains("type:")
        || c.contains("pragma")
        || c.contains("eslint")
        || c.contains("@ts-")
        || c.contains("biome-")
        || c.contains("go:embed")
        || c.contains("leaf_delta_nonce")
        || c.contains("bg_spec_nonce")
        || c.contains("```")
}

fn normalize_python_semantic(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_triple: Option<&str> = None;
    for raw_line in source.lines() {
        let mut line_out = String::new();
        let bytes = raw_line.as_bytes();
        let mut i = 0usize;
        let mut in_single: Option<u8> = None;
        while i < bytes.len() {
            if let Some(delim) = in_triple {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    line_out.push('\\');
                    if let Some(esc_ch) = raw_line[i + 1..].chars().next() {
                        line_out.push(esc_ch);
                        i += 1 + esc_ch.len_utf8();
                        continue;
                    }
                }
                if raw_line[i..].starts_with(delim) {
                    line_out.push_str(delim);
                    i += 3;
                    in_triple = None;
                } else {
                    let ch = raw_line[i..].chars().next().unwrap_or(' ');
                    line_out.push(ch);
                    i += ch.len_utf8();
                }
                continue;
            }
            if let Some(q) = in_single {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    line_out.push('\\');
                    if let Some(esc_ch) = raw_line[i + 1..].chars().next() {
                        line_out.push(esc_ch);
                        i += 1 + esc_ch.len_utf8();
                        continue;
                    }
                }
                if bytes[i] == q {
                    in_single = None;
                }
                let ch = raw_line[i..].chars().next().unwrap_or(' ');
                line_out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            if raw_line[i..].starts_with("\"\"\"") {
                in_triple = Some("\"\"\"");
                line_out.push_str("\"\"\"");
                i += 3;
                continue;
            }
            if raw_line[i..].starts_with("'''") {
                in_triple = Some("'''");
                line_out.push_str("'''");
                i += 3;
                continue;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                in_single = Some(bytes[i]);
                line_out.push(bytes[i] as char);
                i += 1;
                continue;
            }
            if bytes[i] == b'#' {
                let comment = &raw_line[i..];
                if is_preserved_comment_directive(comment) {
                    line_out.push_str(comment.trim_end());
                }
                break;
            }
            let ch = raw_line[i..].chars().next().unwrap_or(' ');
            line_out.push(ch);
            i += ch.len_utf8();
        }
        let trimmed_end = line_out.trim_end();
        if !trimmed_end.trim().is_empty() {
            out.push_str(trimmed_end);
            out.push('\n');
        }
    }
    out
}

fn normalize_c_style_semantic(source: &str) -> String {
    let mut stripped = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut in_quote: Option<u8> = None;
    let mut in_block_comment = false;

    while i < len {
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                if bytes[i] == b'\n' {
                    stripped.push('\n');
                }
                let ch = source[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_quote {
            if bytes[i] == b'\\' && i + 1 < len {
                stripped.push('\\');
                if let Some(esc_ch) = source[i + 1..].chars().next() {
                    stripped.push(esc_ch);
                    i += 1 + esc_ch.len_utf8();
                    continue;
                }
            }
            if bytes[i] == q {
                in_quote = None;
            }
            let ch = source[i..].chars().next().unwrap_or(' ');
            stripped.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            let line_end = source[i..].find('\n').map(|off| i + off).unwrap_or(len);
            let comment = &source[i..line_end];
            if is_preserved_comment_directive(comment) {
                stripped.push_str(comment.trim_end());
            }
            i = line_end;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if matches!(bytes[i], b'"' | b'\'' | b'`') {
            in_quote = Some(bytes[i]);
            stripped.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        stripped.push(ch);
        i += ch.len_utf8();
    }

    let mut out = String::with_capacity(stripped.len());
    for line in stripped.lines() {
        let trimmed_end = line.trim_end();
        if !trimmed_end.trim().is_empty() {
            out.push_str(trimmed_end);
            out.push('\n');
        }
    }
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; bytes.len() * 2];
    for (idx, &b) in bytes.iter().enumerate() {
        out[idx * 2] = HEX[(b >> 4) as usize];
        out[idx * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    String::from_utf8(out).unwrap_or_default()
}

fn ancestor_conftests_for(test_path: &str, conftests: &BTreeSet<String>) -> Vec<String> {
    let mut result = Vec::new();
    if is_frontend_source_path(test_path) {
        for cf in conftests {
            if !is_frontend_source_path(cf) || cf == test_path {
                continue;
            }
            let scope = conftest_scope_dir(cf);
            if scope.is_empty() || test_path.starts_with(&format!("{scope}/")) {
                result.push(cf.clone());
            }
        }
        return result;
    }
    if test_path.ends_with(".rs") {
        let mut prefix = String::new();
        let parent = Path::new(test_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        for comp in parent.components() {
            if let std::path::Component::Normal(os) = comp
                && let Some(seg) = os.to_str()
            {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(seg);
                for cand in [
                    format!("{prefix}/common/mod.rs"),
                    format!("{prefix}/common.rs"),
                ] {
                    if conftests.contains(&cand) && cand != test_path {
                        result.push(cand);
                    }
                }
            }
        }
        return result;
    }
    if conftests.contains("conftest.py") {
        result.push("conftest.py".to_string());
    }
    let mut prefix = String::new();
    let parent = Path::new(test_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
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

fn extract_package_export_target(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => {
            let clean = s.trim().trim_start_matches("./");
            if !clean.is_empty() && !clean.ends_with(".d.ts") {
                Some(clean.to_string())
            } else {
                None
            }
        }
        serde_json::Value::Object(map) => {
            for key in ["source", "import", "module", "default", "require"] {
                if let Some(sub) = map.get(key)
                    && let Some(found) = extract_package_export_target(sub)
                {
                    return Some(found);
                }
            }
            for sub in map.values() {
                if let Some(found) = extract_package_export_target(sub) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_manifest_metadata(text: &str, file_name: &str) -> ParsedPythonFile {
    let mut defined_symbols = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    if file_name == "package.json" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let pkg_name = val
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            if let Some(ref name) = pkg_name {
                defined_symbols.insert(format!("fe_pkg:{name}"));
            }
            // Parse Node.js Subpath Imports (`"imports": { "#utils/*": "./src/utils/*" }`)
            if let Some(imports_map) = val.get("imports").and_then(serde_json::Value::as_object) {
                for (sub_key, sub_val) in imports_map {
                    if sub_key.starts_with('#')
                        && let Some(target) = extract_package_export_target(sub_val)
                    {
                        let k = sub_key.trim_end_matches('*');
                        let v = target.trim_end_matches('*');
                        defined_symbols.insert(format!("fe_subpath:{k}->{v}"));
                    }
                }
            }
            // Parse package `exports` map (`".": "./src/index.ts"`, `"./button": "./src/components/Button.tsx"`)
            if let Some(ref name) = pkg_name
                && let Some(exports_val) = val.get("exports")
            {
                if let Some(s) = exports_val.as_str() {
                    let clean = s.trim().trim_start_matches("./");
                    if !clean.is_empty() {
                        defined_symbols.insert(format!("fe_export:{name}:.->{clean}"));
                    }
                } else if let Some(exports_map) = exports_val.as_object() {
                    let has_subpath_keys = exports_map
                        .keys()
                        .any(|k| k == "." || k.starts_with("./"));
                    if has_subpath_keys {
                        for (exp_key, exp_val) in exports_map {
                            if let Some(target) = extract_package_export_target(exp_val) {
                                let clean_sub = if exp_key == "." {
                                    "."
                                } else {
                                    exp_key.trim_start_matches("./").trim_end_matches('*')
                                };
                                let clean_target = target.trim_end_matches('*');
                                defined_symbols.insert(format!(
                                    "fe_export:{name}:{clean_sub}->{clean_target}"
                                ));
                            }
                        }
                    } else if let Some(root_target) = extract_package_export_target(exports_val) {
                        defined_symbols.insert(format!("fe_export:{name}:.->{root_target}"));
                    }
                }
            }
        }
        return ParsedPythonFile {
            defined_symbols: defined_symbols.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }
    if file_name == "tsconfig.json"
        || file_name == "tsconfig.app.json"
        || file_name == "tsconfig.node.json"
        || file_name == "jsconfig.json"
    {
        for raw in text.lines() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if let Some((left, right)) = line.split_once(':') {
                let left_clean = left.trim().trim_matches(['"', '\'', ' ']);
                if left_clean == "extends" {
                    let ext_val = right
                        .trim()
                        .trim_matches([',', '"', '\'', ' '])
                        .trim_start_matches("./");
                    if !ext_val.is_empty() {
                        defined_symbols.insert(format!("fe_extends:{ext_val}"));
                        data_refs.insert(ext_val.to_string());
                    }
                } else if left.contains('*') && right.contains('*') {
                    let alias_key = left_clean.trim_end_matches('*');
                    let target_val = right
                        .trim()
                        .trim_matches(['[', ']', '"', '\'', ',', ' '])
                        .trim_start_matches("./")
                        .trim_end_matches('*');
                    if !alias_key.is_empty() && !target_val.is_empty() {
                        defined_symbols.insert(format!("fe_alias:{alias_key}->{target_val}"));
                    }
                }
            }
        }
        return ParsedPythonFile {
            defined_symbols: defined_symbols.into_iter().collect(),
            data_refs: data_refs.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }
    if file_name == "go.mod" {
        for raw in text.lines() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("module ") {
                let mod_name = rest.trim().trim_matches(['"', '\'']);
                if !mod_name.is_empty() {
                    defined_symbols.insert(format!("go_mod:{mod_name}"));
                }
            }
        }
        return ParsedPythonFile {
            defined_symbols: defined_symbols.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }
    if file_name.starts_with("vite.config.")
        || file_name.starts_with("vitest.config.")
        || file_name.starts_with("vitest.workspace.")
    {
        let mut parsed = parse_frontend_source(text);
        for raw in text.lines() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if let Some((left, right)) = line.split_once(':') {
                let k = left.trim().trim_matches(['"', '\'', ' ']);
                let v = right
                    .trim()
                    .trim_matches([',', '"', '\'', ' '])
                    .trim_start_matches(['.', '/']);
                if (k.starts_with('@') || k.starts_with('~'))
                    && !v.is_empty()
                    && !v.contains(['(', ')', '{', '}'])
                {
                    let alias_pfx = format!("{}/", k.trim_end_matches('/'));
                    let target_pfx = format!("{}/", v.trim_end_matches('/'));
                    parsed
                        .defined_symbols
                        .push(format!("fe_alias:{alias_pfx}->{target_pfx}"));
                }
            }
        }
        for lit in &parsed.data_refs {
            data_refs.insert(lit.clone());
        }
        parsed.data_refs = data_refs.into_iter().collect();
        return parsed;
    }
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).trim().to_string();
            if file_name == "pyproject.toml" && section == "tool.maturin" {
                module_markers.insert("maturin".to_string());
            }
            continue;
        }
        if file_name == "Cargo.toml" {
            if matches!(section.as_str(), "package" | "lib")
                && let Some(rest) = line.strip_prefix("name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']);
                if !val.is_empty() {
                    let norm_crate = val.replace('-', "_");
                    defined_symbols.insert(norm_crate.clone());
                    if section == "lib" {
                        defined_symbols.insert(format!("pyo3:{norm_crate}"));
                    }
                }
            }
            if section == "lib" && line.contains("cdylib") {
                module_markers.insert("cdylib".to_string());
            }
            if section == "lib" && line.contains("proc-macro") && line.contains("true") {
                module_markers.insert("proc_macro".to_string());
            }
        } else if file_name == "pyproject.toml" {
            if section == "tool.maturin"
                && let Some(rest) = line.strip_prefix("module-name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']);
                if !val.is_empty() {
                    defined_symbols.insert(format!("pyo3:{val}"));
                    if let Some(last) = val.rsplit('.').next() {
                        defined_symbols.insert(format!("pyo3:{last}"));
                    }
                }
            } else if section == "project"
                && let Some(rest) = line.strip_prefix("name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']).replace('-', "_");
                if !val.is_empty() {
                    defined_symbols.insert(val);
                }
            }
        }
    }
    ParsedPythonFile {
        defined_symbols: defined_symbols.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

pub fn rust_crate_dir(rel_path: &str) -> String {
    if rel_path.starts_with("src/")
        || rel_path.starts_with("tests/")
        || rel_path.starts_with("benches/")
        || rel_path.starts_with("examples/")
        || rel_path == "build.rs"
        || rel_path == "Cargo.toml"
    {
        return String::new();
    }
    for marker in ["/src/", "/tests/", "/benches/", "/examples/"] {
        if let Some((prefix, _)) = rel_path.rsplit_once(marker) {
            return prefix.to_string();
        }
    }
    if let Some(prefix) = rel_path.strip_suffix("/build.rs") {
        return prefix.to_string();
    }
    if let Some(prefix) = rel_path.strip_suffix("/Cargo.toml") {
        return prefix.to_string();
    }
    String::new()
}

struct RustModuleIndex {
    crate_to_src_dirs: BTreeMap<String, BTreeSet<String>>,
    pyo3_module_to_lib_rs: BTreeMap<String, BTreeSet<String>>,
    reexports: BTreeMap<(String, String), BTreeSet<String>>,
}

impl RustModuleIndex {
    fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut crate_to_src_dirs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut pyo3_module_to_lib_rs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut crate_dirs = BTreeSet::new();

        for (path, node) in files {
            if path.ends_with(".rs") {
                let cdir = rust_crate_dir(path);
                crate_dirs.insert(cdir.clone());
                if !cdir.is_empty()
                    && let Some(dir_name) = Path::new(&cdir).file_name().and_then(|n| n.to_str())
                {
                    let crate_ident = dir_name.replace('-', "_");
                    crate_to_src_dirs
                        .entry(crate_ident)
                        .or_default()
                        .insert(format!("{cdir}/src"));
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        pyo3_module_to_lib_rs
                            .entry(pyo3_name.to_string())
                            .or_default()
                            .insert(path.clone());
                    }
                }
            } else if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
                let cdir = rust_crate_dir(path);
                let src_dir = if cdir.is_empty() {
                    "src".to_string()
                } else {
                    format!("{cdir}/src")
                };
                let lib_rs = format!("{src_dir}/lib.rs");
                let has_lib = files.contains_key(&lib_rs);
                let is_cdylib = node.parsed.module_markers.iter().any(|m| m == "cdylib");
                let has_explicit_lib_name = node
                    .parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s.starts_with("pyo3:"));
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        if has_lib {
                            pyo3_module_to_lib_rs
                                .entry(pyo3_name.to_string())
                                .or_default()
                                .insert(lib_rs.clone());
                        }
                    } else {
                        crate_to_src_dirs
                            .entry(sym.clone())
                            .or_default()
                            .insert(src_dir.clone());
                        if is_cdylib && has_lib && !has_explicit_lib_name {
                            pyo3_module_to_lib_rs
                                .entry(sym.clone())
                                .or_default()
                                .insert(lib_rs.clone());
                        }
                    }
                }
            }
        }

        // Also link pyproject.toml `[tool.maturin]` modules to matching crate `src/lib.rs`
        for (path, node) in files {
            if path == "pyproject.toml" || path.ends_with("/pyproject.toml") {
                let is_maturin = node.parsed.module_markers.iter().any(|m| m == "maturin");
                if !is_maturin
                    && !node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s.starts_with("pyo3:"))
                {
                    continue;
                }
                let py_dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                let mut candidate_libs = Vec::new();
                let direct_lib = if py_dir.is_empty() {
                    "src/lib.rs".to_string()
                } else {
                    format!("{py_dir}/src/lib.rs")
                };
                if files.contains_key(&direct_lib) {
                    candidate_libs.push(direct_lib);
                }
                if candidate_libs.is_empty() {
                    for k in files.keys() {
                        if k == "src/lib.rs" || k.ends_with("/src/lib.rs") {
                            candidate_libs.push(k.clone());
                        }
                    }
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        for lib in &candidate_libs {
                            pyo3_module_to_lib_rs
                                .entry(pyo3_name.to_string())
                                .or_default()
                                .insert(lib.clone());
                        }
                    } else if is_maturin
                        && !node
                            .parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s.starts_with("pyo3:"))
                    {
                        for lib in &candidate_libs {
                            pyo3_module_to_lib_rs
                                .entry(sym.clone())
                                .or_default()
                                .insert(lib.clone());
                        }
                    }
                }
            }
        }

        let mut index = Self {
            crate_to_src_dirs,
            pyo3_module_to_lib_rs,
            reexports: BTreeMap::new(),
        };

        // Resolve `pub use` re-exports in Rust barrel modules (`lib.rs`, `mod.rs`)
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            for (path, node) in files {
                if !(path.ends_with("/mod.rs")
                    || path == "mod.rs"
                    || path.ends_with("/lib.rs")
                    || path == "src/lib.rs")
                {
                    continue;
                }
                let cdir = rust_crate_dir(path);
                let src_dir = if cdir.is_empty() {
                    "src".to_string()
                } else {
                    format!("{cdir}/src")
                };
                for sym_def in &node.parsed.defined_symbols {
                    if let Some(rest) = sym_def.strip_prefix("rs_reexport:")
                        && let Some((exported_sym, target_part)) = rest.split_once("->")
                        && let Some((lvl_str, mod_and_orig)) = target_part.split_once(':')
                        && let Some((mod_str, orig_sym)) = mod_and_orig.rsplit_once(':')
                    {
                        let level = lvl_str.parse::<usize>().unwrap_or(0);
                        let spec = ImportSpec {
                            level,
                            module: mod_str.to_string(),
                            symbols: vec![orig_sym.to_string()],
                        };
                        let targets = index.resolve_rust_use_spec_raw(
                            path, node, &src_dir, &spec, files,
                        );
                        for t in targets {
                            if t != *path {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .insert(t);
                            }
                        }
                    }
                }
            }
            index.reexports = next_reexports;
        }

        index
    }

    fn resolve_pyo3_import(
        &self,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        if !spec.module.is_empty()
            && let Some(libs) = self.pyo3_module_to_lib_rs.get(&spec.module)
        {
            for lib in libs {
                if files.contains_key(lib) {
                    resolved.insert(lib.clone());
                }
            }
        }
        for sym in &spec.symbols {
            if let Some(libs) = self.pyo3_module_to_lib_rs.get(sym) {
                for lib in libs {
                    if files.contains_key(lib) {
                        resolved.insert(lib.clone());
                    }
                }
            }
            if !spec.module.is_empty() {
                let full = format!("{}.{}", spec.module, sym);
                if let Some(libs) = self.pyo3_module_to_lib_rs.get(&full) {
                    for lib in libs {
                        if files.contains_key(lib) {
                            resolved.insert(lib.clone());
                        }
                    }
                }
            }
        }
        resolved
    }

    fn resolve_file_dependencies(
        &self,
        path: &str,
        node: &FileNode,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut deps = BTreeSet::new();
        let crate_dir = rust_crate_dir(path);
        let src_dir = if crate_dir.is_empty() {
            "src".to_string()
        } else {
            format!("{crate_dir}/src")
        };
        let build_rs = if crate_dir.is_empty() {
            "build.rs".to_string()
        } else {
            format!("{crate_dir}/build.rs")
        };
        if files.contains_key(&build_rs) && path != build_rs {
            deps.insert(build_rs);
        }

        // 1. Resolve `mod <name>;` declarations.
        // For crate/module roots (`lib.rs`, `mod.rs`) outside `tests/` that are not PyO3 entrypoints
        // and do not contain their own inline tests, `pub mod <child>;` declarations mount child modules
        // into the module tree rather than making symbols defined directly in `lib.rs`/`mod.rs`
        // (such as shared `Result`/`Error` types) depend on every sibling module.
        let is_test_tree_mod = path.starts_with("tests/") || path.contains("/tests/");
        let is_crate_or_mod_root = !is_test_tree_mod
            && (path.ends_with("/lib.rs")
                || path == "lib.rs"
                || path.ends_with("/mod.rs")
                || path == "mod.rs");
        let is_pymodule_root = node
            .parsed
            .defined_symbols
            .iter()
            .any(|s| s.starts_with("pyo3:"));
        let skip_root_mod_fanout = is_crate_or_mod_root && !is_pymodule_root && !node.is_test;
        if !skip_root_mod_fanout {
            for decl in &node.parsed.rust_mods {
                if decl.is_inline {
                    continue;
                }
                for child in resolve_rust_mod_decl(path, decl, files) {
                    if child != path {
                        deps.insert(child);
                    }
                }
            }
        }

        // 2. Resolve `use ...;` statements
        for spec in &node.parsed.imports {
            for target in self.resolve_rust_use_spec(path, node, &src_dir, spec, files) {
                if target != path {
                    deps.insert(target);
                }
            }
        }

        // 3. If an integration test (`tests/*.rs`) did not resolve any specific file under `src/`,
        // fall back to linking it to the crate's `src/lib.rs` and its mounted modules if present.
        if is_rust_integration_test_path(path) {
            let src_prefix = format!("{src_dir}/");
            let has_src_dep = deps.iter().any(|d| d.starts_with(&src_prefix));
            let lib_rs = format!("{src_dir}/lib.rs");
            if !has_src_dep
                && let Some(lib_node) = files.get(&lib_rs)
            {
                deps.insert(lib_rs.clone());
                for decl in &lib_node.parsed.rust_mods {
                    if !decl.is_inline {
                        for child in resolve_rust_mod_decl(&lib_rs, decl, files) {
                            deps.insert(child);
                        }
                    }
                }
            }
        }

        deps
    }

    fn resolve_rust_use_spec(
        &self,
        importer_path: &str,
        importer_node: &FileNode,
        src_dir: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw = self.resolve_rust_use_spec_raw(importer_path, importer_node, src_dir, spec, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            return raw;
        }
        let mut resolved = BTreeSet::new();
        for target in &raw {
            if target.ends_with("/mod.rs") || target == "mod.rs" || target.ends_with("/lib.rs") {
                let mut accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexp) = self.reexports.get(&(target.clone(), sym.clone())) {
                        resolved.extend(reexp.iter().cloned());
                    } else {
                        accounted = false;
                    }
                }
                if !accounted {
                    resolved.insert(target.clone());
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    fn resolve_rust_use_spec_raw(
        &self,
        importer_path: &str,
        importer_node: &FileNode,
        src_dir: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        let parent_dir = Path::new(importer_path)
            .parent()
            .map(normalize_rel_str)
            .unwrap_or_default();

        if spec.level > 0 {
            let has_inline_mod = importer_node.parsed.rust_mods.iter().any(|m| m.is_inline);
            if spec.module == "*" && spec.symbols.is_empty() && has_inline_mod && spec.level == 1 {
                return resolved;
            }
            let mut base = Path::new(importer_path)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            let file_name = Path::new(importer_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let extra_climb = if matches!(file_name, "mod.rs" | "lib.rs" | "main.rs") {
                spec.level
            } else {
                spec.level.saturating_sub(1)
            };
            for _ in 0..extra_climb {
                base = base
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
            }
            let base_str = normalize_rel_str(&base);
            if spec.module == "*" {
                for root_cand in ["mod.rs", "lib.rs"] {
                    let cand = if base_str.is_empty() {
                        root_cand.to_string()
                    } else {
                        format!("{base_str}/{root_cand}")
                    };
                    if files.contains_key(&cand) {
                        resolved.insert(cand);
                    }
                }
            } else {
                let parts: Vec<&str> = spec.module.split('.').filter(|s| !s.is_empty()).collect();
                add_rust_module_candidates(&base_str, &parts, &spec.symbols, files, &mut resolved);
            }
            return resolved;
        }

        let parts: Vec<&str> = spec.module.split('.').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return resolved;
        }

        if parts[0] == "crate" {
            let tail = &parts[1..];
            add_rust_module_candidates(src_dir, tail, &spec.symbols, files, &mut resolved);
            if resolved.is_empty() {
                let lib_rs = format!("{src_dir}/lib.rs");
                if files.contains_key(&lib_rs) {
                    resolved.insert(lib_rs);
                }
            }
            return resolved;
        }

        if parts[0] == "self" {
            let tail = &parts[1..];
            let file_stem = Path::new(importer_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let self_dir = if matches!(file_stem, "mod" | "lib" | "main") {
                parent_dir.clone()
            } else if parent_dir.is_empty() {
                file_stem.to_string()
            } else {
                format!("{parent_dir}/{file_stem}")
            };
            add_rust_module_candidates(&self_dir, tail, &spec.symbols, files, &mut resolved);
            add_rust_module_candidates(&parent_dir, tail, &spec.symbols, files, &mut resolved);
            return resolved;
        }

        // 1. Check relative to importer's parent directory (e.g. `use common::setup` in `tests/test_api.rs`)
        add_rust_module_candidates(&parent_dir, &parts, &spec.symbols, files, &mut resolved);

        // 2. Check registered crate names (`use my_crate::module::Item`)
        if let Some(target_src_dirs) = self.crate_to_src_dirs.get(parts[0]) {
            let tail = &parts[1..];
            for target_src in target_src_dirs {
                let before = resolved.len();
                add_rust_module_candidates(target_src, tail, &spec.symbols, files, &mut resolved);
                if resolved.len() == before {
                    let lib_rs = format!("{target_src}/lib.rs");
                    if files.contains_key(&lib_rs) {
                        resolved.insert(lib_rs);
                    }
                }
            }
        }

        // 3. Single-crate fallback when `Cargo.toml` was not loaded in memory:
        // `use any_crate_ident::submod::Item` where `src/submod.rs` exists in the current crate
        if resolved.is_empty() && parts.len() >= 2 {
            add_rust_module_candidates(src_dir, &parts[1..], &spec.symbols, files, &mut resolved);
        } else if resolved.is_empty() && parts.len() == 1 && !spec.symbols.is_empty() {
            add_rust_module_candidates(src_dir, &[], &spec.symbols, files, &mut resolved);
        }

        resolved
    }
}

const FRONTEND_RESOLVE_EXTS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte", ".json",
    ".css", ".scss", ".sass", ".less", ".svg",
];

const FRONTEND_INDEX_FILES: &[&str] = &[
    "index.tsx",
    "index.ts",
    "index.jsx",
    "index.js",
    "index.mjs",
    "index.mts",
];

struct FrontendModuleIndex {
    pkg_to_dir: BTreeMap<String, String>,
    aliases_by_dir: BTreeMap<String, Vec<(String, String)>>,
    subpath_imports_by_dir: BTreeMap<String, Vec<(String, String)>>,
    pkg_exports: BTreeMap<String, Vec<(String, String)>>,
    reexports: BTreeMap<(String, String), BTreeSet<String>>,
    wildcard_reexports: BTreeMap<String, BTreeSet<String>>,
}

impl FrontendModuleIndex {
    fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut pkg_to_dir = BTreeMap::new();
        let mut aliases_by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut subpath_imports_by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut pkg_exports: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut extends_edges: Vec<(String, String)> = Vec::new();

        for (path, node) in files {
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
                } else if let Some(alias_rule) = sym.strip_prefix("fe_alias:")
                    && let Some((alias_pfx, target_pfx)) = alias_rule.split_once("->")
                {
                    aliases_by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push((alias_pfx.to_string(), target_pfx.to_string()));
                } else if let Some(sub_rule) = sym.strip_prefix("fe_subpath:")
                    && let Some((sub_pfx, target_pfx)) = sub_rule.split_once("->")
                {
                    subpath_imports_by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push((sub_pfx.to_string(), target_pfx.to_string()));
                } else if let Some(exp_rule) = sym.strip_prefix("fe_export:")
                    && let Some((pkg_and_sub, target_pfx)) = exp_rule.split_once("->")
                    && let Some((pkg_name, sub_key)) = pkg_and_sub.rsplit_once(':')
                {
                    pkg_exports
                        .entry(pkg_name.to_string())
                        .or_default()
                        .push((sub_key.to_string(), target_pfx.to_string()));
                } else if let Some(ext_rel) = sym.strip_prefix("fe_extends:") {
                    let parent_dir = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
                    if let Some(ext_norm) = normalize_lexical_rel(&parent_dir.join(ext_rel)) {
                        let base_dir = Path::new(&ext_norm)
                            .parent()
                            .map(normalize_rel_str)
                            .unwrap_or_default();
                        extends_edges.push((dir.clone(), base_dir));
                    }
                }
            }
        }

        // Propagate `tsconfig.json` `extends` aliases from base config directory to child config directory
        for (child_dir, base_dir) in extends_edges {
            if let Some(base_aliases) = aliases_by_dir.get(&base_dir).cloned() {
                let entry = aliases_by_dir.entry(child_dir).or_default();
                for pair in base_aliases {
                    if !entry.contains(&pair) {
                        entry.push(pair);
                    }
                }
            }
        }

        let mut index = Self {
            pkg_to_dir,
            aliases_by_dir,
            subpath_imports_by_dir,
            pkg_exports,
            reexports: BTreeMap::new(),
            wildcard_reexports: BTreeMap::new(),
        };

        // Two passes to resolve nested barrel (`index.ts` / `index.tsx`) re-exports
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            let mut next_wildcards = index.wildcard_reexports.clone();
            for (path, node) in files {
                if !is_frontend_barrel_file(path) {
                    continue;
                }
                for sym_def in &node.parsed.defined_symbols {
                    if let Some(rest) = sym_def.strip_prefix("reexport:")
                        && let Some((exported_sym, target_part)) = rest.split_once("->")
                        && let Some((module_spec, orig_sym)) = target_part.rsplit_once(':')
                    {
                        let targets = index.resolve_raw_specifier(path, module_spec, files);
                        for t in targets {
                            if is_frontend_barrel_file(&t)
                                && let Some(forwarded) =
                                    index.reexports.get(&(t.clone(), orig_sym.to_string()))
                            {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .extend(forwarded.iter().cloned());
                            } else {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .insert(t);
                            }
                        }
                    }
                }
                for spec in &node.parsed.imports {
                    if spec.symbols.iter().any(|s| s == "*") {
                        let targets = index.resolve_raw_specifier(path, &spec.module, files);
                        for t in targets {
                            if is_frontend_barrel_file(&t)
                                && let Some(forwarded) = index.wildcard_reexports.get(&t)
                            {
                                next_wildcards
                                    .entry(path.clone())
                                    .or_default()
                                    .extend(forwarded.iter().cloned());
                            }
                            next_wildcards.entry(path.clone()).or_default().insert(t);
                        }
                    }
                }
            }
            index.reexports = next_reexports;
            index.wildcard_reexports = next_wildcards;
        }

        index
    }

    fn resolve_file_dependencies(
        &self,
        path: &str,
        node: &FileNode,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut deps = BTreeSet::new();
        for spec in &node.parsed.imports {
            for resolved in self.resolve_import(path, spec, files) {
                if resolved != path {
                    deps.insert(resolved);
                }
            }
        }
        // Next.js App Router (`app/**/page.tsx`, `app/**/route.ts`) automatic ancestor `layout.tsx` / `template.tsx` / `loading.tsx` linking
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if (path.starts_with("app/") || path.contains("/app/"))
            && (file_name.starts_with("page.") || file_name.starts_with("route."))
        {
            let mut cur = Path::new(path).parent();
            while let Some(dir) = cur {
                let dir_str = normalize_rel_str(dir);
                for conv in ["layout", "template", "loading"] {
                    for ext in [".tsx", ".ts", ".jsx", ".js"] {
                        let cand = if dir_str.is_empty() {
                            format!("{conv}{ext}")
                        } else {
                            format!("{dir_str}/{conv}{ext}")
                        };
                        if cand != path && files.contains_key(&cand) {
                            deps.insert(cand);
                        }
                    }
                }
                if dir_str == "app" || dir_str.ends_with("/app") || dir_str.is_empty() {
                    break;
                }
                cur = dir.parent();
            }
        }
        if node.is_test {
            let parent = Path::new(path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            let file_name = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let snap_in_dir = if parent.is_empty() {
                format!("__snapshots__/{file_name}.snap")
            } else {
                format!("{parent}/__snapshots__/{file_name}.snap")
            };
            if files.contains_key(&snap_in_dir) {
                deps.insert(snap_in_dir);
            }
            let adjacent_snap = format!("{path}.snap");
            if files.contains_key(&adjacent_snap) {
                deps.insert(adjacent_snap);
            }
        }
        deps
    }

    fn resolve_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw_targets = self.resolve_raw_specifier(importer_path, &spec.module, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            let mut expanded = raw_targets.clone();
            for t in &raw_targets {
                if let Some(wild) = self.wildcard_reexports.get(t) {
                    expanded.extend(wild.iter().cloned());
                }
            }
            return expanded;
        }

        let mut resolved = BTreeSet::new();
        for target in &raw_targets {
            if is_frontend_barrel_file(target) {
                let mut all_accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexported) = self.reexports.get(&(target.clone(), sym.clone())) {
                        resolved.extend(reexported.iter().cloned());
                    } else if files
                        .get(target)
                        .is_some_and(|n| n.parsed.defined_symbols.contains(sym))
                    {
                        resolved.insert(target.clone());
                    } else {
                        let barrel_dir = Path::new(target)
                            .parent()
                            .map(normalize_rel_str)
                            .unwrap_or_default();
                        let sym_base = if barrel_dir.is_empty() {
                            sym.clone()
                        } else {
                            format!("{barrel_dir}/{sym}")
                        };
                        let mut sub_matches = BTreeSet::new();
                        add_frontend_path_candidates(&sym_base, files, &mut sub_matches);
                        if !sub_matches.is_empty() {
                            resolved.extend(sub_matches);
                        } else {
                            all_accounted = false;
                        }
                    }
                }
                if !all_accounted {
                    resolved.insert(target.clone());
                    if let Some(wild) = self.wildcard_reexports.get(target) {
                        for w in wild {
                            if files.get(w).is_some_and(|n| {
                                spec.symbols
                                    .iter()
                                    .any(|s| n.parsed.defined_symbols.contains(s))
                            }) {
                                resolved.insert(w.clone());
                            }
                        }
                    }
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    fn resolve_raw_specifier(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        let parent = Path::new(importer_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let pkg_dir = frontend_pkg_dir(importer_path, files);

        // 0. Vite `import.meta.glob('...')` pattern expansion
        if let Some(glob_pat) = raw_module.strip_prefix("glob:") {
            let norm_glob = if glob_pat.starts_with("./") || glob_pat.starts_with("../") {
                normalize_lexical_rel(&parent.join(glob_pat)).unwrap_or_else(|| glob_pat.to_string())
            } else if let Some(rest) = glob_pat.strip_prefix('/') {
                if pkg_dir.is_empty() {
                    rest.to_string()
                } else {
                    format!("{pkg_dir}/{rest}")
                }
            } else {
                glob_pat.to_string()
            };
            if let Ok(glob) = globset::Glob::new(&norm_glob) {
                let matcher = glob.compile_matcher();
                for candidate_path in files.keys() {
                    if candidate_path != importer_path && matcher.is_match(candidate_path) {
                        resolved.insert(candidate_path.clone());
                    }
                }
            }
            return resolved;
        }

        let Some(clean_mod) = clean_frontend_import_specifier(raw_module) else {
            return resolved;
        };

        // 0b. Node.js / Package `#...` Subpath Imports (`#utils/math` -> `src/utils/math.ts`)
        if clean_mod.starts_with('#') {
            for lookup_dir in [&pkg_dir, &String::new()] {
                if let Some(rules) = self.subpath_imports_by_dir.get(lookup_dir) {
                    for (sub_pfx, target_pfx) in rules {
                        if let Some(rest) = clean_mod.strip_prefix(sub_pfx.as_str()) {
                            let mapped = if lookup_dir.is_empty() {
                                format!("{target_pfx}{rest}")
                            } else {
                                format!("{lookup_dir}/{target_pfx}{rest}")
                            };
                            if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                                add_frontend_path_candidates(&norm, files, &mut resolved);
                            }
                        }
                    }
                }
            }
            return resolved;
        }

        // 1. Relative import (`./foo`, `../bar`)
        if clean_mod.starts_with("./")
            || clean_mod.starts_with("../")
            || clean_mod == "."
            || clean_mod == ".."
        {
            if let Some(norm) = normalize_lexical_rel(&parent.join(&clean_mod)) {
                add_frontend_path_candidates(&norm, files, &mut resolved);
            }
            return resolved;
        }

        // 2. Root-relative import (`/src/...`)
        if let Some(stripped_root) = clean_mod.strip_prefix('/') {
            let cand = if pkg_dir.is_empty() {
                stripped_root.to_string()
            } else {
                format!("{pkg_dir}/{stripped_root}")
            };
            add_frontend_path_candidates(&cand, files, &mut resolved);
            if resolved.is_empty() {
                add_frontend_path_candidates(stripped_root, files, &mut resolved);
            }
            return resolved;
        }

        // 3. Configured tsconfig/jsconfig/vite path aliases (check both package dir and workspace root)
        for lookup_dir in [&pkg_dir, &String::new()] {
            if let Some(aliases) = self.aliases_by_dir.get(lookup_dir) {
                for (alias_pfx, target_pfx) in aliases {
                    if let Some(rest) = clean_mod.strip_prefix(alias_pfx.as_str()) {
                        let mapped = if lookup_dir.is_empty() {
                            format!("{target_pfx}{rest}")
                        } else {
                            format!("{lookup_dir}/{target_pfx}{rest}")
                        };
                        if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                            add_frontend_path_candidates(&norm, files, &mut resolved);
                        }
                    }
                }
            }
        }

        // 4. Conventional `@/` or `~/` alias to `{pkg_dir}/src/` or `{pkg_dir}/`
        if let Some(rest) = clean_mod
            .strip_prefix("@/")
            .or_else(|| clean_mod.strip_prefix("~/"))
        {
            for base in [
                if pkg_dir.is_empty() {
                    format!("src/{rest}")
                } else {
                    format!("{pkg_dir}/src/{rest}")
                },
                if pkg_dir.is_empty() {
                    rest.to_string()
                } else {
                    format!("{pkg_dir}/{rest}")
                },
                format!("src/{rest}"),
            ] {
                add_frontend_path_candidates(&base, files, &mut resolved);
            }
            if !resolved.is_empty() {
                return resolved;
            }
        }

        // 5. Monorepo workspace package import (`@scope/ui` or `@scope/ui/Button`)
        for (pkg_name, target_pkg_dir) in &self.pkg_to_dir {
            if &clean_mod == pkg_name {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    for (sub_k, target_v) in exp_rules {
                        if sub_k == "." || sub_k.is_empty() {
                            let mapped = if target_pkg_dir.is_empty() {
                                target_v.clone()
                            } else {
                                format!("{target_pkg_dir}/{target_v}")
                            };
                            add_frontend_path_candidates(&mapped, files, &mut resolved);
                        }
                    }
                }
                for entry_prefix in [
                    if target_pkg_dir.is_empty() {
                        "src".to_string()
                    } else {
                        format!("{target_pkg_dir}/src")
                    },
                    target_pkg_dir.clone(),
                ] {
                    add_frontend_path_candidates(&entry_prefix, files, &mut resolved);
                }
            } else if let Some(subpath) = clean_mod
                .strip_prefix(pkg_name.as_str())
                .and_then(|s| s.strip_prefix('/'))
            {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    for (sub_k, target_v) in exp_rules {
                        if sub_k == subpath {
                            let mapped = if target_pkg_dir.is_empty() {
                                target_v.clone()
                            } else {
                                format!("{target_pkg_dir}/{target_v}")
                            };
                            add_frontend_path_candidates(&mapped, files, &mut resolved);
                        } else if !sub_k.is_empty()
                            && sub_k != "."
                            && let Some(rest) = subpath.strip_prefix(sub_k.as_str())
                        {
                            let mapped = if target_pkg_dir.is_empty() {
                                format!("{target_v}{rest}")
                            } else {
                                format!("{target_pkg_dir}/{target_v}{rest}")
                            };
                            add_frontend_path_candidates(&mapped, files, &mut resolved);
                        }
                    }
                }
                for base in [
                    if target_pkg_dir.is_empty() {
                        format!("src/{subpath}")
                    } else {
                        format!("{target_pkg_dir}/src/{subpath}")
                    },
                    if target_pkg_dir.is_empty() {
                        subpath.to_string()
                    } else {
                        format!("{target_pkg_dir}/{subpath}")
                    },
                ] {
                    add_frontend_path_candidates(&base, files, &mut resolved);
                }
            }
        }

        // 6. BaseUrl fallback (`src/<clean_mod>`)
        if resolved.is_empty() && !clean_mod.starts_with('@') {
            let base = if pkg_dir.is_empty() {
                format!("src/{clean_mod}")
            } else {
                format!("{pkg_dir}/src/{clean_mod}")
            };
            add_frontend_path_candidates(&base, files, &mut resolved);
        }

        resolved
    }
}

fn add_frontend_path_candidates(
    target_path: &str,
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let norm = target_path.trim_matches('/');
    if norm.is_empty() {
        for idx_file in FRONTEND_INDEX_FILES {
            if files.contains_key(*idx_file) {
                resolved.insert((*idx_file).to_string());
            }
        }
        return;
    }

    // 1. Exact file match (e.g. `styles.module.css`, `logo.svg`, `helper.ts`)
    if files.contains_key(norm) {
        resolved.insert(norm.to_string());
    }

    // 2. TypeScript NodeNext/Bundler `.js` / `.jsx` / `.mjs` -> `.ts` / `.tsx` / `.mts` remapping
    for (js_ext, ts_exts) in [
        (".js", &[".ts", ".tsx", ".d.ts"][..]),
        (".jsx", &[".tsx", ".ts"][..]),
        (".mjs", &[".mts", ".ts"][..]),
        (".cjs", &[".cts", ".ts"][..]),
    ] {
        if let Some(stem) = norm.strip_suffix(js_ext) {
            for ts_ext in ts_exts {
                let cand = format!("{stem}{ts_ext}");
                if files.contains_key(&cand) {
                    resolved.insert(cand);
                }
            }
        }
    }

    // 3. Extensionless module path -> try frontend extensions
    for ext in FRONTEND_RESOLVE_EXTS {
        let cand = format!("{norm}{ext}");
        if files.contains_key(&cand) {
            resolved.insert(cand);
        }
    }

    // 4. Directory barrel (`index.tsx`, `index.ts`, `index.jsx`, `index.js`)
    for idx_file in FRONTEND_INDEX_FILES {
        let cand = format!("{norm}/{idx_file}");
        if files.contains_key(&cand) {
            resolved.insert(cand);
        }
    }

    // 5. Sass / SCSS partial convention (`styles/tokens` -> `styles/_tokens.scss`, `styles/_index.scss`)
    let parent = Path::new(norm)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default();
    if let Some(file_name) = Path::new(norm).file_name().and_then(|n| n.to_str())
        && !file_name.starts_with('_')
    {
        let partial_base = if parent.is_empty() {
            format!("_{file_name}")
        } else {
            format!("{parent}/_{file_name}")
        };
        if files.contains_key(&partial_base) {
            resolved.insert(partial_base.clone());
        }
        for ext in [".scss", ".sass", ".less", ".css"] {
            let cand = format!("{partial_base}{ext}");
            if files.contains_key(&cand) {
                resolved.insert(cand);
            }
        }
    }
    for idx_scss in ["_index.scss", "index.scss", "_index.sass", "index.sass"] {
        let cand = format!("{norm}/{idx_scss}");
        if files.contains_key(&cand) {
            resolved.insert(cand);
        }
    }
}

pub fn clean_frontend_import_specifier(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("node:")
        || trimmed.starts_with("virtual:")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("data:")
        || trimmed.contains('\0')
    {
        return None;
    }
    // Strip Vite query parameters such as `?raw`, `?url`, `?worker`, `?inline`, `?react`
    let without_query = trimmed.split('?').next().unwrap_or(trimmed).trim();
    if without_query.is_empty() {
        None
    } else {
        Some(without_query.replace('\\', "/"))
    }
}

fn resolve_rust_mod_decl(
    importer_path: &str,
    decl: &RustModDecl,
    files: &BTreeMap<String, FileNode>,
) -> Vec<String> {
    let mut found = Vec::new();
    let parent = Path::new(importer_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    if let Some(custom) = &decl.custom_path {
        if let Some(norm) = normalize_lexical_rel(&parent.join(custom))
            && files.contains_key(&norm)
        {
            found.push(norm);
        }
        return found;
    }
    let parent_str = normalize_rel_str(parent);
    let stem = Path::new(importer_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut base_dirs = Vec::new();
    if matches!(stem, "lib" | "main" | "mod") || is_rust_integration_test_path(importer_path) {
        base_dirs.push(parent_str);
    } else {
        let sub = if parent_str.is_empty() {
            stem.to_string()
        } else {
            format!("{parent_str}/{stem}")
        };
        base_dirs.push(sub);
        base_dirs.push(parent_str);
    }
    for base in base_dirs {
        for cand in [
            if base.is_empty() {
                format!("{}.rs", decl.name)
            } else {
                format!("{base}/{}.rs", decl.name)
            },
            if base.is_empty() {
                format!("{}/mod.rs", decl.name)
            } else {
                format!("{base}/{}/mod.rs", decl.name)
            },
        ] {
            if files.contains_key(&cand) && !found.contains(&cand) {
                found.push(cand);
            }
        }
    }
    found
}

fn add_rust_module_candidates(
    base_dir: &str,
    parts: &[&str],
    symbols: &[String],
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let joined = parts.join("/");
    let prefix = match (base_dir.is_empty(), joined.is_empty()) {
        (true, true) => String::new(),
        (true, false) => joined,
        (false, true) => base_dir.to_string(),
        (false, false) => format!("{base_dir}/{joined}"),
    };
    if !prefix.is_empty() {
        let rs_cand = format!("{prefix}.rs");
        if files.contains_key(&rs_cand) {
            resolved.insert(rs_cand);
        }
        let mod_cand = format!("{prefix}/mod.rs");
        if files.contains_key(&mod_cand) {
            resolved.insert(mod_cand);
        }
    }
    for sym in symbols {
        if sym == "*" {
            continue;
        }
        let sym_prefix = if prefix.is_empty() {
            sym.clone()
        } else {
            format!("{prefix}/{sym}")
        };
        let rs_cand = format!("{sym_prefix}.rs");
        if files.contains_key(&rs_cand) {
            resolved.insert(rs_cand);
        }
        let mod_cand = format!("{sym_prefix}/mod.rs");
        if files.contains_key(&mod_cand) {
            resolved.insert(mod_cand);
        }
    }
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
                                && let Some(forwarded) =
                                    index.reexports.get(&(t.clone(), sym.clone()))
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
                            let rel_target = parent.join(spec.module.replace('.', "/")).join(sym);
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
        && matches!(
            parts[0],
            "src" | "services" | "apps" | "packages" | "extensions"
        )
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
    StartTypeChecking,
    EndTypeChecking,
    Other,
}

fn is_type_checking_header(line_rem: &str) -> bool {
    let code = line_rem.split('#').next().unwrap_or("").trim();
    matches!(
        code,
        "if TYPE_CHECKING:" | "if typing.TYPE_CHECKING:" | "if t.TYPE_CHECKING:"
    )
}

fn push_py_other(tokens: &mut Vec<PyToken>) {
    if !matches!(tokens.last(), Some(PyToken::Other)) {
        tokens.push(PyToken::Other);
    }
}

fn push_py_newline(tokens: &mut Vec<PyToken>) {
    if !matches!(tokens.last(), Some(PyToken::Newline)) {
        tokens.push(PyToken::Newline);
    }
}

fn tokenize_python(source: &str) -> Vec<PyToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));
    let mut at_line_start = true;
    let mut line_indent = 0usize;
    let mut tc_indent: Option<usize> = None;

    while i < len {
        let b = bytes[i];
        if at_line_start {
            if b == b' ' {
                line_indent += 1;
                i += 1;
                continue;
            } else if b == b'\t' {
                line_indent += 4;
                i += 1;
                continue;
            } else if b == b'\r' {
                i += 1;
                continue;
            } else if b == b'\n' {
                push_py_newline(&mut tokens);
                line_indent = 0;
                i += 1;
                continue;
            } else if b == b'#' {
                i = memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off)
                    .unwrap_or(len);
                continue;
            } else {
                at_line_start = false;
                if let Some(base_indent) = tc_indent
                    && line_indent <= base_indent
                {
                    tokens.push(PyToken::EndTypeChecking);
                    tc_indent = None;
                }
                if tc_indent.is_none() && (b == b'i' || b == b't') {
                    let line_end = memchr::memchr(b'\n', &bytes[i..])
                        .map(|off| i + off)
                        .unwrap_or(len);
                    if is_type_checking_header(&source[i..line_end]) {
                        tc_indent = Some(line_indent);
                        tokens.push(PyToken::StartTypeChecking);
                    }
                }
            }
        }
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
                i = memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off)
                    .unwrap_or(len);
            }
            b'\n' => {
                push_py_newline(&mut tokens);
                at_line_start = true;
                line_indent = 0;
                i += 1;
            }
            b';' => {
                push_py_newline(&mut tokens);
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
                    push_py_other(&mut tokens);
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
                push_py_other(&mut tokens);
                i += 1;
            }
        }
    }
    if tc_indent.is_some() {
        tokens.push(PyToken::EndTypeChecking);
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
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
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
    let mut type_checking_imports = BTreeSet::new();
    let mut env_vars = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut fixtures_defined = BTreeSet::new();
    let mut fixtures_used = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();

    for (idx, tok) in tokens.iter().enumerate() {
        if let PyToken::StringLit(lit) = tok
            && let Some(clean_ref) = normalize_data_ref(lit)
        {
            data_refs.insert(clean_ref);
        }
        if let PyToken::Ident(ident) = tok {
            if ident == "getenv"
                && matches!(tokens.get(idx + 1), Some(PyToken::LParen))
                && let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 2)
                && is_valid_env_var_name(var_name)
            {
                env_vars.insert(var_name.trim().to_string());
            } else if ident == "environ" {
                if matches!(tokens.get(idx + 1), Some(PyToken::Dot))
                    && matches!(tokens.get(idx + 2), Some(PyToken::Ident(m)) if m == "get")
                    && matches!(tokens.get(idx + 3), Some(PyToken::LParen))
                    && let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 4)
                    && is_valid_env_var_name(var_name)
                {
                    env_vars.insert(var_name.trim().to_string());
                } else if matches!(tokens.get(idx + 1), Some(PyToken::LBracket))
                    && let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 2)
                    && matches!(tokens.get(idx + 3), Some(PyToken::RBracket))
                    && is_valid_env_var_name(var_name)
                {
                    env_vars.insert(var_name.trim().to_string());
                }
            }
        }
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut at_statement_start = true;
    let mut in_type_checking = false;
    let mut pending_fixture_decorator = false;
    let mut pending_fixture_custom_name: Option<String> = None;

    while i < n {
        match &tokens[i] {
            PyToken::StartTypeChecking => {
                in_type_checking = true;
                i += 1;
            }
            PyToken::EndTypeChecking => {
                in_type_checking = false;
                at_statement_start = true;
                i += 1;
            }
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
                let is_http_route_deco = matches!(
                    deco_parts.last().copied(),
                    Some("get" | "post" | "put" | "delete" | "patch" | "route" | "api_route")
                );
                if deco_parts.len() >= 3
                    && deco_parts[deco_parts.len() - 2] == "mark"
                    && let Some(marker) = deco_parts.last()
                {
                    markers.insert((*marker).to_string());
                }
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
                        PyToken::Ident(arg_name)
                            if is_fixture_deco
                                && paren_depth == 1
                                && arg_name == "autouse"
                                && matches!(tokens.get(i + 1), Some(PyToken::Assign)) =>
                        {
                            if matches!(tokens.get(i + 2), Some(PyToken::Ident(v)) if v == "True") {
                                module_markers.insert("autouse".to_string());
                            }
                            i += 2;
                        }
                        PyToken::StringLit(route_lit)
                            if is_http_route_deco && paren_depth == 1 && route_lit.starts_with('/') =>
                        {
                            defined_symbols.insert(format!("api_route:{}", route_lit.trim()));
                            i += 1;
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
                    if in_type_checking {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
                at_statement_start = true;
            }
            PyToken::Ident(kw) if at_statement_start && kw == "from" => {
                i += 1;
                if let Some((spec, next_i)) = parse_from_import_statement(&tokens, i) {
                    if in_type_checking {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
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
                            let fx_name = pending_fixture_custom_name.take().unwrap_or(sym_name);
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
            PyToken::Ident(ident) if at_statement_start && ident == "pytestmark" => {
                i += 1;
                let mut module_usefixtures = false;
                while i < n && !matches!(tokens[i], PyToken::Newline) {
                    if matches!(tokens.get(i), Some(PyToken::Ident(name)) if name == "mark")
                        && matches!(tokens.get(i + 1), Some(PyToken::Dot))
                        && let Some(PyToken::Ident(marker)) = tokens.get(i + 2)
                    {
                        module_markers.insert(marker.clone());
                        module_usefixtures |= marker == "usefixtures";
                    }
                    if module_usefixtures
                        && let Some(PyToken::StringLit(fixture)) = tokens.get(i)
                        && !fixture.is_empty()
                    {
                        fixtures_used.insert(fixture.clone());
                    }
                    i += 1;
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

    for spec in &imports {
        if let Some(last_seg) = spec.module.rsplit('.').next()
            && let Some(stem) = last_seg
                .strip_suffix("_pb2_grpc")
                .or_else(|| last_seg.strip_suffix("_pb2"))
        {
            defined_symbols.insert(format!("proto_use:{stem}"));
        }
        for sym in &spec.symbols {
            if let Some(stem) = sym
                .strip_suffix("_pb2_grpc")
                .or_else(|| sym.strip_suffix("_pb2"))
            {
                defined_symbols.insert(format!("proto_use:{stem}"));
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
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: Vec::new(),
        type_checking_imports: type_checking_imports.into_iter().collect(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RsToken {
    Ident(String),
    StringLit(String),
    ColonColon,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Eq,
    Hash,
    Bang,
    Star,
    Other,
}

fn push_rs_other(tokens: &mut Vec<RsToken>) {
    if !matches!(tokens.last(), Some(RsToken::Other)) {
        tokens.push(RsToken::Other);
    }
}

fn tokenize_rust(source: &str) -> Vec<RsToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i = memchr::memchr(b'\n', &bytes[i + 2..])
                    .map(|off| i + 2 + off)
                    .unwrap_or(len);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i += 2;
                let mut depth = 1usize;
                while i < len && depth > 0 {
                    if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b':' if i + 1 < len && bytes[i + 1] == b':' => {
                tokens.push(RsToken::ColonColon);
                i += 2;
            }
            b'{' => {
                tokens.push(RsToken::LBrace);
                i += 1;
            }
            b'}' => {
                tokens.push(RsToken::RBrace);
                i += 1;
            }
            b'(' => {
                tokens.push(RsToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(RsToken::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(RsToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(RsToken::RBracket);
                i += 1;
            }
            b';' => {
                tokens.push(RsToken::Semi);
                i += 1;
            }
            b',' => {
                tokens.push(RsToken::Comma);
                i += 1;
            }
            b'=' => {
                if i + 1 < len && (bytes[i + 1] == b'=' || bytes[i + 1] == b'>') {
                    push_rs_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(RsToken::Eq);
                    i += 1;
                }
            }
            b'#' => {
                tokens.push(RsToken::Hash);
                i += 1;
            }
            b'!' => {
                if i + 1 < len && bytes[i + 1] == b'=' {
                    push_rs_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(RsToken::Bang);
                    i += 1;
                }
            }
            b'*' => {
                tokens.push(RsToken::Star);
                i += 1;
            }
            b'"' => {
                let (lit, next_i) = scan_rust_string(source, i);
                tokens.push(RsToken::StringLit(lit));
                i = next_i;
            }
            b'\'' => {
                // Distinguish character literals ('a', '\n', '¥') from lifetimes ('a, 'static)
                if i + 1 < len && bytes[i + 1] == b'\\' {
                    i += 2;
                    while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                        i += 1;
                    }
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    }
                    push_rs_other(&mut tokens);
                } else if i + 1 < len
                    && let Some(ch) = source[i + 1..].chars().next()
                    && i + 1 + ch.len_utf8() < len
                    && bytes[i + 1 + ch.len_utf8()] == b'\''
                {
                    i += 1 + ch.len_utf8() + 1;
                    push_rs_other(&mut tokens);
                } else {
                    i += 1;
                    while i < len && is_ident_continue(bytes[i]) {
                        i += 1;
                    }
                    push_rs_other(&mut tokens);
                }
            }
            _ if is_ident_start(b) => {
                // Check for raw string literal `r"..."`, `r#"..."#`, `br"..."`, `br#"..."#` or byte string `b"..."`
                if let Some((lit, next_i)) = try_scan_rust_raw_or_byte_string(source, i) {
                    tokens.push(RsToken::StringLit(lit));
                    i = next_i;
                    continue;
                }
                let start = i;
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                tokens.push(RsToken::Ident(source[start..i].to_string()));
            }
            _ => {
                push_rs_other(&mut tokens);
                i += 1;
            }
        }
    }
    tokens
}

fn scan_rust_string(source: &str, quote_idx: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    while i < len {
        let Some(off) = memchr::memchr2(b'"', b'\\', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        return (source[start..i].to_string(), i + 1);
    }
    (source[start..len].to_string(), len)
}

fn try_scan_rust_raw_or_byte_string(source: &str, start_idx: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = start_idx;
    if bytes[i] == b'b' {
        if i + 1 < len && bytes[i + 1] == b'"' {
            return Some(scan_rust_string(source, i + 1));
        }
        if i + 1 < len && bytes[i + 1] == b'r' {
            i += 2;
        } else {
            return None;
        }
    } else if bytes[i] == b'r' {
        i += 1;
    } else {
        return None;
    }
    let mut hashes = 0usize;
    while i < len && bytes[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    if i >= len || bytes[i] != b'"' {
        return None;
    }
    let content_start = i + 1;
    i = content_start;
    while i < len {
        if bytes[i] == b'"' {
            let mut matched = true;
            for h in 0..hashes {
                if i + 1 + h >= len || bytes[i + 1 + h] != b'#' {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Some((source[content_start..i].to_string(), i + 1 + hashes));
            }
        }
        i += 1;
    }
    Some((source[content_start..len].to_string(), len))
}

fn normalize_include_ref(lit: &str) -> Option<String> {
    let trimmed = lit.trim().trim_start_matches("./").replace('\\', "/");
    if trimmed.is_empty()
        || trimmed.len() > 160
        || trimmed.starts_with('/')
        || trimmed.contains(['\n', '\r', '\0', ' ', '{', '}', '*', '?'])
    {
        return None;
    }
    Some(trimmed)
}

pub fn is_valid_env_var_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 128
        && trimmed
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parses a Rust source file (`.rs`) to extract `mod` declarations, `use` imports, `#[test]` / `#[tokio::test]` symbols,
/// `#[ignore]` / `#[serial]` markers, `include_str!` / `include_bytes!` / `include!` data refs, and `env!` / `env::var` keys.
pub fn parse_rust_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_rust(source);
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut rust_mods = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    for tok in &tokens {
        if let RsToken::StringLit(lit) = tok {
            if let Some(rerun) = lit.trim().strip_prefix("cargo:rerun-if-changed=") {
                let clean = rerun.trim().trim_start_matches("./").replace('\\', "/");
                if !clean.is_empty() {
                    data_refs.insert(clean);
                }
            } else if lit.starts_with("/api/") {
                defined_symbols.insert(format!("api_route:{}", lit.trim()));
            } else if let Some(clean_ref) = normalize_data_ref(lit) {
                data_refs.insert(clean_ref);
            }
        }
    }
    if source.lines().any(|l| {
        let t = l.trim_start();
        (t.starts_with("///") || t.starts_with("//!")) && t.contains("```")
    }) {
        markers.insert("doctest".to_string());
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut pending_test_attr = false;
    let mut pending_pymodule_attr = false;
    let mut pending_wasm_attr = false;
    let mut pending_cfg_test = false;
    let mut pending_pub = false;
    let mut pending_mod_path: Option<String> = None;

    while i < n {
        match &tokens[i] {
            RsToken::Hash => {
                let is_inner = matches!(tokens.get(i + 1), Some(RsToken::Bang));
                let bracket_idx = if is_inner { i + 2 } else { i + 1 };
                if matches!(tokens.get(bracket_idx), Some(RsToken::LBracket)) {
                    let mut depth = 1usize;
                    let mut j = bracket_idx + 1;
                    let mut attr_idents = Vec::new();
                    while j < n && depth > 0 {
                        match &tokens[j] {
                            RsToken::LBracket => {
                                depth += 1;
                                j += 1;
                            }
                            RsToken::RBracket => {
                                depth -= 1;
                                j += 1;
                            }
                            RsToken::Ident(id) => {
                                attr_idents.push(id.as_str());
                                if id == "path"
                                    && matches!(tokens.get(j + 1), Some(RsToken::Eq))
                                    && let Some(RsToken::StringLit(p)) = tokens.get(j + 2)
                                    && let Some(clean) = normalize_include_ref(p)
                                {
                                    pending_mod_path = Some(clean);
                                } else if id == "feature"
                                    && matches!(tokens.get(j + 1), Some(RsToken::Eq))
                                    && let Some(RsToken::StringLit(feat)) = tokens.get(j + 2)
                                    && !feat.trim().is_empty()
                                {
                                    markers.insert(format!("feature:{}", feat.trim()));
                                }
                                j += 1;
                            }
                            _ => j += 1,
                        }
                    }
                    let last_ident = attr_idents.last().copied().unwrap_or("");
                    if matches!(last_ident, "test" | "rstest" | "test_case")
                        || (attr_idents.len() >= 2 && last_ident == "test")
                    {
                        pending_test_attr = true;
                    }
                    if last_ident == "pymodule" {
                        pending_pymodule_attr = true;
                    }
                    if last_ident == "wasm_bindgen" || attr_idents.contains(&"wasm_bindgen") {
                        pending_wasm_attr = true;
                        module_markers.insert("wasm_bindgen".to_string());
                    }
                    if attr_idents.contains(&"cfg") && attr_idents.contains(&"test") {
                        pending_cfg_test = true;
                    }
                    if attr_idents.contains(&"ignore") {
                        markers.insert("ignore".to_string());
                    }
                    if attr_idents.contains(&"serial") {
                        markers.insert("serial".to_string());
                        module_markers.insert("serial".to_string());
                    }
                    if attr_idents.contains(&"background_safe") {
                        module_markers.insert("background_safe".to_string());
                    }
                    if attr_idents.contains(&"parallel_safe") {
                        module_markers.insert("parallel_safe".to_string());
                    }
                    for &id in &attr_idents {
                        if id.starts_with("resource_")
                            || matches!(id, "database" | "docker" | "network" | "port" | "live")
                        {
                            markers.insert(id.to_string());
                            if is_inner {
                                module_markers.insert(id.to_string());
                            }
                        }
                    }
                    i = j;
                    continue;
                }
                i += 1;
            }
            RsToken::Ident(kw) if kw == "pub" => {
                pending_pub = true;
                i += 1;
                if matches!(tokens.get(i), Some(RsToken::LParen)) {
                    let mut depth = 1usize;
                    i += 1;
                    while i < n && depth > 0 {
                        match &tokens[i] {
                            RsToken::LParen => depth += 1,
                            RsToken::RParen => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            RsToken::Ident(kw) if kw == "use" => {
                let (specs, next_i) = parse_rust_use_statement(&tokens, i + 1);
                for spec in specs {
                    if pending_pub {
                        for sym in &spec.symbols {
                            if sym != "*" {
                                defined_symbols.insert(format!(
                                    "rs_reexport:{sym}->{}:{}:{sym}",
                                    spec.level, spec.module
                                ));
                                defined_symbols.insert(sym.clone());
                            }
                        }
                    }
                    imports.insert(spec);
                }
                pending_pub = false;
                pending_test_attr = false;
                pending_cfg_test = false;
                pending_mod_path = None;
                i = next_i;
            }
            RsToken::Ident(kw) if kw == "mod" => {
                if let Some(RsToken::Ident(mod_name)) = tokens.get(i + 1) {
                    let is_inline = matches!(tokens.get(i + 2), Some(RsToken::LBrace));
                    let is_external = matches!(tokens.get(i + 2), Some(RsToken::Semi));
                    if pending_pymodule_attr {
                        defined_symbols.insert(format!("pyo3:{mod_name}"));
                    }
                    if is_inline || is_external {
                        rust_mods.insert(RustModDecl {
                            name: mod_name.clone(),
                            custom_path: pending_mod_path.take(),
                            is_inline,
                            is_cfg_test: pending_cfg_test,
                        });
                    }
                    pending_pub = false;
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
                    pending_wasm_attr = false;
                    pending_cfg_test = false;
                    pending_mod_path = None;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(kw) if kw == "fn" => {
                if let Some(RsToken::Ident(fn_name)) = tokens.get(i + 1) {
                    defined_symbols.insert(fn_name.clone());
                    if pending_pymodule_attr {
                        defined_symbols.insert(format!("pyo3:{fn_name}"));
                    }
                    if pending_wasm_attr {
                        defined_symbols.insert(format!("wasm:{fn_name}"));
                    }
                    if pending_test_attr {
                        test_symbols.insert(fn_name.clone());
                    }
                    pending_pub = false;
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
                    pending_wasm_attr = false;
                    pending_cfg_test = false;
                    pending_mod_path = None;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(kw)
                if matches!(
                    kw.as_str(),
                    "struct" | "enum" | "trait" | "type" | "const" | "static"
                ) =>
            {
                if kw == "const" && matches!(tokens.get(i + 1), Some(RsToken::Ident(next)) if next == "fn" || next == "unsafe")
                {
                    // `const fn` modifier before `fn`
                    i += 1;
                    continue;
                }
                if let Some(RsToken::Ident(sym_name)) = tokens.get(i + 1) {
                    defined_symbols.insert(sym_name.clone());
                    pending_test_attr = false;
                    pending_cfg_test = false;
                    pending_mod_path = None;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(kw) if kw == "macro_rules" => {
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && let Some(RsToken::Ident(macro_name)) = tokens.get(i + 2)
                {
                    defined_symbols.insert(macro_name.clone());
                    i += 3;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(mac)
                if matches!(
                    mac.as_str(),
                    "include_str" | "include_bytes" | "include"
                ) =>
            {
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && matches!(
                        tokens.get(i + 2),
                        Some(RsToken::LParen | RsToken::LBracket | RsToken::LBrace)
                    )
                    && let Some(RsToken::StringLit(lit)) = tokens.get(i + 3)
                    && let Some(clean) = normalize_include_ref(lit)
                {
                    data_refs.insert(clean);
                    i += 4;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(mac) if mac == "env" || mac == "option_env" => {
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && matches!(tokens.get(i + 2), Some(RsToken::LParen))
                    && let Some(RsToken::StringLit(var_name)) = tokens.get(i + 3)
                    && is_valid_env_var_name(var_name)
                {
                    env_vars.insert(var_name.trim().to_string());
                    i += 4;
                    continue;
                }
                if mac == "env"
                    && matches!(tokens.get(i + 1), Some(RsToken::ColonColon))
                    && matches!(
                        tokens.get(i + 2),
                        Some(RsToken::Ident(fn_name)) if fn_name == "var" || fn_name == "var_os"
                    )
                    && matches!(tokens.get(i + 3), Some(RsToken::LParen))
                    && let Some(RsToken::StringLit(var_name)) = tokens.get(i + 4)
                    && is_valid_env_var_name(var_name)
                {
                    env_vars.insert(var_name.trim().to_string());
                    i += 5;
                    continue;
                }
                i += 1;
            }
            RsToken::Semi => {
                pending_test_attr = false;
                pending_cfg_test = false;
                pending_mod_path = None;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: Vec::new(),
        fixtures_used: Vec::new(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: rust_mods.into_iter().collect(),
        type_checking_imports: Vec::new(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}

fn parse_rust_use_statement(tokens: &[RsToken], start: usize) -> (Vec<ImportSpec>, usize) {
    let n = tokens.len();
    let mut end = start;
    let mut brace_depth = 0usize;
    while end < n {
        match &tokens[end] {
            RsToken::LBrace => {
                brace_depth += 1;
                end += 1;
            }
            RsToken::RBrace => {
                brace_depth = brace_depth.saturating_sub(1);
                end += 1;
            }
            RsToken::Semi if brace_depth == 0 => {
                break;
            }
            _ => {
                end += 1;
            }
        }
    }
    let slice = &tokens[start..end];
    let next_i = if end < n && matches!(tokens[end], RsToken::Semi) {
        end + 1
    } else {
        end
    };
    let paths = expand_rust_use_slice(&[], slice);
    let mut grouped: BTreeMap<(usize, String), BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        if let Some((level, module, sym)) = rust_use_path_to_parts(&path) {
            let entry = grouped.entry((level, module)).or_default();
            if let Some(s) = sym {
                entry.insert(s);
            }
        }
    }
    let specs = grouped
        .into_iter()
        .map(|((level, module), syms)| ImportSpec {
            level,
            module,
            symbols: syms.into_iter().collect(),
        })
        .collect();
    (specs, next_i)
}

fn expand_rust_use_slice(prefix: &[String], slice: &[RsToken]) -> Vec<Vec<String>> {
    let mut results = Vec::new();
    let mut current = prefix.to_vec();
    let mut i = 0usize;
    let n = slice.len();
    while i < n {
        match &slice[i] {
            RsToken::ColonColon => {
                i += 1;
            }
            RsToken::Ident(kw) if kw == "as" => {
                i += 2;
            }
            RsToken::Ident(seg) => {
                if seg == "self" && !current.is_empty() {
                    // `use crate::foo::{self, bar}` -> `current` itself is imported
                } else {
                    current.push(seg.clone());
                }
                i += 1;
            }
            RsToken::Star => {
                current.push("*".to_string());
                i += 1;
            }
            RsToken::LBrace => {
                // Find matching RBrace and split top-level comma items
                let mut depth = 1usize;
                let mut j = i + 1;
                let mut item_start = j;
                while j < n && depth > 0 {
                    match &slice[j] {
                        RsToken::LBrace => {
                            depth += 1;
                            j += 1;
                        }
                        RsToken::RBrace => {
                            depth -= 1;
                            if depth == 0 {
                                let sub = &slice[item_start..j];
                                if !sub.is_empty() {
                                    results.extend(expand_rust_use_slice(&current, sub));
                                }
                            }
                            j += 1;
                        }
                        RsToken::Comma if depth == 1 => {
                            let sub = &slice[item_start..j];
                            if !sub.is_empty() {
                                results.extend(expand_rust_use_slice(&current, sub));
                            }
                            j += 1;
                            item_start = j;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                return results;
            }
            _ => {
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        results.push(current);
    }
    results
}

fn rust_use_path_to_parts(path: &[String]) -> Option<(usize, String, Option<String>)> {
    if path.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    let mut super_count = 0usize;
    while idx < path.len() && path[idx] == "super" {
        super_count += 1;
        idx += 1;
    }
    let rest = &path[idx..];
    if super_count > 0 {
        if rest.is_empty() {
            return None;
        }
        if rest.len() == 1 {
            return Some((super_count, rest[0].clone(), None));
        }
        let module = rest[..rest.len() - 1].join(".");
        let sym = rest.last().cloned();
        return Some((super_count, module, sym));
    }
    if rest.len() == 1 {
        return Some((0, rest[0].clone(), None));
    }
    let module = rest[..rest.len() - 1].join(".");
    let sym = rest.last().cloned();
    Some((0, module, sym))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum JsToken {
    Ident(String),
    StringLit(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Semi,
    Star,
    Other,
}

fn push_js_other(tokens: &mut Vec<JsToken>) {
    if !matches!(tokens.last(), Some(JsToken::Other)) {
        tokens.push(JsToken::Other);
    }
}

fn tokenize_frontend(source: &str) -> Vec<JsToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i = memchr::memchr(b'\n', &bytes[i + 2..])
                    .map(|off| i + 2 + off)
                    .unwrap_or(len);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i += 2;
                loop {
                    if let Some(off) = memchr::memchr(b'*', &bytes[i..]) {
                        i += off;
                        if i + 1 < len && bytes[i + 1] == b'/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    } else {
                        i = len;
                        break;
                    }
                }
            }
            b'\'' | b'"' => {
                let (lit, next_i) = scan_js_string(source, i, b);
                tokens.push(JsToken::StringLit(lit));
                i = next_i;
            }
            b'`' => {
                let (lit, next_i) = scan_js_template_string(source, i);
                if !lit.is_empty() {
                    tokens.push(JsToken::StringLit(lit));
                }
                i = next_i;
            }
            b'{' => {
                tokens.push(JsToken::LBrace);
                i += 1;
            }
            b'}' => {
                tokens.push(JsToken::RBrace);
                i += 1;
            }
            b'(' => {
                tokens.push(JsToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(JsToken::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(JsToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(JsToken::RBracket);
                i += 1;
            }
            b',' => {
                tokens.push(JsToken::Comma);
                i += 1;
            }
            b'.' => {
                tokens.push(JsToken::Dot);
                i += 1;
            }
            b';' => {
                tokens.push(JsToken::Semi);
                i += 1;
            }
            b'*' => {
                tokens.push(JsToken::Star);
                i += 1;
            }
            _ if is_ident_start(b) || b == b'$' => {
                let start = i;
                i += 1;
                while i < len && (is_ident_continue(bytes[i]) || bytes[i] == b'$') {
                    i += 1;
                }
                tokens.push(JsToken::Ident(source[start..i].to_string()));
            }
            _ => {
                push_js_other(&mut tokens);
                i += 1;
            }
        }
    }
    tokens
}

fn scan_js_string(source: &str, quote_idx: usize, quote: u8) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    while i < len {
        let Some(off) = memchr::memchr3(quote, b'\\', b'\n', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        return (source[start..i].to_string(), (i + 1).min(len));
    }
    (source[start..len].to_string(), len)
}

fn scan_js_template_string(source: &str, quote_idx: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    let mut has_interpolation = false;
    while i < len {
        let Some(off) = memchr::memchr3(b'`', b'\\', b'$', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        if bytes[i] == b'$' && i + 1 < len && bytes[i + 1] == b'{' {
            has_interpolation = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            let lit = if has_interpolation {
                String::new()
            } else {
                source[start..i].to_string()
            };
            return (lit, i + 1);
        }
        i += 1;
    }
    (String::new(), len)
}

/// Parses a frontend source file (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.vue`, `.svelte`)
/// to extract ES/CJS/dynamic imports, `import type` exclusions, Barrel re-exports, Vite asset queries,
/// `import.meta.env` / `process.env` keys, React component/hook definitions, and Vitest/Jest/Node test symbols.
pub fn parse_frontend_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_frontend(source);
    let mut imports = BTreeSet::new();
    let mut type_checking_imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    for tok in &tokens {
        if let JsToken::StringLit(lit) = tok
            && let Some(clean_spec) = clean_frontend_import_specifier(lit)
            && let Some(clean_ref) = normalize_data_ref(&clean_spec)
        {
            data_refs.insert(clean_ref);
        }
    }

    let n = tokens.len();
    let mut i = 0usize;

    while i < n {
        match &tokens[i] {
            JsToken::Ident(kw) if kw == "import" => {
                // 1. `import.meta.env.VITE_...` or `import.meta.vitest`
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(m)) if m == "meta")
                    && matches!(tokens.get(i + 3), Some(JsToken::Dot))
                {
                    if let Some(JsToken::Ident(prop)) = tokens.get(i + 4) {
                        if prop == "vitest" {
                            markers.insert("in_source_vitest".to_string());
                        } else if prop == "glob"
                            && matches!(tokens.get(i + 5), Some(JsToken::LParen))
                        {
                            let mut j = i + 6;
                            while j < n && !matches!(tokens[j], JsToken::RParen) {
                                if let JsToken::StringLit(pat) = &tokens[j] {
                                    let trimmed = pat.trim();
                                    if !trimmed.is_empty() {
                                        imports.insert(ImportSpec {
                                            level: 0,
                                            module: format!("glob:{trimmed}"),
                                            symbols: vec!["*".to_string()],
                                        });
                                    }
                                }
                                j += 1;
                            }
                            i = (j + 1).min(n);
                            continue;
                        } else if prop == "env"
                            && matches!(tokens.get(i + 5), Some(JsToken::Dot))
                            && let Some(JsToken::Ident(env_key)) = tokens.get(i + 6)
                            && is_valid_env_var_name(env_key)
                        {
                            env_vars.insert(env_key.clone());
                            i += 7;
                            continue;
                        }
                    }
                    i += 4;
                    continue;
                }
                // 2. Dynamic `import('...')`
                if matches!(tokens.get(i + 1), Some(JsToken::LParen)) {
                    if let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                        && let Some(clean) = clean_frontend_import_specifier(spec)
                    {
                        imports.insert(ImportSpec {
                            level: 0,
                            module: clean,
                            symbols: Vec::new(),
                        });
                    }
                    i += 2;
                    continue;
                }
                // 3. Static `import ...`
                let (parsed_spec, is_type_only, next_i) = parse_js_import_clause(&tokens, i + 1);
                if let Some(spec) = parsed_spec {
                    if spec.module == "@playwright/test" || spec.module.starts_with("@playwright/") {
                        markers.insert("playwright".to_string());
                        module_markers.insert("e2e".to_string());
                    } else if spec.module == "@storybook/test"
                        || spec.module.starts_with("@storybook/testing-")
                    {
                        markers.insert("storybook_play".to_string());
                    } else if spec.module.contains("msw") {
                        markers.insert("msw".to_string());
                    }
                    for sym in &spec.symbols {
                        defined_symbols.insert(format!("wasm_use:{sym}"));
                    }
                    if let Some(proto_stem) = spec
                        .module
                        .rsplit('/')
                        .next()
                        .and_then(|s| s.strip_suffix("_pb").or_else(|| s.strip_suffix(".pb")))
                    {
                        defined_symbols.insert(format!("proto_use:{proto_stem}"));
                    }
                    if is_type_only {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
            }
            JsToken::Ident(kw) if kw == "fetch" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(url)) = tokens.get(i + 2)
                    && url.trim().starts_with("/api/")
                {
                    let route = url.trim().split('?').next().unwrap_or(url.trim());
                    defined_symbols.insert(format!("api_call:{route}"));
                    i += 3;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "axios" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(_)))
                    && matches!(tokens.get(i + 3), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(url)) = tokens.get(i + 4)
                    && url.trim().starts_with("/api/")
                {
                    let route = url.trim().split('?').next().unwrap_or(url.trim());
                    defined_symbols.insert(format!("api_call:{route}"));
                    i += 5;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "useContext" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::Ident(ctx_name)) = tokens.get(i + 2)
                {
                    defined_symbols.insert(format!("ctx_use:{ctx_name}"));
                    i += 3;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "cy" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && let Some(JsToken::Ident(m)) = tokens.get(i + 2)
                    && matches!(m.as_str(), "visit" | "mount" | "get" | "intercept")
                {
                    markers.insert("cypress".to_string());
                    module_markers.insert("e2e".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "URL" => {
                // `new URL('./worker.ts', import.meta.url)`
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                    && matches!(tokens.get(i + 3), Some(JsToken::Comma))
                    && matches!(tokens.get(i + 4), Some(JsToken::Ident(im)) if im == "import")
                    && let Some(clean) = clean_frontend_import_specifier(spec)
                {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: Vec::new(),
                    });
                    i += 5;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "play" => {
                // Storybook interaction test (`Primary.play = async ...` or `play: async ({ canvasElement }) => ...`)
                if matches!(tokens.get(i + 1), Some(JsToken::Other)) {
                    markers.insert("storybook_play".to_string());
                    test_symbols.insert("play".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "export" => {
                let (reexport_opt, is_type_only, exported_defs, reexport_links, next_i) =
                    parse_js_export_clause(&tokens, i + 1);
                for d in exported_defs {
                    defined_symbols.insert(d);
                }
                for link in reexport_links {
                    defined_symbols.insert(link);
                }
                if let Some(spec) = reexport_opt {
                    if is_type_only {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
            }
            JsToken::Ident(kw) if kw == "require" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                    && let Some(clean) = clean_frontend_import_specifier(spec)
                {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: Vec::new(),
                    });
                    i += 3;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "vi" || kw == "jest" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && let Some(JsToken::Ident(method)) = tokens.get(i + 2)
                    && matches!(
                        method.as_str(),
                        "mock" | "doMock" | "importActual" | "requireActual"
                    )
                    && matches!(tokens.get(i + 3), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(spec)) = tokens.get(i + 4)
                    && let Some(clean) = clean_frontend_import_specifier(spec)
                {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: Vec::new(),
                    });
                    i += 5;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "process" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(e)) if e == "env")
                {
                    if matches!(tokens.get(i + 3), Some(JsToken::Dot))
                        && let Some(JsToken::Ident(env_key)) = tokens.get(i + 4)
                        && is_valid_env_var_name(env_key)
                    {
                        env_vars.insert(env_key.clone());
                        i += 5;
                        continue;
                    }
                    if matches!(tokens.get(i + 3), Some(JsToken::LBracket))
                        && let Some(JsToken::StringLit(env_key)) = tokens.get(i + 4)
                        && matches!(tokens.get(i + 5), Some(JsToken::RBracket))
                        && is_valid_env_var_name(env_key)
                    {
                        env_vars.insert(env_key.trim().to_string());
                        i += 6;
                        continue;
                    }
                }
                i += 1;
            }
            JsToken::Ident(kw) if matches!(kw.as_str(), "test" | "it" | "describe") => {
                let mut j = i + 1;
                while j < n && matches!(tokens.get(j), Some(JsToken::Dot)) {
                    if let Some(JsToken::Ident(modifier)) = tokens.get(j + 1) {
                        match modifier.as_str() {
                            "skip" | "todo" => {
                                markers.insert(modifier.clone());
                            }
                            "concurrent" => {
                                markers.insert("concurrent".to_string());
                                module_markers.insert("parallel_safe".to_string());
                            }
                            "sequential" | "serial" => {
                                markers.insert("serial".to_string());
                                module_markers.insert("serial".to_string());
                            }
                            "only" => {
                                markers.insert("only".to_string());
                            }
                            _ => {}
                        }
                        j += 2;
                        // Handle `.each(...)` call before `('test name', ...)`
                        if modifier == "each" && matches!(tokens.get(j), Some(JsToken::LParen)) {
                            let mut depth = 1usize;
                            j += 1;
                            while j < n && depth > 0 {
                                match &tokens[j] {
                                    JsToken::LParen => depth += 1,
                                    JsToken::RParen => depth -= 1,
                                    _ => {}
                                }
                                j += 1;
                            }
                        }
                    } else {
                        break;
                    }
                }
                if matches!(tokens.get(j), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(name)) = tokens.get(j + 1)
                {
                    let trimmed = name.trim();
                    if !trimmed.is_empty() && trimmed.len() <= 200 {
                        test_symbols.insert(trimmed.to_string());
                    }
                    i = j + 2;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw)
                if matches!(kw.as_str(), "function" | "class" | "const" | "let") =>
            {
                if let Some(JsToken::Ident(sym_name)) = tokens.get(i + 1) {
                    defined_symbols.insert(sym_name.clone());
                    // Detect `const ThemeContext = createContext(...)` or `React.createContext(...)`
                    for lookahead in 2..=6 {
                        if matches!(
                            tokens.get(i + lookahead),
                            Some(JsToken::Ident(fn_id)) if fn_id == "createContext"
                        ) {
                            defined_symbols.insert(format!("ctx_def:{sym_name}"));
                            break;
                        }
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: Vec::new(),
        fixtures_used: Vec::new(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: Vec::new(),
        type_checking_imports: type_checking_imports.into_iter().collect(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}

/// Parses a CSS / SCSS / Sass / Less stylesheet to extract `@import`, `@use`, `@forward`, `url(...)`, and `.className` selectors.
pub fn parse_css_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_frontend(source);
    let mut imports = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let n = tokens.len();
    let mut i = 0usize;

    while i < n {
        match &tokens[i] {
            JsToken::Dot => {
                if let Some(JsToken::Ident(cls_name)) = tokens.get(i + 1) {
                    defined_symbols.insert(format!("css_class:{cls_name}"));
                    defined_symbols.insert(cls_name.clone());
                    i += 2;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if matches!(kw.as_str(), "import" | "use" | "forward") => {
                if let Some(JsToken::StringLit(spec)) = tokens.get(i + 1) {
                    if let Some(clean) = clean_frontend_import_specifier(spec) {
                        let module = if clean.starts_with('.') || clean.starts_with('/') || clean.starts_with('@') || clean.starts_with('~') {
                            clean
                        } else {
                            format!("./{clean}")
                        };
                        imports.insert(ImportSpec {
                            level: 0,
                            module,
                            symbols: Vec::new(),
                        });
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "url" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                    && let Some(clean) = clean_frontend_import_specifier(spec)
                {
                    if let Some(clean_ref) = normalize_data_ref(&clean) {
                        data_refs.insert(clean_ref);
                    }
                    let module = if clean.starts_with('.') || clean.starts_with('/') {
                        clean
                    } else {
                        format!("./{clean}")
                    };
                    imports.insert(ImportSpec {
                        level: 0,
                        module,
                        symbols: Vec::new(),
                    });
                    i += 3;
                    continue;
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

/// Parses polyglot source files: Go (`.go`), C# (`.cs`), Java/Kotlin (`.java`, `.kt`),
/// C/C++ (`.c`, `.cpp`, `.cc`, `.h`, `.hpp`), Protobuf (`.proto`), and GraphQL (`.graphql`, `.gql`).
pub fn parse_polyglot_source(rel_path: &str, source: &str) -> ParsedPythonFile {
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    if rel_path.ends_with(".proto") {
        if let Some(stem) = Path::new(rel_path).file_stem().and_then(|s| s.to_str()) {
            defined_symbols.insert(format!("proto:{stem}"));
        }
        for raw_line in source.lines() {
            let line = raw_line.split("//").next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("import ") {
                let target = rest
                    .trim_start_matches("public ")
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_matches(['"', '\'']);
                if !target.is_empty() {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: target.to_string(),
                        symbols: Vec::new(),
                    });
                }
            } else if let Some(rest) = line
                .strip_prefix("message ")
                .or_else(|| line.strip_prefix("service "))
            {
                let name = rest.split(['{', ' ']).next().unwrap_or("").trim();
                if !name.is_empty() {
                    defined_symbols.insert(name.to_string());
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            defined_symbols: defined_symbols.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    if rel_path.ends_with(".graphql") || rel_path.ends_with(".gql") {
        for raw_line in source.lines() {
            let trimmed = raw_line.trim();
            if let Some(rest) = trimmed.strip_prefix("#import ") {
                let spec = rest.trim().trim_matches(['"', '\'', ';']);
                if !spec.is_empty() {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: spec.to_string(),
                        symbols: Vec::new(),
                    });
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    if rel_path.ends_with(".go") {
        let mut in_import_block = false;
        for raw_line in source.lines() {
            let trimmed = raw_line.trim();
            if let Some(embed) = trimmed.strip_prefix("//go:embed ") {
                for part in embed.split_whitespace() {
                    let clean = part.trim_start_matches("./");
                    if !clean.is_empty() {
                        data_refs.insert(clean.to_string());
                    }
                }
            }
            let line = trimmed.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if in_import_block {
                if line.starts_with(')') {
                    in_import_block = false;
                    continue;
                }
                if let Some(start_q) = line.find('"')
                    && let Some(end_q) = line[start_q + 1..].find('"')
                {
                    let pkg = &line[start_q + 1..start_q + 1 + end_q];
                    if !pkg.is_empty() {
                        imports.insert(ImportSpec {
                            level: 0,
                            module: pkg.to_string(),
                            symbols: Vec::new(),
                        });
                    }
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("import ") {
                let r = rest.trim();
                if r.starts_with('(') {
                    in_import_block = true;
                } else if let Some(start_q) = r.find('"')
                    && let Some(end_q) = r[start_q + 1..].find('"')
                {
                    let pkg = &r[start_q + 1..start_q + 1 + end_q];
                    if !pkg.is_empty() {
                        imports.insert(ImportSpec {
                            level: 0,
                            module: pkg.to_string(),
                            symbols: Vec::new(),
                        });
                    }
                }
            } else if let Some(rest) = line.strip_prefix("func ") {
                let fn_part = rest.trim();
                let fn_name = fn_part.split('(').next().unwrap_or("").trim();
                if !fn_name.is_empty() && !fn_name.contains(' ') {
                    defined_symbols.insert(fn_name.to_string());
                    if (fn_name.starts_with("Test")
                        || fn_name.starts_with("Benchmark")
                        || fn_name.starts_with("Example")
                        || fn_name.starts_with("Fuzz"))
                        && rel_path.ends_with("_test.go")
                    {
                        test_symbols.insert(fn_name.to_string());
                    }
                }
            }
            if let Some(idx) = line.find("os.Getenv(\"") {
                let after = &line[idx + "os.Getenv(\"".len()..];
                if let Some(end_q) = after.find('"') {
                    let var = &after[..end_q];
                    if is_valid_env_var_name(var) {
                        env_vars.insert(var.to_string());
                    }
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            test_symbols: test_symbols.into_iter().collect(),
            defined_symbols: defined_symbols.into_iter().collect(),
            data_refs: data_refs.into_iter().collect(),
            env_vars_referenced: env_vars.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    // C/C++ (`.c`, `.cpp`, `.cc`, `.h`, `.hpp`), C# (`.cs`), Java/Kotlin (`.java`, `.kt`)
    let mut pending_test_attr = false;
    for raw_line in source.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // C/C++ `#include "foo.h"` and `TEST(Suite, Name)` / `TEST_F(Suite, Name)`
        if let Some(inc) = line.strip_prefix("#include ") {
            let trimmed = inc.trim();
            if trimmed.starts_with('"')
                && let Some(end_q) = trimmed[1..].find('"')
            {
                let hdr = &trimmed[1..1 + end_q];
                if !hdr.is_empty() {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: hdr.to_string(),
                        symbols: Vec::new(),
                    });
                }
            }
        } else if (line.starts_with("TEST(") || line.starts_with("TEST_F("))
            && let Some(open_p) = line.find('(')
            && let Some(close_p) = line[open_p + 1..].find(')')
        {
            let args: Vec<&str> = line[open_p + 1..open_p + 1 + close_p]
                .split(',')
                .map(str::trim)
                .collect();
            if args.len() == 2 {
                test_symbols.insert(format!("{}.{}", args[0], args[1]));
            }
        }
        // C# `[Fact]`, `[Theory]`, `[Test]`, `[TestMethod]` and Java/Kotlin `@Test`, `@ParameterizedTest`
        if line.starts_with("[Fact")
            || line.starts_with("[Theory")
            || line.starts_with("[Test")
            || line.starts_with("@Test")
            || line.starts_with("@ParameterizedTest")
        {
            pending_test_attr = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("using ") {
            let ns = rest.trim_end_matches(';').trim();
            if !ns.is_empty() && !ns.contains('(') && !ns.starts_with("System") {
                imports.insert(ImportSpec {
                    level: 0,
                    module: ns.to_string(),
                    symbols: Vec::new(),
                });
            }
        } else if let Some(rest) = line.strip_prefix("import ") {
            let imp = rest.trim_end_matches(';').trim();
            if !imp.is_empty() && !imp.starts_with("java.") && !imp.starts_with("kotlin.") {
                imports.insert(ImportSpec {
                    level: 0,
                    module: imp.to_string(),
                    symbols: Vec::new(),
                });
            }
        }
        for kw in ["class ", "interface ", "struct ", "record "] {
            if let Some(idx) = line.find(kw) {
                let after = &line[idx + kw.len()..];
                let name = after
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if !name.is_empty() {
                    defined_symbols.insert(name.to_string());
                }
            }
        }
        if pending_test_attr
            && let Some(paren_idx) = line.find('(')
        {
            let before_paren = line[..paren_idx].trim();
            if let Some(fn_name) = before_paren.split_whitespace().last() {
                let clean_name = fn_name.trim_start_matches("fun ").trim();
                if !clean_name.is_empty() {
                    test_symbols.insert(clean_name.to_string());
                    defined_symbols.insert(clean_name.to_string());
                }
            }
            pending_test_attr = false;
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        env_vars_referenced: env_vars.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

fn resolve_polyglot_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    let parent_str = normalize_rel_str(parent);

    // 1. Go: `_test.go` files implicitly compile with all non-test `.go` files in the same package directory,
    // and import paths starting with the `go.mod` module prefix resolve to workspace directories.
    if path.ends_with(".go") {
        if path.ends_with("_test.go") {
            for candidate in files.keys() {
                if candidate != path
                    && candidate.ends_with(".go")
                    && !candidate.ends_with("_test.go")
                {
                    let cand_parent = Path::new(candidate)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    if cand_parent == parent_str {
                        deps.insert(candidate.clone());
                    }
                }
            }
        }
        let mut go_modules: Vec<(String, String)> = Vec::new();
        for (cfg_path, cfg_node) in files {
            if cfg_path == "go.mod" || cfg_path.ends_with("/go.mod") {
                let cfg_dir = Path::new(cfg_path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                for sym in &cfg_node.parsed.defined_symbols {
                    if let Some(mod_name) = sym.strip_prefix("go_mod:") {
                        go_modules.push((mod_name.to_string(), cfg_dir.clone()));
                    }
                }
            }
        }
        for spec in &node.parsed.imports {
            for (mod_name, mod_dir) in &go_modules {
                if let Some(rest) = spec.module.strip_prefix(mod_name.as_str()) {
                    let sub = rest.trim_start_matches('/');
                    let target_dir = if mod_dir.is_empty() {
                        sub.to_string()
                    } else if sub.is_empty() {
                        mod_dir.clone()
                    } else {
                        format!("{mod_dir}/{sub}")
                    };
                    for candidate in files.keys() {
                        if candidate != path
                            && candidate.ends_with(".go")
                            && !candidate.ends_with("_test.go")
                        {
                            let cand_parent = Path::new(candidate)
                                .parent()
                                .map(normalize_rel_str)
                                .unwrap_or_default();
                            if cand_parent == target_dir {
                                deps.insert(candidate.clone());
                            }
                        }
                    }
                }
            }
        }
        return deps;
    }

    // 2. Relative header/proto/graphql imports (`#include "foo.h"`, `import "common.proto"`, `#import "./frag.graphql"`)
    for spec in &node.parsed.imports {
        if let Some(norm_rel) = normalize_lexical_rel(&parent.join(&spec.module))
            && files.contains_key(&norm_rel)
            && norm_rel != path
        {
            deps.insert(norm_rel.clone());
            // For `.h` / `.hpp` includes, also link matching `.c` / `.cpp` / `.cc` implementation file
            for hdr_ext in [".hpp", ".h"] {
                if let Some(stem) = norm_rel.strip_suffix(hdr_ext) {
                    for impl_ext in [".cpp", ".cc", ".c"] {
                        let impl_cand = format!("{stem}{impl_ext}");
                        if files.contains_key(&impl_cand) && impl_cand != path {
                            deps.insert(impl_cand);
                        }
                    }
                }
            }
        }
        if let Some(norm_root) = normalize_lexical_rel(Path::new(&spec.module))
            && files.contains_key(&norm_root)
            && norm_root != path
        {
            deps.insert(norm_root);
        }
        // Java/Kotlin `import com.acme.Service` or C# `using Acme.Service` -> match defined class/symbol names
        if let Some(last_seg) = spec.module.rsplit('.').next()
            && !last_seg.is_empty()
            && last_seg != "*"
        {
            let lang_fam = file_language_family(path);
            for (cand_path, cand_node) in files {
                if cand_path != path
                    && file_language_family(cand_path) == lang_fam
                    && cand_node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s == last_seg)
                {
                    deps.insert(cand_path.clone());
                }
            }
        }
    }
    deps
}

fn parse_js_import_clause(
    tokens: &[JsToken],
    mut i: usize,
) -> (Option<ImportSpec>, bool, usize) {
    let n = tokens.len();
    // Side-effect import: `import './styles.css'`
    if let Some(JsToken::StringLit(spec)) = tokens.get(i) {
        let parsed = clean_frontend_import_specifier(spec).map(|module| ImportSpec {
            level: 0,
            module,
            symbols: Vec::new(),
        });
        return (parsed, false, i + 1);
    }

    let mut top_type_only = false;
    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
        && !matches!(tokens.get(i + 1), Some(JsToken::Ident(f)) if f == "from")
    {
        top_type_only = true;
        i += 1;
    }

    let mut value_symbols = BTreeSet::new();
    let mut type_symbols = BTreeSet::new();
    let mut had_named_or_default = false;

    while i < n {
        match &tokens[i] {
            JsToken::Ident(kw) if kw == "from" => {
                if let Some(JsToken::StringLit(spec)) = tokens.get(i + 1) {
                    let is_type_only = top_type_only
                        || (had_named_or_default
                            && value_symbols.is_empty()
                            && !type_symbols.is_empty());
                    let syms = if is_type_only {
                        type_symbols.into_iter().collect()
                    } else {
                        value_symbols.into_iter().collect()
                    };
                    let parsed = clean_frontend_import_specifier(spec).map(|module| ImportSpec {
                        level: 0,
                        module,
                        symbols: syms,
                    });
                    return (parsed, is_type_only, i + 2);
                }
                return (None, false, i + 1);
            }
            JsToken::Star => {
                had_named_or_default = true;
                if top_type_only {
                    type_symbols.insert("*".to_string());
                } else {
                    value_symbols.insert("*".to_string());
                }
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as") {
                    i += 2;
                }
            }
            JsToken::LBrace => {
                had_named_or_default = true;
                i += 1;
                while i < n && !matches!(tokens[i], JsToken::RBrace) {
                    let mut item_type_only = top_type_only;
                    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
                        && matches!(tokens.get(i + 1), Some(JsToken::Ident(_)))
                    {
                        item_type_only = true;
                        i += 1;
                    }
                    if let Some(JsToken::Ident(orig_sym)) = tokens.get(i) {
                        let sym_to_record = orig_sym.clone();
                        i += 1;
                        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as") {
                            i += 2;
                        }
                        if item_type_only {
                            type_symbols.insert(sym_to_record);
                        } else {
                            value_symbols.insert(sym_to_record);
                        }
                    } else {
                        i += 1;
                    }
                    if matches!(tokens.get(i), Some(JsToken::Comma)) {
                        i += 1;
                    }
                }
                if i < n && matches!(tokens[i], JsToken::RBrace) {
                    i += 1;
                }
            }
            JsToken::Ident(default_ident) => {
                had_named_or_default = true;
                let _ = default_ident;
                if top_type_only {
                    type_symbols.insert("default".to_string());
                } else {
                    value_symbols.insert("default".to_string());
                }
                i += 1;
            }
            JsToken::Comma => {
                i += 1;
            }
            JsToken::Semi => {
                return (None, false, i + 1);
            }
            _ => {
                return (None, false, i + 1);
            }
        }
    }
    (None, false, i)
}

fn parse_js_export_clause(
    tokens: &[JsToken],
    mut i: usize,
) -> (Option<ImportSpec>, bool, Vec<String>, Vec<String>, usize) {
    let n = tokens.len();
    let mut defined = Vec::new();
    let mut reexport_links = Vec::new();
    let mut top_type_only = false;

    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type") {
        if matches!(tokens.get(i + 1), Some(JsToken::LBrace | JsToken::Star)) {
            top_type_only = true;
            i += 1;
        } else if let Some(JsToken::Ident(type_name)) = tokens.get(i + 1) {
            defined.push(type_name.clone());
            return (None, true, defined, reexport_links, i + 2);
        }
    }

    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "default") {
        defined.push("default".to_string());
        i += 1;
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "function" || kw == "class")
            && let Some(JsToken::Ident(name)) = tokens.get(i + 1)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 2);
        }
        return (None, false, defined, reexport_links, i);
    }

    if let Some(JsToken::Ident(kw)) = tokens.get(i) {
        if kw == "async"
            && matches!(tokens.get(i + 1), Some(JsToken::Ident(f)) if f == "function")
            && let Some(JsToken::Ident(name)) = tokens.get(i + 2)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 3);
        }
        if matches!(
            kw.as_str(),
            "function" | "class" | "const" | "let" | "var" | "interface" | "enum"
        ) && let Some(JsToken::Ident(name)) = tokens.get(i + 1)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 2);
        }
    }

    // `export * from './sub'` or `export * as ns from './sub'`
    if matches!(tokens.get(i), Some(JsToken::Star)) {
        i += 1;
        let mut alias_name: Option<String> = None;
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as")
            && let Some(JsToken::Ident(ns)) = tokens.get(i + 1)
        {
            alias_name = Some(ns.clone());
            defined.push(ns.clone());
            i += 2;
        }
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "from")
            && let Some(JsToken::StringLit(spec)) = tokens.get(i + 1)
        {
            if let Some(clean) = clean_frontend_import_specifier(spec) {
                if let Some(ns) = alias_name
                    && !top_type_only
                {
                    reexport_links.push(format!("reexport:{ns}->{clean}:*"));
                }
                return (
                    Some(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: vec!["*".to_string()],
                    }),
                    top_type_only,
                    defined,
                    reexport_links,
                    i + 2,
                );
            }
            return (None, top_type_only, defined, reexport_links, i + 2);
        }
        return (None, top_type_only, defined, reexport_links, i);
    }

    // `export { A, B as C, type D } from './sub'` or local `export { A, B }`
    if matches!(tokens.get(i), Some(JsToken::LBrace)) {
        i += 1;
        let mut pairs: Vec<(String, String, bool)> = Vec::new();
        while i < n && !matches!(tokens[i], JsToken::RBrace) {
            let mut item_type = top_type_only;
            if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
                && matches!(tokens.get(i + 1), Some(JsToken::Ident(_)))
            {
                item_type = true;
                i += 1;
            }
            if let Some(JsToken::Ident(orig)) = tokens.get(i) {
                let orig_name = orig.clone();
                let mut exported_name = orig_name.clone();
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as")
                    && let Some(JsToken::Ident(alias)) = tokens.get(i + 1)
                {
                    exported_name = alias.clone();
                    i += 2;
                }
                defined.push(exported_name.clone());
                pairs.push((orig_name, exported_name, item_type));
            } else {
                i += 1;
            }
            if matches!(tokens.get(i), Some(JsToken::Comma)) {
                i += 1;
            }
        }
        if i < n && matches!(tokens[i], JsToken::RBrace) {
            i += 1;
        }
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "from")
            && let Some(JsToken::StringLit(spec)) = tokens.get(i + 1)
        {
            if let Some(clean) = clean_frontend_import_specifier(spec) {
                let value_pairs: Vec<_> = pairs.iter().filter(|(_, _, is_t)| !*is_t).collect();
                let is_type_only = top_type_only || (value_pairs.is_empty() && !pairs.is_empty());
                let syms: Vec<String> = if is_type_only {
                    pairs.iter().map(|(orig, _, _)| orig.clone()).collect()
                } else {
                    for (orig, exp, _) in &value_pairs {
                        reexport_links.push(format!("reexport:{exp}->{clean}:{orig}"));
                    }
                    value_pairs.iter().map(|(orig, _, _)| orig.clone()).collect()
                };
                return (
                    Some(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: syms,
                    }),
                    is_type_only,
                    defined,
                    reexport_links,
                    i + 2,
                );
            }
            return (None, top_type_only, defined, reexport_links, i + 2);
        }
        return (None, top_type_only, defined, reexport_links, i);
    }

    (None, false, defined, reexport_links, i)
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
        assert!(
            parsed
                .test_symbols
                .contains(&"test_create_user".to_string())
        );
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
        assert!(parsed.module_markers.contains(&"usefixtures".to_string()));
    }

    #[test]
    fn isolates_monorepo_service_compartments_and_supports_hash_only_updates() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes("services/svc_a/src/app/config.py", b"VALUE = 1\n");
        graph.insert_file_bytes("services/svc_b/src/app/config.py", b"VALUE = 2\n");
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
        let structure_changed =
            graph.insert_file_bytes("services/svc_a/src/app/config.py", b"VALUE = 100\n");
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
        graph.insert_file_bytes("src/ai-service/routers/__init__.py", b"");
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

        let affected =
            graph.affected_tests(&["src/ai-service/routers/description_generator.py".to_string()]);
        let test_files: Vec<String> = affected.into_iter().map(|a| a.test_file).collect();
        assert_eq!(
            test_files,
            vec![
                "src/ai-service/tests/test_description.py".to_string(),
                "src/ai-service/tests/test_main.py".to_string(),
            ]
        );
        let modules = graph
            .affected_modules(&["src/ai-service/routers/description_generator.py".to_string()]);
        assert!(modules.contains(&"src/ai-service/main.py".to_string()));
    }

    #[test]
    fn honors_custom_pytest_python_files_patterns() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("pytest.ini"),
            "[pytest]\npython_files = check_*.py\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("check_feature.py"),
            "def test_custom():\n    assert True\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("test_default.py"),
            "def test_default():\n    assert True\n",
        )
        .unwrap();
        let workspace = Workspace::new(temp.path(), Vec::new(), Vec::new()).unwrap();
        let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
        assert!(graph.dep_hash_for_test.contains_key("check_feature.py"));
        assert!(!graph.dep_hash_for_test.contains_key("test_default.py"));
    }

    #[test]
    fn honors_multiline_python_files_and_testpaths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\n\
python_files = [\n\
  \"check_*.py\",\n\
  \"*_spec.py\",\n\
]\n\
testpaths = [\n\
  \"tests/unit\",\n\
]\n",
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("tests/unit")).unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("tests/unit/check_feature.py"),
            "def test_feature():\n    assert True\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("tests/unit/other_spec.py"),
            "def test_other():\n    assert True\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("src/check_helper.py"),
            "def test_not_collected():\n    assert True\n",
        )
        .unwrap();
        let workspace = Workspace::new(temp.path(), Vec::new(), Vec::new()).unwrap();
        let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
        assert!(
            graph
                .dep_hash_for_test
                .contains_key("tests/unit/check_feature.py")
        );
        assert!(
            graph
                .dep_hash_for_test
                .contains_key("tests/unit/other_spec.py")
        );
        assert!(!graph.dep_hash_for_test.contains_key("src/check_helper.py"));
    }

    #[test]
    fn parses_rust_mods_use_trees_tests_markers_includes_and_env_vars() {
        let src = r###"
#![allow(dead_code)]
#![cfg_attr(test, parallel_safe)]

pub mod engine;
#[path = "custom_storage.rs"]
mod storage;

use crate::engine::{Runner, Config as EngineConfig, sub::{alpha, beta}};
use super::super::shared::helper;
use std::env;

/*
  /* nested comment with fake #[test] fn test_ignored_comment() {} */
*/
const RAW: &str = r#"
#[test]
fn test_inside_raw_string() {}
"#;

pub struct Service<'a> {
    name: &'a str,
}

#[tokio::test]
#[serial_test::serial]
async fn test_async_engine() {
    let sql = include_str!("../testdata/query.sql");
    let snap = "snapshots/engine.snap";
    let key = env!("API_SECRET_KEY");
    let dyn_key = env::var("RUNTIME_TOKEN").unwrap();
    assert!(!sql.is_empty() && !snap.is_empty() && !key.is_empty() && !dyn_key.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires external cluster"]
    fn test_unit_case() {
        let ch = '\'';
        assert_eq!(ch, '\'');
    }
}
"###;
        let parsed = parse_rust_source(src);
        assert_eq!(
            parsed.test_symbols,
            vec!["test_async_engine".to_string(), "test_unit_case".to_string()]
        );
        assert!(parsed.rust_mods.contains(&RustModDecl {
            name: "engine".to_string(),
            custom_path: None,
            is_inline: false,
            is_cfg_test: false,
        }));
        assert!(parsed.rust_mods.contains(&RustModDecl {
            name: "storage".to_string(),
            custom_path: Some("custom_storage.rs".to_string()),
            is_inline: false,
            is_cfg_test: false,
        }));
        assert!(parsed.rust_mods.contains(&RustModDecl {
            name: "tests".to_string(),
            custom_path: None,
            is_inline: true,
            is_cfg_test: true,
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 0,
            module: "crate.engine".to_string(),
            symbols: vec!["Config".to_string(), "Runner".to_string()],
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 0,
            module: "crate.engine.sub".to_string(),
            symbols: vec!["alpha".to_string(), "beta".to_string()],
        }));
        assert!(parsed.imports.contains(&ImportSpec {
            level: 2,
            module: "shared".to_string(),
            symbols: vec!["helper".to_string()],
        }));
        assert!(parsed.data_refs.contains(&"../testdata/query.sql".to_string()));
        assert!(parsed.data_refs.contains(&"snapshots/engine.snap".to_string()));
        assert!(parsed.markers.contains(&"serial".to_string()));
        assert!(parsed.markers.contains(&"ignore".to_string()));
        assert!(parsed.module_markers.contains(&"parallel_safe".to_string()));
        assert_eq!(
            parsed.env_vars_referenced,
            vec!["API_SECRET_KEY".to_string(), "RUNTIME_TOKEN".to_string()]
        );
        assert!(parsed.defined_symbols.contains(&"Service".to_string()));
    }

    #[test]
    fn builds_rust_dependency_graph_with_unit_integration_common_helper_and_include_str() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "Cargo.toml",
            b"[package]\nname = \"sample-rust\"\nversion = \"0.1.0\"\n",
        );
        graph.insert_file_bytes(
            "src/lib.rs",
            b"pub mod math;\npub mod service;\npub mod isolated;\n",
        );
        graph.insert_file_bytes(
            "src/math.rs",
            b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_add_unit() { assert_eq!(add(1, 2), 3); }\n}\n",
        );
        graph.insert_file_bytes(
            "src/service.rs",
            b"use crate::math::add;\npub fn double(x: i32) -> i32 { add(x, x) }\n",
        );
        graph.insert_file_bytes("testdata/banner.txt", b"hello\n");
        graph.insert_file_bytes(
            "src/isolated.rs",
            b"pub fn banner() -> &'static str { include_str!(\"../testdata/banner.txt\") }\n",
        );
        graph.insert_file_bytes("tests/common/mod.rs", b"pub fn setup() -> i32 { 21 }\n");
        graph.insert_file_bytes(
            "tests/service_test.rs",
            b"mod common;\nuse sample_rust::service::double;\n#[test]\nfn test_service_double() { assert_eq!(double(common::setup()), 42); }\n",
        );
        graph.insert_file_bytes(
            "tests/isolated_test.rs",
            b"use sample_rust::isolated::banner;\n#[test]\nfn test_isolated_banner() { assert_eq!(banner().trim(), \"hello\"); }\n",
        );
        graph.recompute_edges();

        // 1. Editing `src/math.rs` affects `src/math.rs` (inline unit test) and `tests/service_test.rs`, NOT `tests/isolated_test.rs`!
        let affected_math: Vec<String> = graph
            .affected_tests(&["src/math.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_math,
            vec![
                "src/math.rs".to_string(),
                "tests/service_test.rs".to_string()
            ]
        );

        // 2. Editing `testdata/banner.txt` referenced via `include_str!("../testdata/banner.txt")` affects only `tests/isolated_test.rs`!
        let affected_banner: Vec<String> = graph
            .affected_tests(&["testdata/banner.txt".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_banner, vec!["tests/isolated_test.rs".to_string()]);

        // 3. Editing `tests/common/mod.rs` affects all integration tests under `tests/`!
        let affected_common: Vec<String> = graph
            .affected_tests(&["tests/common/mod.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_common,
            vec![
                "tests/isolated_test.rs".to_string(),
                "tests/service_test.rs".to_string()
            ]
        );
    }

    #[test]
    fn excludes_type_checking_only_imports_and_tracks_referenced_env_vars_in_dep_hash() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes("pkg/__init__.py", b"");
        graph.insert_file_bytes("pkg/heavy_types.py", b"class HeavyModel:\n    pass\n");
        graph.insert_file_bytes("pkg/runtime_dep.py", b"def calc() -> int:\n    return 7\n");
        graph.insert_file_bytes(
            "pkg/service.py",
            b"from typing import TYPE_CHECKING\nimport os\nif TYPE_CHECKING:\n    from pkg.heavy_types import HeavyModel\nfrom pkg.runtime_dep import calc\n\ndef run() -> int:\n    flag = os.getenv(\"TCTX_TEST_ROUND4_FLAG\")\n    return calc() if not flag else 0\n",
        );
        graph.insert_file_bytes(
            "tests/test_service.py",
            b"from pkg.service import run\ndef test_run():\n    assert run() == 7\n",
        );
        graph.recompute_edges();

        // 1. `pkg/heavy_types.py` is imported only inside `if TYPE_CHECKING:`, so editing it must NOT affect `tests/test_service.py`!
        let affected_tc = graph.affected_tests(&["pkg/heavy_types.py".to_string()]);
        assert!(affected_tc.is_empty());
        assert_eq!(
            graph.files["pkg/service.py"].parsed.type_checking_imports.len(),
            1
        );

        // 2. `pkg/runtime_dep.py` is a runtime import, so editing it affects `tests/test_service.py`
        let affected_rt = graph.affected_tests(&["pkg/runtime_dep.py".to_string()]);
        assert_eq!(affected_rt.len(), 1);
        assert_eq!(affected_rt[0].test_file, "tests/test_service.py");

        // 3. Changing referenced env var `TCTX_TEST_ROUND4_FLAG` updates `dep_hash` for `tests/test_service.py`
        let hash_before = graph.dep_hash_for_test["tests/test_service.py"].clone();
        unsafe {
            std::env::set_var("TCTX_TEST_ROUND4_FLAG", "enabled_v2");
        }
        graph.refresh_referenced_env_hashes();
        let hash_after = graph.dep_hash_for_test["tests/test_service.py"].clone();
        unsafe {
            std::env::remove_var("TCTX_TEST_ROUND4_FLAG");
        }
        assert_ne!(hash_before, hash_after);
    }

    #[test]
    fn links_pyo3_maturin_rust_extension_changes_to_python_pytest_files() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "pyproject.toml",
            b"[project]\nname = \"hybrid-pkg\"\n[tool.maturin]\nmodule-name = \"hybrid_pkg._native\"\n",
        );
        graph.insert_file_bytes(
            "Cargo.toml",
            b"[package]\nname = \"hybrid-pkg\"\n[lib]\nname = \"_native\"\ncrate-type = [\"cdylib\", \"rlib\"]\n",
        );
        graph.insert_file_bytes(
            "src/fast_kernel.rs",
            b"pub fn dot_product(a: &[f64], b: &[f64]) -> f64 { a.iter().zip(b).map(|(x, y)| x * y).sum() }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_dot_rust() { assert_eq!(dot_product(&[1.0, 2.0], &[3.0, 4.0]), 11.0); }\n}\n",
        );
        graph.insert_file_bytes(
            "src/lib.rs",
            b"pub mod fast_kernel;\nuse pyo3::prelude::*;\n#[pymodule]\nfn _native(m: &Bound<'_, PyModule>) -> PyResult<()> { Ok(()) }\n",
        );
        graph.insert_file_bytes(
            "python/hybrid_pkg/__init__.py",
            b"from ._native import dot_product\n",
        );
        graph.insert_file_bytes(
            "python/hybrid_pkg/pure_py.py",
            b"def greet() -> str:\n    return 'pure'\n",
        );
        graph.insert_file_bytes(
            "tests/test_hybrid.py",
            b"from hybrid_pkg import dot_product\ndef test_dot_py():\n    assert dot_product([1.0], [2.0]) == 2.0\n",
        );
        graph.insert_file_bytes(
            "tests/test_pure.py",
            b"from hybrid_pkg.pure_py import greet\ndef test_greet():\n    assert greet() == 'pure'\n",
        );
        graph.recompute_edges();

        // Editing `src/fast_kernel.rs` must affect BOTH the Rust unit test (`src/fast_kernel.rs`)
        // AND the Python pytest file (`tests/test_hybrid.py`), while leaving `tests/test_pure.py` untouched!
        let affected: Vec<String> = graph
            .affected_tests(&["src/fast_kernel.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected,
            vec![
                "src/fast_kernel.rs".to_string(),
                "tests/test_hybrid.py".to_string(),
            ]
        );

        // Symbol-based target resolution & fuzzy suggestion check
        assert_eq!(
            graph.resolve_target_test_files(&["test_dot_rust".to_string()]),
            vec!["src/fast_kernel.rs".to_string()]
        );
        let suggestions = graph.suggest_closest_targets(&["tests/test_hybrd.py".to_string()]);
        assert!(suggestions.contains(&"tests/test_hybrid.py".to_string()));
    }

    #[test]
    fn parses_and_resolves_react_vite_tsx_barrel_alias_css_and_type_only_imports() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "tsconfig.json",
            b"{\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@/*\": [\"./src/*\"]\n    }\n  }\n}\n",
        );
        graph.insert_file_bytes("src/test/setup.ts", b"import '@testing-library/jest-dom';\n");
        graph.insert_file_bytes(
            "src/types/models.ts",
            b"export interface ButtonProps { label: string; }\n",
        );
        graph.insert_file_bytes("src/assets/logo.svg", b"<svg></svg>\n");
        graph.insert_file_bytes(
            "src/components/Button.module.css",
            b".btn { color: blue; }\n",
        );
        graph.insert_file_bytes(
            "src/components/Button.tsx",
            b"import type { ButtonProps } from '@/types/models';\nimport styles from './Button.module.css';\nimport logoSvg from '@/assets/logo.svg?raw';\nexport function Button(props: ButtonProps) {\n  const api = import.meta.env.VITE_FRONTEND_API_URL;\n  return `${styles.btn}:${logoSvg}:${api}:${props.label}`;\n}\n",
        );
        graph.insert_file_bytes(
            "src/components/Modal.tsx",
            b"export function Modal() { return 'modal'; }\n",
        );
        graph.insert_file_bytes(
            "src/components/index.ts",
            b"export { Button } from './Button';\nexport { Modal } from './Modal';\nexport type { ButtonProps } from '@/types/models';\n",
        );
        graph.insert_file_bytes(
            "src/__tests__/__snapshots__/Button.test.tsx.snap",
            b"exports[`Button renders 1`] = `\"btn\"`;\n",
        );
        graph.insert_file_bytes(
            "src/__tests__/Button.test.tsx",
            b"import { Button } from '@/components';\ndescribe('Button suite', () => {\n  it.concurrent('renders primary button', () => {\n    expect(Button({ label: 'Click' })).toBeTruthy();\n  });\n});\n",
        );
        graph.insert_file_bytes(
            "src/__tests__/Modal.test.tsx",
            b"import { Modal } from '@/components';\ntest('renders modal dialog', () => {\n  expect(Modal()).toBe('modal');\n});\n",
        );
        graph.recompute_edges();

        // 1. Barrel tree-shaking: editing `Button.tsx` affects `Button.test.tsx`, NOT `Modal.test.tsx`!
        let affected_btn: Vec<String> = graph
            .affected_tests(&["src/components/Button.tsx".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_btn, vec!["src/__tests__/Button.test.tsx".to_string()]);

        // 2. CSS Module & Vite `?raw` SVG asset changes propagate to `Button.test.tsx`!
        let affected_css: Vec<String> = graph
            .affected_tests(&["src/components/Button.module.css".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_css, vec!["src/__tests__/Button.test.tsx".to_string()]);
        let affected_svg: Vec<String> = graph
            .affected_tests(&["src/assets/logo.svg".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_svg, vec!["src/__tests__/Button.test.tsx".to_string()]);

        // 3. `import type` / `export type` changes (`src/types/models.ts`) do NOT invalidate runtime tests!
        let affected_types = graph.affected_tests(&["src/types/models.ts".to_string()]);
        assert!(affected_types.is_empty());

        // 4. Snapshot file change affects `Button.test.tsx`!
        let affected_snap: Vec<String> = graph
            .affected_tests(&["src/__tests__/__snapshots__/Button.test.tsx.snap".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_snap,
            vec!["src/__tests__/Button.test.tsx".to_string()]
        );

        // 5. Global frontend setup file (`src/test/setup.ts`) affects all frontend tests!
        let affected_setup: Vec<String> = graph
            .affected_tests(&["src/test/setup.ts".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_setup,
            vec![
                "src/__tests__/Button.test.tsx".to_string(),
                "src/__tests__/Modal.test.tsx".to_string(),
            ]
        );

        // 6. Changing `VITE_FRONTEND_API_URL` referenced via `import.meta.env.VITE_FRONTEND_API_URL` updates Merkle `dep_hash`!
        let hash_before = graph.dep_hash_for_test["src/__tests__/Button.test.tsx"].clone();
        unsafe {
            std::env::set_var("VITE_FRONTEND_API_URL", "https://api.example.com");
        }
        graph.refresh_referenced_env_hashes();
        let hash_after = graph.dep_hash_for_test["src/__tests__/Button.test.tsx"].clone();
        unsafe {
            std::env::remove_var("VITE_FRONTEND_API_URL");
        }
        assert_ne!(hash_before, hash_after);

        let info = graph.file_graph_info("src/components/Button.tsx").unwrap();
        assert_eq!(info.language, "react_tsx");
    }

    #[test]
    fn resolves_typescript_esm_js_extension_imports_in_own_vscode_extension() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = Workspace::new(repo_root, Vec::new(), Vec::new()).unwrap();
        let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
        let affected: Vec<String> = graph
            .affected_tests(&["vscode-extension/src/resultFormatter.ts".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert!(
            affected.contains(&"vscode-extension/test/nativeTools.test.ts".to_string()),
            "expected nativeTools.test.ts in affected: {affected:?}"
        );
    }

    #[test]
    fn resolves_scss_partials_subpath_imports_package_exports_glob_worker_and_storybook() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "package.json",
            br##"{
  "name": "@acme/design-system",
  "imports": {
    "#internal/*": "./src/internal/*"
  },
  "exports": {
    ".": "./src/index.ts",
    "./card": {
      "import": "./src/components/Card.tsx"
    }
  }
}"##,
        );
        graph.insert_file_bytes(
            "tsconfig.json",
            b"{\n  \"extends\": \"./tsconfig.app.json\"\n}\n",
        );
        graph.insert_file_bytes(
            "tsconfig.app.json",
            b"{\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@ui/*\": [\"./src/components/*\"]\n    }\n  }\n}\n",
        );
        graph.insert_file_bytes("src/styles/_tokens.scss", b"$primary: #0055ff;\n");
        graph.insert_file_bytes(
            "src/components/Card.module.scss",
            b"@use '../styles/tokens';\n.card { color: tokens.$primary; }\n",
        );
        graph.insert_file_bytes(
            "src/internal/format.ts",
            b"export function formatTitle(s: string) { return s.toUpperCase(); }\n",
        );
        graph.insert_file_bytes(
            "src/workers/render.worker.ts",
            b"export const WORKER_READY = true;\n",
        );
        graph.insert_file_bytes("src/plugins/chart.ts", b"export const chartPlugin = 1;\n");
        graph.insert_file_bytes(
            "src/components/Card.tsx",
            b"import styles from './Card.module.scss';\nimport { formatTitle } from '#internal/format';\nconst plugins = import.meta.glob('../plugins/*.ts');\nconst worker = new Worker(new URL('../workers/render.worker.ts', import.meta.url));\nexport function Card(title: string) {\n  return `${styles.card}:${formatTitle(title)}:${Object.keys(plugins).length}:${worker}`;\n}\n",
        );
        graph.insert_file_bytes(
            "src/components/Card.stories.tsx",
            b"import { Card } from '@ui/Card';\nexport default { component: Card };\nexport const Primary = {\n  play: async () => {\n    Card('Hello');\n  },\n};\n",
        );
        graph.insert_file_bytes(
            "e2e/card.e2e.ts",
            b"import { test, expect } from '@playwright/test';\nimport { Card } from '@acme/design-system/card';\ntest('renders card in browser', async () => {\n  expect(Card('E2E')).toBeTruthy();\n});\n",
        );
        graph.recompute_edges();

        // 1. Multi-hop SCSS partial chain: `_tokens.scss` -> `Card.module.scss` -> `Card.tsx` -> `Card.stories.tsx` & `e2e/card.e2e.ts`!
        let affected_scss: Vec<String> = graph
            .affected_tests(&["src/styles/_tokens.scss".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_scss,
            vec![
                "e2e/card.e2e.ts".to_string(),
                "src/components/Card.stories.tsx".to_string(),
            ]
        );

        // 2. `#internal/format` subpath import propagates to both tests!
        let affected_subpath: Vec<String> = graph
            .affected_tests(&["src/internal/format.ts".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_subpath.len(), 2);

        // 3. `import.meta.glob('../plugins/*.ts')` and `new URL('../workers/render.worker.ts', import.meta.url)` propagate!
        let affected_glob = graph.affected_tests(&["src/plugins/chart.ts".to_string()]);
        assert_eq!(affected_glob.len(), 2);
        let affected_worker =
            graph.affected_tests(&["src/workers/render.worker.ts".to_string()]);
        assert_eq!(affected_worker.len(), 2);

        // 4. Playwright E2E test has `playwright` and `e2e` markers automatically attached
        let e2e_node = &graph.files["e2e/card.e2e.ts"];
        assert!(e2e_node.parsed.markers.contains(&"playwright".to_string()));
        assert!(e2e_node.parsed.module_markers.contains(&"e2e".to_string()));
    }

    #[test]
    fn semantic_hash_ignores_comments_and_granulizes_conftest_and_rust_pub_use_barrels() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "src/pkg/calc.py",
            b"def compute(x: int) -> int:\n    return x * 2\n",
        );
        graph.insert_file_bytes(
            "tests/conftest.py",
            b"import pytest\n\n@pytest.fixture\ndef db_conn():\n    return 'db'\n",
        );
        graph.insert_file_bytes(
            "tests/test_calc.py",
            b"from pkg.calc import compute\ndef test_compute():\n    assert compute(3) == 6\n",
        );
        graph.insert_file_bytes(
            "tests/test_db.py",
            b"def test_with_db(db_conn):\n    assert db_conn == 'db'\n",
        );
        graph.recompute_edges();

        let calc_hash_before = graph.dep_hash_for_test["tests/test_calc.py"].clone();
        // 1. Adding ordinary comments & blank lines to `src/pkg/calc.py` must NOT change `dep_hash`!
        graph.insert_file_bytes(
            "src/pkg/calc.py",
            b"# Header comment\n\ndef compute(x: int) -> int:  # inline comment\n    # body note\n    return x * 2\n\n",
        );
        graph.recompute_hashes_for_changed(&["src/pkg/calc.py".to_string()]);
        assert_eq!(
            graph.dep_hash_for_test["tests/test_calc.py"],
            calc_hash_before,
            "comment-only and blank-line edits must preserve semantic dep_hash"
        );

        // 2. Editing `tests/conftest.py` (which only defines non-autouse `db_conn`) affects `test_db.py` but NOT `test_calc.py`!
        let affected_cf: Vec<String> = graph
            .affected_tests(&["tests/conftest.py".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_cf, vec!["tests/test_db.py".to_string()]);

        // 3. Rust `pub use` barrel module (`src/models/mod.rs`) tree-shakes sibling modules!
        let mut rs_graph = DependencyGraph::new();
        rs_graph.insert_file_bytes(
            "src/models/user.rs",
            b"pub struct User;\n",
        );
        rs_graph.insert_file_bytes(
            "src/models/order.rs",
            b"pub struct Order;\n",
        );
        rs_graph.insert_file_bytes(
            "src/models/mod.rs",
            b"pub mod user;\npub mod order;\npub use self::user::User;\npub use self::order::Order;\n",
        );
        rs_graph.insert_file_bytes(
            "tests/user_test.rs",
            b"use crate::models::User;\n#[test]\nfn test_user() { let _ = User; }\n",
        );
        rs_graph.recompute_edges();
        let affected_user: Vec<String> = rs_graph
            .affected_tests(&["src/models/user.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_user, vec!["tests/user_test.rs".to_string()]);
        let affected_order = rs_graph.affected_tests(&["src/models/order.rs".to_string()]);
        assert!(
            affected_order.is_empty(),
            "unrelated sibling module in Rust barrel mod.rs must not invalidate user_test.rs"
        );
    }

    #[test]
    fn resolves_nextjs_app_router_wasm_api_contracts_and_polyglot_go_cpp_proto() {
        let mut graph = DependencyGraph::new();
        // 1. Next.js App Router (`app/layout.tsx` -> `app/dashboard/page.tsx`)
        graph.insert_file_bytes(
            "app/layout.tsx",
            b"export default function RootLayout({ children }: any) { return children; }\n",
        );
        graph.insert_file_bytes(
            "app/dashboard/page.tsx",
            b"export default function DashboardPage() { return 'dash'; }\n",
        );
        graph.insert_file_bytes(
            "app/dashboard/page.test.tsx",
            b"import DashboardPage from './page';\ntest('renders dashboard', () => { DashboardPage(); });\n",
        );
        // 2. Full-Stack API route (`/api/v1/orders`) + Wasm (`#[wasm_bindgen]`) + React Context (`ThemeContext`)
        graph.insert_file_bytes(
            "src/backend/routes.py",
            b"@app.get('/api/v1/orders')\ndef list_orders(): return []\n",
        );
        graph.insert_file_bytes(
            "src/wasm/kernel.rs",
            b"#[wasm_bindgen]\npub fn fast_hash(input: &str) -> u32 { 42 }\n",
        );
        graph.insert_file_bytes(
            "src/context/Theme.tsx",
            b"import { createContext } from 'react';\nexport const ThemeContext = createContext('dark');\n",
        );
        graph.insert_file_bytes(
            "src/frontend/Orders.test.tsx",
            b"import { fast_hash } from '../wasm/pkg';\nimport { ThemeContext } from '../context/Theme';\ntest('fetches orders', async () => {\n  const theme = useContext(ThemeContext);\n  const res = await fetch('/api/v1/orders');\n  return fast_hash(theme);\n});\n",
        );
        // 3. Go (`go.mod`, `pkg/calc/math.go`, `pkg/calc/math_test.go`) & Protobuf (`proto/order.proto`)
        graph.insert_file_bytes("go.mod", b"module github.com/acme/demo\n\ngo 1.22\n");
        graph.insert_file_bytes(
            "proto/order.proto",
            b"syntax = \"proto3\";\nmessage OrderRequest { string id = 1; }\n",
        );
        graph.insert_file_bytes(
            "pkg/calc/math.go",
            b"package calc\nfunc Add(a, b int) int { return a + b }\n",
        );
        graph.insert_file_bytes(
            "pkg/calc/math_test.go",
            b"package calc\nimport \"testing\"\nfunc TestAdd(t *testing.T) { if Add(1, 2) != 3 { t.Fatal() } }\n",
        );
        graph.insert_file_bytes(
            "src/backend/test_proto_client.py",
            b"import order_pb2\ndef test_proto(): assert order_pb2 is not None\n",
        );
        graph.insert_file_bytes("src/backend/untested_orphan.py", b"def orphan_helper(): return 0\n");
        graph.recompute_edges();

        // Next.js `app/layout.tsx` change propagates to `app/dashboard/page.test.tsx`!
        let affected_layout: Vec<String> = graph
            .affected_tests(&["app/layout.tsx".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_layout,
            vec!["app/dashboard/page.test.tsx".to_string()]
        );

        // Backend route `/api/v1/orders` and Wasm `fast_hash` propagate to `src/frontend/Orders.test.tsx`!
        let affected_api: Vec<String> = graph
            .affected_tests(&["src/backend/routes.py".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_api,
            vec!["src/frontend/Orders.test.tsx".to_string()]
        );
        let affected_wasm: Vec<String> = graph
            .affected_tests(&["src/wasm/kernel.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_wasm,
            vec!["src/frontend/Orders.test.tsx".to_string()]
        );

        // Go `pkg/calc/math.go` propagates to `pkg/calc/math_test.go`!
        let affected_go: Vec<String> = graph
            .affected_tests(&["pkg/calc/math.go".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_go, vec!["pkg/calc/math_test.go".to_string()]);

        // Protobuf `proto/order.proto` propagates to `src/backend/test_proto_client.py`!
        let affected_proto: Vec<String> = graph
            .affected_tests(&["proto/order.proto".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_proto,
            vec!["src/backend/test_proto_client.py".to_string()]
        );

        // Untested modified file detection
        let untested = graph.untested_modified_files(&[
            "src/backend/untested_orphan.py".to_string(),
            "pkg/calc/math.go".to_string(),
        ]);
        assert_eq!(untested, vec!["src/backend/untested_orphan.py".to_string()]);
    }

    #[test]
    fn normalizes_and_parses_escaped_multibyte_utf8_without_panicking() {
        let py_src = concat!(
            "import re\n",
            "CURRENCY_RE = re.compile(r\"[\\$€£\\¥\\₩\\🦀]+\")\n",
            "TRIPLE = \"\"\"prefix \\¥ \\가 \\🦀 suffix\"\"\"\n",
            "def test_currency():\n",
            "    assert CURRENCY_RE.match(\"¥100\")\n",
        );
        let py_hash_a = semantic_sha256_for_source("tests/test_currency.py", py_src.as_bytes());
        let py_hash_b = semantic_sha256_for_source(
            "tests/test_currency.py",
            format!("{py_src}# trailing comment with ¥ and 한글\n").as_bytes(),
        );
        assert_eq!(py_hash_a, py_hash_b);
        let parsed_py = parse_python_source(py_src);
        assert!(parsed_py.test_symbols.contains(&"test_currency".to_string()));

        let ts_src = concat!(
            "import { parsePrice } from './price';\n",
            "/* multi-byte block comment ¥ 가 🦀 */\n",
            "const pat = \"\\¥\\₩\\🦀\";\n",
            "const tpl = `\\¥\\₩\\🦀`;\n",
            "test('parses yen', () => { expect(parsePrice(pat, tpl)).toBe(100); });\n",
        );
        let ts_hash_a = semantic_sha256_for_source("src/price.test.ts", ts_src.as_bytes());
        let ts_hash_b = semantic_sha256_for_source(
            "src/price.test.ts",
            format!("{ts_src}// comment ¥\n").as_bytes(),
        );
        assert_eq!(ts_hash_a, ts_hash_b);
        let parsed_ts = parse_frontend_source(ts_src);
        assert!(parsed_ts.test_symbols.contains(&"parses yen".to_string()));

        let rs_src = concat!(
            "use crate::currency::parse_symbol;\n",
            "#[test]\n",
            "fn parses_yen_char() {\n",
            "    let ch = '¥';\n",
            "    let escaped = \"\\¥\";\n",
            "    assert_eq!(parse_symbol(ch, escaped), 1);\n",
            "}\n",
        );
        let rs_hash = semantic_sha256_for_source("src/lib.rs", rs_src.as_bytes());
        assert!(!rs_hash.is_empty());
        let parsed_rs = parse_rust_source(rs_src);
        assert!(parsed_rs.test_symbols.contains(&"parses_yen_char".to_string()));
    }

    #[test]
    fn rust_lib_root_shared_types_do_not_couple_unrelated_sibling_modules() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "Cargo.toml",
            b"[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
        );
        graph.insert_file_bytes(
            "src/lib.rs",
            b"pub mod alpha;\npub mod beta;\npub type Result<T> = std::result::Result<T, String>;\n",
        );
        graph.insert_file_bytes(
            "src/alpha.rs",
            b"use crate::Result;\npub fn run_a() -> Result<i32> { Ok(1) }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_a() { assert_eq!(run_a().unwrap(), 1); }\n}\n",
        );
        graph.insert_file_bytes(
            "src/beta.rs",
            b"use crate::Result;\npub fn run_b() -> Result<i32> { Ok(2) }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_b() { assert_eq!(run_b().unwrap(), 2); }\n}\n",
        );
        graph.recompute_edges();

        let affected_alpha: Vec<String> = graph
            .affected_tests(&["src/alpha.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(affected_alpha, vec!["src/alpha.rs".to_string()]);

        let affected_lib: Vec<String> = graph
            .affected_tests(&["src/lib.rs".to_string()])
            .into_iter()
            .map(|a| a.test_file)
            .collect();
        assert_eq!(
            affected_lib,
            vec!["src/alpha.rs".to_string(), "src/beta.rs".to_string()]
        );
    }
}
