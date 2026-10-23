use crate::ast_graph::{
    AffectedTest, DependencyGraph, FileGraphInfo, file_stamp, normalize_rel_str, sha256_hex,
};
use crate::cache::{CachedTestRecord, RootCache, TestOutcome, now_unix_ms};
use crate::catalog::Spec;
use crate::workspace::{Workspace, run_command_with_timeout};
use crate::{Result, invalid};
use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

struct RootState {
    spec: Spec,
    workspace: Workspace,
    graph: DependencyGraph,
    cache: RootCache,
    pending_queue: VecDeque<String>,
    in_flight: BTreeSet<String>,
    last_activity: Instant,
    background_running: bool,
    last_background_error: Option<String>,
}

pub struct ManagedRoot {
    state: Mutex<RootState>,
    condvar: Condvar,
    stopped: AtomicBool,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl ManagedRoot {
    pub fn load(spec: Spec) -> Result<Arc<Self>> {
        let workspace = spec.workspace()?;
        let graph = DependencyGraph::build(&workspace, &spec.policy)?;
        let cache = RootCache::load_or_new(&workspace.root);

        // Queue background execution for dirty git files (or all uncached tests when suite <= 32 files)
        let mut pending_queue = VecDeque::new();
        if spec.policy.background_enabled {
            let dirty = git_dirty_files(&workspace.root).unwrap_or_default();
            if !dirty.is_empty() {
                for item in graph.affected_tests(&dirty) {
                    if cache.get_matching(&item.test_file, &item.dep_hash).is_none()
                        && !pending_queue.contains(&item.test_file)
                    {
                        pending_queue.push_back(item.test_file);
                    }
                }
            } else if graph.dep_hash_for_test.len() <= 32 {
                for (test_file, dep_hash) in &graph.dep_hash_for_test {
                    if cache.get_matching(test_file, dep_hash).is_none() {
                        pending_queue.push_back(test_file.clone());
                    }
                }
            }
        }

        let watch_dirs = workspace.watch_directories_for(
            graph.files.keys().map(Path::new),
            &spec.policy,
        )?;

        let state =RootState {
            spec: spec.clone(),
            workspace,
            graph,
            cache,
            pending_queue,
            in_flight: BTreeSet::new(),
            last_activity: Instant::now(),
            background_running: false,
            last_background_error: None,
        };

        let arc = Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak_for_watcher = weak.clone();
            let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
                if let Ok(event) = res
                    && let Some(strong) = weak_for_watcher.upgrade()
                {
                    strong.handle_fs_event(event);
                }
            })
            .ok()
            .map(|mut w| {
                for dir in &watch_dirs {
                    let _ = w.watch(dir, RecursiveMode::NonRecursive);
                }
                w
            });

