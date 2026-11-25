use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped,
    Error,
}

impl TestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }

    pub fn is_green(self) -> bool {
        matches!(self, Self::Passed | Self::Skipped)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedNodeResult {
    pub nodeid: String,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
    pub output_snippet: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalModuleStamp {
    pub resolved_path: String,
    pub stamp: crate::ast_graph::FileStamp,
    #[serde(default)]
    pub is_dir: bool,
}

pub(super) fn dir_stamp_for_freshness(full_path: &Path) -> Option<crate::ast_graph::FileStamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(full_path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    Some(crate::ast_graph::FileStamp {
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        size: meta.len(),
        ino: meta.ino(),
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedTestRecord {
    pub test_file: String,
    pub dep_hash: String,
    pub outcome: TestOutcome,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub duration_ms: u64,
    pub failed_nodeids: Vec<String>,
    pub output_snippet: String,
    pub updated_unix_ms: u64,
    pub source: String,
    #[serde(default)]
    pub environment_hash: String,
    #[serde(default)]
    pub tctx_version: String,
    #[serde(default)]
    pub runner_mode: String,
    #[serde(default)]
    pub started_unix_ms: u64,
    #[serde(default)]
    pub completed_unix_ms: u64,
    #[serde(default)]
    pub dynamic_dependencies: Vec<String>,
    #[serde(default)]
    pub dynamic_dependencies_complete: bool,
    #[serde(default)]
    pub node_results: BTreeMap<String, CachedNodeResult>,
    #[serde(default)]
    pub external_module_stamps: BTreeMap<String, ExternalModuleStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl CachedTestRecord {
    pub fn external_modules_fresh(&self) -> bool {
        self.external_module_stamps.iter().all(|(lex_path, entry)| {
            let Ok(canon) = std::fs::canonicalize(Path::new(lex_path)) else {
                return false;
            };
            if canon != Path::new(&entry.resolved_path) {
                return false;
            }
            let live = if entry.is_dir {
                dir_stamp_for_freshness(&canon)
            } else {
                crate::ast_graph::file_stamp(&canon)
            };
            live.as_ref() == Some(&entry.stamp)
        })
    }
}

pub(super) fn current_cache_schema() -> u32 {
    7
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationPattern {
    pub test_files: Vec<String>,
    pub frequency: u64,
    pub last_used_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootCache {
    #[serde(default = "current_cache_schema")]
    pub schema: u32,
    pub root: PathBuf,
    pub latest_by_file: BTreeMap<String, CachedTestRecord>,
    pub history_by_key: BTreeMap<String, CachedTestRecord>,
    pub total_saved_ms: u64,
    pub total_cache_hits: u64,
    pub total_delta_runs: u64,
    pub total_background_runs: u64,
    #[serde(default)]
    pub quarantined_keys: BTreeSet<String>,
    #[serde(default)]
    pub persistence_error: Option<String>,
    #[serde(default)]
    pub invocation_patterns: BTreeMap<String, InvocationPattern>,
    #[serde(skip)]
    pub(super) pending_events: Vec<CacheWalEvent>,
    #[serde(default)]
    pub(super) wal_sequence: u64,
    /// Highest WAL sequence of a `Reset` (full invalidation) applied to this cache.
    #[serde(default)]
    pub(super) last_reset_seq: u64,
    /// Highest WAL sequence of an `InvalidateFile` per test file, used during cross-process
    /// rebasing to block stale pre-invalidation upserts without dropping concurrent writes to
    /// unrelated test files.
    #[serde(default)]
    pub(super) last_file_invalidation_seq: BTreeMap<String, u64>,
    #[serde(skip)]
    pub(super) wal_valid_bytes: u64,
    #[serde(skip)]
    pub(super) wal_needs_repair: bool,
    #[serde(skip)]
    pub(super) wal_needs_newline: bool,
    #[serde(skip)]
    pub(super) wal_read_capped: bool,
    #[serde(skip)]
    pub(super) last_snapshot_stamp: Option<(u64, i64, i64)>,
    #[serde(skip)]
    pub(super) last_wal_stamp: Option<(u64, i64, i64)>,
    #[serde(skip)]
    pub(super) cached_memory_bytes: Option<usize>,
    #[serde(skip)]
    pub(super) last_saved_metrics: (u64, u64, u64, u64),
    #[serde(skip)]
    pub(super) pending_invocation_idx: std::collections::HashMap<String, usize>,
    #[serde(skip)]
    pub(super) configured_max_cached_results: usize,
    #[serde(skip)]
    pub(super) configured_max_cache_bytes: usize,
}


