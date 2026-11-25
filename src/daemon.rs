use crate::catalog;
use crate::registration;
use crate::runner::{ManagedRoot, TestRunRequest};
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, IoSlice, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MAX_WIRE: usize = 4 * 1024 * 1024;
const MAX_ONESHOT_WIRE: usize = 512 * 1024;
pub const BINARY_MAGIC: &[u8] = b"TCTXB1\n";
pub const FRAMED_JSON_MAGIC: &[u8] = b"TCTXJ1\n";
pub const TRACE_ARGUMENT: &str = "_tctx_trace";
pub const MAX_ACTIVE_CONNECTIONS: usize = 64;
pub const MAX_CONNECTION_WORKERS: usize = 8;
pub const PERSISTENT_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CLIENT_READ_TIMEOUT_SECS: u64 = 600;
const SYSTEMCTL_START_COOLDOWN: Duration = Duration::from_secs(15);
pub const MAX_RESIDENT_ROOTS: usize = 8;
pub const MAX_DAEMON_RSS_BYTES: u64 = 1024 * 1024 * 1024;
pub const IDLE_ROOT_TIMEOUT: Duration = Duration::from_secs(1_800);
pub const LARGE_ROOT_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
pub const LARGE_ROOT_FILE_THRESHOLD: usize = 5_000;
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn configured_max_daemon_rss_bytes() -> u64 {
    std::env::var("TCTX_MAX_RSS_MB")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|mb| (128..=16_384).contains(mb))
        .and_then(|mb| mb.checked_mul(1024 * 1024))
        .unwrap_or(MAX_DAEMON_RSS_BYTES)
}

fn configured_max_resident_roots() -> usize {
    std::env::var("TCTX_MAX_RESIDENT_ROOTS")
        .ok()
        .and_then(|val| val.trim().parse::<usize>().ok())
        .filter(|n| (1..=64).contains(n))
        .unwrap_or(MAX_RESIDENT_ROOTS)
}

fn configured_idle_root_timeout() -> Duration {
    std::env::var("TCTX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|secs| (60..=86_400).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or(IDLE_ROOT_TIMEOUT)
}

fn configured_large_root_idle_timeout() -> Duration {
    std::env::var("TCTX_LARGE_ROOT_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|secs| (60..=86_400).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or(LARGE_ROOT_IDLE_TIMEOUT)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn trim_allocator() -> bool {
    unsafe { libc::malloc_trim(0) != 0 }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn trim_allocator() -> bool {
    false
}

fn daemon_cgroup_memory_bytes() -> Option<u64> {
    let cgroup_info = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in cgroup_info.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            let trimmed = rel.trim().trim_start_matches('/');
            // Only consult cgroup memory when running inside a dedicated test-context service cgroup;
            // shared container/WSL root cgroups include unrelated processes and host page cache.
            if trimmed.is_empty() || !trimmed.contains("test-context") {
                return None;
            }
            let cg_dir = PathBuf::from("/sys/fs/cgroup").join(trimmed);
            let current = std::fs::read_to_string(cg_dir.join("memory.current"))
                .ok()
                .and_then(|raw| raw.trim().parse::<u64>().ok())?;
            // Subtract reclaimable inactive_file page cache when memory.stat is available
            let inactive_file = std::fs::read_to_string(cg_dir.join("memory.stat"))
                .ok()
                .and_then(|stat| {
                    stat.lines()
                        .find(|l| l.starts_with("inactive_file "))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0);
            return Some(current.saturating_sub(inactive_file));
        }
    }
    None
}

fn daemon_rss_bytes() -> Option<u64> {
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmRSS:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()?
                .checked_mul(1024)
        });
    match (daemon_cgroup_memory_bytes(), rss) {
        (Some(cg), Some(proc_rss)) => Some(cg.max(proc_rss)),
        (Some(cg), None) => Some(cg),
        (None, proc_rss) => proc_rss,
    }
}

