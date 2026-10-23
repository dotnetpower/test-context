use crate::catalog::Spec;
use crate::workspace::BoundedOutput;
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_ZYGOTE_FRAME: usize = 4 * 1024 * 1024;
const ZYGOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

fn wait_readable(fd: i32, timeout: Duration, context: &str) -> Result<()> {
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

struct ActivePgidGuard<'a>(&'a AtomicI32);

impl Drop for ActivePgidGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

struct ParallelPgidGuard<'a> {
    registry: Option<&'a Mutex<BTreeSet<i32>>>,
    pgid: i32,
}

impl Drop for ParallelPgidGuard<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry
            && let Ok(mut guard) = registry.lock()
        {
            guard.remove(&self.pgid);
        }
    }
}

const ZYGOTE_SCRIPT: &str = r#"
import gc
import ctypes
import importlib
import io
import json
import math
import os
import select
import signal
import struct
import resource
import sys
import tempfile
import threading
import time

import pytest
import _pytest.config
import _pytest.main
import _pytest.runner
import _pytest.python
import _pytest.fixtures

MAX_FRAME = 4 * 1024 * 1024

def read_exact(stream, size):
    data = b""
    while len(data) < size:
        chunk = stream.read(size - len(data))
        if not chunk:
            return None
        data += chunk
    return data

def read_frame(stream):
    header = read_exact(stream, 4)
    if header is None:
        return None
    size = struct.unpack(">I", header)[0]
    if size > MAX_FRAME:
        raise ValueError("zygote request frame too large")
    body = read_exact(stream, size)
    if body is None:
        raise EOFError("truncated zygote request")
    return json.loads(body.decode("utf-8"))

def write_frame(stream, value):
    body = json.dumps(value, separators=(",", ":")).encode("utf-8")
    if len(body) > MAX_FRAME:
        body = json.dumps({"error": "zygote response frame too large"}).encode("utf-8")
    stream.write(struct.pack(">I", len(body)))
    stream.write(body)
    stream.flush()

def resolve_plugin(value):
    module_name, sep, attr_path = value.partition(":")
    obj = importlib.import_module(module_name)
    if sep:
        for part in attr_path.split("."):
            obj = getattr(obj, part)
    return obj

safe_preload = {
    "fastapi", "starlette", "pydantic", "httpx", "requests",
    "openai", "azure", "dotenv", "yaml", "PIL",
}
for mod_name in [m.strip() for m in os.environ.get("TCTX_PRELOAD_MODULES", "").split(",") if m.strip()]:
    if mod_name.split(".", 1)[0] not in safe_preload:
        continue
    try:
        importlib.import_module(mod_name)
    except Exception:
        pass

plugins = []
plugin_errors = []
for plugin_value in [p.strip() for p in os.environ.get("TCTX_PYTEST_PLUGINS", "").split(",") if p.strip()]:
    try:
        plugins.append(resolve_plugin(plugin_value))
    except Exception as exc:
        plugin_errors.append(f"{plugin_value}: {exc}")

audit_paths = None
audit_current_node = None
audit_by_node = {}
workspace_root = os.path.realpath(os.getcwd())

def audit_hook(event, args):
    global audit_paths, audit_current_node, audit_by_node
    if audit_paths is None:
        return
    candidate = None
    if event == "open" and args:
        candidate = args[0]
    elif event == "import" and len(args) > 1:
        candidate = args[1]
    if not isinstance(candidate, (str, bytes, os.PathLike)):
        return
    try:
        raw_path = os.fsdecode(os.fspath(candidate))
    except Exception:
        return
    if raw_path.endswith((".py", ".pyc", ".pyo")):
        return
    if os.path.isabs(raw_path):
        normalized = os.path.normpath(raw_path)
        if not normalized.startswith(workspace_root + os.sep):
            return
        rel = os.path.relpath(normalized, workspace_root).replace("\\", "/")
    else:
        rel = os.path.normpath(raw_path).replace("\\", "/").lstrip("./")
        if rel.startswith("../"):
            return
    if any(part.startswith(".") or part in ("__pycache__", ".pytest_cache", ".venv", "venv")
           for part in rel.split("/")):
        return
    if not rel.endswith((".json", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".sql", ".rego", ".csv")):
        return
    audit_paths.add(rel)
    if audit_current_node is not None:
        audit_by_node.setdefault(audit_current_node, set()).add(rel)

