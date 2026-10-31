use crate::ast_graph::{
    AffectedTest, DependencyGraph, FileGraphInfo, file_stamp, normalize_rel_str, sha256_hex,
};
use crate::cache::{CachedNodeResult, CachedTestRecord, RootCache, TestOutcome, now_unix_ms};
use crate::catalog::Spec;
use crate::workspace::{Workspace, run_command_with_timeout, run_command_with_timeout_tracked};
use crate::zygote::{ZygoteManager, cancel_process_group};
use crate::{Result, invalid};
use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestRunRequest {
    pub targets: Vec<String>,
    pub k: Option<String>,
    pub mark: Option<String>,
    #[serde(default = "default_true")]
    pub rerun_failed: bool,
    pub no_cache: bool,
    pub pytest_args: Vec<String>,
}

fn default_true() -> bool {
    true
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

const BACKGROUND_DENY_MARKERS: &[&str] = &[
    "live",
    "integration",
    "destructive",
    "serial",
    "network",
    "external",
    "azure",
    "docker",
];

fn background_eligible(spec: &Spec, graph: &DependencyGraph, test_file: &str) -> bool {
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
    !spec.policy.background_safe_only
        || node
            .parsed
            .module_markers
            .iter()
            .any(|marker| marker == "background_safe")
}

fn background_eligible_with_cache(guard: &RootState, test_file: &str) -> bool {
    background_eligible_from_parts(&guard.spec, &guard.graph, &guard.cache, test_file)
}

fn background_eligible_from_parts(
    spec: &Spec,
    graph: &DependencyGraph,
    _cache: &RootCache,
    test_file: &str,
) -> bool {
    background_eligible(spec, graph, test_file)
}

fn test_resource_keys(graph: &DependencyGraph, test_file: &str) -> BTreeSet<String> {
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
    }
    keys
}