fn configured_client_read_timeout() -> Duration {
    std::env::var("TCTX_CLIENT_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|secs| (5..=3_600).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_CLIENT_READ_TIMEOUT_SECS))
}

fn root_idle_timeout(indexed_files: usize) -> Duration {
    if indexed_files >= LARGE_ROOT_FILE_THRESHOLD {
        configured_large_root_idle_timeout()
    } else {
        configured_idle_root_timeout()
    }
}

fn connection_worker_count(available_parallelism: usize) -> usize {
    available_parallelism.clamp(4, MAX_CONNECTION_WORKERS)
}

fn persistent_connection_expired(last_activity: Instant) -> bool {
    last_activity.elapsed() >= PERSISTENT_CONNECTION_IDLE_TIMEOUT
}

extern "C" fn request_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PersistentProtocol {
    Binary,
    FramedJson,
}

struct IdleConnection {
    stream: UnixStream,
    buf: Vec<u8>,
    /// `Some(proto)` for an established persistent connection; `None` for a newly accepted
    /// connection still awaiting its initial protocol header or one-shot request line.
    protocol: Option<PersistentProtocol>,
    last_activity: Instant,
    partial_since: Option<Instant>,
    _guard: ActiveConnection,
}

enum ConnectionWork {
    New {
        stream: UnixStream,
        guard: ActiveConnection,
    },
    PartialNew {
        stream: UnixStream,
        buf: Vec<u8>,
        deadline: Instant,
        guard: ActiveConnection,
    },
    ReadyPersistent(IdleConnection),
    DeferredPersistent {
        conn: IdleConnection,
        payload: Vec<u8>,
    },
    DeferredOneShot {
        stream: UnixStream,
        req: Request,
        _guard: ActiveConnection,
    },
}

fn is_fast_control_request(req: &Request) -> bool {
    match req.name.as_str() {
        "ping" | "test_cancel" | "test_workspaces" | "shutdown" => true,
        // `test_status(load=true)` can trigger a full cold workspace load; never allow it to
        // occupy the fast control overflow lane and starve `test_cancel`.
        "test_status" => !req
            .arguments
            .get("load")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

struct RuntimeIdentity {
    paths: [PathBuf; 3],
    identities: [(u64, u64); 3],
}

impl RuntimeIdentity {
    fn capture(dir: &Path, sock: &Path, lock: &std::fs::File) -> Result<Self> {
        let paths = [
            dir.to_path_buf(),
            sock.to_path_buf(),
            dir.join("daemon.lock"),
        ];
        let dir_meta = std::fs::symlink_metadata(&paths[0])?;
        let sock_meta = std::fs::symlink_metadata(&paths[1])?;
        let lock_meta = lock.metadata()?;
        let identity = Self {
            paths,
            identities: [
                (dir_meta.dev(), dir_meta.ino()),
                (sock_meta.dev(), sock_meta.ino()),
                (lock_meta.dev(), lock_meta.ino()),
            ],
        };
        identity.check()?;
        Ok(identity)
    }

    fn check(&self) -> Result<()> {
        let euid = unsafe { libc::geteuid() };
        for (idx, path) in self.paths.iter().enumerate() {
            let meta = std::fs::symlink_metadata(path).map_err(|e| {
                invalid(format!(
                    "RUNTIME_UNAVAILABLE path={}: {e}; daemon stopping",
                    path.display()
                ))
            })?;
            let valid_kind = match idx {
                0 => meta.is_dir() && meta.mode() & 0o077 == 0,
                1 => meta.file_type().is_socket() && meta.mode() & 0o777 == 0o600,
                _ => meta.is_file() && meta.mode() & 0o077 == 0,
            };
            if !valid_kind
                || meta.file_type().is_symlink()
                || meta.uid() != euid
                || (meta.dev(), meta.ino()) != self.identities[idx]
            {
                return Err(invalid(format!(
                    "RUNTIME_REPLACED_OR_UNSAFE path={}; daemon stopping",
                    path.display()
                ))
                .into());
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ApplicationError(pub String);

impl std::fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ApplicationError {}

pub fn is_application_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ApplicationError>().is_some()
}

#[derive(Serialize, Deserialize)]
struct BinaryRequest {
    root: Option<PathBuf>,
    name: String,
    arguments_json: Vec<u8>,
    compact: bool,
}

#[derive(Serialize)]
struct BinaryRequestRef<'a> {
    root: Option<&'a Path>,
    name: &'a str,
    arguments_json: &'a [u8],
    compact: bool,
}

#[derive(Serialize, Deserialize)]
struct BinaryResponse {
    ok: bool,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: Option<PathBuf>,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub compact: bool,
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unexpected panic".to_string()
    }
}

struct DaemonMetrics {
    total_requests: u64,
    error_requests: u64,
    test_runs: u64,
    total_saved_ms: u64,
    total_cached_files: u64,
    total_executed_files: u64,
}

impl DaemonMetrics {
    fn to_json(&self) -> Value {
        json!({
            "total_requests": self.total_requests,
            "error_requests": self.error_requests,
            "test_runs": self.test_runs,
            "total_saved_ms": self.total_saved_ms,
            "total_cached_files": self.total_cached_files,
            "total_executed_files": self.total_executed_files,
        })
    }
}

struct MetricsWriter {
    path: PathBuf,
    file: Option<std::fs::File>,
    written_bytes: u64,
}

impl MetricsWriter {
    fn open_for(dir: &Path) -> Option<Self> {
        let path = dir.join("metrics.jsonl");
        let mut writer = Self {
            path,
            file: None,
            written_bytes: 0,
        };
        writer.ensure_open(dir)?;
        Some(writer)
    }

    fn ensure_open(&mut self, dir: &Path) -> Option<()> {
        if self.file.is_some() && self.written_bytes < 2 * 1024 * 1024 {
            return Some(());
        }
        self.file = None;
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_symlink() {
                return None;
            }
            if meta.len() >= 2 * 1024 * 1024 {
                let _ = std::fs::rename(&self.path, dir.join("metrics.jsonl.1"));
                self.written_bytes = 0;
            } else {
                self.written_bytes = meta.len();
            }
        } else {
            self.written_bytes = 0;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
            .ok()?;
        self.file = Some(file);
        Some(())
    }
}

fn append_metrics_jsonl(entry: &Value) {
    static METRICS_WRITER: OnceLock<Mutex<Option<MetricsWriter>>> = OnceLock::new();
    let Ok(dir) = catalog::state_dir() else {
        return;
    };
    let expected_path = dir.join("metrics.jsonl");
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    let slot = METRICS_WRITER.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let needs_reinit = guard
        .as_ref()
        .is_none_or(|writer| writer.path != expected_path);
    if needs_reinit {
        *guard = MetricsWriter::open_for(&dir);
    }
    if let Some(writer) = guard.as_mut() {
        if writer.ensure_open(&dir).is_none() {
            return;
        }
        if let Some(file) = writer.file.as_mut() {
            if file.write_all(&line).is_ok() {
                writer.written_bytes = writer.written_bytes.saturating_add(line.len() as u64);
            } else {
                writer.file = None;
            }
        }
    }
}

struct DaemonHub {
    roots: Mutex<BTreeMap<PathBuf, Arc<ManagedRoot>>>,
    load_gate: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    pending_cancels: Mutex<std::collections::BTreeSet<(PathBuf, String)>>,
    in_flight_loads: AtomicUsize,
    traces: Mutex<VecDeque<Value>>,
    metrics: Mutex<DaemonMetrics>,
    started_at: Instant,
    local_oneshot: bool,
    shutting_down: AtomicBool,
}

struct InFlightLoadGuard<'a>(&'a AtomicUsize);

impl Drop for InFlightLoadGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl DaemonHub {
    fn new() -> Self {
        Self::with_mode(false)
    }

    fn with_mode(local_oneshot: bool) -> Self {
        Self {
            roots: Mutex::new(BTreeMap::new()),
            load_gate: Mutex::new(BTreeMap::new()),
            pending_cancels: Mutex::new(std::collections::BTreeSet::new()),
            in_flight_loads: AtomicUsize::new(0),
            traces: Mutex::new(VecDeque::new()),
            metrics: Mutex::new(DaemonMetrics {
                total_requests: 0,
                error_requests: 0,
                test_runs: 0,
                total_saved_ms: 0,
                total_cached_files: 0,
                total_executed_files: 0,
            }),
            started_at: Instant::now(),
            local_oneshot,
            shutting_down: AtomicBool::new(false),
        }
    }

    fn lock_roots(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<ManagedRoot>>> {
        self.roots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn root_load_gate(&self, root: &Path) -> Arc<Mutex<()>> {
        let mut gates = self
            .load_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            gates
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    fn lock_traces(&self) -> std::sync::MutexGuard<'_, VecDeque<Value>> {
        self.traces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
        for root in roots {
            root.cancel_active();
        }
    }

    fn shutdown_all(&self) {
        let roots = {
            let mut guard = self.lock_roots();
            std::mem::take(&mut *guard)
                .into_values()
                .collect::<Vec<_>>()
        };
        for root in roots {
            root.shutdown();
        }
    }

    fn gc_load_gates(&self) {
        let mut gates = self
            .load_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    }

    fn collect_eviction_candidates(&self) -> Vec<(PathBuf, bool, Duration, usize)> {
        let snapshot: Vec<(PathBuf, Arc<ManagedRoot>)> = self
            .lock_roots()
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        snapshot
            .into_iter()
            .map(|(path, root)| {
                let evictable = root.is_evictable();
                let idle = root.last_activity_elapsed();
                let files = root.indexed_file_count();
                (path, evictable, idle, files)
            })
            .collect()
    }

    fn try_evict_key_if_still_evictable(
        guard: &mut BTreeMap<PathBuf, Arc<ManagedRoot>>,
        key: &Path,
    ) -> Option<Arc<ManagedRoot>> {
        if guard.get(key).is_some_and(|root| root.is_evictable()) {
            guard.remove(key)
        } else {
            None
        }
    }

    fn evict_excess_roots_for_admission(&self, reserved_slots: usize) {
        let max_resident = configured_max_resident_roots();
        let max_mem = configured_max_daemon_rss_bytes();
        let current_mem = daemon_rss_bytes();
        let mut candidates = self.collect_eviction_candidates();
        let mut evicted = Vec::new();
        {
            let mut guard = self.lock_roots();
            while guard.len().saturating_add(reserved_slots) > max_resident {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                let Some(idx) = oldest_idx else {
                    break;
                };
                let (key, _, _, _) = candidates.swap_remove(idx);
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                    evicted.push(old);
                }
            }
            if guard.len() > 1 && current_mem.is_some_and(|rss| rss > max_mem) {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                if let Some(idx) = oldest_idx {
                    let (key, _, _, _) = candidates.swap_remove(idx);
                    if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                        evicted.push(old);
                    }
                }
            }
        }
        let evicted_count = evicted.len();
        for root in evicted {
            root.shutdown();
            drop(root);
        }
        if evicted_count > 0 {
            self.gc_load_gates();
            if !self.local_oneshot {
                eprintln!("[tctx-daemon] event=roots_evicted count={evicted_count}");
            }
            trim_allocator();
        }
    }

    fn evict_idle_or_excess_roots(&self) {
        let mut evicted = Vec::new();
        let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
        for root in roots {
            root.reclaim_idle_zygote();
            root.flush_pending_cache();
        }
        let mut candidates = self.collect_eviction_candidates();
        let max_resident = configured_max_resident_roots();
        let max_mem = configured_max_daemon_rss_bytes();
        let mut current_mem = daemon_rss_bytes();
        {
            let mut guard = self.lock_roots();
            let idle_keys: Vec<PathBuf> = candidates
                .iter()
                .filter(|(_, evictable, idle, files)| *evictable && *idle >= root_idle_timeout(*files))
                .map(|(path, _, _, _)| path.clone())
                .collect();
            for k in &idle_keys {
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, k) {
                    evicted.push(old);
                }
            }
            candidates.retain(|(path, _, _, _)| !idle_keys.contains(path));

            while guard.len() > max_resident {
                let oldest_idx = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                    .max_by_key(|(_, (_, _, idle, _))| *idle)
                    .map(|(idx, _)| idx);
                let Some(idx) = oldest_idx else {
                    break;
                };
                let (key, _, _, _) = candidates.swap_remove(idx);
                if let Some(old) = Self::try_evict_key_if_still_evictable(&mut guard, &key) {
                    evicted.push(old);
                }
            }
        }
        let idle_evicted_count = evicted.len();
        for root in evicted.drain(..) {
            root.shutdown();
            drop(root);
        }
        if idle_evicted_count > 0 {
            trim_allocator();
            current_mem = daemon_rss_bytes();
        }
        // Under sustained memory pressure, evict additional idle roots until under budget
        let mut pressure_evicted = 0usize;
        while current_mem.is_some_and(|rss| rss > max_mem) {
            let next_old = {
                let mut guard = self.lock_roots();
                if guard.len() <= 1 {
                    None
                } else {
                    let oldest_idx = candidates
                        .iter()
                        .enumerate()
                        .filter(|(_, (path, evictable, _, _))| *evictable && guard.contains_key(path))
                        .max_by_key(|(_, (_, _, idle, _))| *idle)
                        .map(|(idx, _)| idx);
                    oldest_idx.and_then(|idx| {
                        let (key, _, _, _) = candidates.swap_remove(idx);
                        Self::try_evict_key_if_still_evictable(&mut guard, &key)
                    })
                }
            };
            let Some(old) = next_old else {
                break;
            };
            old.shutdown();
            drop(old);
            pressure_evicted += 1;
            trim_allocator();
            current_mem = daemon_rss_bytes();
        }
        if pressure_evicted > 0 && !self.local_oneshot {
            eprintln!("[tctx-daemon] event=roots_evicted_pressure count={pressure_evicted}");
        }
        self.gc_load_gates();
        let high_rss = current_mem.is_some_and(|rss| rss > max_mem / 2);
        if high_rss {
            let roots = self.lock_roots().values().cloned().collect::<Vec<_>>();
            for root in roots {
                root.reclaim_idle_zygote();
            }
            trim_allocator();
        }
    }

    fn get_or_load_root(&self, requested_root: &Path) -> Result<crate::runner::RootLease> {
        let canonical = requested_root.canonicalize()?;
        // Resolve the most-specific registered root first (from zero-clone Arc cache)
        // so a loaded parent repo never shadows a registered nested child repo.
        let specs = catalog::load_arc()?;
        if let Some(best_registered) = specs
            .iter()
            .filter(|s| canonical.starts_with(&s.root))
            .max_by_key(|s| s.root.components().count())
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&best_registered.root) {
                return Ok(existing.acquire_lease());
            }
        }
        let mut spec = registration::prepare_or_resolve_spec_canonical(&canonical)?;
        if self.local_oneshot {
            spec.policy.background_enabled = false;
        }
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(existing.acquire_lease());
            }
        }
        // Serialize cold loads per root so concurrent callers for the same root do not duplicate work,
        // while allowing independent workspaces to load concurrently without head-of-line blocking.
        let root_gate = self.root_load_gate(&spec.root);
        let _load_guard = root_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let guard = self.lock_roots();
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(existing.acquire_lease());
            }
        }
        let reserved_slots = self.in_flight_loads.fetch_add(1, Ordering::AcqRel) + 1;
        let _in_flight_guard = InFlightLoadGuard(&self.in_flight_loads);
        if !self.local_oneshot {
            self.evict_excess_roots_for_admission(reserved_slots);
        }
        let load_started = Instant::now();
        let spec_for_load = spec.clone();
        let loaded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            ManagedRoot::load(spec_for_load)
        })) {
            Ok(res) => res?,
            Err(payload) => {
                return Err(invalid(format!(
                    "failed to load workspace root {} (internal panic: {})",
                    spec.root.display(),
                    panic_payload_message(payload.as_ref())
                ))
                .into());
            }
        };
        if !self.local_oneshot {
            eprintln!(
                "[tctx-daemon] event=root_loaded root={} files={} load_ms={:.1}",
                spec.root.display(),
                loaded.indexed_file_count(),
                load_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        let lease = loaded.acquire_lease();
        let mut guard = self.lock_roots();
        // Atomically hand off any pending cold-root cancellations while holding `roots` lock
        // so `test_cancel` can never miss a root being published concurrently.
        let pending_ids: Vec<String> = {
            let mut pending = self
                .pending_cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let matched: Vec<(PathBuf, String)> = pending
                .iter()
                .filter(|(r, _)| r == &spec.root || r.starts_with(&spec.root))
                .cloned()
                .collect();
            let mut ids = Vec::with_capacity(matched.len());
            for key in matched {
                pending.remove(&key);
                ids.push(key.1);
            }
            ids
        };
        for cid in pending_ids {
            loaded.cancel_active_scoped(Some(&cid));
        }
        if let Some(old) = guard.insert(spec.root, Arc::clone(&loaded)) {
            old.shutdown();
        }
        Ok(lease)
    }

    fn loaded_root(&self, requested_root: &Path) -> Result<Option<crate::runner::RootLease>> {
        let requested = requested_root.canonicalize()?;
        let guard = self.lock_roots();
        Ok(guard
            .iter()
            .filter(|(root, _)| requested.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, managed)| managed.acquire_lease()))
    }

    fn record_trace(&self, mut trace: Value) {
        if let Some(obj) = trace.as_object_mut() {
            obj.remove("arguments");
            obj.remove("value");
        }
        let mut guard = self.lock_traces();
        if guard.len() >= 64 {
            guard.pop_front();
        }
        guard.push_back(trace);
    }

    fn capabilities() -> Value {
        json!({
            "schema": 1,
            "test_run": true,
            "test_affected": true,
            "test_graph": true,
            "test_status": true,
            "test_sync": true,
            "test_invalidate": true,
            "test_cancel": true,
            "test_ruff": true,
            "test_explain": true,
            "test_cache_export": true,
            "test_cache_import": true,
            "authorized_worktrees": true,
            "request_tracing": true,
            "trace_clock": "unix_ms",
            "client_trace_outcomes": true,
        })
    }

    fn execute(&self, mut req: Request) -> std::result::Result<Value, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("daemon is shutting down".to_string());
        }
        let started = Instant::now();
        let trace_id = extract_trace_id(&mut req.arguments);
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_inner(&req)
        })) {
            Ok(res) => res,
            Err(payload) => Err(invalid(format!(
                "internal daemon panic: {}",
                panic_payload_message(payload.as_ref())
            ))
            .into()),
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let now_ms = crate::cache::now_unix_ms();

        match outcome {
            Ok(value) => {
                if !self.local_oneshot {
                    let mut m = self
                        .metrics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    m.total_requests = m.total_requests.saturating_add(1);
                    if req.name == "test_run" {
                        m.test_runs = m.test_runs.saturating_add(1);
                        let saved = value["saved_ms"].as_u64().unwrap_or(0);
                        let cached = value["cached_files_count"].as_u64().unwrap_or(0);
                        let executed = value["executed_files_count"].as_u64().unwrap_or(0);
                        m.total_saved_ms = m.total_saved_ms.saturating_add(saved);
                        m.total_cached_files = m.total_cached_files.saturating_add(cached);
                        m.total_executed_files = m.total_executed_files.saturating_add(executed);
                        drop(m);
                        let status_s = value["status"].as_str().unwrap_or("unknown");
                        let mode_s = value["mode"].as_str().unwrap_or("unknown");
                        eprintln!(
                            "[tctx-daemon] op=test_run status={status_s} mode={mode_s} cached={cached} executed={executed} saved_ms={saved} duration_ms={elapsed_ms:.1} root={}",
                            req.root
                                .as_deref()
                                .map_or_else(|| "-".to_string(), |p| p.display().to_string())
                        );
                        append_metrics_jsonl(&json!({
                            "unix_ms": now_ms,
                            "op": "test_run",
                            "root": req.root,
                            "status": status_s,
                            "mode": mode_s,
                            "cached_files": cached,
                            "executed_files": executed,
                            "saved_ms": saved,
                            "duration_ms": elapsed_ms,
                        }));
                    } else {
                        drop(m);
                        if matches!(
                            req.name.as_str(),
                            "test_affected" | "test_graph" | "test_sync" | "test_invalidate" | "test_ruff"
                        ) {
                            append_metrics_jsonl(&json!({
                                "unix_ms": now_ms,
                                "op": req.name,
                                "root": req.root,
                                "ok": true,
                                "duration_ms": elapsed_ms,
                            }));
                        }
                        if elapsed_ms >= 200.0 && req.name != "test_trace" {
                            eprintln!(
                                "[tctx-daemon] op={} outcome=slow duration_ms={elapsed_ms:.1} root={}",
                                req.name,
                                req.root
                                    .as_deref()
                                    .map_or_else(|| "-".to_string(), |p| p.display().to_string())
                            );
                        }
                    }
                }
                if let Some(tid) = trace_id {
                    self.record_trace(json!({
                        "trace_id": tid,
                        "operation": req.name,
                        "root": req.root,
                        "execute_ms": elapsed_ms,
                        "daemon_unix_ms": now_ms,
                        "outcome": "success",
                    }));
                    Ok(json!({
                        "_tctx_trace": {
                            "trace_id": tid,
                            "execute_ms": elapsed_ms,
                            "daemon_unix_ms": now_ms,
                        },
                        "value": value,
                    }))
                } else {
                    Ok(value)
                }
            }
            Err(err) => {
                let err_msg = err.to_string();
                if !self.local_oneshot {
                    let mut m = self
                        .metrics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    m.total_requests = m.total_requests.saturating_add(1);
                    m.error_requests = m.error_requests.saturating_add(1);
                    drop(m);
                    eprintln!(
                        "[tctx-daemon] op={} outcome=error duration_ms={elapsed_ms:.1} root={} err={}",
                        req.name,
                        req.root
                            .as_deref()
                            .map_or_else(|| "-".to_string(), |p| p.display().to_string()),
                        err_msg.lines().next().unwrap_or(&err_msg)
                    );
                    append_metrics_jsonl(&json!({
                        "unix_ms": now_ms,
                        "op": req.name,
                        "root": req.root,
                        "ok": false,
                        "duration_ms": elapsed_ms,
                        "error": err_msg.lines().next().unwrap_or(&err_msg),
                    }));
                }
                Err(err_msg)
            }
        }
    }

    fn execute_inner(&self, req: &Request) -> Result<Value> {
        match req.name.as_str() {
            "ping" => Ok(json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "uptime_secs": self.started_at.elapsed().as_secs(),
                "capabilities": Self::capabilities(),
            })),
            "test_workspaces" => {
                let specs = catalog::load_arc()?;
                Ok(serde_json::to_value(specs.as_ref())?)
            }
            "test_trace" => {
                self.record_trace(req.arguments.clone());
                Ok(json!({"recorded": true}))
            }
            "test_status" => {
                let root_status = if let Some(root_path) = &req.root {
                    if let Some(managed) = self.loaded_root(root_path)? {
                        Some(managed.status())
                    } else if req
                        .arguments
                        .get("load")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Some(self.get_or_load_root(root_path)?.status())
                    } else {
                        let spec = catalog::resolve_spec(root_path)?;
                        Some(json!({
                            "root": spec.root,
                            "loaded": false,
                            "pytest_cmd": spec.effective_pytest_cmd(),
                            "state": "registered_unloaded",
                        }))
                    }
                } else {
                    None
                };
                let registered = catalog::load_arc()?;
                let loaded_roots: Vec<PathBuf> = self.lock_roots().keys().cloned().collect();
                let metrics = self
                    .metrics
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .to_json();
                Ok(json!({
                    "daemon": {
                        "ok": true,
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                        "uptime_secs": self.started_at.elapsed().as_secs(),
                        "registered_roots": registered.len(),
                        "loaded_roots": loaded_roots,
                        "metrics": metrics,
                    },
                    "capabilities": Self::capabilities(),
                    "workspace": root_status,
                }))
            }
            "test_sync" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_sync requires a workspace root"))?;
                let wait = req
                    .arguments
                    .get("wait")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let managed = self.get_or_load_root(root_path)?;
                managed.sync(wait)
            }
            "test_invalidate" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_invalidate requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let managed = self.get_or_load_root(root_path)?;
                managed.invalidate(&files)
            }
            "test_cancel" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cancel requires a workspace root"))?;
                let cancel_id = req.arguments.get("cancel_id").and_then(Value::as_str);
                let spec_root = catalog::resolve_spec(root_path).map(|s| s.root).ok();
                let canonical = std::fs::canonicalize(root_path).unwrap_or_else(|_| PathBuf::from(root_path));
                // Check `roots` and record `pending_cancels` atomically under `roots` lock so a
                // concurrently completing `get_or_load_root` always observes the cancellation.
                let loaded_opt = {
                    let guard = self.lock_roots();
                    let matched = spec_root
                        .as_ref()
                        .and_then(|sr| guard.get(sr).cloned())
                        .or_else(|| guard.get(&canonical).cloned())
                        .or_else(|| {
                            guard
                                .iter()
                                .filter(|(root, _)| canonical.starts_with(root))
                                .max_by_key(|(root, _)| root.components().count())
                                .map(|(_, m)| Arc::clone(m))
                        });
                    if matched.is_none()
                        && let Some(cid) = cancel_id
                    {
                        let key_root = spec_root.clone().unwrap_or_else(|| canonical.clone());
                        let mut pending = self
                            .pending_cancels
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        if pending.len() >= 256 {
                            pending.clear();
                        }
                        pending.insert((key_root, cid.to_string()));
                    }
                    matched
                };
                let cancelled = loaded_opt.is_some_and(|managed| managed.cancel_active_scoped(cancel_id));
                Ok(json!({"cancelled": cancelled}))
            }
            "test_graph" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_graph requires a workspace root"))?;
                let file = req
                    .arguments
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("test_graph requires 'file' argument"))?;
                let managed = self.get_or_load_root(root_path)?;
                let info = managed.graph_info(file)?;
                Ok(serde_json::to_value(info)?)
            }
            "test_affected" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_affected requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let git_diff = req
                    .arguments
                    .get("git_diff")
                    .and_then(Value::as_bool)
                    .unwrap_or(files.is_empty());
                let managed = self.get_or_load_root(root_path)?;
                managed.affected(&files, git_diff)
            }
            "test_run" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_run requires a workspace root"))?;
                let mut args = req.arguments.clone();
                let caller_env: Option<BTreeMap<String, String>> = args
                    .as_object_mut()
                    .and_then(|obj| {
                        obj.remove("root");
                        obj.remove("_tctx_caller_env")
                    })
                    .and_then(|v| serde_json::from_value(v).ok());
                let run_req: TestRunRequest = if args.is_null() {
                    TestRunRequest::default()
                } else {
                    serde_json::from_value(args)?
                };
                if !self.local_oneshot && run_req.env_partition.is_some() {
                    return Err(invalid(
                        "CALLER_ENV_MISMATCH: env_partition is only valid in local isolated execution",
                    )
                    .into());
                }
                if !self.local_oneshot
                    && let Some(ref cenv) = caller_env
                {
                    let daemon_env = capture_caller_env_for_daemon();
                    // Compare the COMPLETE captured CLI environment against the daemon's environment
                    // so even unindexed plugin variables (e.g. `DJANGO_SETTINGS_MODULE`, custom
                    // pytest plugin config vars) isolate cleanly to the caller's process!
                    if cenv != &daemon_env {
                        return Err(invalid(
                            "CALLER_ENV_MISMATCH: caller environment differs from daemon environment",
                        )
                        .into());
                    }
                }
                let canon_req = std::fs::canonicalize(root_path)
                    .unwrap_or_else(|_| PathBuf::from(root_path));
                let canon_root = catalog::resolve_spec(root_path).map(|s| s.root).ok();
                if let Some(cid) = run_req.cancel_id.as_deref() {
                    let mut pending = self
                        .pending_cancels
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let already = pending.remove(&(canon_req.clone(), cid.to_string()))
                        || canon_root
                            .as_ref()
                            .is_some_and(|c| pending.remove(&(c.clone(), cid.to_string())));
                    if already {
                        return Err(invalid(
                            "TEST_CANCELLED: test_run cancelled while workspace was loading",
                        )
                        .into());
                    }
                }
                let managed = self.get_or_load_root(root_path)?;
                if let Some(cid) = run_req.cancel_id.as_deref() {
                    let mut pending = self
                        .pending_cancels
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let already = pending.remove(&(canon_req, cid.to_string()))
                        || canon_root
                            .as_ref()
                            .is_some_and(|c| pending.remove(&(c.clone(), cid.to_string())));
                    if already {
                        return Err(invalid(
                            "TEST_CANCELLED: test_run cancelled while workspace was loading",
                        )
                        .into());
                    }
                }
                let response = managed.run_tests(&run_req)?;
                Ok(serde_json::to_value(response)?)
            }
            "test_ruff" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_ruff requires a workspace root"))?;
                let files: Vec<String> = req
                    .arguments
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let fix = req
                    .arguments
                    .get("fix")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let managed = self.get_or_load_root(root_path)?;
                managed.check_ruff(&files, fix)
            }
            "test_explain" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_explain requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.explain_failures()
            }
            "test_cache_export" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cache_export requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.export_cache_bundle()
            }
            "test_cache_import" => {
                let root_path = req
                    .root
                    .as_deref()
                    .ok_or_else(|| invalid("test_cache_import requires a workspace root"))?;
                let managed = self.get_or_load_root(root_path)?;
                managed.import_cache_bundle(&req.arguments)
            }
            other => Err(invalid(format!("unknown daemon operation: {other}")).into()),
        }
    }
}