            Self {
                state: Mutex::new(state),
                condvar: Condvar::new(),
                stopped: AtomicBool::new(false),
                _watcher: watcher,
            }
        });

        if spec.policy.background_enabled {
            Self::spawn_background_worker(Arc::clone(&arc));
        }

        Ok(arc)
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }

    fn spawn_background_worker(this: Arc<Self>) {
        let _ = std::thread::Builder::new()
            .name("tctx-bg-runner".to_string())
            .spawn(move || {
                this.background_loop();
            });
    }

    fn background_loop(&self) {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            let batch: Vec<(String, String)>;
            let spec: Spec;
            {
                let mut guard = self.state.lock().expect("state lock");
                while guard.pending_queue.is_empty() && !self.stopped.load(Ordering::SeqCst) {
                    guard.background_running = false;
                    let (next_guard, _) = self
                        .condvar
                        .wait_timeout(guard, Duration::from_secs(2))
                        .expect("condvar wait");
                    guard = next_guard;
                }
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                let max_batch = guard.spec.policy.background_batch_size.max(1);
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
                batch = selected;
                spec = guard.spec.clone();
            }

            let files_only: Vec<String> = batch.iter().map(|(f, _)| f.clone()).collect();
            let batch_map: BTreeMap<String, String> = batch.iter().cloned().collect();
            let exec_res = execute_pytest_batch(
                &spec,
                &files_only,
                &batch_map,
                None,
                None,
                &[],
                "background",
            );

            {
                let mut guard = self.state.lock().expect("state lock");
                for (test_file, dep_hash) in &batch {
                    guard.in_flight.remove(&flight_key(test_file, dep_hash));
                }
                match exec_res {
                    Ok((records, _raw_out)) => {
                        let policy = guard.spec.policy.clone();
                        for rec in records {
                            // Only record if the file's live dep_hash hasn't already moved on, or store in history_by_key
                            guard.cache.insert(rec, &policy);
                        }
                        guard.cache.total_background_runs =
                            guard.cache.total_background_runs.saturating_add(1);
                        guard.last_background_error = None;
                        let _ = guard.cache.save();
                    }
                    Err(err) => {
                        guard.last_background_error = Some(err.to_string());
                    }
                }
                guard.background_running = !guard.pending_queue.is_empty();
                self.condvar.notify_all();
            }
        }
    }

    pub fn handle_fs_event(&self, event: Event) {
        // Ignore read/open/access-time events so our own file reads never trigger an inotify feedback loop!
        if matches!(
            event.kind,
            notify::EventKind::Access(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(
                    notify::event::MetadataKind::AccessTime
                ))
        ) {
            return;
        }
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut changed_rels = Vec::new();
        let mut graph_dirty = false;

        for path in &event.paths {
            let Ok(rel) = path.strip_prefix(&guard.workspace.root) else {
                continue;
            };
            if !guard.workspace.is_permitted_rel(rel) {
                continue;
            }
            let rel_str = normalize_rel_str(rel);
            let full = guard.workspace.root.join(rel);
            if let Some(live_stamp) = file_stamp(&full) {
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
                    .read_known_with_limit(rel, guard.spec.policy.max_file_bytes)
                {
                    let new_sha = sha256_hex(&bytes);
                    let unchanged = guard
                        .graph
                        .files
                        .get(&rel_str)
                        .is_some_and(|node| node.sha256 == new_sha);
                    if !unchanged {
                        guard
                            .graph
                            .insert_file_bytes_with_stamp(&rel_str, &bytes, Some(live_stamp));
                        changed_rels.push(rel_str);
                        graph_dirty = true;
                    } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                        node.stamp = Some(live_stamp);
                    }
                }
            } else if !full.exists() && guard.graph.remove_file(&rel_str) {
                changed_rels.push(rel_str);
                graph_dirty = true;
            }
        }

        if graph_dirty {
            guard.graph.recompute_edges();
            if guard.spec.policy.background_enabled {
                let affected = guard.graph.affected_tests(&changed_rels);
                for item in affected.into_iter().rev() {
                    if guard
                        .cache
                        .get_matching(&item.test_file, &item.dep_hash)
                        .is_none()
                    {
                        guard.pending_queue.retain(|p| p != &item.test_file);
                        guard.pending_queue.push_front(item.test_file);
                    }
                }
                self.condvar.notify_all();
            }
        }
    }

    /// Reconciles live disk contents for candidate files before serving a test_run or affected query.
    fn reconcile_live_files_locked(guard: &mut RootState, target_tests: Option<&[String]>) {
        let files_to_check: Vec<String> = if let Some(tests) = target_tests {
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
        } else {
            guard.graph.files.keys().cloned().collect()
        };

        let mut changed = Vec::new();
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
                        guard
                            .graph
                            .insert_file_bytes_with_stamp(&rel_str, &bytes, Some(live_stamp));
                        changed.push(rel_str);
                    } else if let Some(node) = guard.graph.files.get_mut(&rel_str) {
                        node.stamp = Some(live_stamp);
                    }
                }
            } else if !full.exists() && guard.graph.remove_file(&rel_str) {
                changed.push(rel_str);
            }
        }

        if !changed.is_empty() {
            guard.graph.recompute_edges();
            if guard.spec.policy.background_enabled {
                for item in guard.graph.affected_tests(&changed).into_iter().rev() {
                    if guard
                        .cache
                        .get_matching(&item.test_file, &item.dep_hash)
                        .is_none()
                    {
                        guard.pending_queue.retain(|p| p != &item.test_file);
                        guard.pending_queue.push_front(item.test_file);
                    }
                }
            }
        }
    }

    pub fn sync(&self, wait_background: bool) -> Result<serde_json::Value> {
        {
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            guard.last_activity = Instant::now();
            guard.graph = DependencyGraph::build(&guard.workspace, &guard.spec.policy)?;
            if guard.spec.policy.background_enabled {
                let candidates: Vec<(String, String)> = guard
                    .graph
                    .dep_hash_for_test
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (test_file, dep_hash) in candidates {
                    if guard.cache.get_matching(&test_file, &dep_hash).is_none()
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
                let all_tests: Vec<String> = guard.graph.dep_hash_for_test.keys().cloned().collect();
                for test_file in all_tests {
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
                if guard.spec.policy.background_enabled && !guard.pending_queue.contains(&test_file)
                {
                    guard.pending_queue.push_back(test_file);
                }
            }
            self.condvar.notify_all();
            count
        };
        let _ = guard.cache.save();
        Ok(serde_json::json!({
            "root": guard.workspace.root,
            "invalidated_entries": removed,
            "pending_background_queue": guard.pending_queue.len(),
        }))
    }

    pub fn status(&self) -> serde_json::Value {
        let guard = self.state.lock().expect("state lock");
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
        serde_json::json!({
            "root": guard.workspace.root,
            "pytest_cmd": guard.spec.effective_pytest_cmd(),
            "indexed_python_files": guard.graph.files.len(),
            "total_test_files": total_tests,
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
        })
    }

    pub fn graph_info(&self, file: &str) -> Result<FileGraphInfo> {
        let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
        guard.last_activity = Instant::now();
        Self::reconcile_live_files_locked(&mut guard, None);
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
        Self::reconcile_live_files_locked(&mut guard, None);

        let changed_files = if !files.is_empty() {
            files.to_vec()
        } else if use_git_diff {
            git_dirty_files(&guard.workspace.root).unwrap_or_default()
        } else {
            Vec::new()
        };

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
            "total_affected_test_files": items.len(),
            "affected_tests": items,
        }))
    }

    pub fn run_tests(&self, req: &TestRunRequest) -> Result<TestRunResponse> {
        let started = Instant::now();
        let has_subfile_filter = req.k.is_some()
            || req.mark.is_some()
            || req.targets.iter().any(|t| t.contains("::"));

        let mut cached_summaries = Vec::new();
        let mut to_execute = Vec::new();
        let mut dep_hashes_for_exec = BTreeMap::new();
        let mut joined_bg_count = 0usize;
        let mut saved_ms = 0u64;
        let spec: Spec;
        let target_test_files: Vec<String>;

        {
            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            guard.last_activity = Instant::now();

            // 1. Resolve target test files (and reconcile live files on disk)
            let initial_targets = guard.graph.resolve_target_test_files(&req.targets);
            Self::reconcile_live_files_locked(&mut guard, Some(&initial_targets));
            target_test_files = guard.graph.resolve_target_test_files(&req.targets);
            spec = guard.spec.clone();

            if target_test_files.is_empty() {
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
                if !req.no_cache
                    && let Some(rec) = guard.cache.get_matching(tf, &dep_hash)
                    && (rec.outcome.is_green() || (!req.rerun_failed && !has_subfile_filter))
                {
                    saved_ms = saved_ms.saturating_add(rec.duration_ms);
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

            if !cached_summaries.is_empty() {
                guard.cache.total_cache_hits = guard
                    .cache
                    .total_cache_hits
                    .saturating_add(cached_summaries.len() as u64);
                guard.cache.total_saved_ms = guard.cache.total_saved_ms.saturating_add(saved_ms);
            }
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

            let exec_res = execute_pytest_batch(
                &spec,
                &invocation_targets,
                &dep_hashes_for_exec,
                req.k.as_deref(),
                req.mark.as_deref(),
                &req.pytest_args,
                "on_demand",
            );

            let mut guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            for (tf, dh) in &dep_hashes_for_exec {
                guard.in_flight.remove(&flight_key(tf, dh));
            }
            let (records, raw_out) = exec_res?;
            delta_stdout = raw_out;
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
            let _ = guard.cache.save();
            self.condvar.notify_all();
        } else {
            let guard = self.state.lock().map_err(|_| invalid("mutex poisoned"))?;
            let _ = guard.cache.save();
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
        let stdout_summary = if delta_stdout.trim().is_empty() {
            summary_line
        } else {
            format!("{}\n{}", delta_stdout.trim(), summary_line)
        };

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

fn git_dirty_files(root: &Path) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotePath=false", "-C"])
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
    for container in ["services", "packages"] {
        let dir = root.join(container);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let sub_src = entry.path().join("src");
                if sub_src.is_dir() {
                    paths.push(sub_src);
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

fn execute_pytest_batch(
    spec: &Spec,
    invocation_targets: &[String],
    dep_hashes: &BTreeMap<String, String>,
    k_expr: Option<&str>,
    mark_expr: Option<&str>,
    extra_args: &[String],
    source_label: &str,
) -> Result<(Vec<CachedTestRecord>, String)> {
    let pytest_cmd = spec.effective_pytest_cmd();
    let Some((program, base_args)) = pytest_cmd.split_first() else {
        return Err(invalid("empty pytest_cmd").into());
    };

    let tmp_dir = tempfile::tempdir()?;
    let junit_path = tmp_dir.path().join("junit.xml");

    let mut command = Command::new(program);
    command.current_dir(&spec.root);
    command.env(
        "PYTHONPATH",
        build_pythonpath(&spec.root, std::env::var_os("PYTHONPATH")),
    );
    command.args(base_args);
    command.arg("-q");
    command.arg("--tb=short");
    command.arg("--no-header");
    command.arg("-o");
    command.arg("junit_family=xunit1");
    command.arg(format!("--junit-xml={}", junit_path.display()));

    if let Some(k) = k_expr
        && !k.trim().is_empty()
    {
        command.arg("-k").arg(k);
    }
    if let Some(m) = mark_expr
        && !m.trim().is_empty()
    {
        command.arg("-m").arg(m);
    }
    for arg in extra_args {
        command.arg(arg);
    }
    for target in invocation_targets {
        command.arg(target);
    }

    let timeout = Duration::from_secs(spec.policy.pytest_timeout_secs);
    let output = run_command_with_timeout(&mut command, timeout, 2 * 1024 * 1024)?;
    if output.timed_out {
        return Err(invalid(format!(
            "pytest execution timed out after {}s",
            spec.policy.pytest_timeout_secs
        ))
        .into());
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    let combined_out = if stderr_str.trim().is_empty() {
        stdout_str.to_string()
    } else {
        format!("{}\n{}", stdout_str.trim(), stderr_str.trim())
    };
    let snippet = truncate_str(&combined_out, 8192);

    let xml_content = std::fs::read_to_string(&junit_path).unwrap_or_default();
    let per_file = parse_junit_xml_per_file(&xml_content, dep_hashes);

    let now = now_unix_ms();
    let mut records = Vec::new();
    let overall_success = output.status.success();
    let xml_total_ms: u64 = per_file.values().map(|s| s.duration_ms).sum();
    let overhead_per_file = output.duration_ms.saturating_sub(xml_total_ms)
        / (dep_hashes.len().max(1) as u64);

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
            });
        } else {
            // Fallback when JUnit XML had no testcase entry for this file (e.g., collection error or single file)
            let per_file_dur = (output.duration_ms / (dep_hashes.len().max(1) as u64)).max(1);
            let outcome = if overall_success {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };
            records.push(CachedTestRecord {
                test_file: test_file.clone(),
                dep_hash: dep_hash.clone(),
                outcome,
                passed: usize::from(overall_success),
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
            });
        }
    }

    Ok((records, snippet))
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
        let Some(rel_tag_end) = xml[start..].find('>') else {
            break;
        };
        let tag_end = start + rel_tag_end;
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
            entry.failed_nodeids.push(nodeid);
        } else if body.contains("<error") {
            entry.errors += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.failed_nodeids.push(nodeid);
        } else if body.contains("<skipped") {
            entry.skipped += 1;
        } else {
            entry.passed += 1;
        }
    }

    stats
}

fn extract_xml_attr(tag: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{attr_name}=\"");
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].replace('\\', "/"))
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
