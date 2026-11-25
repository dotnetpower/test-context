use super::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestRunRequest {
    pub targets: Vec<String>,
    pub k: Option<String>,
    pub mark: Option<String>,
    #[serde(default = "default_true")]
    pub rerun_failed: bool,
    pub no_cache: bool,
    /// Internal flag set by CLI caller-env mismatch fallback so an isolated execution neither
    /// reads from nor writes back to the shared cache.
    #[serde(default)]
    pub skip_cache_write: bool,
    /// Optional caller-environment partition digest set by local oneshot fallback so CLI runs in
    /// shells differing from the systemd daemon still use isolated incremental caching safely.
    #[serde(default)]
    pub env_partition: Option<String>,
    pub ruff_fix: bool,
    /// When true (set by CLI `tctx run`, `tctx pytest`, `tctx cargo-test`, `tctx vitest`,
    /// `tctx jest`, `tctx node-test`), targetless runs always execute the full workspace suite
    /// instead of scoping large (>64 test) workspaces to dirty/recent-commit files.
    #[serde(default)]
    pub full_suite: bool,
    /// Optional language / runner scope set by CLI drop-ins (`"python"`, `"rust"`, `"vitest"`,
    /// `"jest"`) so `tctx cargo-test`, `tctx pytest`, `tctx vitest`, and `tctx jest` only select
    /// and execute matching language tests (and honor explicit Vitest vs Jest runner selection).
    #[serde(default)]
    pub language_filter: Option<String>,
    /// Optional explicit Cargo target selector set by `tctx cargo-test` (`"lib"`, `"bins"`,
    /// `"bin:<name>"`, `"test:<name>"`) so target ownership resolution and filtering honor
    /// explicit Cargo target flags even for modules shared across multiple targets.
    #[serde(default)]
    pub cargo_target: Option<String>,
    pub pytest_args: Vec<String>,
    #[serde(default, alias = "_tctx_cancel_id")]
    pub cancel_id: Option<String>,
}

