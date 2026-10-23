use crate::catalog;
use crate::registration;
use crate::runner::{ManagedRoot, TestRunRequest};
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_WIRE: usize = 4 * 1024 * 1024;
pub const BINARY_MAGIC: &[u8] = b"TCTXB1\n";
pub const FRAMED_JSON_MAGIC: &[u8] = b"TCTXJ1\n";
pub const TRACE_ARGUMENT: &str = "_tctx_trace";
pub const MAX_ACTIVE_CONNECTIONS: usize = 64;
pub const MAX_CONNECTION_WORKERS: usize = 8;
pub const PERSISTENT_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_RESIDENT_ROOTS: usize = 8;
pub const MAX_DAEMON_RSS_BYTES: u64 = 192 * 1024 * 1024;
pub const IDLE_ROOT_TIMEOUT: Duration = Duration::from_secs(900);
pub const LARGE_ROOT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const LARGE_ROOT_FILE_THRESHOLD: usize = 5_000;
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_allocator() -> bool {
    unsafe { libc::malloc_trim(0) != 0 }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_allocator() -> bool {
    false
}

fn daemon_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

fn root_idle_timeout(indexed_files: usize) -> Duration {
    if indexed_files >= LARGE_ROOT_FILE_THRESHOLD {
        LARGE_ROOT_IDLE_TIMEOUT
    } else {
        IDLE_ROOT_TIMEOUT
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

struct ConnectionWork {
    stream: UnixStream,
    _guard: ActiveConnection,
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

struct DaemonHub {
    roots: Mutex<BTreeMap<PathBuf, Arc<ManagedRoot>>>,
    load_gate: Mutex<()>,
    traces: Mutex<VecDeque<Value>>,
    started_at: Instant,
    local_oneshot: bool,
    shutting_down: AtomicBool,
}

impl DaemonHub {
    fn new() -> Self {
        Self::with_mode(false)
    }

    fn with_mode(local_oneshot: bool) -> Self {
        Self {
            roots: Mutex::new(BTreeMap::new()),
            load_gate: Mutex::new(()),
            traces: Mutex::new(VecDeque::new()),
            started_at: Instant::now(),
            local_oneshot,
            shutting_down: AtomicBool::new(false),
        }
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let roots = self
            .roots
            .lock()
            .map(|guard| guard.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for root in roots {
            root.cancel_active();
        }
    }

    fn shutdown_all(&self) {
        let roots = self
            .roots
            .lock()
            .map(|mut guard| {
                std::mem::take(&mut *guard)
                    .into_values()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for root in roots {
            root.shutdown();
        }
    }

    fn evict_idle_or_excess_roots(&self) {
        let mut evicted = Vec::new();
        let roots = self
            .roots
            .lock()
            .map(|guard| guard.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for root in roots {
            root.reclaim_idle_zygote();
            root.flush_pending_cache();
        }
        if let Ok(mut guard) = self.roots.lock() {
            let idle_keys: Vec<PathBuf> = guard
                .iter()
                .filter(|(_, root)| {
                    root.is_evictable()
                        && root.last_activity_elapsed()
                            >= root_idle_timeout(root.indexed_file_count())
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in idle_keys {
                if let Some(old) = guard.remove(&k) {
                    evicted.push(old);
                }
            }
            while guard.len() >= MAX_RESIDENT_ROOTS {
                let oldest_key = guard
                    .iter()
                    .filter(|(_, r)| r.is_evictable())
                    .max_by_key(|(_, r)| r.last_activity_elapsed())
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest_key
                    && let Some(old) = guard.remove(&k)
                {
                    evicted.push(old);
                } else {
                    break;
                }
            }
            if guard.len() > 1
                && daemon_rss_bytes().is_some_and(|rss| rss > MAX_DAEMON_RSS_BYTES)
                && let Some(oldest_key) = guard
                    .iter()
                    .filter(|(_, root)| root.is_evictable())
                    .max_by_key(|(_, root)| root.last_activity_elapsed())
                    .map(|(key, _)| key.clone())
                && let Some(old) = guard.remove(&oldest_key)
            {
                evicted.push(old);
            }
        }
        for root in evicted {
            root.shutdown();
        }
        trim_allocator();
    }

    fn get_or_load_root(&self, requested_root: &Path) -> Result<Arc<ManagedRoot>> {
        let mut spec = registration::prepare_or_resolve_spec(requested_root)?;
        if self.local_oneshot {
            spec.policy.background_enabled = false;
        }
        {
            let guard = self
                .roots
                .lock()
                .map_err(|_| invalid("hub mutex poisoned"))?;
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(Arc::clone(existing));
            }
        }
        // Serialize cold loads so concurrent callers do not duplicate work or leak background worker threads
        let _load_guard = self
            .load_gate
            .lock()
            .map_err(|_| invalid("load gate poisoned"))?;
        {
            let guard = self
                .roots
                .lock()
                .map_err(|_| invalid("hub mutex poisoned"))?;
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(Arc::clone(existing));
            }
        }
        if !self.local_oneshot {
            self.evict_idle_or_excess_roots();
        }
        let loaded = ManagedRoot::load(spec.clone())?;
        let mut guard = self
            .roots
            .lock()
            .map_err(|_| invalid("hub mutex poisoned"))?;
        if let Some(old) = guard.insert(spec.root, Arc::clone(&loaded)) {
            old.shutdown();
        }
        Ok(loaded)
    }

    fn loaded_root(&self, requested_root: &Path) -> Result<Option<Arc<ManagedRoot>>> {
        let requested = requested_root.canonicalize()?;
        let guard = self
            .roots
            .lock()
            .map_err(|_| invalid("hub mutex poisoned"))?;
        Ok(guard
            .iter()
            .filter(|(root, _)| requested.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, managed)| Arc::clone(managed)))
    }

    fn record_trace(&self, trace: Value) {
        if let Ok(mut guard) = self.traces.lock() {
            if guard.len() >= 64 {
                guard.pop_front();
            }
            guard.push_back(trace);
        }
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
        let outcome = self.execute_inner(&req);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

        match outcome {
            Ok(value) => {
                if let Some(tid) = trace_id {
                    Ok(json!({
                        "_tctx_trace": {
                            "trace_id": tid,
                            "execute_ms": elapsed_ms,
                            "daemon_unix_ms": crate::cache::now_unix_ms(),
                        },
                        "value": value,
                    }))
                } else {
                    Ok(value)
                }
            }
            Err(err) => Err(err.to_string()),
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
                let specs = catalog::load()?;
                Ok(serde_json::to_value(specs)?)
            }
            "test_trace" => {
                self.record_trace(req.arguments.clone());
                Ok(json!({"recorded": true}))
            }
            "test_status" => {
                let registered = catalog::load()?;
                let loaded_roots: Vec<PathBuf> = self
                    .roots
                    .lock()
                    .map(|g| g.keys().cloned().collect())
                    .unwrap_or_default();
                let root_status = if let Some(root_path) = &req.root {
                    let managed = self.get_or_load_root(root_path)?;
                    Some(managed.status())
                } else {
                    None
                };
                Ok(json!({
                    "daemon": {
                        "ok": true,
                        "version": env!("CARGO_PKG_VERSION"),
                        "pid": std::process::id(),
                        "uptime_secs": self.started_at.elapsed().as_secs(),
                        "registered_roots": registered.len(),
                        "loaded_roots": loaded_roots,
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
                let cancelled = self
                    .loaded_root(root_path)?
                    .is_some_and(|managed| managed.cancel_active());
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
                if let Some(obj) = args.as_object_mut() {
                    obj.remove("root");
                }
                let run_req: TestRunRequest = if args.is_null() {
                    TestRunRequest::default()
                } else {
                    serde_json::from_value(args)?
                };
                let managed = self.get_or_load_root(root_path)?;
                let response = managed.run_tests(&run_req)?;
                Ok(serde_json::to_value(response)?)
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
    let listener = UnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;
    let runtime_identity = Arc::new(RuntimeIdentity::capture(&dir, &sock_path, &lock_file)?);

    let hub = Arc::new(DaemonHub::new());
    let active_connections = Arc::new(AtomicUsize::new(0));
    let worker_count = connection_worker_count(
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4),
    );
    let (connection_tx, connection_rx) = sync_channel::<ConnectionWork>(MAX_ACTIVE_CONNECTIONS);
    let connection_rx = Arc::new(Mutex::new(connection_rx));
    let mut connection_workers = Vec::new();
    for index in 0..worker_count {
        let hub_worker = Arc::clone(&hub);
        let rx_worker = Arc::clone(&connection_rx);
        connection_workers.push(
            std::thread::Builder::new()
                .name(format!("tctx-conn-{index}"))
                .spawn(move || {
                    loop {
                        let work = {
                            let Ok(receiver) = rx_worker.lock() else {
                                return;
                            };
                            receiver.recv()
                        };
                        let Ok(work) = work else {
                            return;
                        };
                        let _ = handle_connection(work.stream, &hub_worker);
                    }
                })?,
        );
    }

    // Background maintenance loop for RuntimeIdentity verification and idle root eviction
    let maintenance = {
        let hub_maint = Arc::clone(&hub);
        let id_maint = Arc::clone(&runtime_identity);
        Some(
            std::thread::Builder::new()
                .name("tctx-maint".to_string())
                .spawn(move || {
                    let mut last_identity_check = Instant::now();
                    while !STOP_REQUESTED.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(100));
                        if last_identity_check.elapsed() < Duration::from_secs(5) {
                            continue;
                        }
                        if let Err(err) = id_maint.check() {
                            eprintln!("{err}");
                            hub_maint.shutdown_all();
                            STOP_REQUESTED.store(true, Ordering::Release);
                            break;
                        }
                        hub_maint.evict_idle_or_excess_roots();
                        last_identity_check = Instant::now();
                    }
                })?,
        )
    };

    while !STOP_REQUESTED.load(Ordering::Acquire) {
        let mut descriptor = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
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
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let current = active_connections.fetch_add(1, Ordering::AcqRel);
        if current >= MAX_ACTIVE_CONNECTIONS {
            active_connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let conn_guard = ActiveConnection(Arc::clone(&active_connections));
        match connection_tx.try_send(ConnectionWork {
            stream,
            _guard: conn_guard,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => continue,
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    hub.begin_shutdown();
    drop(connection_tx);
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

fn handle_connection(mut stream: UnixStream, hub: &DaemonHub) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let peek = reader.fill_buf()?;
    if peek.is_empty() {
        return Ok(());
    }

    if peek.starts_with(BINARY_MAGIC) {
        reader.consume(BINARY_MAGIC.len());
        let mut last_activity = Instant::now();
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) && STOP_REQUESTED.load(Ordering::Acquire) =>
                {
                    return Ok(());
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if persistent_connection_expired(last_activity) {
                        return Ok(());
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > MAX_WIRE {
                return Err(invalid("frame exceeds MAX_WIRE").into());
            }
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload)?;
            let bin_req: BinaryRequest = bincode::deserialize(&payload)?;
            let arguments: Value = if bin_req.arguments_json.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bin_req.arguments_json)?
            };
            let req = Request {
                root: bin_req.root,
                name: bin_req.name,
                arguments,
                compact: bin_req.compact,
            };
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
            stream.write_all(&u32::try_from(encoded.len())?.to_be_bytes())?;
            stream.write_all(&encoded)?;
            stream.flush()?;
            last_activity = Instant::now();
        }
    } else if peek.starts_with(FRAMED_JSON_MAGIC) {
        reader.consume(FRAMED_JSON_MAGIC.len());
        let mut last_activity = Instant::now();
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) && STOP_REQUESTED.load(Ordering::Acquire) =>
                {
                    return Ok(());
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if persistent_connection_expired(last_activity) {
                        return Ok(());
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > MAX_WIRE {
                return Err(invalid("frame exceeds MAX_WIRE").into());
            }
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload)?;
            let req: Request = serde_json::from_slice(&payload)?;
            let response_json = match hub.execute(req) {
                Ok(val) => json!({ "result": val }),
                Err(err) => json!({ "error": err }),
            };
            let bytes = serde_json::to_vec(&response_json)?;
            stream.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
            stream.write_all(&bytes)?;
            stream.flush()?;
            last_activity = Instant::now();
        }
    } else {
        // One-shot JSON line protocol
        let mut line = Vec::new();
        reader
            .take(MAX_WIRE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if line.len() > MAX_WIRE {
            return Err(invalid("request too large").into());
        }
        let req: Request = serde_json::from_slice(&line)?;
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

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let sock = catalog::socket_path()?;
        let mut stream = UnixStream::connect(&sock).map_err(|e| {
            invalid(format!(
                "test-context daemon unavailable at {}: {e} (start `tctx daemon` or `systemctl --user start test-context.service`)",
                sock.display()
            ))
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        stream.write_all(BINARY_MAGIC)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    pub fn call(&mut self, request: &Request) -> Result<Value> {
        let bytes = bincode::serialize(&BinaryRequest {
            root: request.root.clone(),
            name: request.name.clone(),
            arguments_json: serde_json::to_vec(&request.arguments)?,
            compact: request.compact,
        })?;
        if bytes.len() > MAX_WIRE {
            return Err(invalid("request too large").into());
        }
        self.stream
            .write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;

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

pub fn request_or_local(req: &Request) -> Result<Value> {
    if let Ok(val) = request(req) {
        return Ok(val);
    }
    // Attempt best-effort user service start before falling back to local oneshot execution
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "start", "test-context.service"])
        .status();
    std::thread::sleep(Duration::from_millis(100));
    if let Ok(val) = request(req) {
        return Ok(val);
    }
    // Fallback to direct oneshot execution (with background worker disabled and explicit shutdown)
    let hub = DaemonHub::with_mode(true);
    let res = hub.execute(req.clone());
    hub.shutdown_all();
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
        let response_json = match client.call(&req) {
            Ok(val) => json!({ "result": val }),
            Err(err) => json!({ "error": err.to_string() }),
        };
        let bytes = serde_json::to_vec(&response_json)?;
        output.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
        output.write_all(&bytes)?;
        output.flush()?;
    }
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
}
