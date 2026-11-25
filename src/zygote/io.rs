use super::*;

pub(super) const MAX_ZYGOTE_FRAME: usize = 4 * 1024 * 1024;
pub(super) const ZYGOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn read_line_with_deadline(
    reader: &mut BufReader<ChildStdout>,
    max_bytes: usize,
    deadline: Instant,
    context: &str,
) -> Result<String> {
    let mut out = Vec::with_capacity(64);
    loop {
        let buffered = reader.buffer();
        if !buffered.is_empty() {
            if let Some(nl_pos) = buffered.iter().position(|&b| b == b'\n') {
                let take = nl_pos + 1;
                if out.len() + take > max_bytes {
                    return Err(invalid(format!("{context} exceeded max line length")).into());
                }
                out.extend_from_slice(&buffered[..take]);
                reader.consume(take);
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            if out.len() + buffered.len() > max_bytes {
                return Err(invalid(format!("{context} exceeded max line length")).into());
            }
            let take = buffered.len();
            out.extend_from_slice(buffered);
            reader.consume(take);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid(format!("{context} timed out")).into());
        }
        wait_readable(reader.get_ref().as_raw_fd(), remaining, context)?;
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(String::from_utf8_lossy(&out).into_owned());
        }
    }
}

pub(super) fn read_exact_with_deadline(
    reader: &mut BufReader<ChildStdout>,
    buf: &mut [u8],
    deadline: Instant,
    context: &str,
) -> Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let buffered = reader.buffer();
        if !buffered.is_empty() {
            let take = buffered.len().min(buf.len() - filled);
            buf[filled..filled + take].copy_from_slice(&buffered[..take]);
            reader.consume(take);
            filled += take;
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid(format!("{context} timed out")).into());
        }
        wait_readable(reader.get_ref().as_raw_fd(), remaining, context)?;
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{context} hit unexpected EOF"),
            )
            .into());
        }
        let take = chunk.len().min(buf.len() - filled);
        buf[filled..filled + take].copy_from_slice(&chunk[..take]);
        reader.consume(take);
        filled += take;
    }
    Ok(())
}

pub(super) fn wait_readable(fd: i32, timeout: Duration, context: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(invalid(format!("{context} timed out")).into());
        }
        let timeout_ms = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready > 0 {
            return Ok(());
        }
        if ready == 0 {
            return Err(invalid(format!("{context} timed out")).into());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

pub(super) fn write_all_with_deadline(stdin: &mut ChildStdin, data: &[u8], deadline: Instant) -> Result<()> {
    let fd = stdin.as_raw_fd();
    let orig_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if orig_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK) };
    }
    struct RestoreFlags(i32, i32);
    impl Drop for RestoreFlags {
        fn drop(&mut self) {
            if self.1 >= 0 {
                let _ = unsafe { libc::fcntl(self.0, libc::F_SETFL, self.1) };
            }
        }
    }
    let _restore = RestoreFlags(fd, orig_flags);

    let mut written = 0usize;
    while written < data.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid("zygote stdin write timed out").into());
        }
        let timeout_ms = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready == 0 {
            return Err(invalid("zygote stdin write timed out").into());
        }
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        match stdin.write(&data[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "zygote stdin closed while writing request",
                )
                .into());
            }
            Ok(n) => written += n,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub(super) struct ActivePgidGuard<'a>(pub(super) &'a AtomicI32);

impl Drop for ActivePgidGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

pub(super) struct ParallelPgidGuard<'a> {
    pub(super) registry: Option<&'a Mutex<BTreeSet<i32>>>,
    pub(super) pgid: i32,
}

impl Drop for ParallelPgidGuard<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry {
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.pgid);
        }
    }
}