pub(super) fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuffDiagnostic {
    pub file: String,
    pub row: usize,
    pub column: usize,
    pub code: String,
    pub message: String,
    pub fixable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredFailure {
    pub test_file: String,
    pub nodeid: String,
    pub file: String,
    pub line: usize,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_frames: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRunSummary {
    pub test_file: String,
    pub dep_hash: String,
    pub outcome: TestOutcome,
    pub source: String,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub duration_ms: u64,
    pub failed_nodeids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestRunResponse {
    pub root: PathBuf,
    pub status: String,
    pub mode: String,
    pub total_test_files: usize,
    pub cached_files_count: usize,
    pub joined_background_files_count: usize,
    pub executed_files_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub duration_ms: u64,
    pub saved_ms: u64,
    pub cached_files: Vec<FileRunSummary>,
    pub executed_files: Vec<FileRunSummary>,
    pub failed_nodeids: Vec<String>,
    #[serde(default)]
    pub ruff_diagnostics: Vec<RuffDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_failures: Vec<StructuredFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_culprit_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untested_modified_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky_tests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_prerequisites: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings_summary: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_exit_code: Option<i32>,
    pub stdout_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AffectedTestStatus {
    pub test_file: String,
    pub dep_hash: String,
    pub cache_state: String,
    pub last_duration_ms: Option<u64>,
    pub test_symbols: Vec<String>,
    pub reason_chain: Vec<String>,
}

pub(super) const BACKGROUND_DENY_MARKERS: &[&str] = &[
    "live",
    "integration",
    "destructive",
    "serial",
    "network",
    "external",
    "azure",
    "docker",
];

pub(super) fn background_eligible(spec: &Spec, graph: &DependencyGraph, test_file: &str) -> bool {
    let Some(node) = graph.files.get(test_file) else {
        return false;
    };
    if node
        .parsed
        .markers
        .iter()
        .chain(node.parsed.module_markers.iter())
        .any(|marker| BACKGROUND_DENY_MARKERS.contains(&marker.as_str()))
    {
        return false;
    }
    if !spec.policy.background_safe_only
        || node
            .parsed
            .module_markers
            .iter()
            .any(|marker| marker == "background_safe")
    {
        return true;
    }
    test_file.ends_with(".rs") && !crate::ast_graph::is_rust_integration_test_path(test_file)
}

pub(super) fn background_eligible_with_cache(guard: &RootState, test_file: &str) -> bool {
    background_eligible_from_parts(&guard.spec, &guard.graph, &guard.cache, test_file)
}

pub(super) fn background_eligible_from_parts(
    spec: &Spec,
    graph: &DependencyGraph,
    _cache: &RootCache,
    test_file: &str,
) -> bool {
    background_eligible(spec, graph, test_file)
}

pub(super) fn test_resource_keys(graph: &DependencyGraph, test_file: &str) -> BTreeSet<String> {
    let Some(node) = graph.files.get(test_file) else {
        return BTreeSet::new();
    };
    let mut keys: BTreeSet<String> = node
        .parsed
        .markers
        .iter()
        .chain(node.parsed.module_markers.iter())
        .filter_map(|marker| {
            marker
                .strip_prefix("resource_")
                .map(ToOwned::to_owned)
                .or_else(|| {
                    matches!(
                        marker.as_str(),
                        "database" | "docker" | "network" | "serial" | "port"
                    )
                    .then(|| marker.clone())
                })
        })
        .collect();
    if test_file.ends_with(".rs") {
        keys.insert("cargo_build".to_string());
    } else if crate::ast_graph::is_frontend_source_path(test_file)
        && node
            .parsed
            .markers
            .iter()
            .any(|m| matches!(m.as_str(), "e2e" | "playwright" | "browser"))
    {
        keys.insert("playwright".to_string());
    }
    keys
}

pub(super) static ACTIVE_FOREGROUND_RUNS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub(super) struct ForegroundRunGuard;

impl ForegroundRunGuard {
    pub(super) fn enter() -> Self {
        ACTIVE_FOREGROUND_RUNS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ForegroundRunGuard {
    fn drop(&mut self) {
        ACTIVE_FOREGROUND_RUNS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct GlobalBackgroundPermit;

impl GlobalBackgroundPermit {
    pub(super) fn try_acquire() -> Option<Self> {
        const MAX_GLOBAL_BG_RUNS: usize = 2;
        if ACTIVE_FOREGROUND_RUNS.load(Ordering::Acquire) > 0 {
            return None;
        }
        let counter = global_bg_active_counter();
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current >= MAX_GLOBAL_BG_RUNS {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for GlobalBackgroundPermit {
    fn drop(&mut self) {
        // Mirror static counter in a helper so try_acquire & Drop share the exact same atomic
        global_bg_active_counter().fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn global_bg_active_counter() -> &'static std::sync::atomic::AtomicUsize {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    &COUNTER
}

pub(super) struct RuffCacheEntry {
    pub(super) sha256: String,
    pub(super) diagnostics: Vec<RuffDiagnostic>,
    pub(super) access_tick: u64,
}

pub(super) fn background_capacity_available(max_ratio_percent: usize) -> bool {
    if std::env::var_os("TCTX_PAUSE_BACKGROUND").is_some_and(|value| value == "1") {
        return false;
    }
    if ACTIVE_FOREGROUND_RUNS.load(Ordering::Acquire) > 0 {
        return false;
    }
    if max_ratio_percent >= 200 {
        return true;
    }
    let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") else {
        return true;
    };
    let Some(load) = loadavg
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
    else {
        return true;
    };
    let cpus = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let ceiling = cpus as f64 * max_ratio_percent as f64 / 100.0;
    load <= ceiling.max(1.0)
}

pub(super) struct RootState {
    pub(super) spec: Spec,
    pub(super) workspace: Workspace,
    pub(super) graph: DependencyGraph,
    pub(super) cache: RootCache,
    pub(super) pending_queue: VecDeque<String>,
    pub(super) in_flight: BTreeSet<String>,
    pub(super) watched_dirs: BTreeSet<PathBuf>,
    pub(super) last_activity: Instant,
    pub(super) background_running: bool,
    pub(super) last_background_error: Option<String>,
    pub(super) last_watcher_error: Option<String>,
    pub(super) background_resources: BTreeSet<String>,
    pub(super) foreground_resources: BTreeMap<String, usize>,
    pub(super) background_runs_deferred_load: u64,
    pub(super) last_safety_scan: Instant,
    pub(super) generation: u64,
    pub(super) watch_events_drained: u64,
    pub(super) background_batches: u64,
    pub(super) last_environment_check: Instant,
    pub(super) env_partition: Option<String>,
    pub(super) had_external_cargo_cfg: bool,
    pub(super) prediction_enqueues: u64,
    pub(super) ruff_cache: std::collections::HashMap<String, RuffCacheEntry>,
    pub(super) ruff_cache_tick: u64,
    pub(super) ruff_config_generation: u64,
    pub(super) ruff_crate_generations: std::collections::HashMap<String, u64>,
    pub(super) pending_ruff_files: BTreeSet<String>,
    /// True whenever a required Go embed/cgo directory OR an ordinary workspace directory watch
    /// could not be installed (e.g. `max_watch_dirs` budget exhausted or `notify` watch error).
    pub(super) required_watches_incomplete: bool,
    /// Directories that could not be watched due to `max_watch_dirs` or watcher errors; preserved
    /// even if another watched directory is deleted until they are watched or deleted themselves.
    pub(super) unwatched_dirs: BTreeSet<PathBuf>,
    /// True when overflow inventory (`files_limited`) failed due to policy limits or skipped
    /// oversized eligible files; kept separate from `graph.discovery_budget_exceeded`.
    pub(super) inventory_incomplete: bool,
    /// Workspace-relative paths of files that could not be read (oversized, unreadable, or
    /// replaced with a symlink); cleared per-file as soon as the file is cleanly read or deleted.
    pub(super) unreadable_files: BTreeSet<String>,
    /// Shared monotonic counter incremented on EVERY local invalidation (global or per-file)
    /// so a subsequent global invalidation ALWAYS strictly exceeds any prior per-file counter.
    pub(super) next_invalidation_seq: u64,
    /// Value of `next_invalidation_seq` at the latest full `invalidate([])`.
    pub(super) invalidation_generation: u64,
    /// Per-file `next_invalidation_seq` at the latest targeted `invalidate([file])`.
    pub(super) file_invalidation_generations: BTreeMap<String, u64>,
    /// Last observed `.gitignore` file stamp so `reload_gitignore_and_prune_locked` always bumps
    /// `generation` on matcher change (preventing concurrent `sync()` from restoring an old matcher).
    pub(super) gitignore_stamp: Option<crate::ast_graph::FileStamp>,
    /// True when frontend environment has untracked external dependencies (e.g. `npm link` outside
    /// workspace or `NODE_OPTIONS` `--require`/`--import`/`--loader`): execute uncached without
    /// rotating `dep_hash` mid-run (preventing false `SOURCE_CHANGED_DURING_TEST`).
    pub(super) fe_env_uncacheable: bool,
    /// True when Python editable plugin/package discovery hit symlinks, read errors, or scan caps:
    /// execute uncached without embedding timestamps in `dep_hash`.
    pub(super) py_env_uncacheable: bool,
}

pub struct RootLease {
    pub(super) root: Arc<ManagedRoot>,
}

impl std::ops::Deref for RootLease {
    type Target = ManagedRoot;

    fn deref(&self) -> &Self::Target {
        &self.root
    }
}

impl Drop for RootLease {
    fn drop(&mut self) {
        self.root.active_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(super) struct ScopedRunHandle {
    pub(super) pgids: Arc<Mutex<BTreeSet<i32>>>,
    pub(super) cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct ScopedCancelState {
    pub(super) active: BTreeMap<String, ScopedRunHandle>,
    pub(super) unscoped_active: BTreeMap<u64, ScopedRunHandle>,
    pub(super) next_unscoped_id: u64,
    pub(super) pre_cancelled: BTreeSet<String>,
}

pub struct ManagedRoot {
    pub(super) state: Mutex<RootState>,
    pub(super) cache_io_lock: Mutex<()>,
    pub(super) env_probe_lock: Mutex<()>,
    pub(super) condvar: Condvar,
    pub(super) stopped: AtomicBool,
    pub(super) active_requests: std::sync::atomic::AtomicUsize,
    pub(super) fs_events_rx: Mutex<Receiver<Event>>,
    pub(super) fs_overflow: Arc<AtomicBool>,
    pub(super) has_go_embeds: Arc<AtomicBool>,
    pub(super) watcher: Mutex<Option<notify::RecommendedWatcher>>,
    pub(super) watcher_filter_dirs: Arc<Mutex<BTreeSet<PathBuf>>>,
    pub(super) watcher_workspace: Arc<Mutex<Workspace>>,
    pub(super) watcher_testpaths: Arc<Mutex<Vec<String>>>,
    pub(super) background_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub(super) ast_save_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub(super) zygote: Mutex<ZygoteManager>,
    pub(super) bg_zygote: Mutex<ZygoteManager>,
    pub(super) active_pgid: Arc<AtomicI32>,
    pub(super) bg_active_pgid: Arc<AtomicI32>,
    pub(super) active_pgids: Mutex<BTreeSet<i32>>,
    pub(super) scoped_cancels: Mutex<ScopedCancelState>,
    pub(super) cancel_requested: AtomicBool,
}


