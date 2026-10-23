use crate::catalog;
use crate::registration;
use crate::runner::{ManagedRoot, TestRunRequest};
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_WIRE: usize = 4 * 1024 * 1024;
pub const BINARY_MAGIC: &[u8] = b"TCTXB1\n";
pub const FRAMED_JSON_MAGIC: &[u8] = b"TCTXJ1\n";
pub const TRACE_ARGUMENT: &str = "_tctx_trace";

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
    traces: Mutex<VecDeque<Value>>,
    started_at: Instant,
}

impl DaemonHub {
    fn new() -> Self {
        Self {
            roots: Mutex::new(BTreeMap::new()),
            traces: Mutex::new(VecDeque::new()),
            started_at: Instant::now(),
        }
    }

    fn get_or_load_root(&self, requested_root: &Path) -> Result<Arc<ManagedRoot>> {
        let spec = registration::prepare_or_resolve_spec(requested_root)?;
        {
            let guard = self.roots.lock().map_err(|_| invalid("hub mutex poisoned"))?;
            if let Some(existing) = guard.get(&spec.root) {
                return Ok(Arc::clone(existing));
            }
        }
        let loaded = ManagedRoot::load(spec.clone())?;
        let mut guard = self.roots.lock().map_err(|_| invalid("hub mutex poisoned"))?;
        let entry = guard
            .entry(spec.root)
            .or_insert_with(|| Arc::clone(&loaded));
        Ok(Arc::clone(entry))
    }

    fn reload_root(&self, requested_root: &Path) -> Result<Arc<ManagedRoot>> {
        let spec = registration::prepare_or_resolve_spec(requested_root)?;
        let loaded = ManagedRoot::load(spec.clone())?;
        let mut guard = self.roots.lock().map_err(|_| invalid("hub mutex poisoned"))?;
        if let Some(old) = guard.insert(spec.root, Arc::clone(&loaded)) {
            old.shutdown();
        }
        Ok(loaded)
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
            "authorized_worktrees": true,
            "request_tracing": true,
            "trace_clock": "unix_ms",
            "client_trace_outcomes": true,
        })
    }

    fn execute(&self, mut req: Request) -> std::result::Result<Value, String> {
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
                let managed = self.reload_root(root_path)?;
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
    val.as_str().map(ToOwned::to_owned)
}

pub fn serve() -> Result<()> {
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
        let _ = std::fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    let hub = Arc::new(DaemonHub::new());
    // Pre-load registered roots asynchronously so background workers start warming up
    if let Ok(specs) = catalog::load() {
        let hub_init = Arc::clone(&hub);
        std::thread::spawn(move || {
            for spec in specs {
                if spec.root.is_dir() {
                    let _ = hub_init.get_or_load_root(&spec.root);
                }
            }
        });
    }

    for incoming in listener.incoming() {
        let Ok(stream) = incoming else {
            continue;
        };
        let hub_clone = Arc::clone(&hub);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &hub_clone);
        });
    }
    Ok(())
}

fn handle_connection(mut stream: UnixStream, hub: &DaemonHub) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let peek = reader.fill_buf()?;
    if peek.is_empty() {
        return Ok(());
    }

    if peek.starts_with(BINARY_MAGIC) {
        reader.consume(BINARY_MAGIC.len());
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
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
        }
    } else if peek.starts_with(FRAMED_JSON_MAGIC) {
        reader.consume(FRAMED_JSON_MAGIC.len());
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
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
        }
    } else {
        // One-shot JSON line protocol
        let mut line = Vec::new();
        reader.take(MAX_WIRE as u64 + 1).read_until(b'\n', &mut line)?;
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
    // Fallback to direct in-process execution if the background daemon is not running yet
    let hub = DaemonHub::new();
    hub.execute(req.clone())
        .map_err(|e| Box::new(ApplicationError(e)) as Box<dyn std::error::Error + Send + Sync>)
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
