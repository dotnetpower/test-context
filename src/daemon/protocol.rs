use super::*;

pub const MAX_WIRE: usize = 4 * 1024 * 1024;
pub(super) const MAX_ONESHOT_WIRE: usize = 512 * 1024;
pub const BINARY_MAGIC: &[u8] = b"TCTXB1\n";
pub const FRAMED_JSON_MAGIC: &[u8] = b"TCTXJ1\n";
pub const TRACE_ARGUMENT: &str = "_tctx_trace";
pub const MAX_ACTIVE_CONNECTIONS: usize = 64;
pub const MAX_CONNECTION_WORKERS: usize = 8;
pub const PERSISTENT_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DEFAULT_CLIENT_READ_TIMEOUT_SECS: u64 = 600;
pub(super) const SYSTEMCTL_START_COOLDOWN: Duration = Duration::from_secs(15);
pub const MAX_RESIDENT_ROOTS: usize = 8;
pub const MAX_DAEMON_RSS_BYTES: u64 = 1024 * 1024 * 1024;
pub const IDLE_ROOT_TIMEOUT: Duration = Duration::from_secs(1_800);
pub const LARGE_ROOT_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
pub const LARGE_ROOT_FILE_THRESHOLD: usize = 5_000;
pub(super) static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn configured_max_daemon_rss_bytes() -> u64 {
    std::env::var("TCTX_MAX_RSS_MB")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|mb| (128..=16_384).contains(mb))
        .and_then(|mb| mb.checked_mul(1024 * 1024))
        .unwrap_or(MAX_DAEMON_RSS_BYTES)
}

pub(super) fn configured_max_resident_roots() -> usize {
    std::env::var("TCTX_MAX_RESIDENT_ROOTS")
        .ok()
        .and_then(|val| val.trim().parse::<usize>().ok())
        .filter(|n| (1..=64).contains(n))
        .unwrap_or(MAX_RESIDENT_ROOTS)
}

pub(super) fn configured_idle_root_timeout() -> Duration {
    std::env::var("TCTX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|secs| (60..=86_400).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or(IDLE_ROOT_TIMEOUT)
}

pub(super) fn configured_large_root_idle_timeout() -> Duration {
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

pub(super) fn daemon_cgroup_memory_bytes() -> Option<u64> {
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

pub(super) fn daemon_rss_bytes() -> Option<u64> {
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

pub(super) fn configured_client_read_timeout() -> Duration {
    std::env::var("TCTX_CLIENT_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.trim().parse::<u64>().ok())
        .filter(|secs| (5..=3_600).contains(secs))
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_CLIENT_READ_TIMEOUT_SECS))
}

pub(super) fn root_idle_timeout(indexed_files: usize) -> Duration {
    if indexed_files >= LARGE_ROOT_FILE_THRESHOLD {
        configured_large_root_idle_timeout()
    } else {
        configured_idle_root_timeout()
    }
}

pub(super) fn connection_worker_count(available_parallelism: usize) -> usize {
    available_parallelism.clamp(4, MAX_CONNECTION_WORKERS)
}

pub(super) fn persistent_connection_expired(last_activity: Instant) -> bool {
    last_activity.elapsed() >= PERSISTENT_CONNECTION_IDLE_TIMEOUT
}

pub(super) extern "C" fn request_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

pub(super) struct ActiveConnection(pub(super) Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PersistentProtocol {
    Binary,
    FramedJson,
}

pub(super) struct IdleConnection {
    pub(super) stream: UnixStream,
    pub(super) buf: Vec<u8>,
    /// `Some(proto)` for an established persistent connection; `None` for a newly accepted
    /// connection still awaiting its initial protocol header or one-shot request line.
    pub(super) protocol: Option<PersistentProtocol>,
    pub(super) last_activity: Instant,
    pub(super) partial_since: Option<Instant>,
    pub(super) _guard: ActiveConnection,
}

pub(super) enum ConnectionWork {
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

pub(super) fn is_fast_control_request(req: &Request) -> bool {
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

pub(super) struct RuntimeIdentity {
    pub(super) paths: [PathBuf; 3],
    pub(super) identities: [(u64, u64); 3],
}

impl RuntimeIdentity {
    pub(super) fn capture(dir: &Path, sock: &Path, lock: &std::fs::File) -> Result<Self> {
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

    pub(super) fn check(&self) -> Result<()> {
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
pub(super) struct BinaryRequest {
    pub(super) root: Option<PathBuf>,
    pub(super) name: String,
    pub(super) arguments_json: Vec<u8>,
    pub(super) compact: bool,
}

#[derive(Serialize)]
pub(super) struct BinaryRequestRef<'a> {
    pub(super) root: Option<&'a Path>,
    pub(super) name: &'a str,
    pub(super) arguments_json: &'a [u8],
    pub(super) compact: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct BinaryResponse {
    pub(super) ok: bool,
    pub(super) payload: Vec<u8>,
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

pub(super) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unexpected panic".to_string()
    }
}


