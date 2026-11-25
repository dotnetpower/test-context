use super::*;

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

pub(super) fn notify_poll_wake(wake_tx: &Mutex<UnixStream>) {
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

pub(super) fn send_overload_error(stream: &mut UnixStream, protocol: Option<PersistentProtocol>) {
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

pub(super) enum FrameReadOutcome {
    Complete(Vec<u8>),
    Eof,
    PartialTimeout,
}

pub(super) fn read_until_len_deadline(
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

pub(super) fn read_next_frame_bounded(
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

pub(super) fn try_pop_buffered_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
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

pub(super) fn decode_persistent_request(protocol: PersistentProtocol, payload: &[u8]) -> Result<Request> {
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

pub(super) fn dispatch_persistent_request(
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

pub(super) fn dispatch_persistent_frame(
    stream: &mut UnixStream,
    protocol: PersistentProtocol,
    payload: &[u8],
    hub: &DaemonHub,
) -> Result<()> {
    let req = decode_persistent_request(protocol, payload)?;
    dispatch_persistent_request(stream, protocol, req, hub)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_new_connection(
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

pub(super) fn handle_connection_work(
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