fn extract_trace_id(arguments: &mut Value) -> Option<String> {
    let obj = arguments.as_object_mut()?;
    let val = obj.remove(TRACE_ARGUMENT)?;
    let s = val.as_str()?;
    if s.is_empty()
        || s.len() > 96
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return None;
    }
    Some(s.to_owned())
}

pub fn serve() -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::Release);
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_stop as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            request_stop as *const () as libc::sighandler_t,
        );
    }
    let dir = catalog::state_dir()?;
    let lock_path = dir.join("daemon.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    lock_file
        .try_lock_exclusive()
        .map_err(|_| invalid("another tctx daemon is already running"))?;

    let sock_path = catalog::socket_path()?;
    if sock_path.exists() {
        let metadata = std::fs::symlink_metadata(&sock_path)?;
        if !metadata.file_type().is_socket()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(invalid(format!(
                "refusing to replace non-socket or unowned runtime path: {}",
                sock_path.display()
            ))
            .into());
        }
        let _ = std::fs::remove_file(&sock_path);
    }
    let tmp_sock = dir.join("sock.tmp");
    let _ = std::fs::remove_file(&tmp_sock);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with("workspaces.json.tmp.")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let listener = UnixListener::bind(&tmp_sock)?;
    listener.set_nonblocking(true)?;
    std::fs::set_permissions(&tmp_sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp_sock, &sock_path)?;
    let runtime_identity = Arc::new(RuntimeIdentity::capture(&dir, &sock_path, &lock_file)?);

    if let Ok(gc_res) = registration::prune(true) {
        let pruned_count = gc_res["pruned_roots"]
            .as_array()
            .map_or(0, std::vec::Vec::len);
        let removed_cache = gc_res["cache_gc"]["removed_files"].as_u64().unwrap_or(0);
        let reclaimed = gc_res["cache_gc"]["reclaimed_bytes"].as_u64().unwrap_or(0);
        if pruned_count > 0 || removed_cache > 0 {
            eprintln!(
                "[tctx-daemon] event=startup_gc pruned_roots={pruned_count} removed_cache_files={removed_cache} reclaimed_bytes={reclaimed}"
            );
        }
    }

    let hub = Arc::new(DaemonHub::new());
    let active_connections = Arc::new(AtomicUsize::new(0));
    let worker_count = connection_worker_count(
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4),
    );
    eprintln!(
        "[tctx-daemon] status=started version={} pid={} workers={worker_count} sock={}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        sock_path.display()
    );
    let (connection_tx, connection_rx) = sync_channel::<ConnectionWork>(MAX_ACTIVE_CONNECTIONS);
    let connection_rx = Arc::new(Mutex::new(connection_rx));
    let (idle_return_tx, idle_return_rx) =
        std::sync::mpsc::channel::<IdleConnection>();
    let (wake_rx, wake_tx) = UnixStream::pair()?;
    wake_rx.set_nonblocking(true)?;
    wake_tx.set_nonblocking(true)?;
    let wake_tx = Arc::new(Mutex::new(wake_tx));
    let maint_stop = Arc::new((Mutex::new(false), Condvar::new()));

    let busy_workers = Arc::new(AtomicUsize::new(0));
    let overflow_workers = Arc::new(AtomicUsize::new(0));
    const MAX_OVERFLOW_CONTROL_WORKERS: usize = 4;

    let mut connection_workers = Vec::new();
    for index in 0..worker_count {
        let hub_worker = Arc::clone(&hub);
        let rx_worker = Arc::clone(&connection_rx);
        let idle_tx_worker = idle_return_tx.clone();
        let wake_tx_worker = Arc::clone(&wake_tx);
        let busy_worker = Arc::clone(&busy_workers);
        connection_workers.push(
            std::thread::Builder::new()
                .name(format!("tctx-conn-{index}"))
                .spawn(move || {
                    loop {
                        let work = {
                            let receiver = rx_worker
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            receiver.recv()
                        };
                        let Ok(work) = work else {
                            return;
                        };
                        busy_worker.fetch_add(1, Ordering::AcqRel);
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _ = handle_connection_work(
                                work,
                                &hub_worker,
                                &idle_tx_worker,
                                &wake_tx_worker,
                                None,
                            );
                        }));
                        busy_worker.fetch_sub(1, Ordering::AcqRel);
                        notify_poll_wake(&wake_tx_worker);
                    }
                })?,
        );
    }

    // Background maintenance loop for RuntimeIdentity verification and idle root eviction
    let maintenance = {
        let hub_maint = Arc::clone(&hub);
        let id_maint = Arc::clone(&runtime_identity);
        let maint_stop_worker = Arc::clone(&maint_stop);
        let wake_tx_maint = Arc::clone(&wake_tx);
        Some(
            std::thread::Builder::new()
                .name("tctx-maint".to_string())
                .spawn(move || {
                    let mut last_gc_check = Instant::now();
                    let (lock, cvar) = &*maint_stop_worker;
                    let mut stopped = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while !*stopped && !STOP_REQUESTED.load(Ordering::Acquire) {
                        let (guard, _) = cvar
                            .wait_timeout(stopped, Duration::from_secs(5))
                            .unwrap_or_else(|p| p.into_inner());
                        stopped = guard;
                        if *stopped || STOP_REQUESTED.load(Ordering::Acquire) {
                            break;
                        }
                        drop(stopped);
                        if let Err(err) = id_maint.check() {
                            eprintln!("{err}");
                            hub_maint.shutdown_all();
                            STOP_REQUESTED.store(true, Ordering::Release);
                            notify_poll_wake(&wake_tx_maint);
                            return;
                        }
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            hub_maint.evict_idle_or_excess_roots();
                        }));
                        if last_gc_check.elapsed() >= Duration::from_secs(3_600) {
                            if let Ok(gc_res) = registration::prune(true) {
                                let pruned = gc_res["pruned_roots"]
                                    .as_array()
                                    .map_or(0, std::vec::Vec::len);
                                let removed =
                                    gc_res["cache_gc"]["removed_files"].as_u64().unwrap_or(0);
                                if pruned > 0 || removed > 0 {
                                    eprintln!(
                                        "[tctx-daemon] event=periodic_gc pruned_roots={pruned} removed_cache_files={removed}"
                                    );
                                }
                            }
                            last_gc_check = Instant::now();
                        }
                        stopped = lock.lock().unwrap_or_else(|p| p.into_inner());
                    }
                })?,
        )
    };

    let mut idle_conns: Vec<IdleConnection> = Vec::new();
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(MAX_ACTIVE_CONNECTIONS + 2);
    let mut wake_drain_buf = [0u8; 64];

    while !STOP_REQUESTED.load(Ordering::Acquire) {
        // Drain any persistent connections returned by workers before polling
        while let Ok(conn) = idle_return_rx.try_recv() {
            if !persistent_connection_expired(conn.last_activity) {
                idle_conns.push(conn);
            }
        }
        // Close expired idle persistent connections (and stalled partial frames after 5s)
        // without consuming worker threads.
        idle_conns.retain(|conn| {
            if let Some(partial_at) = conn.partial_since
                && partial_at.elapsed() >= PARTIAL_FRAME_TIMEOUT
            {
                return false;
            }
            !persistent_connection_expired(conn.last_activity)
        });

        let overflow_saturated = busy_workers.load(Ordering::Acquire) >= worker_count
            && overflow_workers.load(Ordering::Acquire) >= MAX_OVERFLOW_CONTROL_WORKERS;
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: wake_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        // When both base and overflow workers are saturated, suppress polling parked `idle_conns`
        // so level-triggered readable sockets do not busy-spin `poll()`. Both base and overflow
        // workers signal `wake_rx` on completion, waking the loop immediately when capacity returns.
        if !overflow_saturated {
            for conn in &idle_conns {
                pollfds.push(libc::pollfd {
                    fd: conn.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }

        let poll_timeout_ms = 250;
        let ready = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                poll_timeout_ms,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready == 0 {
            continue;
        }
        if pollfds[1].revents != 0 {
            let mut wake_reader = &wake_rx;
            while wake_reader.read(&mut wake_drain_buf).is_ok_and(|n| n > 0) {}
        }

        // Dispatch readable idle persistent / pending-new connections back to worker pool (or a
        // control-only overflow worker if all base workers are busy so `test_cancel`/`ping` never starve).
        let mut idx = if overflow_saturated { 0 } else { idle_conns.len() };
        while idx > 0 {
            idx -= 1;
            let revents = pollfds[2 + idx].revents;
            if revents == 0 {
                continue;
            }
            if revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                idle_conns.swap_remove(idx);
                continue;
            }
            let all_busy = busy_workers.load(Ordering::Acquire) >= worker_count;
            if all_busy {
                if overflow_workers.fetch_add(1, Ordering::AcqRel)
                    < MAX_OVERFLOW_CONTROL_WORKERS
                {
                    let conn = idle_conns.swap_remove(idx);
                    let work = match conn.protocol {
                        Some(_) => ConnectionWork::ReadyPersistent(conn),
                        None => ConnectionWork::PartialNew {
                            stream: conn.stream,
                            buf: conn.buf,
                            deadline: conn.partial_since.unwrap_or(conn.last_activity)
                                + PARTIAL_FRAME_TIMEOUT,
                            guard: conn._guard,
                        },
                    };
                    let hub_ov = Arc::clone(&hub);
                    let idle_tx_ov = idle_return_tx.clone();
                    let wake_tx_ov = Arc::clone(&wake_tx);
                    let conn_tx_ov = connection_tx.clone();
                    let ov_counter = Arc::clone(&overflow_workers);
                    if std::thread::Builder::new()
                        .name("tctx-conn-fast".to_string())
                        .spawn(move || {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let _ = handle_connection_work(
                                    work,
                                    &hub_ov,
                                    &idle_tx_ov,
                                    &wake_tx_ov,
                                    Some(&conn_tx_ov),
                                );
                            }));
                            ov_counter.fetch_sub(1, Ordering::AcqRel);
                            notify_poll_wake(&wake_tx_ov);
                        })
                        .is_err()
                    {
                        overflow_workers.fetch_sub(1, Ordering::AcqRel);
                    }
                    continue;
                }
                overflow_workers.fetch_sub(1, Ordering::AcqRel);
                // Overflow lane is saturated while base workers are busy: leave unclassified
                // connections in `idle_conns` so they are picked up as soon as an overflow worker
                // completes instead of getting stranded behind long-running base work.
                if idle_conns[idx].partial_since.is_none() {
                    idle_conns[idx].partial_since = Some(Instant::now());
                }
                continue;
            }
            let conn = idle_conns.swap_remove(idx);
            let work = match conn.protocol {
                Some(_) => ConnectionWork::ReadyPersistent(conn),
                None => ConnectionWork::PartialNew {
                    stream: conn.stream,
                    buf: conn.buf,
                    deadline: conn.partial_since.unwrap_or(conn.last_activity)
                        + PARTIAL_FRAME_TIMEOUT,
                    guard: conn._guard,
                },
            };
            if let Err(TrySendError::Full(ConnectionWork::ReadyPersistent(mut overloaded))) =
                connection_tx.try_send(work)
            {
                send_overload_error(&mut overloaded.stream, overloaded.protocol);
            }
        }

        if pollfds[0].revents & libc::POLLIN != 0 {
            loop {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error.into()),
                };
                let current = active_connections.fetch_add(1, Ordering::AcqRel);
                if current >= MAX_ACTIVE_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    // Protocol is not yet negotiated on a newly accepted socket; close cleanly
                    // rather than sending line JSON that binary clients would misparse as a frame length.
                    drop(stream);
                    continue;
                }
                let conn_guard = ActiveConnection(Arc::clone(&active_connections));
                let all_busy = busy_workers.load(Ordering::Acquire) >= worker_count;
                if all_busy {
                    if overflow_workers.fetch_add(1, Ordering::AcqRel)
                        < MAX_OVERFLOW_CONTROL_WORKERS
                    {
                        let work = ConnectionWork::New {
                            stream,
                            guard: conn_guard,
                        };
                        let hub_ov = Arc::clone(&hub);
                        let idle_tx_ov = idle_return_tx.clone();
                        let wake_tx_ov = Arc::clone(&wake_tx);
                        let conn_tx_ov = connection_tx.clone();
                        let ov_counter = Arc::clone(&overflow_workers);
                        if std::thread::Builder::new()
                            .name("tctx-conn-fast".to_string())
                            .spawn(move || {
                                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    let _ = handle_connection_work(
                                        work,
                                        &hub_ov,
                                        &idle_tx_ov,
                                        &wake_tx_ov,
                                        Some(&conn_tx_ov),
                                    );
                                }));
                                ov_counter.fetch_sub(1, Ordering::AcqRel);
                                notify_poll_wake(&wake_tx_ov);
                            })
                            .is_err()
                        {
                            overflow_workers.fetch_sub(1, Ordering::AcqRel);
                        }
                        continue;
                    }
                    overflow_workers.fetch_sub(1, Ordering::AcqRel);
                    // Overflow lane is saturated while base workers are busy: park newly accepted
                    // socket in the nonblocking poll loop instead of stranding a potential
                    // `test_cancel` behind long-running base workers.
                    let now = Instant::now();
                    idle_conns.push(IdleConnection {
                        stream,
                        buf: Vec::with_capacity(256),
                        protocol: None,
                        last_activity: now,
                        partial_since: Some(now),
                        _guard: conn_guard,
                    });
                    continue;
                }
                let work = ConnectionWork::New {
                    stream,
                    guard: conn_guard,
                };
                match connection_tx.try_send(work) {
                    Ok(()) => {}
                    Err(TrySendError::Full(ConnectionWork::New { stream, .. })) => {
                        drop(stream);
                    }
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        }
    }
    idle_conns.clear();
    hub.begin_shutdown();
    {
        let (lock, cvar) = &*maint_stop;
        let mut stopped = lock.lock().unwrap_or_else(|p| p.into_inner());
        *stopped = true;
        cvar.notify_all();
    }
    drop(connection_tx);
    drop(idle_return_tx);
    // Drain any idle connections returned just as shutdown began so their ActiveConnection
    // guards release immediately instead of blocking the shutdown wait for 10 seconds.
    while idle_return_rx.try_recv().is_ok() {}
    drop(idle_return_rx);
    let deadline = Instant::now() + Duration::from_secs(10);
    while active_connections.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if let Some(handle) = maintenance {
        let _ = handle.join();
    }
    for handle in connection_workers {
        let _ = handle.join();
    }
    hub.shutdown_all();
    drop(listener);
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

