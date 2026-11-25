use super::*;

pub fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let output = run_command_with_timeout(command, timeout, 4 * 1024 * 1024)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(invalid(format!(
            "command failed ({}): {}",
            output.status,
            stderr.trim()
        ))
        .into());
    }
    Ok(output.stdout)
}

pub struct BoundedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub structured: Option<Vec<u8>>,
    pub cancelled: bool,
}

pub(super) fn drain_bounded_interruptible(
    mut reader: impl Read + std::os::unix::io::AsRawFd,
    max_output_bytes: usize,
    stop_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Vec<u8> {
    let fd = reader.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut kept = Vec::with_capacity(max_output_bytes.min(64 * 1024));
    let mut chunk = [0_u8; 64 * 1024];
    let mut stop_deadline: Option<Instant> = None;
    loop {
        if stop_flag.load(Ordering::Acquire) {
            let deadline = *stop_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
            if Instant::now() >= deadline {
                break;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_ms = if stop_deadline.is_some() { 25 } else { 50 };
        let rc = unsafe { libc::poll(&mut pfd, 1, poll_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        let mut hit_eof = false;
        loop {
            if stop_flag.load(Ordering::Acquire) {
                let deadline = *stop_deadline
                    .get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                if Instant::now() >= deadline {
                    hit_eof = true;
                    break;
                }
            }
            match reader.read(&mut chunk) {
                Ok(0) => {
                    hit_eof = true;
                    break;
                }
                Ok(read) => {
                    if kept.len() < max_output_bytes {
                        let remaining = max_output_bytes - kept.len();
                        kept.extend_from_slice(&chunk[..read.min(remaining)]);
                    }
                    if stop_flag.load(Ordering::Acquire) {
                        let deadline = *stop_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                        if Instant::now() >= deadline {
                            hit_eof = true;
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    hit_eof = true;
                    break;
                }
            }
        }
        if hit_eof || (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0) {
            break;
        }
    }
    kept
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<BoundedOutput> {
    run_command_with_timeout_tracked(command, timeout, max_output_bytes, None, None)
}

pub fn run_command_with_timeout_tracked(
    command: &mut Command,
    timeout: Duration,
    max_output_bytes: usize,
    active_pgid: Option<&AtomicI32>,
    active_pgids: Option<&Mutex<BTreeSet<i32>>>,
) -> Result<BoundedOutput> {
    let started = Instant::now();
    if command.get_envs().all(|(k, _)| k != "PATH")
        && let Some(aug_path) = crate::catalog::augmented_user_path()
    {
        command.env("PATH", aug_path);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Some(active) = active_pgid
        && let Ok(pgid) = i32::try_from(pid)
    {
        active.store(pgid, Ordering::Release);
    }
    let mut already_cancelled = false;
    if let Some(active) = active_pgids
        && let Ok(pgid) = i32::try_from(pid)
    {
        let mut guard = active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.contains(&i32::MIN) {
            already_cancelled = true;
        } else {
            guard.insert(pgid);
        }
    }
    struct PgidReset<'a> {
        single: Option<&'a AtomicI32>,
        many: Option<&'a Mutex<BTreeSet<i32>>>,
        pid: u32,
    }
    impl Drop for PgidReset<'_> {
        fn drop(&mut self) {
            if let Some(active) = self.single {
                active.store(0, Ordering::Release);
            }
            if let Some(active) = self.many
                && let Ok(pgid) = i32::try_from(self.pid)
            {
                active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&pgid);
            }
        }
    }
    let _pgid_reset = PgidReset {
        single: active_pgid,
        many: active_pgids,
        pid,
    };
    if already_cancelled
        && let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let stop_draining = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_stdout = std::sync::Arc::clone(&stop_draining);
    let stop_stderr = std::sync::Arc::clone(&stop_draining);
    let stdout_handle =
        std::thread::spawn(move || drain_bounded_interruptible(stdout_pipe, max_output_bytes, stop_stdout));
    let stderr_handle =
        std::thread::spawn(move || drain_bounded_interruptible(stderr_pipe, max_output_bytes, stop_stderr));

    let mut timed_out = false;
    let mut poll_sleep = Duration::from_micros(250);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            if let Ok(pgid) = i32::try_from(pid)
                && pgid > 0
            {
                unsafe {
                    let _ = libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(poll_sleep);
        poll_sleep = (poll_sleep * 2).min(Duration::from_millis(10));
    };
    // Ensure any orphaned descendant in the child's process group that inherited stdout/stderr is terminated,
    // and bound pipe draining so even a setsid() grandchild keeping stdout/stderr open cannot hang join().
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGKILL);
        }
    }
    stop_draining.store(true, Ordering::Release);

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let cancelled = !timed_out
        && status
            .signal()
            .is_some_and(|signal| matches!(signal, libc::SIGKILL | libc::SIGTERM));
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        duration_ms,
        timed_out,
        structured: None,
        cancelled,
    })
}


