use crate::catalog::Spec;
use crate::workspace::BoundedOutput;
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

const ZYGOTE_SCRIPT: &str = r#"
import gc
import io
import json
import os
import select
import signal
import sys
import tempfile
import time

# 1. Pre-import pytest core and requested 3rd-party libraries (never workspace modules)
import pytest
import _pytest.config
import _pytest.main
import _pytest.junitxml
import _pytest.runner
import _pytest.python
import _pytest.fixtures

preload_env = os.environ.get("TCTX_PRELOAD_MODULES", "")
for mod_name in [m.strip() for m in preload_env.split(",") if m.strip()]:
    try:
        __import__(mod_name)
    except Exception:
        pass

gc.collect()
if hasattr(gc, "freeze"):
    gc.freeze()

proto_in = sys.stdin
proto_out = os.fdopen(os.dup(1), "w", buffering=1, encoding="utf-8")
devnull_fd = os.open(os.devnull, os.O_RDWR)
os.dup2(devnull_fd, 1)

proto_out.write("TCTX_ZYGOTE_READY\n")
proto_out.flush()

for raw_line in proto_in:
    line = raw_line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception as exc:
        proto_out.write(json.dumps({"error": f"invalid request: {exc}"}) + "\n")
        proto_out.flush()
        continue

    args = req.get("args", [])
    timeout_secs = float(req.get("timeout_secs", 180.0))
    started = time.perf_counter()

    r_fd, w_fd = os.pipe()
    out_tmp = tempfile.TemporaryFile(mode="w+b")
    err_tmp = tempfile.TemporaryFile(mode="w+b")

    pid = os.fork()
    if pid == 0:
        try:
            os.close(r_fd)
            os.setsid()
            gc.disable()
            gc.collect = lambda *a, **kw: 0
            os.dup2(out_tmp.fileno(), 1)
            os.dup2(err_tmp.fileno(), 2)
            sys.stdout = io.TextIOWrapper(os.fdopen(1, "wb", closefd=False), encoding="utf-8", errors="replace", write_through=True)
            sys.stderr = io.TextIOWrapper(os.fdopen(2, "wb", closefd=False), encoding="utf-8", errors="replace", write_through=True)
            try:
                code = int(pytest.main(args))
            except SystemExit as exc:
                code = int(exc.code) if isinstance(exc.code, int) else 1
            except Exception as exc:
                sys.stderr.write(f"zygote pytest exception: {exc}\n")
                code = 2
            sys.stdout.flush()
            sys.stderr.flush()
            os.write(w_fd, str(code).encode("ascii"))
        finally:
            try:
                os.close(w_fd)
            except Exception:
                pass
            os._exit(0)
    else:
        os.close(w_fd)
        timed_out = False
        ready, _, _ = select.select([r_fd], [], [], timeout_secs)
        if not ready:
            timed_out = True
            try:
                os.killpg(pid, signal.SIGKILL)
            except Exception:
                try:
                    os.kill(pid, signal.SIGKILL)
                except Exception:
                    pass
            exit_code = 124
        else:
            data = os.read(r_fd, 64)
            try:
                exit_code = int(data.decode("ascii").strip() or "1")
            except Exception:
                exit_code = 1
        os.close(r_fd)
        try:
            os.waitpid(pid, 0)
        except Exception:
            pass

        out_tmp.seek(0)
        err_tmp.seek(0)
        stdout_text = out_tmp.read(2 * 1024 * 1024).decode("utf-8", errors="replace")
        stderr_text = err_tmp.read(2 * 1024 * 1024).decode("utf-8", errors="replace")
        out_tmp.close()
        err_tmp.close()

        duration_ms = max(1, int(round((time.perf_counter() - started) * 1000.0)))
        reply = {
            "exit_code": exit_code,
            "stdout": stdout_text,
            "stderr": stderr_text,
            "duration_ms": duration_ms,
            "timed_out": timed_out,
        }
        proto_out.write(json.dumps(reply) + "\n")
        proto_out.flush()
"#;

#[derive(Serialize)]
struct ZygoteRequest<'a> {
    args: &'a [String],
    timeout_secs: u64,
}

#[derive(Deserialize)]
struct ZygoteReply {
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    duration_ms: Option<u64>,
    timed_out: Option<bool>,
    error: Option<String>,
}

pub struct PytestZygote {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    python_bin: PathBuf,
    pythonpath: std::ffi::OsString,
    preload_modules: String,
    root: PathBuf,
}

impl PytestZygote {
    pub fn spawn(
        spec: &Spec,
        pythonpath: std::ffi::OsString,
        preload_modules: &[String],
    ) -> Result<Self> {
        let python_bin = resolve_python_bin(&spec.effective_pytest_cmd());
        let preload_str = preload_modules.join(",");
        let mut cmd = Command::new(&python_bin);
        cmd.current_dir(&spec.root)
            .env("PYTHONPATH", &pythonpath)
            .env("TCTX_PRELOAD_MODULES", &preload_str)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg("-W")
            .arg("ignore")
            .arg("-c")
            .arg(ZYGOTE_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

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
        stdout.read_line(&mut banner)?;
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
            root: spec.root.clone(),
        })
    }

    pub fn matches_config(&self, spec: &Spec, pythonpath: &std::ffi::OsStr) -> bool {
        self.root == spec.root
            && self.pythonpath == pythonpath
            && self.python_bin == resolve_python_bin(&spec.effective_pytest_cmd())
    }

    pub fn run_pytest(&mut self, args: &[String], timeout_secs: u64) -> Result<BoundedOutput> {
        let started = Instant::now();
        let req = ZygoteRequest { args, timeout_secs };
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.stdin.write_all(&line)?;
        self.stdin.flush()?;

        let mut reply_line = String::new();
        let n = self.stdout.read_line(&mut reply_line)?;
        if n == 0 {
            return Err(invalid("zygote closed stdout unexpectedly").into());
        }
        let reply: ZygoteReply = serde_json::from_str(&reply_line)?;
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
        let _ = &self.preload_modules;
        Ok(BoundedOutput {
            status,
            stdout: reply.stdout.unwrap_or_default().into_bytes(),
            stderr: reply.stderr.unwrap_or_default().into_bytes(),
            duration_ms,
            timed_out: reply.timed_out.unwrap_or(false),
        })
    }

    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PytestZygote {
    fn drop(&mut self) {
        self.shutdown();
    }
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
