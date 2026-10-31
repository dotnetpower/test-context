use crate::policy::MemoryPolicy;
use crate::workspace::{DATA_FILE_EXTENSIONS, PYTEST_CONFIG_FILES, RUST_CONFIG_FILES, Workspace};
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
    pub transitive_dep_ids_for_test: BTreeMap<String, Vec<u32>>,
    pub dep_hash_for_test: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
    pub test_file_patterns: Vec<String>,
    pub testpaths: Vec<String>,
    pub environment_hash: String,
    pub runtime_deps_for_test: BTreeMap<String, BTreeSet<String>>,
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
            test_file_patterns: vec!["test_*.py".to_string(), "*_test.py".to_string()],
            testpaths: Vec::new(),
            environment_hash: String::new(),
            runtime_deps_for_test: BTreeMap::new(),
        }
    }

    pub fn apply_environment_hash(&mut self, environment_hash: String) {
        if self.environment_hash == environment_hash {
            return;
        }
        self.environment_hash = environment_hash;
        let all_files: Vec<String> = self.files.keys().cloned().collect();
        self.recompute_hashes_for_changed(&all_files);
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
        let inventory = workspace.files_limited(policy)?;
        let mut graph = Self::new();
        graph.test_file_patterns = workspace.pytest_file_patterns();
        graph.testpaths = workspace.pytest_testpaths();
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
        let sha = sha256_hex(bytes);
        let file_name = Path::new(rel_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if PYTEST_CONFIG_FILES.contains(&file_name) || RUST_CONFIG_FILES.contains(&file_name) {
            let is_new = !self.config_hashes.contains_key(rel_str);
            self.config_hashes.insert(rel_str.to_string(), sha.clone());
            let parsed = if file_name == "Cargo.toml" || file_name == "pyproject.toml" {
                parse_manifest_metadata(&String::from_utf8_lossy(bytes), file_name)
            } else {
                ParsedPythonFile::default()
            };
            let structure_changed = match self.files.get(rel_str) {
                Some(existing) => existing.parsed != parsed,
                None => true,
            };
            self.files.insert(
                rel_str.to_string(),
                FileNode {
                    path: rel_str.to_string(),
                    sha256: sha,
                    stamp,
                    is_test: false,
                    is_conftest: false,
                    is_config: true,
                    parsed,
                },
            );
            return is_new || structure_changed;
        }
        if file_name.ends_with(".rs") {
            let source = String::from_utf8_lossy(bytes);
            let parsed = parse_rust_source(&source);
            let is_conftest = is_rust_common_test_helper(rel_str);
            let is_test = !is_conftest
                && (!parsed.test_symbols.is_empty() || is_rust_integration_test_path(rel_str));
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
            return structure_changed;
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
        let is_test = !is_conftest
            && matches_test_filename(file_name, &self.test_file_patterns)
            && matches_test_path(rel_str, &self.testpaths);
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
        cfg_prefix.extend_from_slice(b"env:");
        cfg_prefix.extend_from_slice(self.environment_hash.as_bytes());
        cfg_prefix.extend_from_slice(b"\n");
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
                    hash_dep_node(&mut hasher, dep_path, dep_node);
                }
            }
            let dep_hash = hex_encode(&hasher.finalize());
            self.dep_hash_for_test.insert(test_path, dep_hash);
        }
    }

    pub fn refresh_referenced_env_hashes(&mut self) {
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
        let rust_index = RustModuleIndex::build(&self.files);

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
        cfg_prefix.extend_from_slice(b"env:");
        cfg_prefix.extend_from_slice(self.environment_hash.as_bytes());
        cfg_prefix.extend_from_slice(b"\n");
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
                    hash_dep_node(&mut hasher, dep_path, dep_node);
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
        let path_to_id: BTreeMap<String, u32> = self
            .file_index_to_path
            .iter()
            .enumerate()
            .map(|(idx, path)| (path.clone(), u32::try_from(idx).unwrap_or(u32::MAX)))
            .collect();
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); self.file_index_to_path.len()];
        for (source, deps) in &self.direct_imports {
            if let Some(&source_id) = path_to_id.get(source) {
                adj[source_id as usize] = deps
                    .iter()
                    .filter_map(|dep| path_to_id.get(dep).copied())
                    .collect();
            }
        }
        let conftests: BTreeSet<String> = self
            .files
            .values()
            .filter(|node| node.is_conftest)
            .map(|node| node.path.clone())
            .collect();
        let mut cfg_prefix = Vec::new();
        cfg_prefix.extend_from_slice(b"tctx:merkle:v1\n");
        cfg_prefix.extend_from_slice(b"env:");
        cfg_prefix.extend_from_slice(self.environment_hash.as_bytes());
        cfg_prefix.extend_from_slice(b"\n");
        for (path, sha) in &self.config_hashes {
            cfg_prefix.extend_from_slice(b"cfg:");
            cfg_prefix.extend_from_slice(path.as_bytes());
            cfg_prefix.extend_from_slice(b":");
            cfg_prefix.extend_from_slice(sha.as_bytes());
            cfg_prefix.extend_from_slice(b"\n");
        }
        for test_path in tests {
            let Some(&test_id) = path_to_id.get(test_path) else {
                continue;
            };
            let conftest_chain = ancestor_conftests_for(test_path, &conftests);
            let mut seen = vec![false; self.file_index_to_path.len()];
            let mut reachable = vec![test_id];
            let mut queue = VecDeque::from([test_id]);
            seen[test_id as usize] = true;
            for conftest in &conftest_chain {
                if let Some(&id) = path_to_id.get(conftest)
                    && !seen[id as usize]
                {
                    seen[id as usize] = true;
                    reachable.push(id);
                    queue.push_back(id);
                }
            }
            while let Some(id) = queue.pop_front() {
                for &next in &adj[id as usize] {
                    if !seen[next as usize] {
                        seen[next as usize] = true;
                        reachable.push(next);
                        queue.push_back(next);
                    }
                }
            }
            reachable.sort_unstable();
            let mut hasher = Sha256::new();
            hasher.update(&cfg_prefix);
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
        let mut relevant_paths = BTreeSet::new();
        for test_file in test_files {
            relevant_paths.insert(test_file.clone());
            relevant_paths.extend(self.transitive_deps_for(test_file));
        }
        for path in relevant_paths {
            let Some(node) = self.files.get(&path) else {
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
                || self.config_hashes.contains_key(&norm)
            {
                let is_rust_only_cfg = RUST_CONFIG_FILES.contains(&file_name);
                let is_py_only_cfg =
                    PYTEST_CONFIG_FILES.contains(&file_name) && file_name != "pyproject.toml";
                for (test_path, dep_hash) in &self.dep_hash_for_test {
                    let is_rs_test = test_path.ends_with(".rs");
                    if is_py_only_cfg && is_rs_test {
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

            // 2. conftest.py or Rust tests/common helper change affects tests under its scope directory
            if file_name == "conftest.py" || is_rust_common_test_helper(&norm) {
                let scope_dir = conftest_scope_dir(&norm);
                let is_rs_helper = norm.ends_with(".rs");
                for test_path in self.dep_hash_for_test.keys() {
                    if test_path.ends_with(".rs") != is_rs_helper {
                        continue;
                    }
                    if scope_dir.is_empty() || test_path.starts_with(&format!("{scope_dir}/")) {
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
                    let is_rs_helper = curr.ends_with(".rs");
                    for test_path in self.dep_hash_for_test.keys() {
                        if test_path.ends_with(".rs") != is_rs_helper {
                            continue;
                        }
                        if scope_dir.is_empty() || test_path.starts_with(&format!("{scope_dir}/"))
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
                } else if !norm.contains('/') && !norm.ends_with(".py") && !norm.ends_with(".rs") {
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
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() > 128 || b_bytes.len() > 128 {
        return 99;
    }
    let mut prev: Vec<usize> = (0..=b_bytes.len()).collect();
    let mut curr = vec![0usize; b_bytes.len() + 1];
    for (i, &ca) in a_bytes.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_bytes.iter().enumerate() {
            let cost = usize::from(!ca.eq_ignore_ascii_case(&cb));
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_bytes.len()]
}

fn compute_direct_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
    module_index: &ModuleIndex,
    rust_index: &RustModuleIndex,
    fixture_providers: &BTreeMap<String, Vec<String>>,
    runtime_deps_for_test: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    if path.ends_with(".rs") {
        deps.extend(rust_index.resolve_file_dependencies(path, node, files));
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
}

fn matches_test_filename(file_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
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

fn hash_dep_node(hasher: &mut Sha256, dep_path: &str, dep_node: &FileNode) {
    hasher.update(b"py:");
    hasher.update(dep_path.as_bytes());
    hasher.update(b":");
    hasher.update(dep_node.sha256.as_bytes());
    hasher.update(b"\n");
    for env_key in &dep_node.parsed.env_vars_referenced {
        hasher.update(b"env_ref:");
        hasher.update(env_key.as_bytes());
        hasher.update(b"=");
        if let Some(val) = std::env::var_os(env_key) {
            hasher.update(sha256_hex(val.to_string_lossy().as_bytes()).as_bytes());
        }
        hasher.update(b"\n");
    }
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

fn parse_manifest_metadata(text: &str, file_name: &str) -> ParsedPythonFile {
    let mut defined_symbols = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
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

        Self {
            crate_to_src_dirs,
            pyo3_module_to_lib_rs,
        }
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

        // 1. Resolve `mod <name>;` declarations
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

        // 2. Resolve `use ...;` statements
        for spec in &node.parsed.imports {
            for target in self.resolve_rust_use_spec(path, node, &src_dir, spec, files) {
                if target != path {
                    deps.insert(target);
                }
            }
        }

        // 3. If an integration test (`tests/*.rs`) did not resolve any specific file under `src/`,
        // fall back to linking it to the crate's `src/lib.rs` if present.
        if is_rust_integration_test_path(path) {
            let src_prefix = format!("{src_dir}/");
            let has_src_dep = deps.iter().any(|d| d.starts_with(&src_prefix));
            let lib_rs = format!("{src_dir}/lib.rs");
            if !has_src_dep && files.contains_key(&lib_rs) {
                deps.insert(lib_rs);
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

fn tokenize_python(source: &str) -> Vec<PyToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::new();
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
                tokens.push(PyToken::Newline);
                line_indent = 0;
                i += 1;
                continue;
            } else if b == b'#' {
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            } else {
                at_line_start = false;
                if let Some(base_indent) = tc_indent
                    && line_indent <= base_indent
                {
                    tokens.push(PyToken::EndTypeChecking);
                    tc_indent = None;
                }
                let line_end = source[i..]
                    .find('\n')
                    .map(|off| i + off)
                    .unwrap_or(len);
                if tc_indent.is_none() && is_type_checking_header(&source[i..line_end]) {
                    tc_indent = Some(line_indent);
                    tokens.push(PyToken::StartTypeChecking);
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
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                tokens.push(PyToken::Newline);
                at_line_start = true;
                line_indent = 0;
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

fn tokenize_rust(source: &str) -> Vec<RsToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::new();

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
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
                    tokens.push(RsToken::Other);
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
                    tokens.push(RsToken::Other);
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
                // Distinguish character literals ('a', '\n') from lifetimes ('a, 'static)
                if i + 1 < len && bytes[i + 1] == b'\\' {
                    i += 2;
                    while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                        i += 1;
                    }
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    }
                    tokens.push(RsToken::Other);
                } else if i + 2 < len && bytes[i + 2] == b'\'' {
                    i += 3;
                    tokens.push(RsToken::Other);
                } else {
                    i += 1;
                    while i < len && is_ident_continue(bytes[i]) {
                        i += 1;
                    }
                    tokens.push(RsToken::Other);
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
                tokens.push(RsToken::Other);
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
        if bytes[i] == b'\\' {
            i = (i + 2).min(len);
            continue;
        }
        if bytes[i] == b'"' {
            return (source[start..i].to_string(), i + 1);
        }
        i += 1;
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
        if let RsToken::StringLit(lit) = tok
            && let Some(clean_ref) = normalize_data_ref(lit)
        {
            data_refs.insert(clean_ref);
        }
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut pending_test_attr = false;
    let mut pending_pymodule_attr = false;
    let mut pending_cfg_test = false;
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
            RsToken::Ident(kw) if kw == "use" => {
                let (specs, next_i) = parse_rust_use_statement(&tokens, i + 1);
                for spec in specs {
                    imports.insert(spec);
                }
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
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
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
                    if pending_test_attr {
                        test_symbols.insert(fn_name.clone());
                    }
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
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
}