sys.addaudithook(audit_hook)

os.environ["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = "1"
gc.collect()
if hasattr(gc, "freeze"):
    gc.freeze()

proto_in = sys.stdin.buffer
proto_out = os.fdopen(os.dup(1), "wb", buffering=0)
devnull_fd = os.open(os.devnull, os.O_RDWR)
os.dup2(devnull_fd, 1)
os.dup2(devnull_fd, 2)

# Warm pytest configuration/parser/plugin registration without importing workspace tests.
try:
    pytest.main(["--version"], plugins=plugins)
except BaseException:
    pass

proto_out.write(b"TCTX_ZYGOTE_READY\n")
proto_out.flush()

class TctxReporter:
    def __init__(self):
        self.reports = []

    @staticmethod
    def _path(report):
        location = getattr(report, "location", None)
        if location and len(location) >= 1:
            return str(location[0]).replace("\\", "/")
        return str(getattr(report, "fspath", "")).replace("\\", "/")

    def pytest_runtest_logreport(self, report):
        if report.when == "call" or (report.failed and report.when in ("setup", "teardown")):
            self.reports.append({
                "nodeid": report.nodeid,
                "path": self._path(report),
                "when": report.when,
                "outcome": report.outcome,
                "duration_ms": max(0, int(round(float(report.duration) * 1000.0))),
                "longrepr": str(report.longrepr)[:8192] if report.failed else "",
                "dynamic_deps": sorted(audit_by_node.get(report.nodeid, set())),
            })

    def pytest_collectreport(self, report):
        if report.failed:
            self.reports.append({
                "nodeid": report.nodeid,
                "path": str(getattr(report, "fspath", "")).replace("\\", "/"),
                "when": "collect",
                "outcome": "failed",
                "duration_ms": 0,
                "longrepr": str(report.longrepr)[:8192],
                "dynamic_deps": [],
            })

    def pytest_runtest_logstart(self, nodeid, location):
        global audit_current_node
        audit_current_node = nodeid
        audit_by_node.setdefault(nodeid, set())

    def pytest_runtest_logfinish(self, nodeid, location):
        global audit_current_node
        audit_current_node = None

def close_unneeded_fds(keep):
    try:
        for name in os.listdir("/proc/self/fd"):
            try:
                fd = int(name)
            except ValueError:
                continue
            if fd > 2 and fd not in keep:
                try:
                    os.close(fd)
                except OSError:
                    pass
    except Exception:
        pass

while True:
    try:
        req = read_frame(proto_in)
    except Exception as exc:
        write_frame(proto_out, {"error": f"invalid request: {exc}"})
        continue
    if req is None:
        break

    args = req.get("args", [])
    capture_dynamic_deps = bool(req.get("capture_dynamic_deps", False))
    direct_reporter = bool(req.get("direct_reporter", False))
    timeout_secs = float(req.get("timeout_secs", 180.0))
    started = time.perf_counter()

    task_count = len(os.listdir("/proc/self/task")) if os.path.isdir("/proc/self/task") else threading.active_count()
    if task_count != 1:
        write_frame(proto_out, {
            "error": f"ZYGOTE_FORK_UNSAFE active_threads={task_count}",
            "unsafe_fork": True,
        })
        continue

    r_fd, w_fd = os.pipe()
    out_r, out_w = os.pipe()
    err_r, err_w = os.pipe()

    parent_pid = os.getpid()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(r_fd)
            os.close(out_r)
            os.close(err_r)
            os.setsid()
            try:
                libc = ctypes.CDLL(None)
                libc.prctl(1, signal.SIGKILL, 0, 0, 0)  # PR_SET_PDEATHSIG
                if os.getppid() != parent_pid:
                    os._exit(125)
            except Exception:
                pass
            try:
                cpu_limit = max(1, int(math.ceil(timeout_secs)) + 5)
                resource.setrlimit(resource.RLIMIT_CPU, (cpu_limit, cpu_limit))
            except Exception:
                pass
            gc.disable()
            gc.collect = lambda *a, **kw: 0
            os.dup2(out_w, 1)
            os.dup2(err_w, 2)
            os.close(out_w)
            os.close(err_w)
            keep = {w_fd}
            close_unneeded_fds(keep)
            sys.stdout = io.TextIOWrapper(os.fdopen(1, "wb", closefd=False), encoding="utf-8", errors="replace", write_through=True)
            sys.stderr = io.TextIOWrapper(os.fdopen(2, "wb", closefd=False), encoding="utf-8", errors="replace", write_through=True)
            reporter = TctxReporter() if direct_reporter else None
            audit_paths = set() if capture_dynamic_deps else None
            audit_by_node = {}
            audit_current_node = None
            try:
                run_plugins = [*plugins, reporter] if reporter is not None else plugins
                code = int(pytest.main(args, plugins=run_plugins))
            except SystemExit as exc:
                code = int(exc.code) if isinstance(exc.code, int) else 1
            except Exception as exc:
                sys.stderr.write(f"zygote pytest exception: {exc}\n")
                code = 2
            sys.stdout.flush()
            sys.stderr.flush()
            payload = json.dumps({
                "exit_code": code,
                "reports": reporter.reports if reporter is not None else [],
                "dynamic_deps": sorted(audit_paths or ()),
            }, separators=(",", ":")).encode("utf-8")
            payload = payload[:MAX_FRAME]
            offset = 0
            while offset < len(payload):
                offset += os.write(w_fd, payload[offset:])
        finally:
            try:
                os.close(w_fd)
            except Exception:
                pass
            os._exit(0)
    else:
        os.close(w_fd)
        os.close(out_w)
        os.close(err_w)
        os.set_blocking(r_fd, False)
        os.set_blocking(out_r, False)
        os.set_blocking(err_r, False)
        write_frame(proto_out, {"started_pid": pid})
        timed_out = False
        result_chunks = []
        stdout_chunks = []
        stderr_chunks = []
        result_done = False
        open_fds = {r_fd: result_chunks, out_r: stdout_chunks, err_r: stderr_chunks}
        deadline = time.monotonic() + timeout_secs
        while open_fds and not result_done and time.monotonic() < deadline:
            ready, _, _ = select.select(list(open_fds), [], [], max(0.0, deadline - time.monotonic()))
            if not ready:
                break
            for fd in ready:
                try:
                    chunk = os.read(fd, 65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    open_fds.pop(fd, None)
                    try:
                        os.close(fd)
                    except OSError:
                        pass
                    continue
                target = open_fds[fd]
                current = sum(map(len, target))
                if current < 2 * 1024 * 1024:
                    target.append(chunk[: 2 * 1024 * 1024 - current])
                if fd == r_fd:
                    result_done = True
        wait_status = 0
        waited = False
        if not result_done:
            try:
                done_pid, wait_status = os.waitpid(pid, os.WNOHANG)
                waited = done_pid == pid
            except Exception:
                waited = False
            if waited:
                child_result = {"exit_code": 130, "reports": [], "dynamic_deps": []}
            else:
                timed_out = True
                try:
                    os.killpg(pid, signal.SIGKILL)
                except Exception:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except Exception:
                        pass
                child_result = {"exit_code": 124, "reports": [], "dynamic_deps": []}
        else:
            try:
                child_result = json.loads(b"".join(result_chunks).decode("utf-8"))
            except Exception:
                child_result = {"exit_code": 1, "reports": [], "dynamic_deps": []}
        if not waited:
            try:
                _, wait_status = os.waitpid(pid, 0)
            except Exception:
                pass
        # Drain any output written between result delivery and process exit.
        for fd, target in list(open_fds.items()):
            while True:
                try:
                    chunk = os.read(fd, 65536)
                except BlockingIOError:
                    break
                if not chunk:
                    break
                current = sum(map(len, target))
                if current < 2 * 1024 * 1024:
                    target.append(chunk[: 2 * 1024 * 1024 - current])
            try:
                os.close(fd)
            except OSError:
                pass
        cancelled = os.WIFSIGNALED(wait_status) and os.WTERMSIG(wait_status) in (signal.SIGKILL, signal.SIGTERM)
        stdout_text = b"".join(stdout_chunks).decode("utf-8", errors="replace")
        stderr_text = b"".join(stderr_chunks).decode("utf-8", errors="replace")

        duration_ms = max(1, int(round((time.perf_counter() - started) * 1000.0)))
        write_frame(proto_out, {
            "exit_code": int(child_result.get("exit_code", 1)),
            "reports": child_result.get("reports", []),
            "dynamic_deps": child_result.get("dynamic_deps", []),
            "stdout": stdout_text,
            "stderr": stderr_text,
            "duration_ms": duration_ms,
            "timed_out": timed_out,
            "cancelled": cancelled and not timed_out,
            "plugin_errors": plugin_errors,
        })
"#;

#[derive(Serialize)]
struct ZygoteRequest<'a> {
    args: &'a [String],
    timeout_secs: u64,
    capture_dynamic_deps: bool,
    direct_reporter: bool,
}

#[derive(Deserialize)]
struct ZygoteReply {
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    duration_ms: Option<u64>,
    timed_out: Option<bool>,
    cancelled: Option<bool>,
    reports: Option<serde_json::Value>,
    dynamic_deps: Option<Vec<String>>,
    plugin_errors: Option<Vec<String>>,
    error: Option<String>,
}

pub struct PytestZygote {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    python_bin: PathBuf,
    pythonpath: std::ffi::OsString,
    preload_modules: String,
    pytest_plugins: String,
    root: PathBuf,
    plugin_errors: Vec<String>,
    runs: u32,
}

impl PytestZygote {
    pub fn spawn(
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> Result<Self> {
        let python_bin = resolve_python_bin(&spec.effective_pytest_cmd());
        let preload_str = preload_modules.join(",");
        let plugin_str = pytest_plugins.join(",");
        let mut cmd = Command::new(&python_bin);
        cmd.current_dir(&spec.root)
            .env("PYTHONPATH", &pythonpath)
            .env("TCTX_PRELOAD_MODULES", &preload_str)
            .env("TCTX_PYTEST_PLUGINS", &plugin_str)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg("-W")
            .arg("ignore")
            .arg("-c")
            .arg(ZYGOTE_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("missing zygote stdin"))?;
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing zygote stdout"))?;
        let mut stdout = BufReader::new(stdout_pipe);
        let mut banner = String::new();
        if let Err(error) = wait_readable(
            stdout.get_ref().as_raw_fd(),
            ZYGOTE_STARTUP_TIMEOUT,
            "zygote readiness handshake",
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        std::io::BufRead::read_line(&mut stdout, &mut banner)?;
        if banner.trim() != "TCTX_ZYGOTE_READY" {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(format!(
                "zygote failed readiness handshake: {}",
                banner.trim()
            ))
            .into());
        }

        Ok(Self {
            child,
            stdin,
            stdout,
            python_bin,
            pythonpath,
            preload_modules: preload_str,
            pytest_plugins: plugin_str,
            root: spec.root.clone(),
            plugin_errors: Vec::new(),
            runs: 0,
        })
    }

    pub fn matches_config(
        &mut self,
        spec: &Spec,
        pythonpath: &std::ffi::OsStr,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> bool {
        self.child.try_wait().ok().flatten().is_none()
            && self.runs < 200
            && self.root == spec.root
            && self.pythonpath == pythonpath
            && self.python_bin == resolve_python_bin(&spec.effective_pytest_cmd())
            && self.preload_modules == preload_modules.join(",")
            && self.pytest_plugins == pytest_plugins.join(",")
    }

    pub fn run_pytest(
        &mut self,
        args: &[String],
        timeout_secs: u64,
        active_pgid: &AtomicI32,
        capture_dynamic_deps: bool,
        parallel_pgids: Option<&Mutex<BTreeSet<i32>>>,
    ) -> Result<BoundedOutput> {
        let started = Instant::now();
        self.runs = self.runs.saturating_add(1);
        let req = ZygoteRequest {
            args,
            timeout_secs,
            capture_dynamic_deps,
            direct_reporter: capture_dynamic_deps,
        };
        let payload = serde_json::to_vec(&req)?;
        if payload.len() > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote request too large").into());
        }
        self.stdin
            .write_all(&u32::try_from(payload.len())?.to_be_bytes())?;
        self.stdin.write_all(&payload)?;
        self.stdin.flush()?;

        let mut started_length = [0_u8; 4];
        self.stdout.read_exact(&mut started_length)?;
        let length = u32::from_be_bytes(started_length) as usize;
        if length > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote response too large").into());
        }
        let mut reply_bytes = vec![0_u8; length];
        self.stdout.read_exact(&mut reply_bytes)?;
        let started_frame: serde_json::Value = serde_json::from_slice(&reply_bytes)?;
        let pid = started_frame["started_pid"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| invalid("zygote did not return a child pid"))?;
        active_pgid.store(pid, Ordering::Release);
        let _active_guard = ActivePgidGuard(active_pgid);
        if let Some(registry) = parallel_pgids
            && let Ok(mut guard) = registry.lock()
        {
            guard.insert(pid);
        }
        let _parallel_guard = ParallelPgidGuard {
            registry: parallel_pgids,
            pgid: pid,
        };

        let mut final_length = [0_u8; 4];
        self.stdout.read_exact(&mut final_length)?;
        let length = u32::from_be_bytes(final_length) as usize;
        if length > MAX_ZYGOTE_FRAME {
            return Err(invalid("zygote response too large").into());
        }
        let mut reply_bytes = vec![0_u8; length];
        self.stdout.read_exact(&mut reply_bytes)?;
        let reply: ZygoteReply = serde_json::from_slice(&reply_bytes)?;
        if let Some(err) = reply.error {
            return Err(invalid(format!("zygote error: {err}")).into());
        }
        let code = reply.exit_code.unwrap_or(1);
        let status = std::process::ExitStatus::from_raw((code & 0xff) << 8);
        let duration_ms = reply.duration_ms.unwrap_or_else(|| {
            u64::try_from(started.elapsed().as_millis())
                .unwrap_or(1)
                .max(1)
        });
        let structured = capture_dynamic_deps
            .then(|| {
                serde_json::to_vec(&serde_json::json!({
                    "reports": reply.reports.unwrap_or_else(|| serde_json::json!([])),
                    "dynamic_deps": reply.dynamic_deps.unwrap_or_default(),
                }))
                .ok()
            })
            .flatten();
        self.plugin_errors = reply.plugin_errors.unwrap_or_default();
        Ok(BoundedOutput {
            status,
            stdout: reply.stdout.unwrap_or_default().into_bytes(),
            stderr: reply.stderr.unwrap_or_default().into_bytes(),
            duration_ms,
            timed_out: reply.timed_out.unwrap_or(false),
            structured,
            cancelled: reply.cancelled.unwrap_or(false),
        })
    }

    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for PytestZygote {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct ZygoteManager {
    zygote: Option<PytestZygote>,
    failures: u32,
    disabled_until: Option<Instant>,
    last_error: Option<String>,
    active_pgid: Arc<AtomicI32>,
    last_used: Option<Instant>,
}

impl Default for ZygoteManager {
    fn default() -> Self {
        Self {
            zygote: None,
            failures: 0,
            disabled_until: None,
            last_error: None,
            active_pgid: Arc::new(AtomicI32::new(0)),
            last_used: None,
        }
    }
}

impl ZygoteManager {
    pub fn active_handle(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.active_pgid)
    }
    pub fn ensure(
        &mut self,
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
        pytest_plugins: &[String],
    ) -> Result<()> {
        if self
            .disabled_until
            .is_some_and(|deadline| deadline > Instant::now())
        {
            return Err(invalid("zygote circuit breaker is open").into());
        }

        let needs_spawn = self.zygote.as_mut().is_none_or(|zygote| {
            !zygote.matches_config(spec, &pythonpath, preload_modules, pytest_plugins)
        });
        if needs_spawn {
            self.zygote = Some(PytestZygote::spawn(
                spec,
                pythonpath,
                preload_modules,
                pytest_plugins,
            )?);
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        args: &[String],
        timeout_secs: u64,
        capture_dynamic_deps: bool,
        parallel_pgids: Option<&Mutex<BTreeSet<i32>>>,
    ) -> Result<BoundedOutput> {
        let Some(zygote) = self.zygote.as_mut() else {
            return Err(invalid("zygote unavailable").into());
        };
        match zygote.run_pytest(
            args,
            timeout_secs,
            &self.active_pgid,
            capture_dynamic_deps,
            parallel_pgids,
        ) {
            Ok(output) => {
                self.last_used = Some(Instant::now());
                self.failures = 0;
                self.last_error = None;
                Ok(output)
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(error.to_string());
                self.zygote = None;
                if self.failures >= 3 {
                    self.disabled_until = Some(Instant::now() + Duration::from_secs(60));
                }
                Err(error)
            }
        }
    }

    pub fn status(&self) -> serde_json::Value {
        let (pid, rss_bytes) = self
            .zygote
            .as_ref()
            .map(|zygote| {
                let pid = zygote.pid();
                (Some(pid), process_rss_bytes(pid))
            })
            .unwrap_or((None, None));
        serde_json::json!({
            "ready": self.zygote.is_some(),
            "failures": self.failures,
            "circuit_open": self.disabled_until.is_some_and(|deadline| deadline > Instant::now()),
            "last_error": self.last_error,
            "active_pgid": self.active_pgid.load(Ordering::Acquire),
            "plugin_errors": self.zygote.as_ref().map(|zygote| &zygote.plugin_errors),
            "runs": self.zygote.as_ref().map(|zygote| zygote.runs).unwrap_or(0),
            "pid": pid,
            "rss_bytes": rss_bytes,
            "idle_ms": self.last_used.map(|last| u64::try_from(last.elapsed().as_millis()).unwrap_or(u64::MAX)),
        })
    }

    pub fn cancel_active(&self) -> bool {
        let pgid = self.active_pgid.load(Ordering::Acquire);
        if pgid <= 0 {
            return false;
        }
        let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        result == 0
    }

    pub fn shutdown_if_idle(&mut self, timeout: Duration) -> bool {
        if self
            .last_used
            .is_some_and(|last_used| last_used.elapsed() >= timeout)
            && self.active_pgid.load(Ordering::Acquire) == 0
        {
            self.zygote = None;
            self.last_used = None;
            return true;
        }
        false
    }
}

fn process_rss_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

pub fn cancel_process_group(active_pgid: &AtomicI32) -> bool {
    let pgid = active_pgid.load(Ordering::Acquire);
    if pgid <= 0 {
        return false;
    }
    unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 }
}

pub fn resolve_python_bin(pytest_cmd: &[String]) -> PathBuf {
    let Some(first) = pytest_cmd.first() else {
        return PathBuf::from("python3");
    };
    let first_path = Path::new(first);
    if let Some(name) = first_path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with("python") {
            return first_path.to_path_buf();
        }

        if let Some(parent) = first_path.parent() {
            for candidate in ["python3", "python"] {
                let py = parent.join(candidate);
                if py.is_file() {
                    return py;
                }
            }
        }
    }
    PathBuf::from("python3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn readable_wait_has_a_bounded_timeout() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let error = wait_readable(
            reader.as_raw_fd(),
            Duration::from_millis(20),
            "test readiness",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("test readiness timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
