use crate::policy::MemoryPolicy;
use crate::workspace::{PYTEST_CONFIG_FILES, Workspace};
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub path: String,
    pub sha256: String,
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
    pub transitive_deps_for_test: BTreeMap<String, BTreeSet<String>>,
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
            transitive_deps_for_test: BTreeMap::new(),
            dep_hash_for_test: BTreeMap::new(),
            config_hashes: BTreeMap::new(),
        }
    }

    pub fn build(workspace: &Workspace, policy: &MemoryPolicy) -> Result<Self> {
        let inventory = workspace.files_limited(policy)?;
        let mut graph = Self::new();
        let mut total_bytes = 0usize;
        for rel_path in &inventory.files {
            let bytes = match workspace.read_known_with_limit(rel_path, policy.max_file_bytes) {
                Ok(b) => b,
                Err(_) => continue,
            };
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > policy.max_source_bytes {
                return Err(invalid("workspace exceeds max_source_bytes policy limit").into());
            }
            let rel_str = normalize_rel_str(rel_path);
            graph.insert_file_bytes(&rel_str, &bytes);
        }
        graph.recompute_edges();
        Ok(graph)
    }

    pub fn insert_file_bytes(&mut self, rel_str: &str, bytes: &[u8]) {
        let sha = sha256_hex(bytes);
        let file_name = Path::new(rel_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if PYTEST_CONFIG_FILES.contains(&file_name) {
            self.config_hashes.insert(rel_str.to_string(), sha.clone());
            self.files.insert(
                rel_str.to_string(),
                FileNode {
                    path: rel_str.to_string(),
                    sha256: sha,
                    is_test: false,
                    is_conftest: false,
                    is_config: true,
                    parsed: ParsedPythonFile::default(),
                },
            );
            return;
        }
        if !file_name.ends_with(".py") {
            return;
        }
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_python_source(&source);
        let is_conftest = file_name == "conftest.py";
        let is_test = !is_conftest && is_test_filename(file_name);
        self.files.insert(
            rel_str.to_string(),
            FileNode {
                path: rel_str.to_string(),
                sha256: sha,
                is_test,
                is_conftest,
                is_config: false,
                parsed,
            },
        );
    }

    pub fn remove_file(&mut self, rel_str: &str) -> bool {
        let removed_cfg = self.config_hashes.remove(rel_str).is_some();
        let removed_file = self.files.remove(rel_str).is_some();
        removed_cfg || removed_file
    }

    /// Recomputes module resolution, forward/reverse import edges, conftest chains, and Merkle dep_hashes.
    pub fn recompute_edges(&mut self) {
        self.direct_imports.clear();
        self.reverse_imports.clear();
        self.conftest_for_test.clear();
        self.transitive_deps_for_test.clear();
        self.dep_hash_for_test.clear();

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
            // If this file uses fixtures defined in a conftest.py or fixture module, link matching providers
            for fixture_name in &node.parsed.fixtures_used {
                if let Some(providers) = fixture_providers.get(fixture_name) {
                    for provider in providers {
                        if provider != path && is_provider_applicable(provider, path) {
                            deps.insert(provider.clone());
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

        // Collect all conftest.py paths
        let conftests: BTreeSet<String> = self
            .files
            .values()
            .filter(|n| n.is_conftest)
            .map(|n| n.path.clone())
            .collect();

        // Compute transitive closure & Merkle dep_hash for every test file
        let test_files: Vec<String> = self
            .files
            .values()
            .filter(|n| n.is_test)
            .map(|n| n.path.clone())
            .collect();

        for test_path in test_files {
            let ancestor_conftests = ancestor_conftests_for(&test_path, &conftests);
            let mut reachable = BTreeSet::new();
            let mut queue = VecDeque::new();

            reachable.insert(test_path.clone());
            queue.push_back(test_path.clone());

            for cf in &ancestor_conftests {
                if reachable.insert(cf.clone()) {
                    queue.push_back(cf.clone());
                }
            }

            while let Some(curr) = queue.pop_front() {
                if let Some(imports) = self.direct_imports.get(&curr) {
                    for next in imports {
                        if reachable.insert(next.clone()) {
                            queue.push_back(next.clone());
                        }
                    }
                }
            }

            let mut hasher = Sha256::new();
            hasher.update(b"tctx:merkle:v1\n");
            for (cfg_path, cfg_sha) in &self.config_hashes {
                hasher.update(b"cfg:");
                hasher.update(cfg_path.as_bytes());
                hasher.update(b":");
                hasher.update(cfg_sha.as_bytes());
                hasher.update(b"\n");
            }
            for dep_path in &reachable {
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
            self.transitive_deps_for_test
                .insert(test_path.clone(), reachable);
            self.dep_hash_for_test.insert(test_path, dep_hash);
        }
    }

    pub fn test_files(&self) -> Vec<String> {
        self.dep_hash_for_test.keys().cloned().collect()
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
        let transitive_dependencies = self
            .transitive_deps_for_test
            .get(&norm)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
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
                    resolved.extend(matches.iter().cloned());
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
                            resolved.extend(matches.iter().cloned());
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
    // 1. Full path from repo root
    names.insert(parts.join("."));

    // 2. If there is a `src` or `python` segment (e.g. `src/pkg/mod.py`, `services/foo/src/pkg/mod.py`, `packages/bar/src/pkg/mod.py`),
    // register suffix after `src` or `python`.
    for (idx, &seg) in parts.iter().enumerate() {
        if (seg == "src" || seg == "python") && idx + 1 < parts.len() {
            names.insert(parts[idx + 1..].join("."));
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
                let (lit, next_i) = scan_string_literal(source, i);
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
                    let (lit, next_i) = scan_string_literal(source, i);
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

fn scan_string_literal(source: &str, start_quote_idx: usize) -> (String, usize) {
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
    while i < len {
        if bytes[i] == b'\\' {
            i = (i + 2).min(len);
            continue;
        }
        if is_triple {
            if i + 2 < len && bytes[i] == quote && bytes[i + 1] == quote && bytes[i + 2] == quote {
                let lit = source[content_start..i].to_string();
                return (lit, i + 3);
            }
            i += 1;
        } else {
            if bytes[i] == quote || bytes[i] == b'\n' {
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

/// Parses a Python source file to extract imports, test symbols, and pytest fixtures.
pub fn parse_python_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_python(source);
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut fixtures_defined = BTreeSet::new();
    let mut fixtures_used = BTreeSet::new();

    let n = tokens.len();
    let mut i = 0usize;
    let mut at_statement_start = true;
    let mut pending_fixture_decorator = false;

    while i < n {
        match &tokens[i] {
            PyToken::Newline => {
                at_statement_start = true;
                i += 1;
            }
            PyToken::At if at_statement_start => {
                // Decorator: check if `@pytest.fixture` or `@fixture`
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
                if deco_parts.last().copied() == Some("fixture") {
                    pending_fixture_decorator = true;
                }
                // Skip rest of decorator line (including balanced parens)
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
                    i += 1;
                    if is_class {
                        if sym_name.starts_with("Test") {
                            test_symbols.insert(sym_name);
                        }
                        pending_fixture_decorator = false;
                    } else {
                        let is_test_fn = sym_name.starts_with("test_") || sym_name.ends_with("_test");
                        if is_test_fn {
                            test_symbols.insert(sym_name.clone());
                        }
                        if pending_fixture_decorator {
                            fixtures_defined.insert(sym_name);
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
            PyToken::Comma if depth == 1 => {
                expect_param_name = true;
                i += 1;
            }
            PyToken::Colon | PyToken::Assign if depth == 1 => {
                expect_param_name = false;
                i += 1;
            }
            PyToken::Ident(name) if depth == 1 && expect_param_name => {
                params.push(name.clone());
                expect_param_name = false;
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
}