fn notify_poll_wake(wake_tx: &Mutex<UnixStream>) {
    // 1-byte write on a nonblocking Unix socketpair FD is lock-free and atomic on Unix,
    // avoiding missed wakes when multiple workers finish simultaneously.
    let guard = wake_tx.lock().unwrap_or_else(|p| p.into_inner());
    let fd = guard.as_raw_fd();
    drop(guard);
    let byte: u8 = 1;
    unsafe {
        let _ = libc::write(fd, (&byte as *const u8).cast::<libc::c_void>(), 1);
    }
}

fn send_overload_error(stream: &mut UnixStream, protocol: Option<PersistentProtocol>) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    match protocol {
        Some(PersistentProtocol::Binary) => {
            let reply = BinaryResponse {
                ok: false,
                payload: b"daemon overloaded; retry".to_vec(),
            };
            if let Ok(encoded) = bincode::serialize(&reply) {
                let _ = write_framed(stream, &encoded);
            }
        }
        Some(PersistentProtocol::FramedJson) => {
            let bytes = br#"{"error":"daemon overloaded; retry"}"#;
            let _ = write_framed(stream, bytes);
        }
        None => {
            let _ = stream.write_all(b"{\"error\":\"daemon overloaded; retry\"}\n");
            let _ = stream.flush();
        }
    }
}

