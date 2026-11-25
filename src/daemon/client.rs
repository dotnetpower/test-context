use super::*;

pub struct Client {
    pub(super) stream: UnixStream,
    pub(super) reader: BufReader<UnixStream>,
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

pub(super) fn is_timeout_error(error: &(dyn std::error::Error + 'static)) -> bool {
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

pub(super) fn is_idempotent_request(req: &Request) -> bool {
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

pub(super) fn try_start_user_service_once() -> bool {
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

pub(super) fn capture_caller_env_for_daemon() -> BTreeMap<String, String> {
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

pub(super) fn is_daemon_env_or_schema_fallback_err(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<ApplicationError>().is_some_and(|e| {
        e.0.starts_with("CALLER_ENV_MISMATCH") || e.0.contains("unknown field")
    })
}

pub(super) fn request_or_local_inner(local_req: &Request, wire_req: &Request) -> Result<Value> {
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

pub(super) fn write_framed(stream: &mut impl Write, payload: &[u8]) -> Result<()> {
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

pub(super) fn is_transport_disconnect(error: &(dyn std::error::Error + 'static)) -> bool {
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


