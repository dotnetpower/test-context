use super::*;

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
    #[serde(default)]
    pub api_semantic_sha256: String,
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
    pub conftest_for_test: BTreeMap<String, Vec<String>>,
    pub file_index_to_path: Vec<String>,
    pub path_to_id: std::collections::HashMap<String, u32>,
    pub forward_adj: Vec<Vec<u32>>,
    pub reverse_adj: Vec<Vec<u32>>,
    pub conftest_paths: BTreeSet<String>,
    pub symbol_to_test_files: BTreeMap<String, Vec<String>>,
    pub transitive_dep_ids_for_test: BTreeMap<String, Vec<u32>>,
    pub dep_hash_for_test: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
    pub test_file_patterns: Vec<String>,
    pub test_function_patterns: Vec<String>,
    pub testpaths: Vec<String>,
    pub testpaths_fallback_active: bool,
    pub environment_hash: String,
    pub non_python_environment_hash: String,
    /// Frontend-only environment digest (Node runtime + Vite `.env*` files, including gitignored
    /// `.env.test.local`), hashed ONLY into frontend tests so adding/editing `.env` or frontend
    /// files NEVER invalidates cached Rust/Python tests!
    pub frontend_environment_hash: String,
    /// Directories explicitly referenced as build inputs (e.g. `cargo::rerun-if-changed=assets`).
    pub referenced_data_dirs: BTreeSet<String>,
    pub runtime_deps_for_test: BTreeMap<String, BTreeSet<String>>,
    /// Files referenced by raw-byte reads/embeds (e.g. Rust `include_str!`, frontend `?raw`, Python
    /// raw file reads) that must be hashed by raw `sha256` bytes rather than `semantic_sha256`.
    pub raw_embedded_files: BTreeSet<String>,
    /// Required directories that must be watched before caching results (e.g. referenced data
    /// dirs or hidden pytest `testpaths` directories).
    pub required_watch_dirs: BTreeSet<String>,
    /// Live workspace root path (not persisted in snapshot) used to detect unindexed/gitignored
    /// runtime files (e.g. `foo.js` existing on disk beside an indexed `foo.ts`!).
    pub workspace_root: Option<std::path::PathBuf>,
    /// Workspace-relative paths of explicitly referenced data/fixture files (e.g.
    /// `existsSync("fixtures/ready.bin")`) that do not yet exist on disk, so later creation
    /// emits a watcher event, admits the file, and invalidates tests.
    pub missing_data_ref_candidates: BTreeSet<String>,
    pub tsconfig_extends_rev: BTreeMap<String, BTreeSet<String>>,
    /// Per-importer pure barrel references `(barrel_path, imported_sym)` for O(1) exact hashing.
    pub barrel_imports_by_importer: BTreeMap<u32, Vec<(String, String)>>,
    /// Reverse index `barrel_path -> Vec<importer_id>` so editing a pure barrel marks its consumers affected.
    pub barrel_to_importers: BTreeMap<String, Vec<u32>>,
    /// Side-effectful barrels from the last full/incremental edge build so incremental rebuilds
    /// fall back to full edge recomputation whenever propagated barrel side-effect status changes.
    pub last_side_effectful_barrels: BTreeSet<String>,
    /// Set to `true` when referenced fixture/config discovery hits `max_files` / `max_source_bytes`
    /// so callers fail closed (disable cache reuse) rather than caching against an incomplete graph.
    pub discovery_budget_exceeded: bool,
    /// Test files whose transitive dependencies contain a dynamic/untrackable construct
    /// (e.g. computed `import(expr)` or `process.env[k]`), so cache reuse is disabled ONLY for
    /// those affected tests rather than globally across unrelated workspace tests.
    pub untrackable_tests: BTreeSet<String>,
    /// Set to `true` when initial workspace inventory skipped more oversized eligible files than
    /// could be tracked individually in `unreadable_inventory_files`.
    pub skipped_oversized_inventory: bool,
    /// Workspace-relative paths that failed to read or exceeded `max_file_bytes` during initial
    /// graph build; propagated into `RootState::unreadable_files` so caching fails closed and
    /// auto-recovers once those files are restored or deleted.
    pub unreadable_inventory_files: BTreeSet<String>,
    pub last_env_ref_digest: String,
    pub local_python_roots: BTreeSet<String>,
    pub cached_conventions: Option<serde_json::Value>,
    pub(super) persisted_edges_valid: bool,
    pub(super) persisted_runtime_deps: Option<BTreeMap<String, BTreeSet<String>>>,
    pub(super) provider_structure_dirty: bool,
    pub(super) test_membership_dirty: bool,
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}