enum FrameReadOutcome {
    Complete(Vec<u8>),
    Eof,
    PartialTimeout,
}

fn read_until_len_deadline(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    target_len: usize,
    deadline: Instant,
    soft_timeout: bool,
) -> std::io::Result<Option<bool>> {
    let mut chunk = [0u8; 4096];
    while buf.len() < target_len {
        let now = Instant::now();
        if now >= deadline {
            if soft_timeout {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "partial frame timed out",
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let _ = stream.set_read_timeout(Some(remaining.min(Duration::from_millis(200))));
        let to_read = (target_len - buf.len()).min(chunk.len());
        match stream.read(&mut chunk[..to_read]) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(Some(false));
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated frame",
                ));
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                if STOP_REQUESTED.load(Ordering::Acquire) {
                    return Ok(Some(false));
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(Some(true))
}

fn read_next_frame_bounded(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    timeout: Duration,
    soft_timeout: bool,
) -> Result<FrameReadOutcome> {
    let deadline = Instant::now() + timeout;
    match read_until_len_deadline(stream, buf, 4, deadline, soft_timeout)? {
        None => return Ok(FrameReadOutcome::PartialTimeout),
        Some(false) => return Ok(FrameReadOutcome::Eof),
        Some(true) => {}
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_WIRE {
        return Err(invalid("frame exceeds MAX_WIRE").into());
    }
    match read_until_len_deadline(stream, buf, 4 + len, deadline, soft_timeout)? {
        None => return Ok(FrameReadOutcome::PartialTimeout),
        Some(false) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated frame payload",
            )
            .into());
        }
        Some(true) => {}
    }
    let payload = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    if buf.capacity() > 64 * 1024 && buf.is_empty() {
        buf.shrink_to(4096);
    }
    Ok(FrameReadOutcome::Complete(payload))
}