fn background_capacity_available(max_ratio_percent: usize) -> bool {
    if std::env::var_os("TCTX_PAUSE_BACKGROUND").is_some_and(|value| value == "1") {
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

struct RootState {
    spec: Spec,
    workspace: Workspace,
    graph: DependencyGraph,
    cache: RootCache,
    pending_queue: VecDeque<String>,
    in_flight: BTreeSet<String>,
    watched_dirs: BTreeSet<PathBuf>,
    last_activity: Instant,
    background_running: bool,
    last_background_error: Option<String>,
    background_resources: BTreeSet<String>,
    background_runs_deferred_load: u64,
    last_safety_scan: Instant,
    generation: u64,
    watch_events_drained: u64,
    background_batches: u64,
    last_environment_check: Instant,
    prediction_enqueues: u64,
}

pub struct ManagedRoot {
    state: Mutex<RootState>,
    condvar: Condvar,
    stopped: AtomicBool,
    fs_events_rx: Mutex<Receiver<Event>>,
    fs_overflow: Arc<AtomicBool>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    watcher_filter_dirs: Arc<Mutex<BTreeSet<PathBuf>>>,
    background_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    zygote: Mutex<ZygoteManager>,
    active_pgid: Arc<AtomicI32>,
    active_pgids: Mutex<BTreeSet<i32>>,
    cancel_requested: AtomicBool,
}

impl ManagedRoot {
    pub fn load(spec: Spec) -> Result<Arc<Self>> {
        let workspace = spec.workspace()?;
        let mut graph = DependencyGraph::build(&workspace, &spec.policy)?;
        let cache = RootCache::load_or_new(&workspace.root);
        let mut runtime_changed = false;
        for record in cache.latest_by_file.values() {
            runtime_changed |= graph.apply_runtime_dependencies(
                &workspace,
                &spec.policy,
                &record.test_file,
                &record.dynamic_dependencies,
            );
        }
        if runtime_changed {
            graph.recompute_edges();
        }
        let env_hash = environment_fingerprint_for_graph(&spec, &graph);
        graph.apply_environment_hash(env_hash);

        // Queue background execution for dirty git files (or all uncached tests when suite <= 32 files)
        let mut pending_queue = VecDeque::new();
        if spec.policy.background_enabled {
            let dirty = git_dirty_files(&workspace.root).unwrap_or_default();
            if !dirty.is_empty() {
                for item in graph.affected_tests(&dirty) {
                    if cache
                        .get_matching(&item.test_file, &item.dep_hash)
                        .is_none()
                        && background_eligible_from_parts(&spec, &graph, &cache, &item.test_file)
                        && !pending_queue.contains(&item.test_file)
                    {
                        pending_queue.push_back(item.test_file);
                    }
                }
            }
        }

        let watch_dirs =
            workspace.watch_directories_for(graph.files.keys().map(Path::new), &spec.policy)?;
        let watched_dirs_set: BTreeSet<PathBuf> = watch_dirs.iter().cloned().collect();
        let watcher_filter_dirs = Arc::new(Mutex::new(watched_dirs_set.clone()));
        let watcher_filter_for_callback = Arc::clone(&watcher_filter_dirs);
        let watcher_workspace = workspace.clone();

        let state = RootState {
            spec: spec.clone(),
            workspace,
            graph,
            cache,
            pending_queue,
            in_flight: BTreeSet::new(),
            watched_dirs: watched_dirs_set,
            last_activity: Instant::now(),
            background_running: false,
            last_background_error: None,
            background_resources: BTreeSet::new(),
            background_runs_deferred_load: 0,
            last_safety_scan: Instant::now(),
            generation: 1,
            watch_events_drained: 0,
            background_batches: 0,
            last_environment_check: Instant::now(),
            prediction_enqueues: 0,
        };

        let (fs_tx, fs_rx) = sync_channel(4096);
        let fs_overflow = Arc::new(AtomicBool::new(false));
        let overflow_for_cb = Arc::clone(&fs_overflow);

        let arc = Arc::new({
            let zygote_manager = ZygoteManager::default();
            let active_pgid = zygote_manager.active_handle();
            Self {
                state: Mutex::new(state),
                condvar: Condvar::new(),
                stopped: AtomicBool::new(false),
                fs_events_rx: Mutex::new(fs_rx),
                fs_overflow,
                watcher: Mutex::new(None),
                watcher_filter_dirs,
                background_worker: Mutex::new(None),
                zygote: Mutex::new(zygote_manager),
                active_pgid,
                active_pgids: Mutex::new(BTreeSet::new()),
                cancel_requested: AtomicBool::new(false),
            }
        });
        let weak_for_watcher = Arc::downgrade(&arc);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let event = match res {
                Ok(event) => event,
                Err(_) => {
                    overflow_for_cb.store(true, Ordering::Release);
                    if let Some(strong) = weak_for_watcher.upgrade() {
                        strong.condvar.notify_all();
                    }
                    return;
                }
            };
            // Never let our own file reads or access-time updates trigger an inotify feedback storm
            if matches!(
                event.kind,
                notify::EventKind::Access(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
                        notify::event::MetadataKind::AccessTime
                    ))
            ) {
                return;
            }
            if !event.need_rescan()
                && !event.paths.is_empty()
                && event.paths.iter().all(|path| {
                    if path.is_dir() {
                        return watcher_filter_for_callback
                            .lock()
                            .map(|directories| directories.contains(path))
                            .unwrap_or(false);
                    }
                    path.strip_prefix(&watcher_workspace.root)
                        .ok()
                        .is_none_or(|relative| {
                            !watcher_workspace.is_permitted_rel(relative)
                                && !watcher_workspace.is_permitted_data_rel(relative)
                        })
                })
            {
                return;
            }
            if event.need_rescan() {
                overflow_for_cb.store(true, Ordering::Release);
            }
            if let Err(err) = fs_tx.try_send(event)
                && matches!(err, TrySendError::Full(_))
            {
                overflow_for_cb.store(true, Ordering::Release);
            }
            if let Some(strong) = weak_for_watcher.upgrade() {
                strong.condvar.notify_all();
            }
        })?;
        for dir in &watch_dirs {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }
        *arc.watcher
            .lock()
            .map_err(|_| invalid("watcher mutex poisoned"))? = Some(watcher);

        if spec.policy.background_enabled {
            Self::spawn_background_worker(&arc)?;
        }

        Ok(arc)
    }

    pub fn last_activity_elapsed(&self) -> Duration {
        self.state
            .lock()
            .map(|g| g.last_activity.elapsed())
            .unwrap_or_default()
    }

    pub fn indexed_file_count(&self) -> usize {
        self.state
            .lock()
            .map(|guard| guard.graph.files.len())
            .unwrap_or(usize::MAX)
    }

    pub fn is_evictable(&self) -> bool {
        self.active_pgid.load(Ordering::Acquire) <= 0
            && self
                .active_pgids
                .lock()
                .map(|pgids| pgids.is_empty())
                .unwrap_or(false)
            && self
                .state
                .lock()
                .map(|guard| {
                    !guard.background_running
                        && guard.in_flight.is_empty()
                        && guard.pending_queue.is_empty()
                })
                .unwrap_or(false)
    }

    pub fn reclaim_idle_zygote(&self) -> bool {
        let timeout_secs = std::env::var("TCTX_ZYGOTE_IDLE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| (1..=3_600).contains(value))
            .unwrap_or(60);
        self.zygote
            .lock()
            .map(|mut manager| manager.shutdown_if_idle(Duration::from_secs(timeout_secs)))
            .unwrap_or(false)
    }

    pub fn flush_pending_cache(&self) {
        if let Ok(mut guard) = self.state.lock()
            && guard.cache.has_pending_events()
        {
            match guard.cache.save() {
                Ok(()) => guard.cache.persistence_error = None,
                Err(error) => guard.cache.persistence_error = Some(error.to_string()),
            }
        }
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cancel_active();
        self.condvar.notify_all();
        if let Ok(mut w) = self.watcher.lock() {
            *w = None;
        }
        if let Ok(mut worker) = self.background_worker.lock()
            && let Some(handle) = worker.take()
        {
            let _ = handle.join();
        }
        if let Ok(mut guard) = self.state.lock()
            && let Err(error) = guard.cache.save()
        {
            guard.cache.persistence_error = Some(error.to_string());
        }
        if let Ok(mut z) = self.zygote.lock() {
            *z = ZygoteManager::default();
        }
    }

    fn enqueue_affected_locked(guard: &mut RootState, affected: Vec<AffectedTest>) {
        let affected_files: Vec<String> =
            affected.iter().map(|item| item.test_file.clone()).collect();
        let predicted = guard.cache.predicted_affected_tests(&affected_files);
        guard.prediction_enqueues = guard
            .prediction_enqueues
            .saturating_add(u64::try_from(predicted.len()).unwrap_or(u64::MAX));
        let predicted_set: BTreeSet<String> = predicted.iter().cloned().collect();
        let mut ordered = predicted;
        ordered.extend(
            affected_files
                .into_iter()
                .filter(|test_file| !predicted_set.contains(test_file)),
        );
        for test_file in ordered.into_iter().rev() {
            let Some(dep_hash) = guard.graph.dep_hash_for_test.get(&test_file) else {
                continue;
            };
            if guard.cache.get_matching(&test_file, dep_hash).is_none()
                && background_eligible_with_cache(guard, &test_file)
            {
                guard.pending_queue.retain(|queued| queued != &test_file);
                guard.pending_queue.push_front(test_file);
            }
        }
    }

    pub fn cancel_active(&self) -> bool {
        let mut cancelled = cancel_process_group(&self.active_pgid);
        if let Ok(pgids) = self.active_pgids.lock() {
            for &pgid in pgids.iter() {
                cancelled |= unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 };
            }
        }
        if cancelled {
            self.cancel_requested.store(true, Ordering::Release);
        }
        cancelled
    }

    fn spawn_background_worker(this: &Arc<Self>) -> Result<()> {
        let worker_root = Arc::clone(this);
        let handle = std::thread::Builder::new()
            .name("tctx-bg-runner".to_string())
            .spawn(move || {
                worker_root.background_loop();
            })?;
        let mut slot = this
            .background_worker
            .lock()
            .map_err(|_| invalid("background worker mutex poisoned"))?;
        *slot = Some(handle);
        Ok(())
    }

    fn background_loop(&self) {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            if !background_capacity_available(
                self.state
                    .lock()
                    .map(|guard| guard.spec.policy.background_max_load_ratio_percent)
                    .unwrap_or(75),
            ) {
                if let Ok(mut guard) = self.state.lock() {
                    guard.background_runs_deferred_load =
                        guard.background_runs_deferred_load.saturating_add(1);
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            let batch: Vec<(String, String)>;
            let spec: Spec;
            let preload_modules: Vec<String>;
            let environment_hash: String;
            let capture_dynamic_dependencies: bool;
            {
                let mut guard = self.state.lock().expect("state lock");
                self.drain_fs_events_locked(&mut guard);
                while guard.pending_queue.is_empty() && !self.stopped.load(Ordering::SeqCst) {
                    guard.background_running = false;
                    let (next_guard, _) = self
                        .condvar
                        .wait_timeout(guard, Duration::from_secs(2))
                        .expect("condvar wait");
                    guard = next_guard;
                    if !self.stopped.load(Ordering::SeqCst) {
                        // Coalesce burst file saves over a 40ms debounce window before recomputing graph
                        drop(guard);
                        std::thread::sleep(Duration::from_millis(40));
                        guard = self.state.lock().expect("state lock");
                        self.drain_fs_events_locked(&mut guard);
                    }
                }
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                let max_batch = guard.spec.policy.background_batch_size.max(1);
                let mut sorted: Vec<String> = guard.pending_queue.drain(..).collect();
                let prediction_rank: BTreeMap<String, usize> = guard
                    .cache
                    .predicted_affected_tests(&sorted)
                    .into_iter()
                    .enumerate()
                    .map(|(rank, test_file)| (test_file, rank))
                    .collect();
                if guard.background_batches % 4 == 3 {
                    sorted.sort_by_key(|test_file| {
                        (
                            prediction_rank
                                .get(test_file)
                                .copied()
                                .unwrap_or(usize::MAX),
                            std::cmp::Reverse(
                                guard
                                    .cache
                                    .latest_by_file
                                    .get(test_file)
                                    .map(|record| record.duration_ms)
                                    .unwrap_or(0),
                            ),
                        )
                    });
                } else {
                    sorted.sort_by_key(|test_file| {
                        (
                            prediction_rank
                                .get(test_file)
                                .copied()
                                .unwrap_or(usize::MAX),
                            guard
                                .cache
                                .latest_by_file
                                .get(test_file)
                                .map(|record| record.duration_ms)
                                .unwrap_or(0),
                        )
                    });
                }
                guard.pending_queue.extend(sorted);
                let mut selected = Vec::new();
                while selected.len() < max_batch {
                    let Some(test_file) = guard.pending_queue.pop_front() else {
                        break;
                    };
                    let Some(dep_hash) = guard.graph.dep_hash_for_test.get(&test_file).cloned()
                    else {
                        continue;
                    };
                    // Skip if already cached or currently in flight
                    if guard.cache.get_matching(&test_file, &dep_hash).is_some() {
                        continue;
                    }
                    let flight_key = flight_key(&test_file, &dep_hash);
                    if guard.in_flight.insert(flight_key) {
                        selected.push((test_file, dep_hash));
                    }
                }
                if selected.is_empty() {
                    guard.background_running = false;
                    continue;
                }
                guard.background_running = true;
                guard.background_batches = guard.background_batches.saturating_add(1);
                guard.background_resources = selected
                    .iter()
                    .flat_map(|(test_file, _)| {
                        test_resource_keys(&guard.graph, test_file).into_iter()
                    })
                    .collect();
                spec = guard.spec.clone();
                let has_py = selected.iter().any(|(tf, _)| tf.ends_with(".py"));
                preload_modules = if has_py {
                    guard.graph.external_preload_modules_for_tests(
                        &selected
                            .iter()
                            .map(|(test_file, _)| test_file.clone())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    Vec::new()
                };
                batch = selected;
                environment_hash = guard.graph.environment_hash.clone();
                capture_dynamic_dependencies = batch.iter().any(|(test_file, dep_hash)| {
                    guard
                        .cache
                        .get_matching(test_file, dep_hash)
                        .is_none_or(|record| !record.dynamic_dependencies_complete)
                });
            }

            let files_only: Vec<String> = batch.iter().map(|(f, _)| f.clone()).collect();
            let batch_map: BTreeMap<String, String> = batch.iter().cloned().collect();
            let plugins = pytest_plugin_manifest_for_targets(&spec, &files_only);
            let exec_res = execute_pytest_batch(
                &spec,
                &files_only,
                &batch_map,
                None,
                None,
                &[],
                "background",
                Some(&self.zygote),
                Some(&self.active_pgid),
                Some(&self.active_pgids),
                &preload_modules,
                &environment_hash,
                &plugins,
                capture_dynamic_dependencies,
            );
            if self.cancel_requested.swap(false, Ordering::AcqRel) {
                let mut guard = self.state.lock().expect("state lock");
                for (test_file, dep_hash) in &batch {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                guard.last_background_error =
                    Some("TEST_CANCELLED: background pytest was terminated".to_string());
                guard.background_running = !guard.pending_queue.is_empty();
                guard.background_resources.clear();
                self.condvar.notify_all();
                continue;
            }
            let exec_res = exec_res.map(|(mut records, raw)| {
                records.shrink_to_fit();
                (records, raw)
            });

            {
                let mut guard = self.state.lock().expect("state lock");
                for (test_file, dep_hash) in &batch {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                Self::reconcile_live_files_locked(&mut guard, Some(&files_only));
                let unstable: BTreeSet<String> = batch
                    .iter()
                    .filter(|(test_file, expected_hash)| {
                        guard.graph.dep_hash_for_test.get(test_file) != Some(expected_hash)
                    })
                    .map(|(test_file, _)| test_file.clone())
                    .collect();
                for test_file in unstable.iter().rev() {
                    if background_eligible_with_cache(&guard, test_file) {
                        guard.pending_queue.retain(|queued| queued != test_file);
                        guard.pending_queue.push_front(test_file.clone());
                    }
                }
                match exec_res {
                    Ok((mut records, _raw_out)) => {
                        let workspace = guard.workspace.clone();
                        let policy_snapshot = guard.spec.policy.clone();
                        let mut runtime_changed = false;
                        for record in &records {
                            runtime_changed |= guard.graph.apply_runtime_dependencies(
                                &workspace,
                                &policy_snapshot,
                                &record.test_file,
                                &record.dynamic_dependencies,
                            );
                        }
                        if runtime_changed {
                            guard.graph.recompute_edges();
                            for record in &mut records {
                                if let Some(dep_hash) =
                                    guard.graph.dep_hash_for_test.get(&record.test_file)
                                {
                                    record.dep_hash.clone_from(dep_hash);
                                }
                            }
                        }
                        let policy = guard.spec.policy.clone();
                        for rec in records {
                            if !unstable.contains(&rec.test_file) {
                                guard.cache.insert(rec, &policy);
                            }
                        }
                        guard.cache.total_background_runs =
                            guard.cache.total_background_runs.saturating_add(1);
                        guard.last_background_error = None;
                        match guard.cache.save() {
                            Ok(()) => guard.cache.persistence_error = None,
                            Err(error) => {
                                guard.cache.persistence_error = Some(error.to_string());
                            }
                        }
                    }
                    Err(err) => {
                        guard.last_background_error = Some(err.to_string());
                    }
                }
                guard.background_running = !guard.pending_queue.is_empty();
                guard.background_resources.clear();
                self.condvar.notify_all();
            }
        }
    }

    fn drain_fs_events_locked(&self, guard: &mut RootState) {
        let mut raw_paths = BTreeSet::new();
        let mut overflowed = self.fs_overflow.swap(false, Ordering::AcqRel);
        if let Ok(rx) = self.fs_events_rx.lock() {
            while let Ok(event) = rx.try_recv() {
                guard.watch_events_drained = guard.watch_events_drained.saturating_add(1);
                if event.need_rescan() {
                    overflowed = true;
                }
                for p in event.paths {
                    raw_paths.insert(p);
                }
            }
        }
        if overflowed {
            Self::reconcile_live_files_locked(guard, None);
            guard.last_safety_scan = Instant::now();
            return;
        }
        if raw_paths.is_empty() {
            return;
        }

        let mut changed_rels = Vec::new();
        let mut structure_changed = false;
        let mut new_watch_dirs = Vec::new();

        for path in raw_paths {
            if path.is_dir() {
                if path.starts_with(&guard.workspace.root)
                    && guard.watched_dirs.len() < guard.spec.policy.max_watch_dirs
                    && guard.watched_dirs.insert(path.clone())
                {
                    new_watch_dirs.push(path);
                }
                continue;
            }
            let Ok(rel) = path.strip_prefix(&guard.workspace.root) else {
                continue;
            };
            let rel_str = normalize_rel_str(rel);
            let is_known_data = guard.graph.files.contains_key(&rel_str);
            if !guard.workspace.is_permitted_rel(rel) && !is_known_data {
                continue;
            }
            let full = guard.workspace.root.join(rel);
            let mut live_stamp = file_stamp(&full);
            if live_stamp.is_none() && guard.graph.files.contains_key(&rel_str) {
                // Brief 25ms retry window for atomic editor rename saves
                std::thread::sleep(Duration::from_millis(25));
                live_stamp = file_stamp(&full);
            }
            if let Some(stamp) = live_stamp {
                if let Some(parent) = full.parent()
                    && parent.starts_with(&guard.workspace.root)
                    && guard.watched_dirs.len() < guard.spec.policy.max_watch_dirs
                    && guard.watched_dirs.insert(parent.to_path_buf())
                {
                    new_watch_dirs.push(parent.to_path_buf());
                }
                if guard
                    .graph
                    .files
                    .get(&rel_str)
                    .and_then(|n| n.stamp)
                    .is_some_and(|s| s == stamp)
                {
                    continue;
                }
                if let Ok(bytes) = guard
                    .workspace
                    .read_known_with_limit(rel, guard.spec.policy.max_file_bytes)
                {
                    let new_sha = sha256_hex(&bytes);
                    let unchanged = guard
                        .graph
                        .files
                        .get(&rel_str)
                        .is_some_and(|node| node.sha256 == new_sha);
                    if !unchanged {
                        if guard
                            .graph
                            .insert_file_bytes_with_stamp(&rel_str, &bytes, Some(stamp))
                        {
                            structure_changed = true;
                        }
                        changed_rels.push(rel_str);
                    } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                        node.stamp = Some(stamp);
                    }
                }
            } else if !full.exists() && guard.graph.remove_file(&rel_str) {
                changed_rels.push(rel_str);
                structure_changed = true;
            }
        }

        if !new_watch_dirs.is_empty()
            && let Ok(mut w_opt) = self.watcher.lock()
            && let Some(ref mut w) = *w_opt
        {
            for dir in new_watch_dirs {
                let _ = w.watch(&dir, RecursiveMode::NonRecursive);
                if let Ok(mut directories) = self.watcher_filter_dirs.lock() {
                    directories.insert(dir);
                }
            }
        }

        if !changed_rels.is_empty() {
            guard.generation = guard.generation.saturating_add(1);
            if structure_changed {
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                guard.graph.recompute_structure_incremental(&changed_rels);
            } else {
                guard.graph.recompute_hashes_for_changed(&changed_rels);
            }
            if guard.spec.policy.background_enabled {
                let affected = guard.graph.affected_tests(&changed_rels);
                Self::enqueue_affected_locked(guard, affected);
                self.condvar.notify_all();
            }
        }
    }

    fn ingest_explicit_paths_locked(guard: &mut RootState, raw_paths: &[String]) {
        let mut added_any = false;
        for raw in raw_paths {
            let file_part = raw.split("::").next().unwrap_or(raw);
            let norm = file_part
                .trim()
                .trim_start_matches("./")
                .trim_end_matches('/')
                .replace('\\', "/");
            if norm.is_empty() || norm == "." || guard.graph.files.contains_key(&norm) {
                continue;
            }

            let rel_path = Path::new(&norm);
            if !guard.workspace.is_permitted_rel(rel_path)
                && !guard.workspace.is_permitted_data_rel(rel_path)
            {
                continue;
            }
            let full = guard.workspace.root.join(rel_path);
            if let Some(stamp) = file_stamp(&full)
                && let Ok(bytes) = guard
                    .workspace
                    .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
            {
                guard
                    .graph
                    .insert_file_bytes_with_stamp(&norm, &bytes, Some(stamp));
                added_any = true;
            }
        }
        if added_any {
            guard
                .graph
                .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
            guard.graph.recompute_edges();
        }
    }

    fn maybe_safety_reconcile_locked(guard: &mut RootState) {
        if guard.last_safety_scan.elapsed() >= Duration::from_secs(60) {
            Self::reconcile_live_files_locked(guard, None);
            guard.last_safety_scan = Instant::now();
        }
        if guard.last_environment_check.elapsed() >= Duration::from_secs(60) {
            let environment_hash = environment_fingerprint_for_graph(&guard.spec, &guard.graph);
            guard.graph.apply_environment_hash(environment_hash);
            guard.last_environment_check = Instant::now();
        }
    }

    /// Reconciles live disk contents for candidate files before serving a test_run or affected query.
    /// Returns `true` if the dependency graph structure changed.
    fn reconcile_live_files_locked(guard: &mut RootState, target_tests: Option<&[String]>) -> bool {
        let files_to_check: Vec<String> = if let Some(tests) = target_tests {
            let num_indexed = guard.graph.file_index_to_path.len();
            if num_indexed == guard.graph.files.len() && num_indexed > 0 {
                let mut seen = vec![false; num_indexed];
                let mut extra = BTreeSet::new();
                for t in tests {
                    if let Some(ids) = guard.graph.transitive_dep_ids_for_test.get(t) {
                        for &id in ids {
                            if let Some(slot) = seen.get_mut(id as usize) {
                                *slot = true;
                            }
                        }
                    } else {
                        extra.insert(t.clone());
                    }
                }
                let mut list: Vec<String> = seen
                    .into_iter()
                    .enumerate()
                    .filter(|&(_, flag)| flag)
                    .map(|(idx, _)| guard.graph.file_index_to_path[idx].clone())
                    .collect();
                for cfg in guard.graph.config_hashes.keys() {
                    if !list.contains(cfg) {
                        list.push(cfg.clone());
                    }
                }
                for item in extra {
                    if !list.contains(&item) {
                        list.push(item);
                    }
                }
                list
            } else {
                let mut set = BTreeSet::new();
                for t in tests {
                    set.insert(t.clone());
                    for dep in guard.graph.transitive_deps_for(t) {
                        set.insert(dep);
                    }
                }
                for cfg in guard.graph.config_hashes.keys() {
                    set.insert(cfg.clone());
                }
                set.into_iter().collect()
            }
        } else {
            guard.graph.files.keys().cloned().collect()
        };

        let mut changed = Vec::new();
        let mut structure_changed = false;
        for rel_str in files_to_check {
            let rel_path = Path::new(&rel_str);
            let full = guard.workspace.root.join(rel_path);
            if let Some(live_stamp) = file_stamp(&full) {
                // Fast metadata stamp check avoids reading file bytes when mtime/size/inode are unchanged
                if guard
                    .graph
                    .files
                    .get(&rel_str)
                    .and_then(|n| n.stamp)
                    .is_some_and(|s| s == live_stamp)
                {
                    continue;
                }
                if let Ok(bytes) = guard
                    .workspace
                    .read_known_with_limit(rel_path, guard.spec.policy.max_file_bytes)
                {
                    let live_sha = sha256_hex(&bytes);
                    let indexed_sha = guard.graph.files.get(&rel_str).map(|n| n.sha256.as_str());
                    if indexed_sha != Some(live_sha.as_str()) {
                        if guard.graph.insert_file_bytes_with_stamp(
                            &rel_str,
                            &bytes,
                            Some(live_stamp),
                        ) {
                            structure_changed = true;
                        }
                        changed.push(rel_str);
                    } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                        node.stamp = Some(live_stamp);
                    }
                }
            } else if !full.exists() && guard.graph.remove_file(&rel_str) {
                changed.push(rel_str);
                structure_changed = true;
            }
        }

        if !changed.is_empty() {
            guard.generation = guard.generation.saturating_add(1);
            if structure_changed {
                guard
                    .graph
                    .discover_referenced_data_files(&guard.workspace, &guard.spec.policy);
                guard.graph.recompute_edges();
            } else {
                guard.graph.recompute_hashes_for_changed(&changed);
            }
            if guard.spec.policy.background_enabled {
                let affected = guard.graph.affected_tests(&changed);
                Self::enqueue_affected_locked(guard, affected);
            }
        }
        guard.graph.refresh_referenced_env_hashes();
        structure_changed
    }

    pub fn sync(&self, wait_background: bool) -> Result<serde_json::Value> {
        {
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            guard.last_activity = Instant::now();
            guard.graph = DependencyGraph::build(&guard.workspace, &guard.spec.policy)?;
            guard.generation = guard.generation.saturating_add(1);
            let environment_hash = environment_fingerprint_for_graph(&guard.spec, &guard.graph);
            guard.graph.apply_environment_hash(environment_hash);
            if guard.spec.policy.background_enabled {
                let candidates: Vec<(String, String)> = guard
                    .graph
                    .dep_hash_for_test
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (test_file, dep_hash) in candidates {
                    if guard.cache.get_matching(&test_file, &dep_hash).is_none()
                        && background_eligible_with_cache(&guard, &test_file)
                        && !guard.pending_queue.contains(&test_file)
                    {
                        guard.pending_queue.push_back(test_file);
                    }
                }
                self.condvar.notify_all();
            }
        }

        if wait_background {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            while (!guard.pending_queue.is_empty() || !guard.in_flight.is_empty())
                && Instant::now() < deadline
            {
                let (next_guard, _) = self
                    .condvar
                    .wait_timeout(guard, Duration::from_millis(100))
                    .map_err(|_| invalid("mutex poisoned"))?;
                guard = next_guard;
            }
        }

        Ok(self.status())
    }

    pub fn invalidate(&self, files: &[String]) -> Result<serde_json::Value> {
        let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
        guard.last_activity = Instant::now();
        let removed = if files.is_empty() {
            let count = guard.cache.invalidate_all();
            if guard.spec.policy.background_enabled {
                let all_tests: Vec<String> =
                    guard.graph.dep_hash_for_test.keys().cloned().collect();
                for test_file in all_tests {
                    if !background_eligible_with_cache(&guard, &test_file) {
                        continue;
                    }
                    if !guard.pending_queue.contains(&test_file) {
                        guard.pending_queue.push_back(test_file);
                    }
                }
                self.condvar.notify_all();
            }
            count
        } else {
            let resolved = guard.graph.resolve_target_test_files(files);
            let mut count = 0usize;
            for test_file in resolved {
                count += guard.cache.invalidate_file(&test_file);
                if guard.spec.policy.background_enabled
                    && background_eligible_with_cache(&guard, &test_file)
                    && !guard.pending_queue.contains(&test_file)
                {
                    guard.pending_queue.push_back(test_file);
                }
            }
            self.condvar.notify_all();
            count
        };
        match guard.cache.save() {
            Ok(()) => guard.cache.persistence_error = None,
            Err(error) => guard.cache.persistence_error = Some(error.to_string()),
        }
        Ok(serde_json::json!({
            "root": guard.workspace.root,
            "invalidated_entries": removed,
            "pending_background_queue": guard.pending_queue.len(),
        }))
    }

    pub fn status(&self) -> serde_json::Value {
        let mut guard = self.state.lock().expect("state lock");
        self.drain_fs_events_locked(&mut guard);
        let total_tests = guard.graph.dep_hash_for_test.len();
        let mut cached_passed = 0usize;
        let mut cached_failed = 0usize;
        let mut uncached = 0usize;
        for (test_file, dep_hash) in &guard.graph.dep_hash_for_test {
            match guard.cache.get_matching(test_file, dep_hash) {
                Some(rec) if rec.outcome.is_green() => cached_passed += 1,
                Some(_) => cached_failed += 1,
                None => uncached += 1,
            }
        }
        let root = guard.workspace.root.clone();
        let pytest_cmd = guard.spec.effective_pytest_cmd();
        let cargo_cmd = guard.spec.effective_cargo_cmd();
        let indexed_rust_files = guard
            .graph
            .files
            .keys()
            .filter(|k| k.ends_with(".rs"))
            .count();
        let total_rust_test_files = guard
            .graph
            .dep_hash_for_test
            .keys()
            .filter(|k| k.ends_with(".rs"))
            .count();
        let total_python_test_files = guard
            .graph
            .dep_hash_for_test
            .keys()
            .filter(|k| k.ends_with(".py"))
            .count();
        let background_safe_only = guard.spec.policy.background_safe_only;
        let background_deferred = guard.background_runs_deferred_load;
        let generation = guard.generation;
        let watch_events_drained = guard.watch_events_drained;
        let background_batches = guard.background_batches;
        let prediction_enqueues = guard.prediction_enqueues;
        let environment_check_age_ms =
            u64::try_from(guard.last_environment_check.elapsed().as_millis()).unwrap_or(u64::MAX);
        let safety_scan_age_ms =
            u64::try_from(guard.last_safety_scan.elapsed().as_millis()).unwrap_or(u64::MAX);
        let watched_directories = guard.watched_dirs.len();
        let watcher_overflow = self.fs_overflow.load(Ordering::Acquire);
        let cache_storage = guard.cache.storage_metrics();
        let payload = serde_json::json!({
            "root": root,
            "pytest_cmd": pytest_cmd,
            "cargo_cmd": cargo_cmd,
            "indexed_python_files": guard.graph.files.len(),
            "indexed_rust_files": indexed_rust_files,
            "total_test_files": total_tests,
            "total_python_test_files": total_python_test_files,
            "total_rust_test_files": total_rust_test_files,
            "cached_passed_files": cached_passed,
            "cached_failed_files": cached_failed,
            "uncached_or_stale_files": uncached,
            "background_pending_queue": guard.pending_queue.len(),
            "background_in_flight": guard.in_flight.len(),
            "background_running": guard.background_running,
            "last_background_error": guard.last_background_error,
            "total_saved_ms": guard.cache.total_saved_ms,
            "total_cache_hits": guard.cache.total_cache_hits,
            "total_delta_runs": guard.cache.total_delta_runs,
            "total_background_runs": guard.cache.total_background_runs,
            "background_safe_only": background_safe_only,
            "background_runs_deferred_load": background_deferred,
            "watcher_overflow_pending": watcher_overflow,
            "generation": generation,
            "watch_events_drained": watch_events_drained,
            "background_batches": background_batches,
            "prediction_enqueues": prediction_enqueues,
            "environment_check_age_ms": environment_check_age_ms,
            "safety_scan_age_ms": safety_scan_age_ms,
            "watched_directories": watched_directories,
            "cache_quarantined_keys": guard.cache.quarantined_keys.len(),
            "cache_persistence_error": guard.cache.persistence_error,
            "cache_storage": cache_storage,
        });
        drop(guard);
        let zygote = self
            .zygote
            .lock()
            .map(|manager| manager.status())
            .unwrap_or_else(|_| serde_json::json!({"ready":false,"error":"mutex_poisoned"}));
        let mut payload = payload;
        payload["zygote"] = zygote;
        payload
    }

    pub fn graph_info(&self, file: &str) -> Result<FileGraphInfo> {
        let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
        guard.last_activity = Instant::now();
        self.drain_fs_events_locked(&mut guard);
        Self::ingest_explicit_paths_locked(&mut guard, &[file.to_string()]);
        Self::maybe_safety_reconcile_locked(&mut guard);
        guard.graph.file_graph_info(file).ok_or_else(|| {
            invalid(format!(
                "file is not indexed in the Python AST graph: {file}"
            ))
            .into()
        })
    }

    pub fn affected(&self, files: &[String], use_git_diff: bool) -> Result<serde_json::Value> {
        let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
        guard.last_activity = Instant::now();
        self.drain_fs_events_locked(&mut guard);
        Self::ingest_explicit_paths_locked(&mut guard, files);
        Self::maybe_safety_reconcile_locked(&mut guard);

        let changed_files = if !files.is_empty() {
            files.to_vec()
        } else if use_git_diff {
            git_dirty_files(&guard.workspace.root).unwrap_or_default()
        } else {
            Vec::new()
        };

        let affected_modules = guard.graph.affected_modules(&changed_files);
        let affected_raw: Vec<AffectedTest> = guard.graph.affected_tests(&changed_files);
        let mut items = Vec::with_capacity(affected_raw.len());
        for a in affected_raw {
            let f_key = flight_key(&a.test_file, &a.dep_hash);
            let (cache_state, last_duration_ms) = if guard.in_flight.contains(&f_key) {
                ("running_in_background".to_string(), None)
            } else if let Some(rec) = guard.cache.get_matching(&a.test_file, &a.dep_hash) {
                (
                    format!("cached_{}", rec.outcome.as_str()),
                    Some(rec.duration_ms),
                )
            } else if guard.pending_queue.contains(&a.test_file) {
                ("queued_in_background".to_string(), None)
            } else {
                ("uncached".to_string(), None)
            };
            items.push(AffectedTestStatus {
                test_file: a.test_file,
                dep_hash: a.dep_hash,
                cache_state,
                last_duration_ms,
                test_symbols: a.test_symbols,
                reason_chain: a.reason_chain,
            });
        }

        Ok(serde_json::json!({
            "root": guard.workspace.root,
            "changed_files": changed_files,
            "affected_modules": affected_modules,
            "total_affected_test_files": items.len(),
            "affected_tests": items,
        }))
    }

    /// Re-runs once or twice when sources were edited mid-run, so the caller receives a result for the
    /// current content instead of a discarded one. Long attempts are not repeated.
    pub fn run_tests(&self, req: &TestRunRequest) -> Result<TestRunResponse> {
        const MAX_STABILITY_RETRIES: u32 = 2;
        const MAX_RETRYABLE_ATTEMPT: Duration = Duration::from_secs(60);
        let mut attempt = 0;
        loop {
            let attempt_started = Instant::now();
            match self.run_tests_once(req) {
                Err(error)
                    if is_source_changed_error(error.as_ref())
                        && attempt < MAX_STABILITY_RETRIES
                        && attempt_started.elapsed() < MAX_RETRYABLE_ATTEMPT
                        && !self.stopped.load(Ordering::Acquire) =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
                }
                result => return result,
            }
        }
    }

    fn run_tests_once(&self, req: &TestRunRequest) -> Result<TestRunResponse> {
        let started = Instant::now();
        let has_subfile_filter =
            req.k.is_some() || req.mark.is_some() || req.targets.iter().any(|t| t.contains("::"));
        let learn_invocation = !has_subfile_filter && req.pytest_args.is_empty();

        let mut cached_summaries = Vec::new();
        let mut cached_failure_outputs = Vec::new();
        let mut to_execute = Vec::new();
        let mut dep_hashes_for_exec = BTreeMap::new();
        let mut joined_bg_count = 0usize;
        let mut saved_ms = 0u64;
        let spec: Spec;
        let preload_modules: Vec<String>;
        let environment_hash: String;
        let parallel_delta: bool;
        let capture_dynamic_dependencies: bool;
        let target_test_files: Vec<String>;

        {
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            guard.last_activity = Instant::now();
            self.drain_fs_events_locked(&mut guard);
            Self::ingest_explicit_paths_locked(&mut guard, &req.targets);

            // 1. Resolve target test files (and reconcile live files on disk)
            Self::maybe_safety_reconcile_locked(&mut guard);
            let initial_targets = guard.graph.resolve_target_test_files(&req.targets);
            let structure_changed =
                Self::reconcile_live_files_locked(&mut guard, Some(&initial_targets));
            target_test_files = if structure_changed {
                guard.graph.resolve_target_test_files(&req.targets)
            } else {
                initial_targets
            };
            spec = guard.spec.clone();
            environment_hash = guard.graph.environment_hash.clone();
            parallel_delta = guard.spec.policy.parallel_delta_enabled
                && target_test_files.len() > 1
                && target_test_files.iter().all(|test_file| {
                    guard.graph.files.get(test_file).is_some_and(|node| {
                        node.parsed
                            .module_markers
                            .iter()
                            .any(|marker| marker == "parallel_safe")
                    })
                });
            capture_dynamic_dependencies = target_test_files.iter().any(|test_file| {
                guard
                    .graph
                    .dep_hash_for_test
                    .get(test_file)
                    .and_then(|dep_hash| guard.cache.get_matching(test_file, dep_hash))
                    .is_none_or(|record| !record.dynamic_dependencies_complete)
            });

            if target_test_files.is_empty() {
                if !req.targets.is_empty() {
                    let suggestions = guard.graph.suggest_closest_targets(&req.targets);
                    let hint = if suggestions.is_empty() {
                        String::new()
                    } else {
                        format!("; did_you_mean=[{}]", suggestions.join(", "))
                    };
                    return Err(invalid(format!(
                        "NO_TESTS_MATCHED targets={}; check pytest python_files/testpaths or cargo test targets configuration or target spelling{hint}",
                        req.targets.join(",")
                    ))
                    .into());
                }
                return Ok(TestRunResponse {
                    root: guard.workspace.root.clone(),
                    status: "no_tests".to_string(),
                    mode: "no_tests".to_string(),
                    total_test_files: 0,
                    cached_files_count: 0,
                    joined_background_files_count: 0,
                    executed_files_count: 0,
                    passed: 0,
                    failed: 0,
                    skipped: 0,
                    errors: 0,
                    duration_ms: 0,
                    saved_ms: 0,
                    cached_files: Vec::new(),
                    executed_files: Vec::new(),
                    failed_nodeids: Vec::new(),
                    stdout_summary: "=== no tests matched in 0.00s ===".to_string(),
                });
            }

            let requested_resources: BTreeSet<String> = target_test_files
                .iter()
                .flat_map(|test_file| test_resource_keys(&guard.graph, test_file).into_iter())
                .collect();
            let resource_deadline = Instant::now() + Duration::from_secs(30);
            while !requested_resources.is_disjoint(&guard.background_resources)
                && Instant::now() < resource_deadline
            {
                let (next_guard, _) = self
                    .condvar
                    .wait_timeout(guard, Duration::from_millis(50))
                    .map_err(|_| invalid("mutex poisoned"))?;
                guard = next_guard;
            }
            if !requested_resources.is_disjoint(&guard.background_resources) {
                return Err(invalid(format!(
                    "TEST_RESOURCE_BUSY resources={}",
                    requested_resources
                        .intersection(&guard.background_resources)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .into());
            }

            // 2. If any targeted test file is currently in flight in the background worker for the exact current dep_hash,
            // wait briefly (singleflight join) so we don't spawn a duplicate pytest process!
            if !req.no_cache {
                let wait_deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let any_in_flight = target_test_files.iter().any(|tf| {
                        guard
                            .graph
                            .dep_hash_for_test
                            .get(tf)
                            .is_some_and(|dh| guard.in_flight.contains(&flight_key(tf, dh)))
                    });
                    if !any_in_flight || Instant::now() >= wait_deadline {
                        break;
                    }
                    joined_bg_count = joined_bg_count.saturating_add(1);
                    let (next_guard, _) = self
                        .condvar
                        .wait_timeout(guard, Duration::from_millis(50))
                        .map_err(|_| invalid("mutex poisoned"))?;
                    guard = next_guard;
                }
            }

            // 3. Partition targeted test files into Cached vs To-Execute
            for tf in &target_test_files {
                let Some(dep_hash) = guard.graph.dep_hash_for_test.get(tf).cloned() else {
                    continue;
                };
                let requested_nodeids: Vec<String> = req
                    .targets
                    .iter()
                    .filter_map(|t| {
                        let (file_part, _) = t.split_once("::")?;
                        let norm = file_part.trim().trim_start_matches("./").replace('\\', "/");
                        if &norm == tf {
                            Some(t.trim_start_matches("./").replace('\\', "/"))
                        } else {
                            None
                        }
                    })
                    .collect();
                let is_node_cache_eligible = guard.graph.files.get(tf).is_some_and(|node| {
                    node.parsed
                        .module_markers
                        .iter()
                        .any(|marker| marker == "node_cache_safe")
                        || (tf.ends_with(".rs")
                            && !node
                                .parsed
                                .markers
                                .iter()
                                .any(|marker| marker == "serial"))
                });
                if !req.no_cache
                    && !requested_nodeids.is_empty()
                    && is_node_cache_eligible
                    && let Some(record) = guard.cache.get_matching(tf, &dep_hash)
                {
                    let nodes: Option<Vec<_>> = requested_nodeids
                        .iter()
                        .map(|nodeid| record.node_results.get(nodeid))
                        .collect();
                    if let Some(nodes) = nodes
                        && (nodes.iter().all(|n| n.outcome.is_green()) || !req.rerun_failed)
                    {
                        let mut passed = 0;
                        let mut failed = 0;
                        let mut skipped = 0;
                        let mut errors = 0;
                        let mut duration_ms = 0;
                        let mut failed_nodeids = Vec::new();
                        for node in nodes {
                            duration_ms += node.duration_ms;
                            match node.outcome {
                                TestOutcome::Passed => passed += 1,
                                TestOutcome::Failed => {
                                    failed += 1;
                                    failed_nodeids.push(node.nodeid.clone());
                                }
                                TestOutcome::Skipped => skipped += 1,
                                TestOutcome::Error => {
                                    errors += 1;
                                    failed_nodeids.push(node.nodeid.clone());
                                }
                            }
                        }
                        saved_ms = saved_ms.saturating_add(duration_ms);
                        cached_summaries.push(FileRunSummary {
                            test_file: tf.clone(),
                            dep_hash: record.dep_hash.clone(),
                            outcome: if failed > 0 {
                                TestOutcome::Failed
                            } else if errors > 0 {
                                TestOutcome::Error
                            } else if passed > 0 {
                                TestOutcome::Passed
                            } else {
                                TestOutcome::Skipped
                            },
                            source: format!("cached_node({})", record.source),
                            passed,
                            failed,
                            skipped,
                            errors,
                            duration_ms,
                            failed_nodeids,
                        });
                        continue;
                    }
                }
                let can_use_cache = !req.no_cache
                    && req.k.is_none()
                    && req.mark.is_none()
                    && requested_nodeids.is_empty();

                if can_use_cache
                    && let Some(rec) = guard.cache.get_matching(tf, &dep_hash)
                    && (rec.outcome.is_green() || (!req.rerun_failed && !has_subfile_filter))
                {
                    saved_ms = saved_ms.saturating_add(rec.duration_ms);
                    if !rec.outcome.is_green() && !rec.output_snippet.trim().is_empty() {
                        cached_failure_outputs.push(rec.output_snippet.clone());
                    }
                    cached_summaries.push(FileRunSummary {
                        test_file: tf.clone(),
                        dep_hash: rec.dep_hash.clone(),
                        outcome: rec.outcome,
                        source: format!("cached({})", rec.source),
                        passed: rec.passed,
                        failed: rec.failed,
                        skipped: rec.skipped,
                        errors: rec.errors,
                        duration_ms: rec.duration_ms,
                        failed_nodeids: rec.failed_nodeids.clone(),
                    });
                    continue;
                }
                // Remove from background pending queue since we will run it right now
                guard.pending_queue.retain(|p| p != tf);
                guard.in_flight.insert(flight_key(tf, &dep_hash));
                dep_hashes_for_exec.insert(tf.clone(), dep_hash);
                to_execute.push(tf.clone());
            }

            // Prioritize previously failed test files first (Fail-First scheduling), then faster files
            sort_targets_fail_first(&mut to_execute, &guard.cache);

            if !cached_summaries.is_empty() {
                guard.cache.total_cache_hits = guard
                    .cache
                    .total_cache_hits
                    .saturating_add(cached_summaries.len() as u64);
                guard.cache.total_saved_ms = guard.cache.total_saved_ms.saturating_add(saved_ms);
            }
            preload_modules = if to_execute.iter().any(|tf| tf.ends_with(".py")) {
                guard.graph.external_preload_modules_for_tests(&to_execute)
            } else {
                Vec::new()
            };
        }

        // 4. Execute delta tests if any remain uncached
        let mut executed_summaries = Vec::new();
        let mut delta_stdout = String::new();
        if !to_execute.is_empty() {
            // If user passed explicit nodeids (`tests/test_a.py::test_x`) for files in `to_execute`, pass those nodeids
            let invocation_targets: Vec<String> = if req.targets.iter().any(|t| t.contains("::")) {
                req.targets
                    .iter()
                    .filter(|t| {
                        let f = t.split("::").next().unwrap_or(t).trim_start_matches("./");
                        to_execute.iter().any(|ex| ex == f)
                    })
                    .cloned()
                    .collect()
            } else {
                to_execute.clone()
            };

            let plugin_manifest = pytest_plugin_manifest_for_targets(&spec, &invocation_targets);
            let exec_res = if parallel_delta && invocation_targets.len() > 1 {
                execute_pytest_parallel(
                    &spec,
                    &invocation_targets,
                    &dep_hashes_for_exec,
                    req.k.as_deref(),
                    req.mark.as_deref(),
                    &req.pytest_args,
                    &preload_modules,
                    &environment_hash,
                    &plugin_manifest,
                    capture_dynamic_dependencies,
                    &self.active_pgids,
                )
            } else {
                execute_pytest_batch(
                    &spec,
                    &invocation_targets,
                    &dep_hashes_for_exec,
                    req.k.as_deref(),
                    req.mark.as_deref(),
                    &req.pytest_args,
                    "on_demand",
                    Some(&self.zygote),
                    Some(&self.active_pgid),
                    Some(&self.active_pgids),
                    &preload_modules,
                    &environment_hash,
                    &plugin_manifest,
                    capture_dynamic_dependencies,
                )
            };
            if self.cancel_requested.swap(false, Ordering::AcqRel) {
                let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
                for (test_file, dep_hash) in &dep_hashes_for_exec {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                self.condvar.notify_all();
                return Err(invalid("TEST_CANCELLED: pytest process group was terminated").into());
            }
            let (mut records, raw_out) = exec_res?;

            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            for (tf, dh) in &dep_hashes_for_exec {
                guard.in_flight.remove(&flight_key(tf, dh));
            }
            Self::reconcile_live_files_locked(&mut guard, Some(&to_execute));
            let unstable: Vec<String> = dep_hashes_for_exec
                .iter()
                .filter(|(test_file, expected_hash)| {
                    guard.graph.dep_hash_for_test.get(*test_file) != Some(*expected_hash)
                })
                .map(|(test_file, _)| test_file.clone())
                .collect();
            if !unstable.is_empty() {
                let mut requeued = 0usize;
                for test_file in unstable.iter().rev() {
                    if background_eligible_with_cache(&guard, test_file) {
                        guard.pending_queue.retain(|queued| queued != test_file);
                        guard.pending_queue.push_front(test_file.clone());
                        requeued += 1;
                    }
                }
                self.condvar.notify_all();
                return Err(invalid(format!(
                    "SOURCE_CHANGED_DURING_TEST files={}; result discarded ({requeued} requeued for background); the file was edited while tests ran, retry once edits settle",
                    unstable.join(",")
                ))
                .into());
            }
            delta_stdout = raw_out;
            let mut runtime_changed = false;
            let workspace_snapshot = guard.workspace.clone();
            let policy_snapshot = guard.spec.policy.clone();
            for record in &records {
                runtime_changed |= guard.graph.apply_runtime_dependencies(
                    &workspace_snapshot,
                    &policy_snapshot,
                    &record.test_file,
                    &record.dynamic_dependencies,
                );
            }
            if runtime_changed {
                guard.graph.recompute_edges();
                let environment_hash = guard.graph.environment_hash.clone();
                guard.graph.apply_environment_hash(environment_hash);
                for record in &mut records {
                    if let Some(dep_hash) = guard.graph.dep_hash_for_test.get(&record.test_file) {
                        record.dep_hash.clone_from(dep_hash);
                    }
                }
            }
            let policy = guard.spec.policy.clone();
            for rec in records {
                executed_summaries.push(FileRunSummary {
                    test_file: rec.test_file.clone(),
                    dep_hash: rec.dep_hash.clone(),
                    outcome: rec.outcome,
                    source: "executed".to_string(),
                    passed: rec.passed,
                    failed: rec.failed,
                    skipped: rec.skipped,
                    errors: rec.errors,
                    duration_ms: rec.duration_ms,
                    failed_nodeids: rec.failed_nodeids.clone(),
                });
                // Only persist whole-file runs into the canonical cache
                if !has_subfile_filter && req.pytest_args.is_empty() {
                    guard.cache.insert(rec, &policy);
                }
            }
            guard.cache.total_delta_runs = guard.cache.total_delta_runs.saturating_add(1);
            match guard.cache.save() {
                Ok(()) => guard.cache.persistence_error = None,
                Err(error) => guard.cache.persistence_error = Some(error.to_string()),
            }
            self.condvar.notify_all();
        }

        // 5. Aggregate final counts and build pytest-compatible summary
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut errors = 0usize;
        let mut failed_nodeids = Vec::new();
        let mut any_failed_file = false;

        for item in cached_summaries.iter().chain(executed_summaries.iter()) {
            passed += item.passed;
            failed += item.failed;
            skipped += item.skipped;
            errors += item.errors;
            if !item.outcome.is_green() {
                any_failed_file = true;
            }
            for nid in &item.failed_nodeids {
                if !failed_nodeids.contains(nid) {
                    failed_nodeids.push(nid.clone());
                }
            }
        }

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = if any_failed_file || failed > 0 || errors > 0 {
            "failed".to_string()
        } else {
            "passed".to_string()
        };
        if status == "passed" && learn_invocation {
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            guard.cache.record_invocation(&target_test_files);
        }
        let mode = if executed_summaries.is_empty() {
            "instant_cache_hit".to_string()
        } else if cached_summaries.is_empty() {
            "full_run".to_string()
        } else {
            "hybrid_delta".to_string()
        };

        let summary_line = format!(
            "=== {} ({} passed, {} failed, {} skipped) in {:.3}s [test-context: {} cached files, {} executed files, saved {:.2}s] ===",
            status.to_uppercase(),
            passed,
            failed,
            skipped,
            duration_ms as f64 / 1000.0,
            cached_summaries.len(),
            executed_summaries.len(),
            saved_ms as f64 / 1000.0
        );
        let mut output_parts = Vec::new();
        for cached_out in cached_failure_outputs {
            if !cached_out.trim().is_empty() {
                output_parts.push(cached_out.trim().to_string());
            }
        }
        if !delta_stdout.trim().is_empty() {
            output_parts.push(delta_stdout.trim().to_string());
        }
        if skipped > 0 {
            output_parts.push(format!(
                "[test-context] {skipped} skipped tests are never cached or treated as verified. TCTX runs pytest in the daemon environment, so tests that skip for a missing variable or service (see the SKIPPED lines above) must be run directly with the project's own wrapper."
            ));
        }
        if cached_summaries.iter().any(|item| !item.outcome.is_green()) {
            output_parts.push(
                "[test-context] A failure above was replayed from the cache. Pass rerun_failed (or no_cache) to re-execute it if it may depend on environment."
                    .to_string(),
            );
        }
        output_parts.push(summary_line);
        let stdout_summary = output_parts.join("\n");

        Ok(TestRunResponse {
            root: spec.root,
            status,
            mode,
            total_test_files: target_test_files.len(),
            cached_files_count: cached_summaries.len(),
            joined_background_files_count: joined_bg_count,
            executed_files_count: executed_summaries.len(),
            passed,
            failed,
            skipped,
            errors,
            duration_ms,
            saved_ms,
            cached_files: cached_summaries,
            executed_files: executed_summaries,
            failed_nodeids,
            stdout_summary,
        })
    }
}

fn flight_key(test_file: &str, dep_hash: &str) -> String {
    format!("{test_file}@{dep_hash}")
}

pub fn sort_targets_fail_first(targets: &mut [String], cache: &RootCache) {
    targets.sort_by_key(|tf| {
        let prev_failed = cache
            .latest_by_file
            .get(tf)
            .is_some_and(|rec| !rec.outcome.is_green());
        let prev_dur = cache
            .latest_by_file
            .get(tf)
            .map(|rec| rec.duration_ms)
            .unwrap_or(u64::MAX / 2);
        (!prev_failed, prev_dur, tf.clone())
    });
}

fn is_source_changed_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.to_string().starts_with("SOURCE_CHANGED_DURING_TEST")
}

#[cfg(test)]
fn environment_fingerprint(spec: &Spec) -> String {
    environment_fingerprint_with_python(spec, true)
}

fn environment_fingerprint_for_graph(spec: &Spec, graph: &DependencyGraph) -> String {
    let has_python =
        !spec.pytest_cmd.is_empty() || graph.files.keys().any(|path| path.ends_with(".py"));
    environment_fingerprint_with_python(spec, has_python)
}

fn environment_fingerprint_with_python(spec: &Spec, include_python: bool) -> String {
    let pytest_cmd = spec.effective_pytest_cmd();
    let (runtime_identity, plugins) = if include_python {
        python_runtime_identity(spec)
    } else {
        ("rust-only".to_string(), Vec::new())
    };
    let mut evidence = Vec::new();
    evidence.extend_from_slice(b"tctx-env-v4\n");
    evidence.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
    evidence.push(b'\n');
    evidence.extend_from_slice(spec.root.to_string_lossy().as_bytes());
    evidence.push(b'\n');
    evidence.extend_from_slice(pytest_cmd.join("\0").as_bytes());
    evidence.push(b'\n');

    evidence.extend_from_slice(runtime_identity.as_bytes());
    for plugin in plugins {
        evidence.extend_from_slice(b"plugin:");
        evidence.extend_from_slice(plugin.as_bytes());
        evidence.push(b'\n');
    }

    let mut env_keys = vec![
        "PYTHONHASHSEED".to_string(),
        "TZ".to_string(),
        "LANG".to_string(),
        "LC_ALL".to_string(),
        "PYTEST_ADDOPTS".to_string(),
        "RUSTFLAGS".to_string(),
        "CARGO_TARGET_DIR".to_string(),
    ];
    if let Some(extra) = std::env::var_os("TCTX_ENV_KEYS") {
        env_keys.extend(
            extra
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|value| {
                    !value.is_empty()
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .map(ToOwned::to_owned),
        );
    }
    env_keys.sort();
    env_keys.dedup();
    for key in env_keys {
        evidence.extend_from_slice(key.as_bytes());
        evidence.push(b'=');
        if let Some(value) = std::env::var_os(&key) {
            evidence.extend_from_slice(sha256_hex(value.to_string_lossy().as_bytes()).as_bytes());
        }
        evidence.push(b'\n');
    }
    sha256_hex(&evidence)
}

fn pytest_plugin_manifest(spec: &Spec) -> Vec<String> {
    python_runtime_identity(spec).1
}

fn pytest_plugin_manifest_for_targets(spec: &Spec, targets: &[String]) -> Vec<String> {
    if targets
        .iter()
        .any(|t| !t.split("::").next().unwrap_or(t).ends_with(".rs"))
    {
        pytest_plugin_manifest(spec)
    } else {
        Vec::new()
    }
}

#[derive(Clone)]
struct CachedPythonRuntime {
    captured: Instant,
    identity: String,
    plugins: Vec<String>,
}

fn python_runtime_identity(spec: &Spec) -> (String, Vec<String>) {
    static CACHE: OnceLock<Mutex<BTreeMap<String, CachedPythonRuntime>>> = OnceLock::new();
    static FAILURE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unavailable = || {
        let nonce = FAILURE_NONCE.fetch_add(1, Ordering::Relaxed);
        (
            format!("runtime-identity-unavailable:{}:{nonce}", now_unix_ms()),
            Vec::new(),
        )
    };
    let python = crate::zygote::resolve_python_bin(&spec.effective_pytest_cmd());
    let cache_key = format!(
        "{}\0{}",
        python.to_string_lossy(),
        spec.effective_pytest_cmd().join("\0")
    );
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.get(&cache_key)
        && cached.captured.elapsed() < Duration::from_secs(60)
    {
        return (cached.identity.clone(), cached.plugins.clone());
    }

    let script = "import importlib.metadata as m,json,platform,sys,pytest\n\
eps=m.entry_points()\n\
items=eps.select(group='pytest11') if hasattr(eps,'select') else eps.get('pytest11',[])\n\
print(json.dumps({'identity':[sys.version,sys.executable,platform.platform(),pytest.__version__],\
'plugins':sorted({ep.value for ep in items})},separators=(',',':')))";
    let mut command = Command::new(python);
    command.arg("-c").arg(script);
    let Ok(output) = run_command_with_timeout(&mut command, Duration::from_secs(5), 256 * 1024)
    else {
        return unavailable();
    };
    if !output.status.success() {
        return unavailable();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return unavailable();
    };
    let identity = value["identity"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let mut plugins: Vec<String> = value["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.contains(['\0', '\r', '\n', ' '])
        })
        .map(ToOwned::to_owned)
        .collect();
    plugins.sort();
    plugins.dedup();
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key,
            CachedPythonRuntime {
                captured: Instant::now(),
                identity: identity.clone(),
                plugins: plugins.clone(),
            },
        );
    }
    (identity, plugins)
}