fn try_pop_buffered_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_WIRE {
        return Err(invalid("frame exceeds MAX_WIRE").into());
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let payload = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    Ok(Some(payload))
}

fn decode_persistent_request(protocol: PersistentProtocol, payload: &[u8]) -> Result<Request> {
    match protocol {
        PersistentProtocol::Binary => {
            let bin_req: BinaryRequest = bincode::deserialize(payload)?;
            let arguments: Value = if bin_req.arguments_json.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bin_req.arguments_json)?
            };
            Ok(Request {
                root: bin_req.root,
                name: bin_req.name,
                arguments,
                compact: bin_req.compact,
            })
        }
        PersistentProtocol::FramedJson => Ok(serde_json::from_slice(payload)?),
    }
}

fn dispatch_persistent_request(
    stream: &mut UnixStream,
    protocol: PersistentProtocol,
    req: Request,
    hub: &DaemonHub,
) -> Result<()> {
    match protocol {
        PersistentProtocol::Binary => {
            let reply = match hub.execute(req) {
                Ok(val) => BinaryResponse {
                    ok: true,
                    payload: serde_json::to_vec(&val)?,
                },
                Err(err) => BinaryResponse {
                    ok: false,
                    payload: err.into_bytes(),
                },
            };
            let encoded = bincode::serialize(&reply)?;
            write_framed(stream, &encoded)?;
        }
        PersistentProtocol::FramedJson => {
            let response_json = match hub.execute(req) {
                Ok(val) => json!({ "result": val }),
                Err(err) => json!({ "error": err }),
            };
            let bytes = serde_json::to_vec(&response_json)?;
            write_framed(stream, &bytes)?;
        }
    }
    Ok(())
}

fn dispatch_persistent_frame(
    stream: &mut UnixStream,
    protocol: PersistentProtocol,
    payload: &[u8],
    hub: &DaemonHub,
) -> Result<()> {
    let req = decode_persistent_request(protocol, payload)?;
    dispatch_persistent_request(stream, protocol, req, hub)
}

#[allow(clippy::too_many_arguments)]
fn handle_new_connection(
    mut stream: UnixStream,
    mut buf: Vec<u8>,
    deadline: Instant,
    guard: ActiveConnection,
    hub: &DaemonHub,
    idle_return_tx: &std::sync::mpsc::Sender<IdleConnection>,
    wake_tx: &Mutex<UnixStream>,
    control_defer_tx: Option<&std::sync::mpsc::SyncSender<ConnectionWork>>,
) -> Result<()> {
    // Overflow control workers use tight read/write timeouts (200ms/500ms) so slow/stalled
    // clients cannot occupy all overflow slots and starve `test_cancel`/`ping`.
    let write_timeout = if control_defer_tx.is_some() {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(60)
    };
    stream.set_write_timeout(Some(write_timeout))?;
    // Overflow control workers use a short 200ms soft read cap so slow/partial connections
    // cannot occupy all overflow slots and starve `test_cancel`/`ping`.
    let soft_deadline = control_defer_tx
        .map(|_| (Instant::now() + Duration::from_millis(200)).min(deadline));
    let active_deadline = soft_deadline.unwrap_or(deadline);
    let mut tmp = [0u8; 4096];
    // Read initial header bytes to classify protocol
    while buf.len() < BINARY_MAGIC.len() && !buf.contains(&b'\n') {
        let now = Instant::now();
        if STOP_REQUESTED.load(Ordering::Acquire) {
            return Ok(());
        }
        if now >= active_deadline {
            if control_defer_tx.is_some() && now < deadline {
                // Return partially-received new connection to the nonblocking poll loop so slow
                // control requests (`test_cancel`) still get the overflow lane once complete.
                let partial_start = deadline
                    .checked_sub(PARTIAL_FRAME_TIMEOUT)
                    .unwrap_or(now);
                let _ = idle_return_tx.send(IdleConnection {
                    stream,
                    buf,
                    protocol: None,
                    last_activity: now,
                    partial_since: Some(partial_start),
                    _guard: guard,
                });
                notify_poll_wake(wake_tx);
            }
            return Ok(());
        }
        let _ = stream.set_read_timeout(Some(
            active_deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(200)),
        ));
        match stream.read(&mut tmp) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if buf.len() + n > MAX_ONESHOT_WIRE + 1 {
                    return Err(invalid("request too large").into());
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if buf.starts_with(BINARY_MAGIC) || buf.starts_with(FRAMED_JSON_MAGIC) {
        let protocol = if buf.starts_with(BINARY_MAGIC) {
            buf.drain(..BINARY_MAGIC.len());
            PersistentProtocol::Binary
        } else {
            buf.drain(..FRAMED_JSON_MAGIC.len());
            PersistentProtocol::FramedJson
        };
        let mut conn = IdleConnection {
            stream,
            buf,
            protocol: Some(protocol),
            last_activity: Instant::now(),
            partial_since: None,
            _guard: guard,
        };
        // If the client already pipelined the first frame, process it immediately
        while let Some(payload) = try_pop_buffered_frame(&mut conn.buf)? {
            if let Some(defer_tx) = control_defer_tx {
                let req = decode_persistent_request(protocol, &payload)?;
                if !is_fast_control_request(&req) {
                    if let Err(TrySendError::Full(ConnectionWork::DeferredPersistent {
                        mut conn,
                        ..
                    })) = defer_tx.try_send(ConnectionWork::DeferredPersistent { conn, payload })
                    {
                        send_overload_error(&mut conn.stream, conn.protocol);
                    }
                    return Ok(());
                }
                dispatch_persistent_request(&mut conn.stream, protocol, req, hub)?;
            } else {
                dispatch_persistent_frame(&mut conn.stream, protocol, &payload, hub)?;
            }
            conn.last_activity = Instant::now();
        }
        conn.partial_since = (!conn.buf.is_empty()).then(Instant::now);
        let _ = idle_return_tx.send(conn);
        notify_poll_wake(wake_tx);
        return Ok(());
    }

    // One-shot JSON line protocol with bounded allocation
    while !buf.contains(&b'\n') {
        let now = Instant::now();
        if STOP_REQUESTED.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "one-shot request timed out",
            )
            .into());
        }
        if now >= active_deadline {
            if control_defer_tx.is_some() && now < deadline {
                let partial_start = deadline
                    .checked_sub(PARTIAL_FRAME_TIMEOUT)
                    .unwrap_or(now);
                let _ = idle_return_tx.send(IdleConnection {
                    stream,
                    buf,
                    protocol: None,
                    last_activity: now,
                    partial_since: Some(partial_start),
                    _guard: guard,
                });
                notify_poll_wake(wake_tx);
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "one-shot request timed out",
            )
            .into());
        }
        let _ = stream.set_read_timeout(Some(
            active_deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(200)),
        ));
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > MAX_ONESHOT_WIRE {
                    return Err(invalid("request too large").into());
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        buf.truncate(nl);
    }
    let req: Request = serde_json::from_slice(&buf)?;
    if let Some(defer_tx) = control_defer_tx
        && !is_fast_control_request(&req)
    {
        if let Err(TrySendError::Full(ConnectionWork::DeferredOneShot {
            mut stream,
            ..
        })) = defer_tx.try_send(ConnectionWork::DeferredOneShot {
            stream,
            req,
            _guard: guard,
        }) {
            send_overload_error(&mut stream, None);
        }
        return Ok(());
    }
    let response_json = match hub.execute(req) {
        Ok(val) => json!({ "result": val }),
        Err(err) => json!({ "error": err }),
    };
    let mut bytes = serde_json::to_vec(&response_json)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn handle_connection_work(
    work: ConnectionWork,
    hub: &DaemonHub,
    idle_return_tx: &std::sync::mpsc::Sender<IdleConnection>,
    wake_tx: &Mutex<UnixStream>,
    control_defer_tx: Option<&std::sync::mpsc::SyncSender<ConnectionWork>>,
) -> Result<()> {
    match work {
        ConnectionWork::New { stream, guard } => handle_new_connection(
            stream,
            Vec::with_capacity(256),
            Instant::now() + PARTIAL_FRAME_TIMEOUT,
            guard,
            hub,
            idle_return_tx,
            wake_tx,
            control_defer_tx,
        ),
        ConnectionWork::PartialNew {
            stream,
            buf,
            deadline,
            guard,
        } => handle_new_connection(
            stream,
            buf,
            deadline,
            guard,
            hub,
            idle_return_tx,
            wake_tx,
            control_defer_tx,
        ),
        ConnectionWork::ReadyPersistent(mut conn) => {
            let Some(protocol) = conn.protocol else {
                return Ok(());
            };
            // Overflow control workers use short read/write timeouts so slow readers or partial
            // frames never occupy all overflow slots and starve `test_cancel`/`ping`.
            let (timeout, soft) = if control_defer_tx.is_some() {
                let _ = conn
                    .stream
                    .set_write_timeout(Some(Duration::from_millis(500)));
                (Duration::from_millis(200), true)
            } else {
                let _ = conn.stream.set_write_timeout(Some(Duration::from_secs(60)));
                (PARTIAL_FRAME_TIMEOUT, false)
            };
            let payload = match read_next_frame_bounded(&mut conn.stream, &mut conn.buf, timeout, soft)? {
                FrameReadOutcome::Complete(p) => p,
                FrameReadOutcome::Eof => return Ok(()),
                FrameReadOutcome::PartialTimeout => {
                    // Track partial-frame arrival time separately from idle `last_activity` so an
                    // idle persistent connection sending a slow frame gets a full 5s partial window.
                    if conn.partial_since.is_none() {
                        conn.partial_since = Some(Instant::now());
                    }
                    conn.last_activity = Instant::now();
                    let _ = idle_return_tx.send(conn);
                    notify_poll_wake(wake_tx);
                    return Ok(());
                }
            };
            if let Some(defer_tx) = control_defer_tx {
                let req = decode_persistent_request(protocol, &payload)?;
                if !is_fast_control_request(&req) {
                    if let Err(TrySendError::Full(ConnectionWork::DeferredPersistent {
                        mut conn,
                        ..
                    })) = defer_tx.try_send(ConnectionWork::DeferredPersistent { conn, payload })
                    {
                        send_overload_error(&mut conn.stream, conn.protocol);
                    }
                    return Ok(());
                }
                dispatch_persistent_request(&mut conn.stream, protocol, req, hub)?;
            } else {
                dispatch_persistent_frame(&mut conn.stream, protocol, &payload, hub)?;
            }
            conn.last_activity = Instant::now();
            while let Some(next_payload) = try_pop_buffered_frame(&mut conn.buf)? {
                if let Some(defer_tx) = control_defer_tx {
                    let req = decode_persistent_request(protocol, &next_payload)?;
                    if !is_fast_control_request(&req) {
                        if let Err(TrySendError::Full(ConnectionWork::DeferredPersistent {
                            mut conn,
                            ..
                        })) = defer_tx.try_send(ConnectionWork::DeferredPersistent {
                            conn,
                            payload: next_payload,
                        }) {
                            send_overload_error(&mut conn.stream, conn.protocol);
                        }
                        return Ok(());
                    }
                    dispatch_persistent_request(&mut conn.stream, protocol, req, hub)?;
                } else {
                    dispatch_persistent_frame(&mut conn.stream, protocol, &next_payload, hub)?;
                }
                conn.last_activity = Instant::now();
            }
            conn.partial_since = (!conn.buf.is_empty()).then(Instant::now);
            let _ = idle_return_tx.send(conn);
            notify_poll_wake(wake_tx);
            Ok(())
        }
        ConnectionWork::DeferredPersistent { mut conn, payload } => {
            let Some(protocol) = conn.protocol else {
                return Ok(());
            };
            let _ = conn.stream.set_write_timeout(Some(Duration::from_secs(60)));
            dispatch_persistent_frame(&mut conn.stream, protocol, &payload, hub)?;
            conn.last_activity = Instant::now();
            while let Some(next_payload) = try_pop_buffered_frame(&mut conn.buf)? {
                dispatch_persistent_frame(&mut conn.stream, protocol, &next_payload, hub)?;
                conn.last_activity = Instant::now();
            }
            conn.partial_since = (!conn.buf.is_empty()).then(Instant::now);
            let _ = idle_return_tx.send(conn);
            notify_poll_wake(wake_tx);
            Ok(())
        }
        ConnectionWork::DeferredOneShot {
            mut stream,
            req,
            _guard,
        } => {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
            let response_json = match hub.execute(req) {
                Ok(val) => json!({ "result": val }),
                Err(err) => json!({ "error": err }),
            };
            let mut bytes = serde_json::to_vec(&response_json)?;
            bytes.push(b'\n');
            stream.write_all(&bytes)?;
            stream.flush()?;
            Ok(())
        }
    }
}

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect() -> Result<Self> {
        Self::connect_with_timeout(configured_client_read_timeout())
    }

    pub fn connect_with_timeout(read_timeout: Duration) -> Result<Self> {
        let sock = catalog::socket_path()?;
        let mut stream = UnixStream::connect(&sock).map_err(|e| {
            invalid(format!(
                "test-context daemon unavailable at {}: {e} (start `tctx daemon` or `systemctl --user start test-context.service`)",
                sock.display()
            ))
        })?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        stream.write_all(BINARY_MAGIC)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    pub fn call(&mut self, request: &Request) -> Result<Value> {
        let arguments_json = if request.arguments.is_null()
            || request
                .arguments
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
        {
            Vec::new()
        } else {
            serde_json::to_vec(&request.arguments)?
        };
        let bytes = bincode::serialize(&BinaryRequestRef {
            root: request.root.as_deref(),
            name: &request.name,
            arguments_json: &arguments_json,
            compact: request.compact,
        })?;
        if bytes.len() > MAX_WIRE {
            return Err(invalid("request too large").into());
        }
        write_framed(&mut self.stream, &bytes)?;

        let mut length = [0u8; 4];
        self.reader.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_WIRE {
            return Err(invalid("daemon response too large").into());
        }
        let mut response = vec![0u8; length];
        self.reader.read_exact(&mut response)?;
        let response: BinaryResponse = bincode::deserialize(&response)?;
        if response.ok {
            Ok(serde_json::from_slice(&response.payload)?)
        } else {
            Err(ApplicationError(String::from_utf8(response.payload)?).into())
        }
    }
}