fn git_dirty_files(root: &Path) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.args(["--no-optional-locks", "-c", "core.quotePath=false", "-C"])
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=all"]);
    let out = run_command_with_timeout(&mut cmd, Duration::from_secs(5), 1024 * 1024)?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in text.lines() {
        if line.len() > 3 {
            let path_part = line[3..].trim();
            let actual = path_part.split(" -> ").last().unwrap_or(path_part);
            files.push(actual.to_string());
        }
    }
    Ok(files)
}

fn build_pythonpath(root: &Path, existing: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let mut paths = vec![root.to_path_buf()];
    let src = root.join("src");
    if src.is_dir() {
        paths.push(src);
    }
    for container in ["src", "services", "packages", "apps", "extensions"] {
        let dir = root.join(container);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if !sub.is_dir() {
                    continue;
                }
                let sub_src = sub.join("src");
                if sub_src.is_dir() && !paths.contains(&sub_src) {
                    paths.push(sub_src);
                }
                if (sub.join("requirements.txt").is_file()
                    || sub.join("pyproject.toml").is_file()
                    || sub.join("pytest.ini").is_file()
                    || sub.join("main.py").is_file()
                    || sub.join("conftest.py").is_file())
                    && !paths.contains(&sub)
                {
                    paths.push(sub);
                }
            }
        }
    }
    if let Some(old) = existing {
        for p in std::env::split_paths(&old) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    std::env::join_paths(paths).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn execute_pytest_parallel(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    preload_modules: &[String],
    environment_hash: &str,
    pytest_plugins: &[String],
    capture_dynamic_dependencies: bool,
    active_pgids: &Mutex<BTreeSet<i32>>,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 4);
    let mut all_records = Vec::new();
    let mut outputs = Vec::new();
    for wave in invocation_targets.chunks(parallelism) {
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .iter()
                .map(|target| {
                    scope.spawn(move || {
                        let file = target.split("::").next().unwrap_or(target);
                        let mut one_hash = BTreeMap::new();
                        if let Some(hash) = dep_hashes.get(file) {
                            one_hash.insert(file.to_string(), hash.clone());
                        }
                        let local_manager = Mutex::new(ZygoteManager::default());
                        execute_pytest_batch(
                            spec,
                            std::slice::from_ref(target),
                            &one_hash,
                            k_expr,
                            mark_expr,
                            extra_args,
                            "on_demand_parallel",
                            Some(&local_manager),
                            None,
                            Some(active_pgids),
                            preload_modules,
                            environment_hash,
                            pytest_plugins,
                            capture_dynamic_dependencies,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| invalid("parallel pytest worker panicked").into())
                        .and_then(|result| result)
                })
                .collect::<Vec<Result<(Vec<CachedTestRecord>, String)>>>()
        });
        for result in results {
            let (mut records, output) = result?;
            all_records.append(&mut records);
            if !output.trim().is_empty() {
                outputs.push(output);
            }
        }
    }
    Ok((all_records, outputs.join("\n")))
}

#[allow(clippy::too_many_arguments)]
fn execute_pytest_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    zygote_slot: Option<&Mutex<ZygoteManager>>,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    preload_modules: &[String],
    environment_hash: &str,
    pytest_plugins: &[String],
    capture_dynamic_dependencies: bool,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let rs_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| t.split("::").next().unwrap_or(t).ends_with(".rs"))
        .cloned()
        .collect();
    let py_targets: Vec<String> = invocation_targets
        .iter()
        .filter(|t| !t.split("::").next().unwrap_or(t).ends_with(".rs"))
        .cloned()
        .collect();

    if !rs_targets.is_empty() && py_targets.is_empty() {
        return execute_cargo_test_batch(
            spec,
            &rs_targets,
            dep_hashes,
            k_expr,
            mark_expr,
            extra_args,
            source_label,
            active_pgid,
            active_pgids,
            environment_hash,
        );
    } else if !rs_targets.is_empty() && !py_targets.is_empty() {
        let rs_hashes: BTreeMap<String, String> = dep_hashes
            .iter()
            .filter(|(k, _)| k.ends_with(".rs"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let py_hashes: BTreeMap<String, String> = dep_hashes
            .iter()
            .filter(|(k, _)| !k.ends_with(".rs"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let (mut rs_recs, rs_out) = execute_cargo_test_batch(
            spec,
            &rs_targets,
            &rs_hashes,
            k_expr,
            mark_expr,
            extra_args,
            source_label,
            active_pgid,
            active_pgids,
            environment_hash,
        )?;
        let (mut py_recs, py_out) = execute_pytest_batch(
            spec,
            &py_targets,
            &py_hashes,
            k_expr,
            mark_expr,
            extra_args,
            source_label,
            zygote_slot,
            active_pgid,
            active_pgids,
            preload_modules,
            environment_hash,
            pytest_plugins,
            capture_dynamic_dependencies,
        )?;
        rs_recs.append(&mut py_recs);
        let combined = match (rs_out.trim().is_empty(), py_out.trim().is_empty()) {
            (true, _) => py_out,
            (_, true) => rs_out,
            (false, false) => format!("{}\n{}", rs_out.trim(), py_out.trim()),
        };
        return Ok((rs_recs, combined));
    }

    let run_started_unix_ms = now_unix_ms();
    let pytest_cmd = spec.effective_pytest_cmd();
    let Some((program, base_args)) = pytest_cmd.split_first() else {
        return Err(invalid("empty pytest_cmd").into());
    };

    let tmp_dir = tempfile::tempdir()?;
    let junit_path = tmp_dir.path().join("junit.xml");
    let pythonpath = build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH"));

    let mut pytest_cli_args: Vec<String> = vec![
        "-q".to_string(),
        "--tb=short".to_string(),
        "--no-header".to_string(),
        "-p".to_string(),
        "no:cacheprovider".to_string(),
    ];
    if let Some(k) = k_expr
        && !k.trim().is_empty()
    {
        pytest_cli_args.push("-k".to_string());
        pytest_cli_args.push(k.to_string());
    }
    if let Some(m) = mark_expr
        && !m.trim().is_empty()
    {
        pytest_cli_args.push("-m".to_string());
        pytest_cli_args.push(m.to_string());
    }
    for arg in extra_args {
        pytest_cli_args.push(arg.clone());
    }
    for target in invocation_targets {
        pytest_cli_args.push(target.clone());
    }

    let timeout_secs = spec.policy.pytest_timeout_secs;
    let mut output_opt = None;
    if let Some(slot) = zygote_slot
        && let Ok(mut manager) = slot.lock()
        && manager
            .ensure(spec, pythonpath.clone(), preload_modules, pytest_plugins)
            .is_ok()
        && let Ok(out) = manager.run(
            &pytest_cli_args,
            timeout_secs,
            capture_dynamic_dependencies,
            active_pgids,
        )
    {
        output_opt = Some(out);
    }

    let used_zygote = output_opt.is_some();
    let output = if let Some(out) = output_opt {
        out
    } else {
        let mut command = Command::new(program);
        command.current_dir(&spec.root);
        command.env("PYTHONPATH", &pythonpath);
        command.args(base_args);
        command.arg("-o");
        command.arg("junit_family=xunit1");
        command.arg(format!("--junit-xml={}", junit_path.display()));
        command.args(&pytest_cli_args);
        let timeout = Duration::from_secs(timeout_secs);
        run_command_with_timeout_tracked(
            &mut command,
            timeout,
            2 * 1024 * 1024,
            active_pgid,
            active_pgids,
        )?
    };
    if output.timed_out {
        return Err(invalid(format!(
            "pytest execution timed out after {}s",
            spec.policy.pytest_timeout_secs
        ))
        .into());
    }
    if output.cancelled {
        return Err(invalid("TEST_CANCELLED: pytest child process group was terminated").into());
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    let combined_out = if stderr_str.trim().is_empty() {
        stdout_str.to_string()
    } else {
        format!("{}\n{}", stdout_str.trim(), stderr_str.trim())
    };
    let snippet = truncate_str(&combined_out, 8192);

    let (per_file, dynamic_dependencies) = if let Some(structured) = output.structured.as_deref() {
        parse_structured_reports(structured, dep_hashes)
    } else {
        let xml_content = std::fs::read_to_string(&junit_path).unwrap_or_default();
        (
            parse_junit_xml_per_file(&xml_content, dep_hashes),
            Vec::new(),
        )
    };

    let now = now_unix_ms();
    let mut records = Vec::new();
    let overall_success = output.status.success();
    let xml_total_ms: u64 = per_file.values().map(|s| s.duration_ms).sum();
    let overhead_per_file =
        output.duration_ms.saturating_sub(xml_total_ms) / (dep_hashes.len().max(1) as u64);

    for (test_file, dep_hash) in dep_hashes {
        if let Some(stats) = per_file.get(test_file) {
            let outcome = if stats.failed > 0 {
                TestOutcome::Failed
            } else if stats.errors > 0 {
                TestOutcome::Error
            } else if stats.passed > 0 {
                TestOutcome::Passed
            } else if stats.skipped > 0 && overall_success {
                TestOutcome::Skipped
            } else if overall_success {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: stats.passed,
                failed: stats.failed,
                skipped: stats.skipped,
                errors: stats.errors,
                duration_ms: stats.duration_ms.saturating_add(overhead_per_file).max(1),
                failed_nodeids: stats.failed_nodeids.clone(),
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    snippet.clone()
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: if used_zygote {
                    "zygote".to_string()
                } else {
                    "subprocess".to_string()
                },
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: if stats.dynamic_dependencies.is_empty()
                    && dep_hashes.len() == 1
                {
                    dynamic_dependencies.clone()
                } else {
                    stats.dynamic_dependencies.iter().cloned().collect()
                },
                dynamic_dependencies_complete: capture_dynamic_dependencies,
                node_results: stats.node_results.clone(),
            });
        } else {
            // Fallback when JUnit XML had no testcase entry for this file (e.g., collection error or single file)
            // If this was a multi-file batch that failed due to a collection error in one file,
            // only record a failure for files explicitly mentioned in the error output; leave unmentioned sibling files uncached rather than poisoning them.
            if !overall_success
                && dep_hashes.len() > 1
                && !combined_out.contains(test_file.as_str())
            {
                continue;
            }
            let per_file_dur = (output.duration_ms / (dep_hashes.len().max(1) as u64)).max(1);
            // Without a per-file report a clean exit is not evidence that this file passed (all of its
            // tests may have been skipped), so it is reported but never cached as a pass.
            let outcome = if overall_success {
                TestOutcome::Skipped
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: 0,
                failed: usize::from(!overall_success),
                skipped: 0,
                errors: 0,
                duration_ms: per_file_dur,
                failed_nodeids: if overall_success {
                    Vec::new()
                } else {
                    vec![test_file.clone()]
                },
                output_snippet: if overall_success {
                    String::new()
                } else {
                    snippet.clone()
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: if used_zygote {
                    "zygote".to_string()
                } else {
                    "subprocess".to_string()
                },
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: if dep_hashes.len() == 1 {
                    dynamic_dependencies.clone()
                } else {
                    Vec::new()
                },
                dynamic_dependencies_complete: capture_dynamic_dependencies,
                node_results: BTreeMap::new(),
            });
        }
    }

    Ok((records, snippet))
}

fn rust_module_filter_for_file(test_file: &str) -> Option<String> {
    let rel_after_src = if let Some(rest) = test_file.strip_prefix("src/") {
        rest
    } else if let Some((_, rest)) = test_file.rsplit_once("/src/") {
        rest
    } else {
        return None;
    };
    if matches!(rel_after_src, "lib.rs" | "main.rs") || rel_after_src.starts_with("bin/") {
        return None;
    }
    let stripped = if let Some(s) = rel_after_src.strip_suffix("/mod.rs") {
        s
    } else {
        rel_after_src.strip_suffix(".rs")?
    };
    if stripped.is_empty() {
        None
    } else {
        Some(format!("{}::", stripped.replace('/', "::")))
    }
}

fn parse_cargo_test_output(
    combined_out: &str,
    test_file: &str,
    duration_ms: u64,
    overall_success: bool,
) -> FileXmlStats {
    let mut stats = FileXmlStats {
        duration_ms: duration_ms.max(1),
        ..FileXmlStats::default()
    };
    let mut cases: Vec<(String, TestOutcome)> = Vec::new();
    for raw_line in combined_out.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name_part, status_part)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let test_name = name_part.trim();
        if test_name.is_empty() || test_name.contains(' ') {
            continue;
        }
        let status = status_part.trim();
        let outcome = if status == "ok" {
            stats.passed += 1;
            TestOutcome::Passed
        } else if status == "FAILED" {
            stats.failed += 1;
            TestOutcome::Failed
        } else if status == "ignored" || status.starts_with("ignored,") {
            stats.skipped += 1;
            TestOutcome::Skipped
        } else {
            continue;
        };
        cases.push((test_name.to_string(), outcome));
    }

    let case_count = cases.len().max(1) as u64;
    let per_case_ms = (duration_ms / case_count).max(1);
    let snippet = truncate_str(combined_out, 2048);

    for (full_name, outcome) in cases {
        let short_name = full_name.rsplit("::").next().unwrap_or(&full_name);
        let primary_nodeid = format!("{test_file}::{short_name}");
        let full_nodeid = format!("{test_file}::{full_name}");
        if outcome == TestOutcome::Failed && !stats.failed_nodeids.contains(&primary_nodeid) {
            stats.failed_nodeids.push(primary_nodeid.clone());
        }
        let out_snip = if outcome.is_green() {
            String::new()
        } else {
            snippet.clone()
        };
        stats.node_results.insert(
            primary_nodeid.clone(),
            CachedNodeResult {
                nodeid: primary_nodeid,
                outcome,
                duration_ms: per_case_ms,
                output_snippet: out_snip.clone(),
            },
        );
        if full_nodeid != format!("{test_file}::{short_name}") {
            stats.node_results.insert(
                full_nodeid.clone(),
                CachedNodeResult {
                    nodeid: full_nodeid,
                    outcome,
                    duration_ms: per_case_ms,
                    output_snippet: out_snip,
                },
            );
        }
    }

    if stats.passed == 0 && stats.failed == 0 && stats.skipped == 0 && !overall_success {
        if combined_out.contains("error[E") || combined_out.contains("could not compile") {
            stats.errors = 1;
        } else {
            stats.failed = 1;
        }
        stats.failed_nodeids.push(test_file.to_string());
    }

    stats
}

fn demux_cargo_lib_output(
    combined_out: &str,
    lib_files: &[String],
    duration_ms: u64,
    overall_success: bool,
) -> BTreeMap<String, FileXmlStats> {
    let mut prefixes: Vec<(String, String)> = lib_files
        .iter()
        .filter_map(|tf| rust_module_filter_for_file(tf).map(|pfx| (pfx, tf.clone())))
        .collect();
    // Longest module prefix first so nested submodules (`engine::parser::`) match before parent (`engine::`)
    prefixes.sort_by_key(|(pfx, _)| std::cmp::Reverse(pfx.len()));
    let fallback_lib = lib_files
        .iter()
        .find(|tf| tf.ends_with("src/lib.rs"))
        .cloned();

    let mut raw_cases_by_file: BTreeMap<String, Vec<(String, TestOutcome)>> = BTreeMap::new();
    for tf in lib_files {
        raw_cases_by_file.insert(tf.clone(), Vec::new());
    }

    let mut total_cases = 0usize;
    for raw_line in combined_out.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name_part, status_part)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let test_name = name_part.trim();
        if test_name.is_empty() || test_name.contains(' ') {
            continue;
        }
        let status = status_part.trim();
        let outcome = if status == "ok" {
            TestOutcome::Passed
        } else if status == "FAILED" {
            TestOutcome::Failed
        } else if status == "ignored" || status.starts_with("ignored,") {
            TestOutcome::Skipped
        } else {
            continue;
        };
        let matched_file = prefixes
            .iter()
            .find(|(pfx, _)| test_name.starts_with(pfx.as_str()))
            .map(|(_, tf)| tf.clone())
            .or_else(|| fallback_lib.clone());
        if let Some(tf) = matched_file
            && let Some(bucket) = raw_cases_by_file.get_mut(&tf)
        {
            bucket.push((test_name.to_string(), outcome));
            total_cases += 1;
        }
    }

    let per_case_ms = (duration_ms / (total_cases.max(1) as u64)).max(1);
    let snippet = truncate_str(combined_out, 2048);
    let mut result = BTreeMap::new();

    for tf in lib_files {
        let cases = raw_cases_by_file.remove(tf).unwrap_or_default();
        let file_dur = (per_case_ms * (cases.len().max(1) as u64)).max(1);
        let mut stats = FileXmlStats {
            duration_ms: file_dur,
            ..FileXmlStats::default()
        };
        for (full_name, outcome) in cases {
            match outcome {
                TestOutcome::Passed => stats.passed += 1,
                TestOutcome::Failed => stats.failed += 1,
                TestOutcome::Skipped => stats.skipped += 1,
                TestOutcome::Error => stats.errors += 1,
            }
            let short_name = full_name.rsplit("::").next().unwrap_or(&full_name);
            let primary_nodeid = format!("{tf}::{short_name}");
            let full_nodeid = format!("{tf}::{full_name}");
            if outcome == TestOutcome::Failed && !stats.failed_nodeids.contains(&primary_nodeid) {
                stats.failed_nodeids.push(primary_nodeid.clone());
            }
            let out_snip = if outcome.is_green() {
                String::new()
            } else {
                snippet.clone()
            };
            stats.node_results.insert(
                primary_nodeid.clone(),
                CachedNodeResult {
                    nodeid: primary_nodeid,
                    outcome,
                    duration_ms: per_case_ms,
                    output_snippet: out_snip.clone(),
                },
            );
            if full_nodeid != format!("{tf}::{short_name}") {
                stats.node_results.insert(
                    full_nodeid.clone(),
                    CachedNodeResult {
                        nodeid: full_nodeid,
                        outcome,
                        duration_ms: per_case_ms,
                        output_snippet: out_snip,
                    },
                );
            }
        }
        if stats.passed == 0
            && stats.failed == 0
            && stats.skipped == 0
            && !overall_success
            && (lib_files.len() == 1 || combined_out.contains(tf.as_str()))
        {
            if combined_out.contains("error[E") || combined_out.contains("could not compile") {
                stats.errors = 1;
            } else {
                stats.failed = 1;
            }
            stats.failed_nodeids.push(tf.clone());
        }
        result.insert(tf.clone(), stats);
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn execute_cargo_test_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
    environment_hash: &str,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let cargo_cmd = spec.effective_cargo_cmd();
    let Some((program, base_args)) = cargo_cmd.split_first() else {
        return Err(invalid("empty cargo_cmd").into());
    };
    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs);
    let mut records = Vec::new();
    let mut all_outputs = Vec::new();
    let has_explicit_node = invocation_targets.iter().any(|t| t.contains("::"));

    // Group eligible `--lib` unit test files by crate_dir so multiple unit test modules in the same crate
    // run in a single `cargo test --lib` invocation instead of spawning cargo N times.
    let mut lib_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut individual_files: Vec<String> = Vec::new();

    for test_file in dep_hashes.keys() {
        let crate_dir = crate::ast_graph::rust_crate_dir(test_file);
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let is_lib_unit = !has_explicit_node
            && !crate::ast_graph::is_rust_integration_test_path(test_file)
            && !test_file.ends_with("src/main.rs")
            && test_file != "src/main.rs"
            && !test_file.contains("src/bin/")
            && workdir.join("src/lib.rs").is_file();
        if is_lib_unit {
            lib_groups
                .entry(crate_dir)
                .or_default()
                .push(test_file.clone());
        } else {
            individual_files.push(test_file.clone());
        }
    }

    for (crate_dir, group_files) in lib_groups {
        if group_files.len() <= 1 {
            individual_files.extend(group_files);
            continue;
        }
        let run_started_unix_ms = now_unix_ms();
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let mut command = Command::new(program);
        command.current_dir(&workdir);
        command.args(base_args);
        command.arg("test").arg("--lib");
        if let Some(k) = k_expr
            && !k.trim().is_empty()
        {
            command.arg(k);
        }
        for arg in extra_args {
            command.arg(arg);
        }
        if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
            command.arg("--").arg("--ignored");
        }
        let output = run_command_with_timeout_tracked(
            &mut command,
            timeout,
            2 * 1024 * 1024,
            active_pgid,
            active_pgids,
        )?;
        if output.timed_out {
            return Err(invalid(format!(
                "cargo test execution timed out after {}s",
                spec.policy.pytest_timeout_secs
            ))
            .into());
        }
        if output.cancelled {
            return Err(
                invalid("TEST_CANCELLED: cargo test child process group was terminated").into(),
            );
        }
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty()) {
            (true, _) => stderr_str.to_string(),
            (_, true) => stdout_str.to_string(),
            (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
        };
        let snippet = truncate_str(&combined_out, 8192);
        if !snippet.trim().is_empty() {
            all_outputs.push(snippet.clone());
        }
        let overall_success = output.status.success();
        let mut demuxed =
            demux_cargo_lib_output(&combined_out, &group_files, output.duration_ms, overall_success);
        let now = now_unix_ms();
        for test_file in group_files {
            let Some(dep_hash) = dep_hashes.get(&test_file) else {
                continue;
            };
            let stats = demuxed.remove(&test_file).unwrap_or_default();
            if stats.passed == 0
                && stats.failed == 0
                && stats.skipped == 0
                && stats.errors == 0
                && !overall_success
            {
                continue;
            }
            let outcome = if stats.failed > 0 {
                TestOutcome::Failed
            } else if stats.errors > 0 {
                TestOutcome::Error
            } else if stats.passed > 0 && stats.skipped == 0 {
                TestOutcome::Passed
            } else if stats.skipped > 0 && overall_success {
                TestOutcome::Skipped
            } else if overall_success {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: stats.passed,
                failed: stats.failed,
                skipped: stats.skipped,
                errors: stats.errors,
                duration_ms: stats.duration_ms.max(1),
                failed_nodeids: stats.failed_nodeids,
                output_snippet: if outcome.is_green() {
                    String::new()
                } else {
                    snippet.clone()
                },
                updated_unix_ms: now,
                source: source_label.to_string(),
                environment_hash: environment_hash.to_string(),
                tctx_version: env!("CARGO_PKG_VERSION").to_string(),
                runner_mode: "cargo_test_batch".to_string(),
                started_unix_ms: run_started_unix_ms,
                completed_unix_ms: now,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: true,
                node_results: stats.node_results,
            });
        }
    }

    for test_file in &individual_files {
        let Some(dep_hash) = dep_hashes.get(test_file) else {
            continue;
        };
        let run_started_unix_ms = now_unix_ms();
        let crate_dir = crate::ast_graph::rust_crate_dir(test_file);
        let workdir = if crate_dir.is_empty() {
            spec.root.clone()
        } else {
            spec.root.join(&crate_dir)
        };
        let explicit_node_filter: Option<String> = invocation_targets.iter().find_map(|t| {
            let (f, node) = t.split_once("::")?;
            let norm_f = f.trim().trim_start_matches("./").replace('\\', "/");
            if &norm_f == test_file {
                Some(node.to_string())
            } else {
                None
            }
        });

        let mut command = Command::new(program);
        command.current_dir(&workdir);
        command.args(base_args);
        command.arg("test");

        if crate::ast_graph::is_rust_integration_test_path(test_file) {
            if let Some(stem) = Path::new(test_file).file_stem().and_then(|s| s.to_str()) {
                command.arg("--test").arg(stem);
            }
        } else if test_file.ends_with("src/main.rs") || test_file == "src/main.rs" {
            command.arg("--bins");
        } else if let Some((_, bin_rel)) = test_file.rsplit_once("src/bin/")
            && let Some(stem) = Path::new(bin_rel).file_stem().and_then(|s| s.to_str())
        {
            command.arg("--bin").arg(stem);
        } else if workdir.join("src/lib.rs").is_file() {
            command.arg("--lib");
        } else {
            command.arg("--bins");
        }

        let filter_arg = explicit_node_filter
            .or_else(|| k_expr.map(ToOwned::to_owned))
            .or_else(|| rust_module_filter_for_file(test_file));
        if let Some(ref f) = filter_arg
            && !f.trim().is_empty()
        {
            command.arg(f);
        }
        for arg in extra_args {
            command.arg(arg);
        }
        if mark_expr.is_some_and(|m| m == "ignore" || m == "ignored") {
            command.arg("--").arg("--ignored");
        }

        let output = run_command_with_timeout_tracked(
            &mut command,
            timeout,
            2 * 1024 * 1024,
            active_pgid,
            active_pgids,
        )?;
        if output.timed_out {
            return Err(invalid(format!(
                "cargo test execution timed out after {}s",
                spec.policy.pytest_timeout_secs
            ))
            .into());
        }
        if output.cancelled {
            return Err(
                invalid("TEST_CANCELLED: cargo test child process group was terminated").into(),
            );
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        let combined_out = match (stdout_str.trim().is_empty(), stderr_str.trim().is_empty()) {
            (true, _) => stderr_str.to_string(),
            (_, true) => stdout_str.to_string(),
            (false, false) => format!("{}\n{}", stdout_str.trim(), stderr_str.trim()),
        };
        let snippet = truncate_str(&combined_out, 8192);
        if !snippet.trim().is_empty() {
            all_outputs.push(snippet.clone());
        }
        let overall_success = output.status.success();
        let stats =
            parse_cargo_test_output(&combined_out, test_file, output.duration_ms, overall_success);
        let outcome = if stats.failed > 0 {
            TestOutcome::Failed
        } else if stats.errors > 0 {
            TestOutcome::Error
        } else if stats.passed > 0 && stats.skipped == 0 {
            TestOutcome::Passed
        } else if stats.skipped > 0 && overall_success {
            TestOutcome::Skipped
        } else if overall_success {
            TestOutcome::Passed
        } else {
            TestOutcome::Failed
        };
        let now = now_unix_ms();
        records.push(CachedTestRecord {
            test_file: test_file.clone(),
            dep_hash: dep_hash.clone(),
            outcome,
            passed: stats.passed,
            failed: stats.failed,
            skipped: stats.skipped,
            errors: stats.errors,
            duration_ms: stats.duration_ms,
            failed_nodeids: stats.failed_nodeids,
            output_snippet: if outcome.is_green() {
                String::new()
            } else {
                snippet
            },
            updated_unix_ms: now,
            source: source_label.to_string(),
            environment_hash: environment_hash.to_string(),
            tctx_version: env!("CARGO_PKG_VERSION").to_string(),
            runner_mode: "cargo_test".to_string(),
            started_unix_ms: run_started_unix_ms,
            completed_unix_ms: now,
            dynamic_dependencies: Vec::new(),
            dynamic_dependencies_complete: true,
            node_results: stats.node_results,
        });
    }

    Ok((records, all_outputs.join("\n")))
}

fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Default, Debug)]
struct FileXmlStats {
    passed: usize,
    failed: usize,
    skipped: usize,
    errors: usize,
    duration_ms: u64,
    failed_nodeids: Vec<String>,
    node_results: BTreeMap<String, CachedNodeResult>,
    dynamic_dependencies: BTreeSet<String>,
}

#[derive(Deserialize)]
struct DirectReport {
    nodeid: String,
    path: String,
    when: String,
    outcome: String,
    duration_ms: u64,
    longrepr: String,
    #[serde(default)]
    dynamic_deps: Vec<String>,
}

#[derive(Deserialize)]
struct DirectPayload {
    reports: Vec<DirectReport>,
    #[serde(default)]
    dynamic_deps: Vec<String>,
}

fn parse_structured_reports(
    bytes: &[u8],
    known_files: &BTreeMap<String, String>,
) -> (BTreeMap<String, FileXmlStats>, Vec<String>) {
    let Ok(payload) = serde_json::from_slice::<DirectPayload>(bytes) else {
        return (BTreeMap::new(), Vec::new());
    };
    let mut stats: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    for report in payload.reports {
        let file_hint = report.nodeid.split("::").next().unwrap_or(&report.path);
        let Some(test_file) = match_testcase_to_file(Some(file_hint), "", known_files) else {
            continue;
        };
        let entry = stats.entry(test_file.clone()).or_default();
        entry
            .dynamic_dependencies
            .extend(report.dynamic_deps.iter().cloned());
        entry.duration_ms = entry.duration_ms.saturating_add(report.duration_ms);
        match report.outcome.as_str() {
            "passed" if report.when == "call" => {
                entry.passed += 1;
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Passed,
                        duration_ms: report.duration_ms,
                        output_snippet: String::new(),
                    },
                );
            }
            "skipped" => {
                entry.skipped += 1;
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Skipped,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            "failed" if report.when == "call" => {
                entry.failed += 1;
                entry.failed_nodeids.push(report.nodeid.clone());
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Failed,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            "failed" => {
                entry.errors += 1;
                entry.failed_nodeids.push(report.nodeid.clone());
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Error,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    (stats, payload.dynamic_deps)
}

fn find_xml_tag_end(xml: &str, start_idx: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut i = start_idx;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            in_quote = Some(b);
        } else if b == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_junit_xml_per_file(
    xml: &str,
    known_files: &BTreeMap<String, String>,
) -> BTreeMap<String, FileXmlStats> {
    let mut stats: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    if xml.is_empty() {
        return stats;
    }

    let mut pos = 0usize;
    while let Some(rel_start) = xml[pos..].find("<testcase ") {
        let start = pos + rel_start;
        let Some(tag_end) = find_xml_tag_end(xml, start) else {
            break;
        };
        let self_closing = xml[..tag_end].ends_with('/');
        let header = &xml[start..=tag_end];

        let file_attr = extract_xml_attr(header, "file");
        let classname_attr = extract_xml_attr(header, "classname").unwrap_or_default();
        let name_attr = extract_xml_attr(header, "name").unwrap_or_default();
        let time_secs: f64 = extract_xml_attr(header, "time")
            .and_then(|t| t.parse::<f64>().ok())
            .unwrap_or(0.0);

        let body = if self_closing {
            pos = tag_end + 1;
            ""
        } else if let Some(rel_close) = xml[tag_end + 1..].find("</testcase>") {
            let close_start = tag_end + 1 + rel_close;
            pos = close_start + "</testcase>".len();
            &xml[tag_end + 1..close_start]
        } else {
            pos = tag_end + 1;
            ""
        };

        let Some(matched_file) =
            match_testcase_to_file(file_attr.as_deref(), &classname_attr, known_files)
        else {
            continue;
        };

        let entry = stats.entry(matched_file.clone()).or_default();
        let case_ms = (time_secs * 1000.0).round() as u64;
        entry.duration_ms = entry.duration_ms.saturating_add(case_ms);

        if body.contains("<failure") {
            entry.failed += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.failed_nodeids.push(nodeid.clone());
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Failed,
                    duration_ms: case_ms,
                    output_snippet: body.chars().take(8192).collect(),
                },
            );
        } else if body.contains("<error") {
            entry.errors += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.failed_nodeids.push(nodeid.clone());
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Error,
                    duration_ms: case_ms,
                    output_snippet: body.chars().take(8192).collect(),
                },
            );
        } else if body.contains("<skipped") {
            entry.skipped += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Skipped,
                    duration_ms: case_ms,
                    output_snippet: String::new(),
                },
            );
        } else {
            entry.passed += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Passed,
                    duration_ms: case_ms,
                    output_snippet: String::new(),
                },
            );
        }
    }

    stats
}