pub fn request(req: &Request) -> Result<Value> {
    let mut client = Client::connect()?;
    client.call(req)
}

fn is_timeout_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(io_error) = candidate.downcast_ref::<std::io::Error>()
            && matches!(
                io_error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        {
            return true;
        }
        current = candidate.source();
    }
    false
}

fn is_idempotent_request(req: &Request) -> bool {
    match req.name.as_str() {
        "ping" | "test_workspaces" | "test_status" | "test_graph" | "test_affected"
        | "test_explain" | "test_cache_export" => true,
        "test_ruff" => !req
            .arguments
            .get("fix")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

fn try_start_user_service_once() -> bool {
    static START_GATE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let gate = START_GATE.get_or_init(|| Mutex::new(None));
    let mut last_attempt = gate.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(ts) = *last_attempt
        && ts.elapsed() < SYSTEMCTL_START_COOLDOWN
    {
        return false;
    }
    *last_attempt = Some(Instant::now());
    std::process::Command::new("systemctl")
        .args(["--user", "start", "test-context.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn request_pooled_or_local(req: &Request) -> Result<Value> {
    // Non-idempotent requests (ruff_fix=true, test_invalidate, test_cache_import) use a fresh
    // connection so a stale pooled socket disconnect can never cause duplicate execution.
    if !is_idempotent_request(req) {
        return request_or_local(req);
    }
    const MAX_POOLED_CLIENTS: usize = 4;
    static CLIENT_POOL: OnceLock<Mutex<Vec<(PathBuf, Client)>>> = OnceLock::new();
    let sock = catalog::socket_path().ok();
    let pool = CLIENT_POOL.get_or_init(|| Mutex::new(Vec::new()));
    let popped = sock.as_ref().and_then(|expected| {
        let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .iter()
            .position(|(p, _)| p == expected)
            .map(|idx| guard.swap_remove(idx).1)
    });
    if let Some(mut client) = popped {
        match client.call(req) {
            Ok(val) => {
                if let Some(sock_path) = sock {
                    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
                    if guard.len() < MAX_POOLED_CLIENTS {
                        guard.push((sock_path, client));
                    }
                }
                return Ok(val);
            }
            Err(err) if err.downcast_ref::<ApplicationError>().is_some() => {
                if let Some(sock_path) = sock {
                    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
                    if guard.len() < MAX_POOLED_CLIENTS {
                        guard.push((sock_path, client));
                    }
                }
                return Err(err);
            }
            Err(err) if is_timeout_error(err.as_ref()) => return Err(err),
            Err(_) => {
                // Stale idle pooled connection closed by daemon; safe to retry idempotent request
            }
        }
    }
    if let Ok(mut fresh) = Client::connect() {
        match fresh.call(req) {
            Ok(val) => {
                if let Some(sock_path) = sock {
                    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
                    if guard.len() < MAX_POOLED_CLIENTS {
                        guard.push((sock_path, fresh));
                    }
                }
                return Ok(val);
            }
            Err(err) if err.downcast_ref::<ApplicationError>().is_some() => {
                if let Some(sock_path) = sock {
                    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
                    if guard.len() < MAX_POOLED_CLIENTS {
                        guard.push((sock_path, fresh));
                    }
                }
                return Err(err);
            }
            Err(err) if is_timeout_error(err.as_ref()) => return Err(err),
            Err(_) => {}
        }
    }
    request_or_local(req)
}

fn capture_caller_env_for_daemon() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut overflow_buf: Vec<u8> = Vec::new();
    let mut overflow_count = 0usize;
    let mut all_vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    all_vars.sort_by(|a, b| a.0.as_encoded_bytes().cmp(b.0.as_encoded_bytes()));
    for (raw_k, raw_v) in all_vars {
        let k_bytes = raw_k.as_encoded_bytes();
        let v_bytes = raw_v.as_encoded_bytes();
        let k_encoded = match raw_k.to_str() {
            Some(ks) if ks.len() <= 256 => ks.to_string(),
            _ => format!("__raw_key_sha256__:{}", crate::ast_graph::sha256_hex(k_bytes)),
        };
        // Losslessly encode non-UTF-8 or >256-byte values with SHA-256 so valid Unix raw-byte
        // env vars never panic and large values never disappear from mismatch detection.
        let encoded_val = match raw_v.to_str() {
            Some(vs) if vs.len() <= 256 => vs.to_string(),
            _ => format!("sha256:{}", crate::ast_graph::sha256_hex(v_bytes)),
        };
        if out.len() < 1024 {
            out.insert(k_encoded, encoded_val);
        } else {
            overflow_buf.extend_from_slice(k_encoded.as_bytes());
            overflow_buf.push(b'=');
            overflow_buf.extend_from_slice(encoded_val.as_bytes());
            overflow_buf.push(0);
            overflow_count += 1;
        }
    }
    if overflow_count > 0 {
        let digest = crate::ast_graph::sha256_hex(&overflow_buf);
        out.insert(
            "__tctx_overflow_env_sha__".to_string(),
            format!("{overflow_count}:{digest}"),
        );
    }
    out
}

/// CLI-only variant of `request_or_local` that forwards the caller's environment snapshot on
/// `test_run` so shell overrides (e.g. `PYTEST_ADDOPTS=...`, `DJANGO_SETTINGS_MODULE=...`) run
/// in-process with the caller's exact isolated environment on mismatch, while MCP and internal
/// daemon callers stay on `request_or_local` in the daemon's environment.
pub fn request_or_local_cli(req: &Request) -> Result<Value> {
    if req.name == "test_run" {
        let mut with_env = req.clone();
        if let Some(obj) = with_env.arguments.as_object_mut()
            && !obj.contains_key("_tctx_caller_env")
        {
            obj.insert(
                "_tctx_caller_env".to_string(),
                serde_json::to_value(capture_caller_env_for_daemon()).unwrap_or(Value::Null),
            );
        }
        return request_or_local_inner(req, &with_env);
    }
    request_or_local_inner(req, req)
}

pub fn request_or_local(req: &Request) -> Result<Value> {
    request_or_local_inner(req, req)
}

fn is_daemon_env_or_schema_fallback_err(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<ApplicationError>().is_some_and(|e| {
        e.0.starts_with("CALLER_ENV_MISMATCH") || e.0.contains("unknown field")
    })
}

fn request_or_local_inner(local_req: &Request, wire_req: &Request) -> Result<Value> {
    let mut env_mismatch_local_fallback = false;
    if let Ok(mut client) = Client::connect() {
        match client.call(wire_req) {
            Ok(val) => return Ok(val),
            Err(err) if is_daemon_env_or_schema_fallback_err(err.as_ref()) => {
                env_mismatch_local_fallback = true;
            }
            Err(err) if err.downcast_ref::<ApplicationError>().is_some() => return Err(err),
            Err(err) if is_timeout_error(err.as_ref()) => return Err(err),
            // Once connected and request was sent, never silently re-execute a non-idempotent
            // mutation (e.g. test_run/ruff_fix/test_invalidate) on an ambiguous disconnect.
            Err(err) if !is_idempotent_request(local_req) => return Err(err),
            Err(_) => {}
        }
    }
    // Attempt best-effort user service start (singleflight + cooldown) before local fallback
    if !env_mismatch_local_fallback && try_start_user_service_once() {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(mut client) = Client::connect() {
                match client.call(wire_req) {
                    Ok(val) => return Ok(val),
                    Err(err) if is_daemon_env_or_schema_fallback_err(err.as_ref()) => {
                        env_mismatch_local_fallback = true;
                        break;
                    }
                    Err(err) if err.downcast_ref::<ApplicationError>().is_some() => {
                        return Err(err);
                    }
                    Err(err) if is_timeout_error(err.as_ref()) => return Err(err),
                    Err(err) if !is_idempotent_request(local_req) => return Err(err),
                    Err(_) => {}
                }
            }
        }
    }
    // Fallback to direct oneshot execution (with background worker disabled and explicit shutdown).
    // For ANY CLI one-shot `test_run` (whether triggered by a caller-env mismatch OR because the
    // daemon is unavailable — detected by `_tctx_caller_env` on `wire_req`), partition ALL
    // language cache bases by the COMPLETE SHA-256 of the caller's environment (`env_partition`)
    // so repeated CLI runs in the same shell get full incremental cache hits while staying 100%
    // isolated from the daemon and from any other shell environment!
    let mut exec_req = local_req.clone();
    let is_cli_env_scoped = env_mismatch_local_fallback
        || wire_req
            .arguments
            .as_object()
            .is_some_and(|o| o.contains_key("_tctx_caller_env"));
    if is_cli_env_scoped
        && exec_req.name == "test_run"
        && let Some(obj) = exec_req.arguments.as_object_mut()
    {
        let caller_env_bytes = serde_json::to_vec(&capture_caller_env_for_daemon())
            .unwrap_or_default();
        let caller_env_fp = crate::ast_graph::sha256_hex(&caller_env_bytes);
        obj.insert("env_partition".to_string(), Value::String(caller_env_fp));
    }
    let hub = DaemonHub::with_mode(true);
    let res = hub.execute(exec_req);
    hub.shutdown_all();
    drop(hub);
    let _ = trim_allocator();
    res.map_err(|e| Box::new(ApplicationError(e)) as Box<dyn std::error::Error + Send + Sync>)
}

pub fn native_bridge(mut input: impl Read, mut output: impl Write) -> Result<()> {
    let mut client = Client::connect()?;
    output.write_all(b"TCTXN1\n")?;
    output.flush()?;
    let mut magic = [0u8; 7];
    input.read_exact(&mut magic)?;
    if magic != FRAMED_JSON_MAGIC {
        return Err(invalid("native bridge requires TCTXJ1 framed IPC").into());
    }

    loop {
        let mut len_buf = [0u8; 4];
        match input.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_WIRE {
            return Err(invalid("frame exceeds MAX_WIRE").into());
        }
        let mut payload = vec![0u8; len];
        input.read_exact(&mut payload)?;
        let req: Request = serde_json::from_slice(&payload)?;
        let call_outcome = match client.call(&req) {
            Ok(val) => Ok(val),
            Err(err) if is_transport_disconnect(err.as_ref()) && is_idempotent_request(&req) => {
                match Client::connect() {
                    Ok(mut reconnected) => {
                        let retry = reconnected.call(&req);
                        client = reconnected;
                        retry
                    }
                    Err(_) => return Err(err),
                }
            }
            Err(err) => Err(err),
        };
        let response_json = match call_outcome {
            Ok(val) => json!({ "result": val }),
            Err(err) if err.downcast_ref::<ApplicationError>().is_some() => {
                // Only keep the persistent bridge open for clean daemon-framed ApplicationErrors;
                // every transport, timeout, framing, or oversized-response error closes the bridge
                // so a desynchronized socket can never bleed unread bytes into the next request.
                json!({ "error": err.to_string() })
            }
            Err(err) => return Err(err),
        };
        let bytes = serde_json::to_vec(&response_json)?;
        write_framed(&mut output, &bytes)?;
    }
}

fn write_framed(stream: &mut impl Write, payload: &[u8]) -> Result<()> {
    let len_bytes = u32::try_from(payload.len())?.to_be_bytes();
    let mut header_off = 0usize;
    let mut payload_off = 0usize;
    while header_off < len_bytes.len() || payload_off < payload.len() {
        let mut slices = [IoSlice::new(&[]), IoSlice::new(&[])];
        let count = if header_off < len_bytes.len() {
            slices[0] = IoSlice::new(&len_bytes[header_off..]);
            if payload_off < payload.len() {
                slices[1] = IoSlice::new(&payload[payload_off..]);
                2
            } else {
                1
            }
        } else {
            slices[0] = IoSlice::new(&payload[payload_off..]);
            1
        };
        let written = match stream.write_vectored(&slices[..count]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write framed payload",
                )
                .into());
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if header_off < len_bytes.len() {
            let take_header = written.min(len_bytes.len() - header_off);
            header_off += take_header;
            payload_off += written - take_header;
        } else {
            payload_off += written;
        }
    }
    stream.flush()?;
    Ok(())
}

fn is_transport_disconnect(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(io_error) = candidate.downcast_ref::<std::io::Error>()
            && matches!(
                io_error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return true;
        }
        current = candidate.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_connection_guard_releases_exactly_one_slot() {
        let count = Arc::new(AtomicUsize::new(1));
        {
            let _guard = ActiveConnection(Arc::clone(&count));
            assert_eq!(count.load(Ordering::Acquire), 1);
        }
        assert_eq!(count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn shutdown_gate_rejects_queued_requests() {
        let hub = DaemonHub::new();
        hub.begin_shutdown();
        let error = hub
            .execute(Request {
                root: None,
                name: "ping".to_string(),
                arguments: json!({}),
                compact: false,
            })
            .unwrap_err();
        assert_eq!(error, "daemon is shutting down");
    }

    #[test]
    fn allocator_trim_is_safe_without_loaded_roots() {
        let _ = trim_allocator();
    }

    #[test]
    fn daemon_rss_probe_reports_current_process_memory() {
        assert!(daemon_rss_bytes().is_some_and(|rss| rss > 0));
    }

    #[test]
    fn large_roots_use_shorter_idle_retention() {
        assert_eq!(root_idle_timeout(4_999), IDLE_ROOT_TIMEOUT);
        assert_eq!(root_idle_timeout(5_000), LARGE_ROOT_IDLE_TIMEOUT);
    }

    #[test]
    fn connection_workers_are_bounded_for_local_daemon_load() {
        assert_eq!(connection_worker_count(1), 4);
        assert_eq!(connection_worker_count(8), 8);
        assert_eq!(connection_worker_count(64), MAX_CONNECTION_WORKERS);
    }

    #[test]
    fn persistent_connections_expire_after_idle_deadline() {
        let recent = Instant::now();
        assert!(!persistent_connection_expired(recent));
        let stale = Instant::now() - PERSISTENT_CONNECTION_IDLE_TIMEOUT;
        assert!(persistent_connection_expired(stale));
    }

    #[test]
    fn native_bridge_classifies_transport_disconnects() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            let error = std::io::Error::new(kind, "transport closed");
            assert!(is_transport_disconnect(&error));
        }
        let application_error = ApplicationError("test failed".to_string());
        assert!(!is_transport_disconnect(&application_error));
    }

    #[test]
    fn status_loading_is_explicit() {
        assert!(
            !json!({})
                .get("load")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        assert!(
            json!({"load": true})
                .get("load")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
    }

    #[test]
    fn daemon_hub_recovers_from_poisoned_load_gate_and_roots_mutex() {
        let hub = Arc::new(DaemonHub::new());
        let hub_for_panic = Arc::clone(&hub);
        let _ = std::thread::spawn(move || {
            let _gate = hub_for_panic.load_gate.lock().unwrap();
            let _roots = hub_for_panic.roots.lock().unwrap();
            panic!("simulated panic while holding load_gate and roots");
        })
        .join();

        assert!(hub.load_gate.is_poisoned());
        assert!(hub.roots.is_poisoned());

        let pong = hub
            .execute(Request {
                root: None,
                name: "ping".to_string(),
                arguments: json!({}),
                compact: false,
            })
            .expect("ping succeeds after poison");
        assert_eq!(pong["ok"], json!(true));

        let temp = tempfile::tempdir().unwrap();
        let err = hub
            .get_or_load_root(&temp.path().join("nonexistent-subdir"))
            .err()
            .expect("missing directory returns error, not load gate poisoned");
        assert!(
            !err.to_string().contains("poisoned"),
            "unexpected poisoned error: {err}"
        );
    }
}