fn extract_xml_attr(tag: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{attr_name}=\"");
    let mut search_from = 0usize;
    while let Some(rel_idx) = tag[search_from..].find(&pattern) {
        let idx = search_from + rel_idx;
        if idx == 0 || tag.as_bytes()[idx - 1].is_ascii_whitespace() {
            let val_start = idx + pattern.len();
            let val_end = tag[val_start..].find('"')? + val_start;
            return Some(tag[val_start..val_end].replace('\\', "/"));
        }
        search_from = idx + pattern.len();
    }
    None
}

fn match_testcase_to_file(
    file_attr: Option<&str>,
    classname: &str,
    known_files: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(f) = file_attr {
        let norm = f.trim_start_matches("./");
        if known_files.contains_key(norm) {
            return Some(norm.to_string());
        }
        for k in known_files.keys() {
            if k.ends_with(norm) || norm.ends_with(k.as_str()) {
                return Some(k.clone());
            }
        }
    }
    if !classname.is_empty() {
        let parts: Vec<&str> = classname.split('.').collect();
        for len in (1..=parts.len()).rev() {
            let candidate_rel = format!("{}.py", parts[..len].join("/"));
            if known_files.contains_key(&candidate_rel) {
                return Some(candidate_rel);
            }
            for k in known_files.keys() {
                if k.ends_with(&candidate_rel) {
                    return Some(k.clone());
                }
            }
        }
    }
    if known_files.len() == 1 {
        return known_files.keys().next().cloned();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junit_xml_parser_handles_angle_brackets_inside_quoted_attributes() {
        let mut known = BTreeMap::new();
        known.insert("tests/test_cmp.py".to_string(), "hash1".to_string());
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="pytest" errors="0" failures="1" skipped="0" tests="2">
    <testcase classname="tests.test_cmp" name="test_gt[a &gt; b -> c > d]" file="tests/test_cmp.py" time="0.012" />
    <testcase classname="tests.test_cmp" name="test_fail[x > y]" file="tests/test_cmp.py" time="0.005">
      <failure message="assert 1 > 2">AssertionError</failure>
    </testcase>
  </testsuite>
</testsuites>"#;
        let parsed = parse_junit_xml_per_file(xml, &known);
        let st = parsed
            .get("tests/test_cmp.py")
            .expect("stats for test_cmp.py");
        assert_eq!(st.passed, 1);
        assert_eq!(st.failed, 1);
        assert_eq!(
            st.failed_nodeids,
            vec!["tests/test_cmp.py::test_fail[x > y]"]
        );
    }

    #[test]
    fn background_policy_requires_module_opt_in_and_denies_live_markers() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path(), Vec::new(), Vec::new()).unwrap();
        let base_spec = Spec {
            root: root.path().to_path_buf(),
            scopes: Vec::new(),
            globs: Vec::new(),
            excludes: Vec::new(),
            default_excludes: true,
            pytest_cmd: Vec::new(),
            policy: Default::default(),
            allow_worktrees: false,
            managed_parent: None,
        };
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "test_safe.py",
            b"import pytest\npytestmark = pytest.mark.background_safe\ndef test_ok(): pass\n",
        );
        graph.insert_file_bytes(
            "test_live.py",
            b"import pytest\npytestmark = [pytest.mark.background_safe, pytest.mark.live]\ndef test_live(): pass\n",
        );
        graph.insert_file_bytes("test_plain.py", b"def test_plain(): pass\n");
        graph.recompute_edges();
        let _ = workspace;
        assert!(background_eligible(&base_spec, &graph, "test_safe.py"));
        assert!(!background_eligible(&base_spec, &graph, "test_live.py"));
        assert!(!background_eligible(&base_spec, &graph, "test_plain.py"));
        let mut cache = RootCache::new(root.path().to_path_buf());
        cache.record_invocation(&["test_plain.py".to_string()]);
        assert!(
            !background_eligible_from_parts(&base_spec, &graph, &cache, "test_plain.py"),
            "learned foreground invocations must not bypass background-safe opt-in"
        );
    }

    #[test]
    fn unavailable_python_identity_never_reuses_a_cache_partition() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let broken_python = root.path().join("python-broken");
        std::fs::write(&broken_python, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&broken_python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = Spec {
            root: root.path().to_path_buf(),
            scopes: Vec::new(),
            globs: Vec::new(),
            excludes: Vec::new(),
            default_excludes: true,
            pytest_cmd: vec![broken_python.to_string_lossy().into_owned()],
            policy: Default::default(),
            allow_worktrees: false,
            managed_parent: None,
        };
        let first = environment_fingerprint(&spec);
        let second = environment_fingerprint(&spec);
        assert_ne!(first, second);
    }

    #[test]
    fn resource_keys_are_derived_from_module_markers() {
        let mut graph = DependencyGraph::new();
        graph.insert_file_bytes(
            "test_db.py",
            b"import pytest\npytestmark = [pytest.mark.background_safe, pytest.mark.resource_postgres, pytest.mark.serial]\ndef test_db(): pass\n",
        );
        graph.recompute_edges();
        let keys = test_resource_keys(&graph, "test_db.py");
        assert!(keys.contains("postgres"));
        assert!(keys.contains("serial"));
    }

    #[test]
    fn parses_cargo_test_output_and_compile_errors_with_node_results() {
        let out = r#"
running 3 tests
test math::tests::test_add_unit ... ok
test math::tests::test_sub_unit ... FAILED
test math::tests::test_slow ... ignored, requires cluster

failures:
    math::tests::test_sub_unit

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s
"#;
        let stats = parse_cargo_test_output(out, "src/math.rs", 15, false);
        assert_eq!(stats.passed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.errors, 0);
        assert_eq!(
            stats.failed_nodeids,
            vec!["src/math.rs::test_sub_unit".to_string()]
        );
        assert_eq!(
            stats.node_results["src/math.rs::test_add_unit"].outcome,
            TestOutcome::Passed
        );
        assert_eq!(
            stats.node_results["src/math.rs::math::tests::test_add_unit"].outcome,
            TestOutcome::Passed
        );

        let compile_err = "error[E0425]: cannot find value `missing` in this scope\nerror: could not compile `sample`";
        let err_stats = parse_cargo_test_output(compile_err, "src/lib.rs", 20, false);
        assert_eq!(err_stats.errors, 1);
        assert_eq!(err_stats.failed_nodeids, vec!["src/lib.rs".to_string()]);
        assert_eq!(
            rust_module_filter_for_file("src/engine/parser.rs"),
            Some("engine::parser::".to_string())
        );
        assert_eq!(rust_module_filter_for_file("src/lib.rs"), None);
    }

    #[test]
    fn fail_first_scheduler_orders_previously_failed_tests_before_passing_and_slow_tests() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = RootCache::new(root.path().to_path_buf());
        let policy = crate::policy::MemoryPolicy::default();
        cache.insert(
            CachedTestRecord {
                test_file: "tests/test_slow_pass.py".to_string(),
                dep_hash: "h1".to_string(),
                outcome: TestOutcome::Passed,
                passed: 1,
                failed: 0,
                skipped: 0,
                errors: 0,
                duration_ms: 800,
                failed_nodeids: Vec::new(),
                output_snippet: String::new(),
                updated_unix_ms: 100,
                source: "on_demand".to_string(),
                environment_hash: String::new(),
                tctx_version: String::new(),
                runner_mode: String::new(),
                started_unix_ms: 0,
                completed_unix_ms: 100,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: true,
                node_results: BTreeMap::new(),
            },
            &policy,
        );
        cache.insert(
            CachedTestRecord {
                test_file: "tests/test_prev_failed.rs".to_string(),
                dep_hash: "h2".to_string(),
                outcome: TestOutcome::Failed,
                passed: 0,
                failed: 1,
                skipped: 0,
                errors: 0,
                duration_ms: 500,
                failed_nodeids: vec!["tests/test_prev_failed.rs::test_bug".to_string()],
                output_snippet: "assertion failed".to_string(),
                updated_unix_ms: 200,
                source: "on_demand".to_string(),
                environment_hash: String::new(),
                tctx_version: String::new(),
                runner_mode: String::new(),
                started_unix_ms: 100,
                completed_unix_ms: 200,
                dynamic_dependencies: Vec::new(),
                dynamic_dependencies_complete: true,
                node_results: BTreeMap::new(),
            },
            &policy,
        );
        let mut targets = vec![
            "tests/test_slow_pass.py".to_string(),
            "tests/test_unknown.py".to_string(),
            "tests/test_prev_failed.rs".to_string(),
        ];
        sort_targets_fail_first(&mut targets, &cache);
        assert_eq!(
            targets,
            vec![
                "tests/test_prev_failed.rs".to_string(),
                "tests/test_slow_pass.py".to_string(),
                "tests/test_unknown.py".to_string(),
            ]
        );
    }

    #[test]
    fn demux_cargo_lib_output_splits_single_lib_run_across_source_files() {
        let out = r#"
running 4 tests
test calc::tests::test_add ... ok
test calc::sub::tests::test_sub ... ok
test engine::tests::test_run ... FAILED
test tests::test_root_lib ... ok

failures:
    engine::tests::test_run

test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
"#;
        let files = vec![
            "src/lib.rs".to_string(),
            "src/calc.rs".to_string(),
            "src/calc/sub.rs".to_string(),
            "src/engine.rs".to_string(),
        ];
        let demuxed = demux_cargo_lib_output(out, &files, 40, false);
        assert_eq!(demuxed["src/calc.rs"].passed, 1);
        assert_eq!(demuxed["src/calc.rs"].failed, 0);
        assert_eq!(demuxed["src/calc/sub.rs"].passed, 1);
        assert_eq!(demuxed["src/engine.rs"].passed, 0);
        assert_eq!(demuxed["src/engine.rs"].failed, 1);
        assert_eq!(
            demuxed["src/engine.rs"].failed_nodeids,
            vec!["src/engine.rs::test_run".to_string()]
        );
        assert_eq!(demuxed["src/lib.rs"].passed, 1);
    }
}
