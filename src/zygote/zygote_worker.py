import sys
# Pop workspace/PYTHONPATH entries during stdlib bootstrap so a local workspace module like
# `resource.py` or `json.py` can NEVER shadow a bootstrap import before auditing is installed.
_orig_sys_path = list(sys.path)
_bootstrap_cwd = None
sys.path = [p for p in sys.path if p not in ("", ".") and not p.startswith(tuple(x for x in (sys.prefix, sys.base_prefix) if False))]
import os as _os_early
_parent_startup_root = _os_early.path.realpath(_os_early.getcwd())
_pp_raw = [p for p in _os_early.environ.get("PYTHONPATH", "").split(_os_early.pathsep) if p]
_pp_reals = {_parent_startup_root} | {_os_early.path.realpath(p) for p in _pp_raw}
sys.path = [
    p for p in _orig_sys_path
    if p not in ("", ".") and _os_early.path.realpath(p) not in _pp_reals
]

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
import sysconfig
import tempfile
import threading
import time

# Restore full `sys.path` now that bootstrap stdlib modules are imported safely, and install
# startup auditing BEFORE `sitecustomize`/`.pth`, `pytest`, and preloads.
# If ANY workspace/PYTHONPATH directory contains a module shadowing a bootstrap stdlib module
# (e.g. `resource.py`, `json.py`, `select.py`), fail closed so subprocess pytest runs with normal
# `sys.path` precedence instead of reusing a parent that already imported stdlib `resource`.
_bootstrap_mods = (
    "gc", "ctypes", "importlib", "io", "json", "math", "os", "select",
    "signal", "struct", "resource", "sysconfig", "tempfile", "threading", "time",
)
_bootstrap_shadowed = False
for _pp_dir in [_parent_startup_root] + _pp_raw:
    if not _pp_dir:
        continue
    for _bm in _bootstrap_mods:
        if os.path.isfile(os.path.join(_pp_dir, _bm + ".py")) or os.path.isfile(os.path.join(_pp_dir, _bm, "__init__.py")):
            _bootstrap_shadowed = True
            break
    if _bootstrap_shadowed:
        break
sys.path = _orig_sys_path

_parent_startup_active = True
_parent_startup_pp_prefixes = [_parent_startup_root] + [
    os.path.realpath(p) for p in os.environ.get("PYTHONPATH", "").split(os.pathsep) if p
]
_parent_startup_data_rels = set()
_parent_startup_data_abs = set()
_parent_startup_pre_stamps = {}
_parent_startup_untrackable = bool(_bootstrap_shadowed)
_startup_lock = threading.Lock()
_startup_tls = threading.local()
_parent_py_code_dirs = set()
try:
    for _k in ("stdlib", "platstdlib", "purelib", "platlib"):
        _p = sysconfig.get_path(_k)
        if _p:
            _rp = os.path.realpath(_p)
            if _rp != _parent_startup_root and not _rp.startswith(_parent_startup_root + os.sep):
                _parent_py_code_dirs.add(_rp)
except Exception:
    pass

_startup_orig_stat = os.stat
_startup_orig_lstat = os.lstat
_startup_orig_access = os.access
_startup_orig_readlink = os.readlink

def _startup_stamp(p):
    try:
        st = _startup_orig_stat(p)
        return (
            int(st.st_mtime_ns // 1_000_000_000),
            int(st.st_mtime_ns % 1_000_000_000),
            int(st.st_size),
            int(st.st_ino),
            int(st.st_ctime_ns // 1_000_000_000),
            int(st.st_ctime_ns % 1_000_000_000),
        )
    except Exception:
        return None

def _startup_has_symlink_component(raw_p):
    try:
        abs_p = raw_p if os.path.isabs(raw_p) else os.path.join(_parent_startup_root, raw_p)
        parts = abs_p.split(os.sep)
        cur = os.sep if abs_p.startswith(os.sep) else ""
        for part in parts:
            if not part or part == ".":
                continue
            cur = os.path.join(cur, part) if cur else part
            if part == "..":
                continue
            try:
                if os.path.islink(cur):
                    return True
            except Exception:
                pass
    except Exception:
        pass
    return False

def _startup_is_internal_caller():
    try:
        f = sys._getframe(2)
        while f is not None:
            fn = f.f_code.co_filename or ""
            # Distinguish executable `.pth` lines (`co_filename == "<string>"` invoked from
            # `site.addpackage`!) from the top-level zygote `-c` script itself (`f.f_back is None`).
            if fn == "<string>":
                if f.f_back is not None and (f.f_back.f_code.co_filename or "") in ("<frozen site>",) and f.f_back.f_code.co_name == "addpackage":
                    return False
                f = f.f_back
                continue
            if fn.startswith("<frozen importlib") or fn.startswith("<frozen _") or fn == "<frozen site>":
                return True
            if fn in ("<frozen posixpath>", "<frozen genericpath>", "<frozen ntpath>", "<frozen stat>"):
                f = f.f_back
                continue
            if fn and not fn.startswith("<"):
                try:
                    real_fn = os.path.realpath(fn)
                except Exception:
                    real_fn = fn
                # Workspace files (even under `packaging/` or `pytest/`) are NEVER internal machinery.
                if real_fn == _parent_startup_root or real_fn.startswith(_parent_startup_root + os.sep):
                    return False
                bn = os.path.basename(fn)
                if bn in ("posixpath.py", "genericpath.py", "ntpath.py", "pathlib.py", "stat.py"):
                    f = f.f_back
                    continue
                if bn in ("site.py", "sysconfig.py"):
                    return True
                norm_fn = real_fn.replace("\\", "/")
                if any(tok in norm_fn for tok in ("/_pytest/", "/pytest/", "/pluggy/", "/importlib/", "/pkg_resources/", "/packaging/")):
                    return True
                return False
            f = f.f_back
    except Exception:
        pass
    return False

_STARTUP_MUTABLE_ETC = frozenset({
    "/etc/hosts", "/etc/resolv.conf", "/etc/nsswitch.conf",
    "/etc/passwd", "/etc/group", "/etc/fstab", "/etc/localtime", "/etc/timezone",
    "/etc/mime.types", "/etc/services", "/etc/protocols", "/etc/gai.conf",
    "/etc/os-release", "/etc/magic",
})

def _startup_is_exempt_external(real_p, is_open, internal_caller=False, lex_p=None):
    # Do NOT exempt non-internal user/sitecustomize opens of `.py`/`.dist-info` files under
    # site-packages OR mutable kernel/host state (`/proc/`, `/sys/`, `/etc/hosts`, `/etc/resolv.conf`,
    # `/etc/localtime`, `/etc/os-release` even when symlinked to `/usr/lib/os-release`!);
    # only interpreter/pytest internals may read those without tainting preload.
    if real_p in ("/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"):
        return True
    if not internal_caller and (
        real_p in _STARTUP_MUTABLE_ETC
        or (lex_p and (lex_p in _STARTUP_MUTABLE_ETC or lex_p.startswith(("/proc/", "/sys/"))))
    ):
        return False
    if internal_caller and real_p in ("/etc/localtime", "/etc/hosts", "/etc/resolv.conf"):
        return True
    if not is_open and any(real_p == ep or real_p.startswith(ep + os.sep) for ep in _parent_py_code_dirs):
        return False
    if internal_caller and real_p.startswith(("/proc/", "/sys/", "/dev/")):
        return True
    return real_p.startswith(("/usr/lib/", "/usr/lib64/", "/lib/", "/lib64/", "/etc/ld.so"))

def _startup_map_pyc_to_src(real_p):
    # Use `importlib.util.source_from_cache` so optimized bytecode (`helper.cpython-312.opt-1.pyc`
    # under `PYTHONOPTIMIZE=1`) maps accurately to `helper.py`, not `helper.cpython-312.py`!
    try:
        import importlib.util as _iu
        _cand_src = _iu.source_from_cache(real_p)
        if isinstance(_cand_src, str) and os.path.isfile(_cand_src):
            return os.path.realpath(_cand_src)
    except Exception:
        pass
    _parent_dir = os.path.dirname(real_p)
    if os.path.basename(_parent_dir) != "__pycache__":
        return None
    _base = os.path.basename(real_p)
    _parts = _base.rsplit(".", 2)
    if len(_parts) != 3 or not _parts[0]:
        return None
    _cand_src = os.path.join(os.path.dirname(_parent_dir), _parts[0] + ".py")
    if os.path.isfile(_cand_src):
        return os.path.realpath(_cand_src)
    return None

def _parent_observe_startup_path(path_arg, is_open=False, is_metadata_or_dir=False):
    global _parent_startup_untrackable
    if not _parent_startup_active or getattr(_startup_tls, "in_obs", False):
        return
    _startup_tls.in_obs = True
    try:
        internal_caller = _startup_is_internal_caller()
        if path_arg is None:
            if is_metadata_or_dir and not internal_caller:
                with _startup_lock:
                    _parent_startup_untrackable = True
            return
        if isinstance(path_arg, int):
            if not internal_caller:
                with _startup_lock:
                    _parent_startup_untrackable = True
            return
        if not isinstance(path_arg, (str, bytes, os.PathLike)):
            return
        try:
            raw_p = os.fsdecode(os.fspath(path_arg))
        except Exception:
            return
        if not raw_p:
            return
        try:
            lex_p = os.path.normpath(os.path.abspath(raw_p))
            real_p = os.path.realpath(raw_p)
        except Exception:
            return
        # Always capture pre-read stamp BEFORE reading ANY `.py` / `.pyc` file opened during
        # startup (both workspace/PYTHONPATH AND external sitecustomize helpers!). When a `.pyc`
        # bytecode file is opened, stamp BOTH the `.pyc` file itself AND its mapped `.py` source
        # so in-place bytecode edits without touching `.py` also restart the Zygote and invalidate!
        if is_open and real_p.endswith((".py", ".pyc", ".pyo")):
            _startup_targets = []
            if real_p.endswith((".pyc", ".pyo")):
                if os.path.isfile(real_p):
                    _startup_targets.append(real_p)
                _mapped_py = _startup_map_pyc_to_src(real_p)
                if _mapped_py is None and (
                    any(real_p == pp or real_p.startswith(pp + os.sep) for pp in _parent_startup_pp_prefixes)
                    or not any(real_p.startswith(d) for d in _parent_py_code_dirs)
                ):
                    with _startup_lock:
                        _parent_startup_untrackable = True
                elif _mapped_py and os.path.isfile(_mapped_py):
                    _startup_targets.append(_mapped_py)
            elif os.path.isfile(real_p):
                _startup_targets.append(real_p)
            for _stamp_target in _startup_targets:
                _pre_mod = _startup_stamp(_stamp_target)
                with _startup_lock:
                    if _stamp_target not in _parent_startup_pre_stamps:
                        _parent_startup_pre_stamps[_stamp_target] = _pre_mod
                    if real_p.endswith((".pyc", ".pyo")) and not any(
                        _stamp_target.startswith(d) for d in _parent_py_code_dirs
                    ) or _stamp_target.endswith((".pyc", ".pyo")) or "/site-packages/" in _stamp_target.replace("\\", "/") or "/dist-packages/" in _stamp_target.replace("\\", "/"):
                        # Retain BOTH the `.pyc` bytecode AND its mapped `.py` source in parent
                        # reuse stamps so unloading a preloaded helper from `sys.modules` still
                        # restarts the Zygote parent when its `.py` source changes!
                        _parent_startup_data_abs.add(_stamp_target)
        if internal_caller:
            return
        in_ws_lex = lex_p == _parent_startup_root or lex_p.startswith(_parent_startup_root + os.sep)
        in_ws_real = real_p == _parent_startup_root or real_p.startswith(_parent_startup_root + os.sep)
        if not in_ws_lex and not in_ws_real:
            if not _startup_is_exempt_external(real_p, is_open, internal_caller=internal_caller, lex_p=lex_p):
                with _startup_lock:
                    _parent_startup_untrackable = True
            return
        # Direct (non-importlib) reads or probes of ANY workspace file (including `.py` read as
        # data) are tracked; metadata probes, directory enumerations, ANY symlink traversal
        # (including `link/../config` that normalizes equal), and workspace-root probes fail closed.
        if (
            is_metadata_or_dir
            or lex_p != real_p
            or real_p == _parent_startup_root
            or _startup_has_symlink_component(raw_p)
        ):
            with _startup_lock:
                _parent_startup_untrackable = True
            return
        if is_open and os.path.isfile(real_p):
            _rel = os.path.relpath(real_p, _parent_startup_root).replace(os.sep, "/")
            _pre = _startup_stamp(real_p)
            with _startup_lock:
                _parent_startup_data_rels.add(_rel)
                _parent_startup_data_abs.add(real_p)
                if real_p not in _parent_startup_pre_stamps:
                    _parent_startup_pre_stamps[real_p] = _pre
        else:
            with _startup_lock:
                _parent_startup_untrackable = True
    finally:
        _startup_tls.in_obs = False

def _parent_startup_audit(event, args):
    global _parent_startup_untrackable
    if not _parent_startup_active:
        return
    if event == "open":
        _parent_observe_startup_path(args[0] if args else None, is_open=True, is_metadata_or_dir=False)
    elif event in ("os.listdir", "os.scandir"):
        _parent_observe_startup_path(args[0] if args else None, is_open=False, is_metadata_or_dir=True)
    elif event in (
        "sqlite3.connect",
        "subprocess.Popen",
        "os.system",
        "os.posix_spawn",
        "os.fork",
        "os.forkpty",
        "os.exec",
        "socket.bind",
        "socket.connect",
        "socket.sendmsg",
        "socket.sendto",
        "socket.sethostname",
        "socket.getaddrinfo",
        "socket.gethostbyname",
        "socket.gethostbyaddr",
        "socket.getnameinfo",
        "http.client.connect",
        "urllib.Request",
        "ctypes.dlopen",
        "ctypes.dlsym",
        "ctypes.call_function",
        "ctypes.call_cdeclfunction",
        "ctypes.cdata",
        "ctypes.string_at",
        "ctypes.wstring_at",
    ):
        with _startup_lock:
            _parent_startup_untrackable = True

def _startup_wrapped_stat(path, *a, **kw):
    if _parent_startup_active:
        _parent_observe_startup_path(path, is_open=False, is_metadata_or_dir=True)
    return _startup_orig_stat(path, *a, **kw)

def _startup_wrapped_lstat(path, *a, **kw):
    if _parent_startup_active:
        _parent_observe_startup_path(path, is_open=False, is_metadata_or_dir=True)
    return _startup_orig_lstat(path, *a, **kw)

def _startup_wrapped_access(path, *a, **kw):
    if _parent_startup_active:
        _parent_observe_startup_path(path, is_open=False, is_metadata_or_dir=True)
    return _startup_orig_access(path, *a, **kw)

def _startup_wrapped_readlink(path, *a, **kw):
    if _parent_startup_active:
        _parent_observe_startup_path(path, is_open=False, is_metadata_or_dir=True)
    return _startup_orig_readlink(path, *a, **kw)

os.stat = _startup_wrapped_stat
os.lstat = _startup_wrapped_lstat
os.access = _startup_wrapped_access
os.readlink = _startup_wrapped_readlink
sys.addaudithook(_parent_startup_audit)

# Only fail closed for genuinely suspicious custom `.pth` code (`sitecustomize`/`usercustomize`
# or executable `.pth` lines that do NOT match exact known standard editable/namespace/distutils
# packaging shims — rejecting arbitrary appended statements after `;`).
def _is_known_safe_pth_line(s):
    s = s.strip()
    if s in (
        "import _distutils_hack; _distutils_hack.add_shim()",
        "import _virtualenv",
        "import sys; sys.__plen = len(sys.path)",
        "import sys; new = sys.path[sys.__plen:]; del sys.path[sys.__plen:]; p = getattr(sys, '__egginsert', 0); sys.path[p:p] = new; sys.__egginsert = p + len(new)",
    ):
        return True
    # PEP 660 setuptools/hatchling/maturin single-statement editable finder shims:
    # `import __editable___foo_finder; __editable___foo_finder.install()`
    if (s.startswith("import __editable__") or s.startswith("import _editable_impl_")) and s.count(";") <= 1:
        parts = [p.strip() for p in s.split(";")]
        if len(parts) == 1:
            return parts[0].isidentifier() or parts[0][7:].strip().isidentifier()
        if len(parts) == 2:
            mod = parts[0][7:].strip()
            return mod.isidentifier() and parts[1] == f"{mod}.install()"
    # Standard setuptools namespace-package `.pth` shim (validate exact statement structure):
    _ns_pfx = "import sys, types, os;has_mfs = sys.version_info > (3, 5);p = os.path.join(sys._getframe(1).f_locals['sitedir'], *("
    _ns_mid = ");importlib = has_mfs and __import__('importlib.util');has_mfs and __import__('importlib.machinery');m = has_mfs and sys.modules.setdefault("
    if s.startswith(_ns_pfx) and _ns_mid in s and s.endswith(" or None") and s.count(";") == 5:
        return True
    return False

if "sitecustomize" in sys.modules or "usercustomize" in sys.modules:
    _parent_startup_untrackable = True
import site
_startup_tls.in_obs = True
try:
    for _sp_fn in (getattr(site, "getsitepackages", None), getattr(site, "getusersitepackages", None)):
        if callable(_sp_fn):
            try:
                _res = _sp_fn()
                _items = [_res] if isinstance(_res, str) else list(_res)
                for _sp in _items:
                    if _sp:
                        _rsp = os.path.realpath(_sp)
                        if _rsp != _parent_startup_root and not _rsp.startswith(_parent_startup_root + os.sep):
                            _parent_py_code_dirs.add(_rsp)
                        if os.path.isdir(_rsp):
                            try:
                                for _ent in os.listdir(_rsp):
                                    if _ent.endswith(".pth"):
                                        _pth_p = os.path.join(_rsp, _ent)
                                        try:
                                            with open(_pth_p, "r", encoding="utf-8", errors="ignore") as _pf:
                                                for _pl in _pf:
                                                    _s = _pl.lstrip()
                                                    if _s.startswith(("import ", "import\t")) and not _is_known_safe_pth_line(_s):
                                                        _parent_startup_untrackable = True
                                                        break
                                        except Exception:
                                            pass
                            except Exception:
                                pass
            except Exception:
                pass
finally:
    _startup_tls.in_obs = False

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

def _trim_json_str_to_budget(text, max_json_bytes):
    if max_json_bytes <= 2 or not text:
        return ""
    encoded = json.dumps(text).encode("utf-8")
    if len(encoded) <= max_json_bytes:
        return text
    lo, hi = 0, len(text)
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if len(json.dumps(text[:mid]).encode("utf-8")) <= max_json_bytes:
            lo = mid
        else:
            hi = mid - 1
    return text[:lo]

def write_frame(stream, value):
    body = json.dumps(value, separators=(",", ":")).encode("utf-8")
    if len(body) > MAX_FRAME and isinstance(value, dict) and ("stdout" in value or "stderr" in value):
        trimmed = dict(value)
        orig_out = trimmed.get("stdout", "") or ""
        orig_err = trimmed.get("stderr", "") or ""
        trimmed["stdout"] = ""
        trimmed["stderr"] = ""
        base_body = json.dumps(trimmed, separators=(",", ":")).encode("utf-8")
        if len(base_body) + 64 < MAX_FRAME:
            budget = MAX_FRAME - len(base_body) - 64
            out_budget = budget // 2 if orig_err else budget
            err_budget = budget - out_budget
            trimmed["stdout"] = _trim_json_str_to_budget(orig_out, out_budget)
            trimmed["stderr"] = _trim_json_str_to_budget(orig_err, err_budget)
            body = json.dumps(trimmed, separators=(",", ":")).encode("utf-8")
    if len(body) > MAX_FRAME:
        body = json.dumps({"error": "zygote response frame too large"}).encode("utf-8")
    stream.write(struct.pack(">I", len(body)))
    stream.write(body)
    stream.flush()

def resolve_plugin(value):
    import importlib.metadata as _im
    return _im.EntryPoint(name="", value=value.strip(), group="pytest11").load()

safe_preload = {
    "fastapi", "starlette", "pydantic", "httpx", "requests",
    "openai", "azure", "dotenv", "yaml", "PIL",
    "numpy", "scipy", "pandas", "sqlalchemy", "aiohttp",
    "attrs", "cryptography", "orjson", "ujson",
}
for mod_name in [m.strip() for m in os.environ.get("TCTX_PRELOAD_MODULES", "").split(",") if m.strip()]:
    if mod_name.split(".", 1)[0] not in safe_preload:
        continue
    try:
        importlib.import_module(mod_name)
    except Exception:
        pass

_parent_startup_active = False
os.stat = _startup_orig_stat
os.lstat = _startup_orig_lstat
os.access = _startup_orig_access
os.readlink = _startup_orig_readlink
# If any preloaded module left a background thread alive after startup, fork() is unsafe and
# subsequent thread reads would escape auditing: mark startup untrackable so Rust falls back to
# a fresh subprocess.
if threading.active_count() > 1:
    _parent_startup_untrackable = True
# Validate pre-read stamps captured before startup opens completed: if any file changed during
# startup preload, mark startup untrackable so the parent is never reused or cached.
for _sp_abs, _sp_pre in list(_parent_startup_pre_stamps.items()):
    if _sp_pre is None or _startup_stamp(_sp_abs) != _sp_pre:
        _parent_startup_untrackable = True

_orig_disable_autoload = os.environ.get("PYTEST_DISABLE_PLUGIN_AUTOLOAD")
_autoload_disabled = bool(_orig_disable_autoload)
plugins = []
deferred_plugins = []
plugin_errors = []
workspace_root = os.path.realpath(os.getcwd())

import importlib.util as _importlib_util

_raw_plugin_entries = [p.strip() for p in os.environ.get("TCTX_PYTEST_PLUGINS", "").split(",") if p.strip()]
_plugin_discovery_unavailable = "__TCTX_PLUGIN_DISCOVERY_UNAVAILABLE__" in _raw_plugin_entries
if not _autoload_disabled and not _plugin_discovery_unavailable:
    # Defer ALL third-party pytest11 plugins to the forked child (resolved lazily inside
    # `load_setuptools_entrypoints` AFTER `-p` plugins) so plugin import-time reads are audited,
    # explicit `-p` load ordering is preserved, and module globals never go stale in the parent.
    for raw_entry in _raw_plugin_entries:
        if "=" in raw_entry:
            ep_name, plugin_value = raw_entry.split("=", 1)
        else:
            ep_name, plugin_value = None, raw_entry
        deferred_plugins.append((ep_name, plugin_value))

# Track the exact `pytest11` plugin source trees that `python_runtime_identity` SHA-256 hashes
# (`_dir_sig`/`_file_sig`) and `stamp_plugin_locs_digest` stat-checks on every run, so plugin
# self-introspection (e.g. `anyio` reading its own `__init__.py` via `inspect`/`linecache`) stays
# cacheable while plugin reads of unfingerprinted packages (e.g. `other_pkg/config.py`) fail closed.
_fingerprinted_plugin_dirs = set()
_fingerprinted_plugin_files = set()
for _ep_n, _ep_v in deferred_plugins:
    try:
        _top = _ep_v.split(":", 1)[0].strip().split(".", 1)[0].strip()
        _sp = _importlib_util.find_spec(_top) if _top else None
        if _sp is not None:
            for _loc in list(getattr(_sp, "submodule_search_locations", None) or []):
                if isinstance(_loc, str) and os.path.isdir(_loc):
                    _rloc = os.path.realpath(_loc)
                    if os.path.basename(_rloc.rstrip("/\\")) not in ("site-packages", "dist-packages"):
                        _fingerprinted_plugin_dirs.add(_rloc)
            _orig = getattr(_sp, "origin", None)
            if isinstance(_orig, str) and os.path.isfile(_orig):
                _fingerprinted_plugin_files.add(os.path.realpath(_orig))
    except Exception:
        pass
_fingerprinted_plugin_dirs = tuple(sorted(_fingerprinted_plugin_dirs))
_fingerprinted_plugin_files = frozenset(_fingerprinted_plugin_files)
_FINGERPRINT_SKIP_DIRS = frozenset({
    "__pycache__", ".git", ".hg", ".svn", ".venv", "node_modules", "target",
    ".pytest_cache", ".mypy_cache", ".ruff_cache", ".eggs",
})
_STD_DIST_METADATA_FILES = frozenset({
    "METADATA", "PKG-INFO", "entry_points.txt", "RECORD",
    "direct_url.json", "INSTALLER", "WHEEL", "top_level.txt",
    "requires.txt", "dependency_links.txt",
})
_STAMPED_DIST_ROOTS = frozenset(
    os.path.realpath(p)
    for p in sys.path
    if isinstance(p, str)
    and p
    and os.path.isdir(p)
    and os.path.realpath(p) != workspace_root
    and not os.path.realpath(p).startswith(workspace_root + os.sep)
)

def _in_fingerprinted_plugin_tree(real_norm, traversed_symlink):
    if traversed_symlink:
        return False
    if real_norm in _fingerprinted_plugin_files:
        return True
    for d in _fingerprinted_plugin_dirs:
        if real_norm.startswith(d + os.sep):
            rel_parts = os.path.relpath(real_norm, d).replace("\\", "/").split("/")
            if any(
                part in _FINGERPRINT_SKIP_DIRS or part.endswith((".egg-info", ".dist-info"))
                for part in rel_parts[:-1]
            ):
                return False
            leaf = rel_parts[-1]
            if leaf.endswith((".pyc", ".pyo")) or leaf == ".DS_Store":
                return False
            return True
    return False

audit_paths = None
audit_shared_paths = None
audit_module_paths = None
audit_incomplete = False
audit_current_node = None
audit_current_file = None
audit_in_test_phase = False
audit_in_call = False
audit_by_node = {}
_ext_mod_pre_stamps = None
_ext_mod_pre_targets = None
_ext_mod_observed_files = None
_stat_tls = threading.local()

import importlib.util as _importlib_util

import site as _site_mod
import tempfile as _tempfile

_STDLIB_DIRS = tuple(
    d.rstrip(os.sep) + os.sep
    for d in {
        os.path.realpath(os.path.dirname(os.__file__)),
        os.path.dirname(os.__file__),
    }
    if d
)

_SITE_PKG_DIRS = []
try:
    for _sp in (_site_mod.getsitepackages() or []):
        if _sp:
            _SITE_PKG_DIRS.append(_sp)
            _SITE_PKG_DIRS.append(os.path.realpath(_sp))
except Exception:
    pass
try:
    _usp = _site_mod.getusersitepackages()
    if _usp:
        _SITE_PKG_DIRS.append(_usp)
        _SITE_PKG_DIRS.append(os.path.realpath(_usp))
except Exception:
    pass

_PY_PREFIX_SUBDIRS = tuple(
    os.path.join(pfx, sub).rstrip(os.sep) + os.sep
    for pfx in {sys.prefix, sys.base_prefix}
    if pfx
    for sub in ("lib", "lib64", "include", "bin")
) + tuple(d.rstrip(os.sep) + os.sep for d in set(_SITE_PKG_DIRS) if d)

_SYSTEM_EXACT_EXEMPT = frozenset({
    "/etc/localtime", "/etc/timezone", "/etc/hosts", "/etc/resolv.conf",
    "/etc/nsswitch.conf", "/etc/passwd", "/etc/group", "/etc/protocols",
    "/etc/services", "/etc/mime.types", "/etc/ld.so.cache", "/etc/ld.so.conf",
    "/etc/gai.conf", "/etc/magic", "/etc/os-release", "/etc/fstab",
} | {
    os.path.join(pfx, "pyvenv.cfg")
    for pfx in {sys.prefix, sys.base_prefix}
    if pfx
} | {
    d.rstrip(os.sep) for d in set(_SITE_PKG_DIRS) if d
})

_SYSTEM_EXTERNAL_PREFIXES = tuple(
    p.rstrip(os.sep) + os.sep
    for p in (
        "/usr/lib", "/usr/lib64", "/usr/share/zoneinfo", "/usr/share/locale",
        "/usr/share/ca-certificates", "/usr/share/misc", "/usr/include",
        "/lib", "/lib64", "/proc", "/sys", "/dev",
        "/etc/ssl", "/etc/ca-certificates", "/etc/pki", "/etc/ld.so.conf.d",
    )
    if p
) + _STDLIB_DIRS + _PY_PREFIX_SUBDIRS

_current_pytest_config = None
_current_pytest_basetemp = None
_resolving_basetemp = False

def _resolve_current_pytest_basetemp():
    global _current_pytest_basetemp, _resolving_basetemp
    if _current_pytest_basetemp is not None:
        return _current_pytest_basetemp
    if _resolving_basetemp or _current_pytest_config is None:
        return None
    factory = getattr(_current_pytest_config, "_tmp_path_factory", None)
    if factory is None:
        return None
    bt = getattr(factory, "_basetemp", None)
    if bt is None:
        return None
    _resolving_basetemp = True
    try:
        _current_pytest_basetemp = os.path.realpath(str(bt))
    except Exception:
        return None
    finally:
        _resolving_basetemp = False
    return _current_pytest_basetemp

def _is_stdlib_file(real_f):
    norm_f = real_f.replace("\\", "/")
    if "/site-packages/" in norm_f or "/dist-packages/" in norm_f:
        return False
    return any(real_f.startswith(d) for d in _STDLIB_DIRS)

_DIRECT_OPEN_OS_EXEMPT_PREFIXES = (
    "/dev/null", "/dev/zero",
    "/etc/ld.so.conf.d/",
)

def _is_direct_workspace_open(skip_frames=2):
    # Returns True when workspace code directly calls `open()` / `Path.read_text()` (through only
    # thin path/io wrappers), distinguishing explicit user reads of mutable files under
    # `site-packages` from library-internal wheel-asset reads (e.g. `certifi`/`tzdata`).
    try:
        frame = sys._getframe(skip_frames)
        for _ in range(12):
            if frame is None:
                break
            fname = frame.f_code.co_filename or ""
            if not fname or fname in _FROZEN_PATH_WRAPPERS:
                frame = frame.f_back
                continue
            if fname.startswith("<"):
                return False
            real_f = os.path.realpath(fname)
            if os.path.basename(real_f) in _PATH_WRAPPER_BASENAMES:
                frame = frame.f_back
                continue
            if real_f.startswith(workspace_root + os.sep):
                rel_f = os.path.relpath(real_f, workspace_root).replace("\\", "/")
                return not any(part in _AUDIT_IGNORED_PARTS for part in rel_f.split("/"))
            return False
    except Exception:
        pass
    return False

_MUTABLE_ETC_PATHS = frozenset({
    "/etc/hosts", "/etc/resolv.conf", "/etc/nsswitch.conf",
    "/etc/passwd", "/etc/group", "/etc/fstab", "/etc/localtime", "/etc/timezone",
    "/etc/mime.types", "/etc/services", "/etc/protocols", "/etc/gai.conf",
    "/etc/os-release", "/etc/magic",
})

def _is_external_user_fixture_path(real_norm, allow_dirs=False, explicit_open=False, direct_user_open=False, lex_norm=None):
    # Any explicit user-code or plugin-fixture open/stat/listdir outside `workspace_root` that is
    # NOT an OS/interpreter runtime path or pytest `tmp_path` (e.g. `/tmp/shared-fixture.json`,
    # `/tmp/fixture.py`, `/etc/myapp/config.json`) cannot be tracked by the workspace AST/watcher.
    # Direct workspace opens of mutable files under `site-packages`/`dist-packages` OR mutable
    # `/etc/*` host config files (`/etc/hosts`, `/etc/resolv.conf`, `/etc/os-release` even when
    # symlinked to `/usr/lib/os-release`!) are also flagged.
    if not real_norm or not os.path.isabs(real_norm):
        return False
    basetemp = _resolve_current_pytest_basetemp()
    if basetemp is not None and (
        real_norm == basetemp
        or real_norm.startswith(basetemp + os.sep)
    ):
        # Exempt ONLY the verified current-run pytest `tmp_path` scratch directory, NOT
        # persistent files under `/tmp/pytest-of-<user>/pytest-current/` from other runs!
        return False
    if not allow_dirs and not explicit_open and real_norm.endswith((".py", ".pyc", ".pyo", ".so", ".dylib", ".dll")):
        return False
    if (explicit_open or allow_dirs) and (
        real_norm.startswith(("/proc/", "/sys/"))
        or real_norm in _MUTABLE_ETC_PATHS
        or (lex_norm and (lex_norm.startswith(("/proc/", "/sys/")) or lex_norm in _MUTABLE_ETC_PATHS))
    ):
        # Workspace/plugin-initiated reads or directory enumerations (direct or indirect via
        # stdlib such as `linecache.getline`, `platform.freedesktop_os_release()`, or
        # `os.listdir("/sys/class/net")`) of mutable `/proc/`, `/sys/`, or host `/etc/*` files
        # (checked on BOTH lexical and resolved paths so `/etc/os-release -> /usr/lib/os-release`
        # is never exempted!) can never be safely cached.
        return True
    if (explicit_open or allow_dirs) and any(
        (p == pfx or p.startswith(pfx + "/"))
        for p in (real_norm, lex_norm)
        if p
        for pfx in ("/etc/ssl", "/etc/ca-certificates", "/etc/pki")
    ):
        # User/plugin reads (direct or indirect via stdlib such as `configparser.read`) AND
        # directory enumerations under mutable certificate stores (`/etc/ssl`, `/etc/ca-certificates`,
        # `/etc/pki`) can never be safely cached.
        return True
    if direct_user_open:
        if real_norm in _MUTABLE_ETC_PATHS:
            return True
        if real_norm in _SYSTEM_EXACT_EXEMPT:
            return False
        norm_slash = real_norm.replace("\\", "/")
        if "/site-packages/" in norm_slash or "/dist-packages/" in norm_slash:
            return True
        return not any(real_norm.startswith(pfx) for pfx in _DIRECT_OPEN_OS_EXEMPT_PREFIXES)
    if real_norm in _SYSTEM_EXACT_EXEMPT:
        return False
    return not any(real_norm.startswith(pfx) for pfx in _SYSTEM_EXTERNAL_PREFIXES)

_AUDIT_IGNORED_PARTS = (
    ".git", ".hg", ".svn", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".tox", ".nox", ".venv", "__pycache__", "venv", "node_modules",
)

_PATH_WRAPPER_BASENAMES = (
    "os.py", "posixpath.py", "ntpath.py", "genericpath.py",
    "pathlib.py", "glob.py", "io.py", "codecs.py",
)

_FROZEN_PATH_WRAPPERS = (
    "<frozen os>", "<frozen posixpath>", "<frozen ntpath>", "<frozen genericpath>",
    "<frozen pathlib>", "<frozen glob>", "<frozen io>", "<frozen codecs>",
)

_RUNNER_BOUNDARY_PREFIXES = (
    "<frozen importlib",
)

def _is_runner_internal_file(real_f):
    norm_f = real_f.replace("\\", "/")
    if "/_pytest/" in norm_f or "/pluggy/" in norm_f or "/importlib/" in norm_f:
        return True
    if "/site-packages/" in norm_f or "/dist-packages/" in norm_f:
        for pkg in ("/iniconfig/", "/packaging/", "/py/"):
            if pkg in norm_f:
                return True
    return False

def _caller_below_pytest(skip_frames=2):
    # Walk up frames through stdlib/3rd-party helpers (e.g. `shutil.which`, `pathlib`, `glob`)
    # until hitting either a workspace frame (user/project code) or a pytest/importlib runner
    # boundary. Workspace membership is checked BEFORE runner exemptions so project paths like
    # `/repo/py/tests/` are never misclassified as runner internals.
    saw_plugin = False
    try:
        frame = sys._getframe(skip_frames)
        for _ in range(24):
            if frame is None:
                break
            fname = frame.f_code.co_filename or ""
            if not fname:
                frame = frame.f_back
                continue
            if fname in _FROZEN_PATH_WRAPPERS:
                frame = frame.f_back
                continue
            if any(fname.startswith(pfx) for pfx in _RUNNER_BOUNDARY_PREFIXES):
                return (None, saw_plugin)
            if not fname.startswith("<"):
                real_f = os.path.realpath(fname)
                if real_f.startswith(workspace_root + os.sep):
                    rel_f = os.path.relpath(real_f, workspace_root).replace("\\", "/")
                    if not any(part in _AUDIT_IGNORED_PARTS for part in rel_f.split("/")):
                        # Preserve `saw_plugin` so a workspace test calling into an external library
                        # that reads mutable site-packages data still flags `is_plugin_data_open`.
                        return (rel_f, saw_plugin)
                if _is_runner_internal_file(real_f):
                    return (None, saw_plugin)
                if not _is_stdlib_file(real_f) and os.path.basename(real_f) not in _PATH_WRAPPER_BASENAMES:
                    saw_plugin = True
            frame = frame.f_back
    except Exception:
        pass
    return (None, saw_plugin)

def _has_workspace_caller_below_pytest(skip_frames=2):
    rel_f, _ = _caller_below_pytest(skip_frames + 1)
    return rel_f

def _map_pyc_to_source(norm_path):
    # Map PEP 3147 `__pycache__/foo.bar.cpython-312.pyc` via `importlib.util.source_from_cache`
    # so dotted module names map to `foo.bar.py` instead of `foo.py`.
    parent = os.path.dirname(norm_path)
    if os.path.basename(parent) == "__pycache__":
        try:
            candidate = _importlib_util.source_from_cache(norm_path)
            if os.path.isfile(candidate):
                return candidate
        except Exception:
            pass
        return None
    for ext in (".pyc", ".pyo"):
        if norm_path.endswith(ext):
            candidate = norm_path[:-len(ext)] + ".py"
            if os.path.isfile(candidate):
                return candidate
    return None

def _is_audit_worker_thread():
    # Pytest itself always executes on the main thread; any non-main thread running while
    # `audit_paths` is active (during EITHER collection OR test execution!) was spawned by
    # test/subject code (e.g. `ThreadPoolExecutor.submit(os.path.exists, ...)` or
    # `pool.submit(open, ...)`) and its call stack may contain only stdlib frames.
    return audit_paths is not None and threading.current_thread() is not threading.main_thread()

def _record_workspace_dep(rel, is_open, caller_rel=None):
    global audit_incomplete
    if any(part in _AUDIT_IGNORED_PARTS for part in rel.split("/")):
        # Explicit user-code, plugin-fixture, or worker-thread opens under ignored directories
        # (e.g. `node_modules/pkg/data.json` or `.venv/.../*.py`) are not tracked by the AST graph.
        if is_open:
            if caller_rel is not None or _is_audit_worker_thread():
                audit_incomplete = True
            else:
                ws_caller, is_plugin = _caller_below_pytest(3)
                if ws_caller is not None or is_plugin:
                    audit_incomplete = True
        return
    if not rel.endswith((
        ".py", ".json", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".sql", ".rego", ".csv",
        ".snap", ".golden", ".proto", ".graphql", ".gql", ".txt",
        ".css", ".scss", ".sass", ".less", ".styl", ".stylus", ".svg", ".mdx", ".html",
    )):
        if is_open:
            audit_incomplete = True
        return
    audit_paths.add(rel)
    if audit_current_node is not None:
        audit_by_node.setdefault(audit_current_node, set()).add(rel)
    if not audit_in_call and caller_rel is not None and audit_module_paths is not None:
        # Collection-time or fixture-time read by a specific workspace module: attribute to that
        # module so unrelated tests in the same batch remain isolated.
        if caller_rel.endswith("/conftest.py") or caller_rel == "conftest.py":
            if audit_shared_paths is not None:
                audit_shared_paths.add(rel)
        else:
            audit_module_paths.setdefault(caller_rel, set()).add(rel)
        return
    is_shared = not audit_in_call
    if not is_shared and audit_module_paths is not None:
        try:
            frame = sys._getframe(2)
            for _ in range(20):
                if frame is None:
                    break
                fname = frame.f_code.co_filename
                if fname:
                    real_f = os.path.realpath(fname)
                    if real_f.startswith(workspace_root + os.sep):
                        mod_rel = os.path.relpath(real_f, workspace_root).replace("\\", "/")
                        if mod_rel.endswith("/conftest.py") or mod_rel == "conftest.py":
                            is_shared = True
                            break
                        audit_module_paths.setdefault(mod_rel, set()).add(rel)
                frame = frame.f_back
        except Exception:
            pass
    if is_shared and audit_shared_paths is not None:
        audit_shared_paths.add(rel)

def audit_hook(event, args):
    global audit_paths, audit_shared_paths, audit_module_paths, audit_incomplete, audit_current_node, audit_current_file, audit_in_test_phase, audit_in_call, audit_by_node
    if audit_paths is None:
        return
    if event in (
        "subprocess.Popen",
        "os.system",
        "os.posix_spawn",
        "os.fork",
        "os.forkpty",
        "os.exec",
        "socket.bind",
        "socket.connect",
        "socket.sendmsg",
        "socket.sendto",
        "socket.sethostname",
        "socket.getaddrinfo",
        "socket.gethostbyname",
        "socket.gethostbyaddr",
        "socket.getnameinfo",
        "http.client.connect",
        "urllib.Request",
        "ctypes.dlopen",
        "ctypes.dlsym",
        "ctypes.call_function",
        "ctypes.call_cdeclfunction",
        "ctypes.cdata",
        "ctypes.string_at",
        "ctypes.wstring_at",
    ):
        # Child-process reads, network/service calls, and native ctypes FFI cannot be captured in the local file hash.
        audit_incomplete = True
        return
    if event == "sqlite3.connect":
        # Native sqlite3 connections (including `:memory:`) can execute `ATTACH DATABASE` without
        # emitting another `sqlite3.connect` audit event, and read SQLite/WAL files directly in C.
        # Mark dependency capture incomplete so SQLite-backed tests are never replayed stale!
        audit_incomplete = True
        return
    if event in ("os.listdir", "os.scandir", "glob.glob"):
        # Track directory enumeration both during test phase AND during collection when triggered
        # by user/project code, pytest11 plugins, OR worker threads spawned during collection/test!
        user_caller, is_plugin = _caller_below_pytest(2)
        is_worker_thread = _is_audit_worker_thread()
        if not audit_in_test_phase and user_caller is None and not is_plugin and not is_worker_thread:
            return
        dir_cand = args[0] if args else None
        if isinstance(dir_cand, int):
            audit_incomplete = True
            return
        if dir_cand is None:
            # No-argument `os.listdir()` / `os.scandir()` enumerates `os.getcwd()`, which may
            # have been changed to an external directory via `os.chdir("/tmp/shared")`.
            dir_cand = "."
        if isinstance(dir_cand, (str, bytes, os.PathLike)):
            try:
                raw_dir = os.fsdecode(os.fspath(dir_cand))
                _prev_tls = getattr(_stat_tls, "active", False)
                _stat_tls.active = True
                try:
                    cwd_real = os.path.realpath(os.getcwd())
                    raw_abs_dir = raw_dir if os.path.isabs(raw_dir) else os.path.join(cwd_real, raw_dir)
                    norm_dir = os.path.realpath(raw_abs_dir)
                    lex_dir = os.path.normpath(raw_abs_dir)
                finally:
                    _stat_tls.active = _prev_tls
                in_ws_real = norm_dir == workspace_root or norm_dir.startswith(workspace_root + os.sep)
                in_ws_lex = lex_dir == workspace_root or lex_dir.startswith(workspace_root + os.sep)
                if in_ws_real or in_ws_lex:
                    rel_dir = os.path.relpath(norm_dir if in_ws_real else lex_dir, workspace_root).replace("\\", "/")
                    if user_caller is not None or is_worker_thread or is_plugin or not any(part in _AUDIT_IGNORED_PARTS for part in rel_dir.split("/")):
                        audit_incomplete = True
                elif (user_caller is not None or is_plugin or is_worker_thread) and _is_external_user_fixture_path(norm_dir, allow_dirs=True, lex_norm=lex_dir):
                    audit_incomplete = True
            except Exception:
                pass
        return
    candidate = None
    if event == "open" and args:
        candidate = args[0]
        # Ignore write-only opens so tests writing output files never invalidate themselves endlessly.
        mode = args[1] if len(args) > 1 else None
        flags = args[2] if len(args) > 2 else None
        if isinstance(mode, str) and mode and mode[0] in ("w", "a", "x") and "+" not in mode:
            return
        if isinstance(flags, int) and (flags & os.O_WRONLY) != 0 and (flags & os.O_RDWR) == 0:
            return
    elif event == "import" and len(args) > 1:
        candidate = args[1]
    if not isinstance(candidate, (str, bytes, os.PathLike)):
        return
    try:
        raw_path = os.fsdecode(os.fspath(candidate))
    except Exception:
        return
    # Walk path components BEFORE collapsing `..` so:
    # 1. `workspace/link/../input.json` (where `link -> dir`, lex_norm == real_norm) is still
    #    caught as a symlink traversal;
    # 2. `workspace/input.json` -> `/outside/file` (workspace-originating symlink pointing outside)
    #    is still flagged incomplete instead of silently returning;
    # 3. external symlinks pointing into `workspace_root` are also flagged incomplete.
    _prev_tls = getattr(_stat_tls, "active", False)
    _stat_tls.active = True
    try:
        cwd_real = os.path.realpath(os.getcwd())
        raw_abs = raw_path if os.path.isabs(raw_path) else os.path.join(cwd_real, raw_path)
        real_norm = os.path.realpath(raw_abs)
        lex_norm = os.path.normpath(raw_abs)
        traversed_symlink = lex_norm != real_norm
        if not traversed_symlink:
            # Check each prefix component of `raw_abs` (without collapsing `..`) for symlinks.
            cur_prefix = os.sep if raw_abs.startswith(os.sep) else ""
            for part in raw_abs.split(os.sep):
                if not part or part == ".":
                    continue
                cur_prefix = os.path.join(cur_prefix, part) if cur_prefix else part
                if part != ".." and os.path.islink(cur_prefix):
                    traversed_symlink = True
                    break
    except Exception:
        return
    finally:
        _stat_tls.active = _prev_tls
    in_ws_real = real_norm.startswith(workspace_root + os.sep)
    in_ws_lex = lex_norm.startswith(workspace_root + os.sep)
    # Record pre-read stamps for `site-packages`/`dist-packages` modules (INCLUDING workspace-local
    # virtualenvs like `.venv/lib/python3.12/site-packages/...` whose ignored directory is NOT
    # tracked by the workspace AST graph!) BEFORE the open completes!
    if event == "open" and _ext_mod_pre_stamps is not None:
        _lex_sl = lex_norm.replace("\\", "/")
        _real_sl = real_norm.replace("\\", "/")
        if (
            "/site-packages/" in _lex_sl
            or "/dist-packages/" in _lex_sl
            or "/site-packages/" in _real_sl
            or "/dist-packages/" in _real_sl
        ):
            def _pre_stamp_path_and_ancestors(_lp, _rp):
                global audit_incomplete
                if _ext_mod_pre_targets is not None:
                    _prev_t = _ext_mod_pre_targets.get(_lp)
                    if _prev_t is not None and _prev_t != _rp:
                        audit_incomplete = True
                    else:
                        _ext_mod_pre_targets[_lp] = _rp
                if _rp not in _ext_mod_pre_stamps:
                    _ext_mod_pre_stamps[_rp] = _startup_stamp(_rp)
                for _base_p in (_lp, _rp):
                    _cur_d = os.path.dirname(_base_p)
                    for _ in range(16):
                        if not _cur_d or _cur_d == os.path.dirname(_cur_d):
                            break
                        if os.path.basename(_cur_d) in ("site-packages", "dist-packages"):
                            break
                        _cd_sl = _cur_d.replace("\\", "/")
                        if "/site-packages/" not in _cd_sl and "/dist-packages/" not in _cd_sl:
                            break
                        _cd_r = os.path.realpath(_cur_d)
                        if _ext_mod_pre_targets is not None:
                            _prev_dt = _ext_mod_pre_targets.get(_cur_d)
                            if _prev_dt is not None and _prev_dt != _cd_r:
                                audit_incomplete = True
                            else:
                                _ext_mod_pre_targets[_cur_d] = _cd_r
                        if _cd_r not in _ext_mod_pre_stamps:
                            _ext_mod_pre_stamps[_cd_r] = _startup_stamp(_cd_r)
                        _cur_d = os.path.dirname(_cur_d)

            _pre_stamp_path_and_ancestors(lex_norm, real_norm)
            if real_norm.endswith((".py", ".pyc", ".pyo", ".so", ".pyd", ".dylib", ".dll")):
                if _ext_mod_observed_files is not None:
                    _ext_mod_observed_files.add((lex_norm, real_norm))
            if real_norm.endswith((".pyc", ".pyo")):
                _mapped_src = _map_pyc_to_source(real_norm)
                if _mapped_src is not None and os.path.isfile(_mapped_src):
                    _mapped_real = os.path.realpath(_mapped_src)
                    _pre_stamp_path_and_ancestors(_mapped_src, _mapped_real)
                    if _ext_mod_observed_files is not None:
                        _ext_mod_observed_files.add((_mapped_src, _mapped_real))
    if not in_ws_real and not in_ws_lex:
        if event == "open":
            ws_caller, is_plugin = _caller_below_pytest(2)
            # Also catch external reads from worker threads (during collection OR test execution,
            # e.g. `ThreadPoolExecutor.submit(Path("/tmp/fixture").read_text)`) whose stack has
            # only stdlib frames!
            is_worker_thread = _is_audit_worker_thread()
            if ws_caller is not None or is_plugin or is_worker_thread:
                direct_ws = ws_caller is not None and _is_direct_workspace_open(2)
                # Also treat explicit plugin-originated reads under `site-packages` (including `.py`
                # config modules opened directly as data, since `importlib` imports already stop at
                # `<frozen importlib` in `_caller_below_pytest`) as untracked data, EXCEPT files
                # inside fingerprinted `pytest11` plugin trees and top-level stamped packaging
                # metadata files directly inside `<stamped_sys_path_dir>/*.dist-info` / `*.egg-info`.
                in_fingerprinted_plugin = _in_fingerprinted_plugin_tree(
                    real_norm, traversed_symlink
                )
                _meta_dir = os.path.dirname(real_norm)
                _meta_parent = os.path.dirname(_meta_dir)
                is_std_dist_meta = (
                    not traversed_symlink
                    and _meta_parent in _STAMPED_DIST_ROOTS
                    and os.path.basename(_meta_dir).endswith((".dist-info", ".egg-info"))
                    and os.path.basename(real_norm) in _STD_DIST_METADATA_FILES
                )
                is_plugin_data_open = (
                    is_plugin
                    and not in_fingerprinted_plugin
                    and not is_std_dist_meta
                    and not real_norm.endswith((".so", ".dylib", ".dll"))
                )
                if _is_external_user_fixture_path(
                    real_norm,
                    explicit_open=True,
                    direct_user_open=(direct_ws or is_plugin_data_open or is_worker_thread),
                    lex_norm=lex_norm,
                ):
                    audit_incomplete = True
        return
    if traversed_symlink:
        rel_check = os.path.relpath(real_norm if in_ws_real else lex_norm, workspace_root).replace("\\", "/")
        user_caller, is_plugin = _caller_below_pytest(2)
        if user_caller is not None or _is_audit_worker_thread() or is_plugin:
            # Explicit user-code, worker-thread, or plugin-fixture read through ANY workspace symlink
            # (during collection OR test execution, even under ignored dirs) must mark capture incomplete.
            audit_incomplete = True
        elif audit_in_test_phase and not any(part in _AUDIT_IGNORED_PARTS for part in rel_check.split("/")):
            audit_incomplete = True
        return
    normalized = real_norm
    caller_rel = None
    is_worker_ws_open = _is_audit_worker_thread()
    if normalized.endswith((".pyc", ".pyo")):
        if not audit_in_test_phase and not is_worker_ws_open:
            user_caller, is_plugin = _caller_below_pytest(2)
            if user_caller is None and not is_plugin:
                return
            caller_rel = user_caller
        mapped = _map_pyc_to_source(normalized)
        if mapped is None:
            audit_incomplete = True
            return
        normalized = mapped
    if normalized.endswith(".py") and not audit_in_test_phase and not is_worker_ws_open:
        # Static collection-time imports go through `importlib` (stopped at runner boundary)
        # and are tracked by the AST graph; capture direct user-code `open()`/`exec()` reads
        # during collection (attributed to the calling workspace module) AND third-party plugin
        # or worker-thread collection reads (recorded as shared dependencies!).
        user_caller, is_plugin = _caller_below_pytest(2)
        if user_caller is None and not is_plugin:
            return
        caller_rel = user_caller
    rel = os.path.relpath(normalized, workspace_root).replace("\\", "/")
    _record_workspace_dep(rel, event == "open", caller_rel=caller_rel)

sys.addaudithook(audit_hook)

os.environ["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = "1"
gc.collect()
if hasattr(gc, "freeze"):
    gc.freeze()

# Capture original stdout/stderr encoding & error-handler settings BEFORE redirecting fds 1/2 to
# /dev/null so forked test children preserve caller stream semantics (e.g. PYTHONIOENCODING).
_orig_stdout_enc = getattr(sys.stdout, "encoding", None) or "utf-8"
_orig_stdout_err = getattr(sys.stdout, "errors", None) or "replace"
_orig_stderr_enc = getattr(sys.stderr, "encoding", None) or "utf-8"
_orig_stderr_err = getattr(sys.stderr, "errors", None) or "replace"

proto_in = sys.stdin.buffer
proto_out = os.fdopen(os.dup(1), "wb", buffering=0)
devnull_fd = os.open(os.devnull, os.O_RDWR)
os.dup2(devnull_fd, 1)
os.dup2(devnull_fd, 2)

# Warm pytest configuration/parser/plugin registration without importing workspace tests.
try:
    pytest.main(["--version"], plugins=[obj for _, obj in plugins])
except BaseException:
    pass
# Restore original `PYTEST_DISABLE_PLUGIN_AUTOLOAD` right after warmup so forked test children
# and initial `conftest.py` files observe the exact unmodified environment of ordinary `pytest`.
if _orig_disable_autoload is None:
    os.environ.pop("PYTEST_DISABLE_PLUGIN_AUTOLOAD", None)
else:
    os.environ["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = _orig_disable_autoload

_loaded_tops = sorted({k.split(".", 1)[0] for k in sys.modules.keys() if k})
_loaded_local_files = set(_parent_startup_data_abs)
_loaded_local_rels = set(_parent_startup_data_rels)
_loaded_local_untrackable = bool(_parent_startup_untrackable)
_loaded_py_module_files = []
_root_prefix = os.path.realpath(os.getcwd())
_pp_prefixes = [_root_prefix] + [
    os.path.realpath(p) for p in os.environ.get("PYTHONPATH", "").split(os.pathsep) if p
]
for _m in list(sys.modules.values()):
    _mf = getattr(_m, "__file__", None)
    if isinstance(_mf, str) and _mf:
        if _mf.endswith(".py") and not _mf.endswith(os.sep + "__init__.py"):
            _loaded_py_module_files.append(_mf)
        _rmf = os.path.realpath(_mf)
        if any(_rmf == _p or _rmf.startswith(_p + os.sep) for _p in _pp_prefixes):
            _loaded_local_files.add(_mf)
            if _rmf.startswith(_root_prefix + os.sep):
                _rel = os.path.relpath(_rmf, _root_prefix).replace(os.sep, "/")
                _loaded_local_rels.add(_rel)
            else:
                _loaded_local_untrackable = True
        elif not any(_rmf == ep or _rmf.startswith(ep + os.sep) for ep in _parent_py_code_dirs):
            # External non-stdlib helper imported by `sitecustomize` / `.pth`: stamp its source
            # file so in-place edits restart the Zygote parent cleanly.
            _loaded_local_files.add(_mf)
        elif "/site-packages/" in _rmf.replace("\\", "/") or "/dist-packages/" in _rmf.replace("\\", "/"):
            # Stamp ALL preloaded site-packages module files so in-place edits to installed
            # third-party packages always restart the Zygote parent instead of reusing stale code.
            _loaded_local_files.add(_mf)
_ready_stamps = {}
for _lf in _loaded_local_files:
    _ready_stamps[_lf] = _parent_startup_pre_stamps.get(_lf) or _startup_stamp(_lf)
_ready_payload = json.dumps({
    "modules": _loaded_tops,
    "local_files": sorted(_loaded_local_files),
    "local_stamps": _ready_stamps,
    "py_module_files": sorted(set(_loaded_py_module_files)),
    "untrackable": bool(_loaded_local_untrackable),
})
proto_out.write(("TCTX_ZYGOTE_READY " + _ready_payload + "\n").encode("utf-8"))
proto_out.flush()

class TctxReporter:
    MAX_TOTAL_LONGREPR_BYTES = 512 * 1024

    def __init__(self):
        self.reports = []
        self.longrepr_bytes = 0
        self.collected_counts = {}

    @pytest.hookimpl(trylast=True)
    def pytest_configure(self, config):
        global _current_pytest_config, _current_pytest_basetemp
        _current_pytest_config = config
        _current_pytest_basetemp = None
        _resolve_current_pytest_basetemp()

    def pytest_unconfigure(self, config):
        global _current_pytest_config, _current_pytest_basetemp
        _current_pytest_config = None
        _current_pytest_basetemp = None

    def pytest_collection_modifyitems(self, items):
        counts = {}
        for item in items:
            nodeid = getattr(item, "nodeid", "")
            if "::" in nodeid:
                rel_p = nodeid.split("::", 1)[0].replace("\\", "/")
                counts[rel_p] = counts.get(rel_p, 0) + 1
        self.collected_counts = counts

    @staticmethod
    def _path(report):
        location = getattr(report, "location", None)
        if location and len(location) >= 1:
            return str(location[0]).replace("\\", "/")
        return str(getattr(report, "fspath", "")).replace("\\", "/")

    def _bounded_longrepr(self, raw):
        remaining = self.MAX_TOTAL_LONGREPR_BYTES - self.longrepr_bytes
        if remaining <= 0:
            return "[truncated: suite failure output budget reached]"
        limit = min(8192, remaining)
        encoded = str(raw).encode("utf-8", "ignore")
        if len(encoded) > limit:
            encoded = encoded[:limit]
        self.longrepr_bytes += len(encoded)
        return encoded.decode("utf-8", "ignore")

    def pytest_runtest_logreport(self, report):
        if report.when == "call" or (report.failed and report.when in ("setup", "teardown")) or (report.skipped and report.when == "setup"):
            self.reports.append({
                "nodeid": report.nodeid,
                "path": self._path(report),
                "when": report.when,
                "outcome": report.outcome,
                "duration_ms": max(0, int(round(float(report.duration) * 1000.0))),
                "longrepr": self._bounded_longrepr(report.longrepr) if report.failed else "",
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
                "longrepr": self._bounded_longrepr(report.longrepr),
                "dynamic_deps": [],
            })

    def pytest_runtest_logstart(self, nodeid, location):
        global audit_current_node, audit_current_file, audit_in_test_phase
        audit_current_node = nodeid
        rel_test = nodeid.split("::", 1)[0]
        audit_current_file = os.path.realpath(os.path.join(workspace_root, rel_test))
        audit_in_test_phase = True
        audit_by_node.setdefault(nodeid, set())

    @pytest.hookimpl(hookwrapper=True)
    def pytest_runtest_call(self, item):
        global audit_in_call
        audit_in_call = True
        try:
            yield
        finally:
            audit_in_call = False

    def pytest_runtest_logfinish(self, nodeid, location):
        global audit_current_node, audit_current_file, audit_in_test_phase, audit_in_call
        audit_current_node = None
        audit_current_file = None
        audit_in_test_phase = False
        audit_in_call = False

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

_proc_task_checked = False
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
    timeout_secs = float(req.get("timeout_secs", 300.0))
    started = time.perf_counter()

    # Always verify OS thread count before os.fork() so native C-extension threads cannot deadlock child.
    task_count = None
    try:
        with open("/proc/self/status", "r", encoding="utf-8", errors="ignore") as _sf:
            for _line in _sf:
                if _line.startswith("Threads:"):
                    task_count = int(_line.split()[1])
                    break
    except Exception:
        pass
    if task_count is None:
        task_count = len(os.listdir("/proc/self/task")) if os.path.isdir("/proc/self/task") else threading.active_count()
    if task_count != 1:
        write_frame(proto_out, {
            "pre_start_error": f"ZYGOTE_FORK_UNSAFE active_threads={task_count}",
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
            try:
                os.setsid()
            except OSError:
                try:
                    os.setpgid(0, 0)
                except OSError:
                    pass
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
            # Redirect child fd 0 to `/dev/null` (matching subprocess pytest) so a test reading
            # stdin with capture disabled never hangs reading the Zygote protocol pipe!
            os.dup2(devnull_fd, 0)
            os.dup2(out_w, 1)
            os.dup2(err_w, 2)
            os.close(out_w)
            os.close(err_w)
            keep = {w_fd}
            close_unneeded_fds(keep)
            sys.stdin = io.TextIOWrapper(os.fdopen(0, "rb", closefd=False), encoding=_orig_stdout_enc, errors=_orig_stdout_err)
            sys.stdout = io.TextIOWrapper(os.fdopen(1, "wb", closefd=False), encoding=_orig_stdout_enc, errors=_orig_stdout_err, write_through=True)
            sys.stderr = io.TextIOWrapper(os.fdopen(2, "wb", closefd=False), encoding=_orig_stderr_enc, errors=_orig_stderr_err, write_through=True)
            reporter = TctxReporter()
            audit_paths = set() if capture_dynamic_deps else None
            audit_shared_paths = set(_loaded_local_rels) if capture_dynamic_deps else None
            audit_module_paths = {} if capture_dynamic_deps else None
            audit_incomplete = bool(_loaded_local_untrackable) if capture_dynamic_deps else False
            audit_by_node = {}
            _ext_mod_pre_stamps = dict(_ready_stamps) if capture_dynamic_deps else None
            _ext_mod_pre_targets = {} if capture_dynamic_deps else None
            _ext_mod_observed_files = set() if capture_dynamic_deps else None
            if _ext_mod_pre_targets is not None:
                for _lf in _loaded_local_files:
                    try:
                        _lf_abs = os.path.abspath(_lf)
                        _lf_real = os.path.realpath(_lf_abs)
                        _ext_mod_pre_targets[_lf_abs] = _lf_real
                        if _ext_mod_observed_files is not None and _lf_real.endswith(
                            (".py", ".pyc", ".pyo", ".so", ".pyd", ".dylib", ".dll")
                        ):
                            _ext_mod_observed_files.add((_lf_abs, _lf_real))
                    except Exception:
                        pass
            audit_current_node = None
            audit_current_file = None
            audit_in_test_phase = False
            audit_in_call = False
            if capture_dynamic_deps:
                # Instrument `os.stat`, `os.lstat`, and `os.access` inside the forked child so
                # user-code or library-mediated probes (`Path.exists()`, `shutil.which()`,
                # `os.stat(...).st_mode`, etc.) against workspace paths mark capture incomplete
                # because filesystem metadata/existence/permissions are not captured by content SHA-256.
                import stat as _stat_mod
                _orig_stat = os.stat
                _orig_lstat = os.lstat
                _orig_fstat = os.fstat
                _orig_access = os.access
                _orig_open = os.open
                _orig_readlink = os.readlink
                def _is_realpath_frame():
                    try:
                        f = sys._getframe(2)
                        if f is not None and f.f_code.co_name in ("_joinrealpath", "realpath", "resolve"):
                            fn = f.f_code.co_filename or ""
                            if fn in _FROZEN_PATH_WRAPPERS or os.path.basename(fn) in _PATH_WRAPPER_BASENAMES:
                                return True
                    except Exception:
                        pass
                    return False
                def _observe_stat_path(path_arg, direct_user_open=False):
                    global audit_incomplete
                    if isinstance(path_arg, int):
                        audit_incomplete = True
                        return
                    if not isinstance(path_arg, (str, bytes, os.PathLike)):
                        return
                    try:
                        raw_p = os.fsdecode(os.fspath(path_arg))
                    except Exception:
                        return
                    norm_p = os.path.normpath(raw_p if os.path.isabs(raw_p) else os.path.join(os.getcwd(), raw_p))
                    if not norm_p.startswith(workspace_root + os.sep) and norm_p != workspace_root:
                        try:
                            real_p = os.path.realpath(norm_p)
                            if real_p == workspace_root or real_p.startswith(workspace_root + os.sep):
                                norm_p = real_p
                            elif _is_external_user_fixture_path(real_p, allow_dirs=True, direct_user_open=direct_user_open, lex_norm=norm_p):
                                audit_incomplete = True
                                return
                            else:
                                return
                        except Exception:
                            return
                    if norm_p == workspace_root:
                        audit_incomplete = True
                        return
                    # `_observe_stat_path` is only invoked for explicit user/library probes from
                    # workspace code (`_has_workspace_caller_below_pytest`), so even ignored-dir
                    # probes (e.g. `Path("node_modules/pkg/data.json").exists()`) mark incomplete.
                    audit_incomplete = True
                def _wrapped_stat(path, *a, **kw):
                    global audit_incomplete
                    if not getattr(_stat_tls, "active", False):
                        _stat_tls.active = True
                        try:
                            caller_rel, is_plugin = _caller_below_pytest(2)
                            is_worker = _is_audit_worker_thread()
                            has_dir_fd = kw.get("dir_fd") is not None
                            if caller_rel is not None or is_plugin or is_worker:
                                if has_dir_fd:
                                    audit_incomplete = True
                                else:
                                    direct_ws = is_worker or (caller_rel is not None and _is_direct_workspace_open(2))
                                    _observe_stat_path(path, direct_user_open=direct_ws)
                        finally:
                            _stat_tls.active = False
                    return _orig_stat(path, *a, **kw)
                def _wrapped_lstat(path, *a, **kw):
                    global audit_incomplete
                    if getattr(_stat_tls, "active", False):
                        return _orig_lstat(path, *a, **kw)
                    _stat_tls.active = True
                    try:
                        is_rp = _is_realpath_frame()
                        caller_rel, is_plugin = _caller_below_pytest(2)
                        is_worker = _is_audit_worker_thread()
                        is_tracked_caller = caller_rel is not None or is_plugin or is_worker
                        has_dir_fd = kw.get("dir_fd") is not None
                        direct_ws = is_worker or (caller_rel is not None and _is_direct_workspace_open(2))
                        try:
                            res = _orig_lstat(path, *a, **kw)
                        except OSError:
                            if is_tracked_caller:
                                if has_dir_fd:
                                    audit_incomplete = True
                                else:
                                    _observe_stat_path(path, direct_user_open=direct_ws)
                            raise
                        # Allow non-symlink `Path(__file__).resolve()` / `os.path.realpath()` calls
                        # without disabling caching, while still flagging symlinks and direct lstat.
                        if is_tracked_caller and (not is_rp or _stat_mod.S_ISLNK(res.st_mode)):
                            if has_dir_fd:
                                audit_incomplete = True
                            else:
                                _observe_stat_path(path, direct_user_open=direct_ws)
                        return res
                    finally:
                        _stat_tls.active = False
                def _wrapped_access(path, mode, *a, **kw):
                    global audit_incomplete
                    if not getattr(_stat_tls, "active", False):
                        _stat_tls.active = True
                        try:
                            caller_rel, is_plugin = _caller_below_pytest(2)
                            is_worker = _is_audit_worker_thread()
                            has_dir_fd = kw.get("dir_fd") is not None
                            if caller_rel is not None or is_plugin or is_worker:
                                if has_dir_fd:
                                    audit_incomplete = True
                                else:
                                    direct_ws = is_worker or (caller_rel is not None and _is_direct_workspace_open(2))
                                    _observe_stat_path(path, direct_user_open=direct_ws)
                        finally:
                            _stat_tls.active = False
                    return _orig_access(path, mode, *a, **kw)
                def _wrapped_open(path, flags, *a, **kw):
                    global audit_incomplete
                    if not getattr(_stat_tls, "active", False) and kw.get("dir_fd") is not None:
                        _stat_tls.active = True
                        try:
                            is_wronly = isinstance(flags, int) and (flags & os.O_WRONLY) != 0 and (flags & os.O_RDWR) == 0
                            caller_rel, is_plugin = _caller_below_pytest(2)
                            if not is_wronly and (caller_rel is not None or is_plugin or _is_audit_worker_thread()):
                                audit_incomplete = True
                        finally:
                            _stat_tls.active = False
                    return _orig_open(path, flags, *a, **kw)
                def _wrapped_readlink(path, *a, **kw):
                    global audit_incomplete
                    if not getattr(_stat_tls, "active", False):
                        _stat_tls.active = True
                        try:
                            caller_rel, is_plugin = _caller_below_pytest(2)
                            is_worker = _is_audit_worker_thread()
                            if caller_rel is not None or is_plugin or is_worker:
                                if kw.get("dir_fd") is not None:
                                    audit_incomplete = True
                                else:
                                    direct_ws = is_worker or (caller_rel is not None and _is_direct_workspace_open(2))
                                    _observe_stat_path(path, direct_user_open=direct_ws)
                        finally:
                            _stat_tls.active = False
                    return _orig_readlink(path, *a, **kw)
                def _wrapped_fstat(fd, *a, **kw):
                    global audit_incomplete
                    if not getattr(_stat_tls, "active", False):
                        _stat_tls.active = True
                        try:
                            caller_rel, is_plugin = _caller_below_pytest(2)
                            is_worker = _is_audit_worker_thread()
                            if caller_rel is not None or is_plugin or is_worker:
                                exempt_tmp = False
                                try:
                                    basetemp = _resolve_current_pytest_basetemp()
                                    if basetemp is not None and isinstance(fd, int):
                                        fd_target = os.path.realpath(_orig_readlink(f"/proc/self/fd/{fd}"))
                                        if fd_target == basetemp or fd_target.startswith(basetemp + os.sep):
                                            exempt_tmp = True
                                except Exception:
                                    exempt_tmp = False
                                if not exempt_tmp:
                                    audit_incomplete = True
                        finally:
                            _stat_tls.active = False
                    return _orig_fstat(fd, *a, **kw)
                os.stat = _wrapped_stat
                os.lstat = _wrapped_lstat
                os.fstat = _wrapped_fstat
                os.access = _wrapped_access
                os.open = _wrapped_open
                os.readlink = _wrapped_readlink
                try:
                    import ssl as _tctx_ssl
                    def _wrap_ssl_cert_loader(_orig_fn):
                        def _wrapped_ssl_fn(self, *a, **kw):
                            global audit_incomplete
                            if audit_paths is not None:
                                if audit_in_test_phase or _is_audit_worker_thread():
                                    # During the test execution phase (including ThreadPoolExecutor
                                    # worker threads whose stack contains only stdlib frames!), ANY
                                    # native OpenSSL certificate load bypasses Python `open` audit.
                                    audit_incomplete = True
                                else:
                                    try:
                                        _c_rel, _c_plug = _caller_below_pytest(2)
                                        if _c_rel is not None or _c_plug:
                                            audit_incomplete = True
                                    except Exception:
                                        audit_incomplete = True
                            return _orig_fn(self, *a, **kw)
                        return _wrapped_ssl_fn
                    for _ssl_m in (
                        "load_verify_locations",
                        "load_cert_chain",
                        "load_default_certs",
                        "set_default_verify_paths",
                    ):
                        _orig_ssl_m = getattr(_tctx_ssl.SSLContext, _ssl_m, None)
                        if callable(_orig_ssl_m):
                            setattr(_tctx_ssl.SSLContext, _ssl_m, _wrap_ssl_cert_loader(_orig_ssl_m))
                except Exception:
                    pass
            def _config_disables_autoload():
                cfg_paths = []
                idx = 0
                while idx < len(args):
                    if args[idx] == "-c" and idx + 1 < len(args):
                        p = args[idx + 1]
                        cfg_paths.append(p if os.path.isabs(p) else os.path.join(workspace_root, p))
                        idx += 2
                        continue
                    for pfx in ("-c=", "--config-file="):
                        if args[idx].startswith(pfx):
                            p = args[idx][len(pfx):]
                            cfg_paths.append(p if os.path.isabs(p) else os.path.join(workspace_root, p))
                    idx += 1
                if not cfg_paths:
                    cfg_paths = [
                        os.path.join(workspace_root, n)
                        for n in (
                            "pytest.toml", ".pytest.toml", "pytest.ini", ".pytest.ini",
                            "pyproject.toml", "setup.cfg", "tox.ini",
                        )
                    ]
                for cfg_p in cfg_paths:
                    if os.path.isfile(cfg_p):
                        try:
                            with open(cfg_p, "r", encoding="utf-8", errors="ignore") as _cf:
                                if "--disable-plugin-autoload" in _cf.read(128 * 1024):
                                    return True
                        except Exception:
                            pass
                return False
            _disable_for_run = (
                _autoload_disabled
                or "--disable-plugin-autoload" in args
                or any("--disable-plugin-autoload" in a for a in args)
                or "--disable-plugin-autoload" in os.environ.get("PYTEST_ADDOPTS", "")
                or _config_disables_autoload()
            )
            class _TctxPluginImportError(BaseException):
                pass
            if not _disable_for_run and plugin_errors:
                payload = json.dumps({
                    "pre_start_error": f"ZYGOTE_PLUGIN_IMPORT_ERROR: {'; '.join(plugin_errors[:4])}",
                    "error": f"ZYGOTE_PLUGIN_IMPORT_ERROR: {'; '.join(plugin_errors[:4])}",
                }).encode("utf-8")
                offset = 0
                while offset < len(payload):
                    offset += os.write(w_fd, payload[offset:])
                os._exit(0)
            # Narrow the `try` strictly to patching `load_setuptools_entrypoints` BEFORE calling
            # `pytest.main(...)`, and resolve `deferred_plugins` lazily INSIDE `_tctx_load_ep`
            # so explicit `-p <plugin>` arguments load FIRST (matching ordinary pytest order).
            # When plugin discovery failed (`_plugin_discovery_unavailable`), do NOT patch
            # `load_setuptools_entrypoints` so pytest loads entry points natively.
            patched_ep = False
            if not _disable_for_run and not _plugin_discovery_unavailable:
                try:
                    import _pytest.config as _pytest_cfg_mod
                    _orig_load_ep = _pytest_cfg_mod.PytestPluginManager.load_setuptools_entrypoints
                    def _tctx_load_ep(pm, group, name=None):
                        if group != "pytest11":
                            return _orig_load_ep(pm, group, name=name)
                        import importlib.metadata as _im
                        try:
                            from pluggy._manager import DistFacade as _DistFacade
                        except Exception:
                            _DistFacade = None
                        _dist_by_ep = {}
                        try:
                            for _d in list(_im.distributions()):
                                for _ep in _d.entry_points:
                                    if _ep.group == "pytest11":
                                        _k = (_ep.name, _ep.value.split("[", 1)[0].strip())
                                        _dist_by_ep.setdefault(_k, []).append(_d)
                        except Exception:
                            _dist_by_ep = {}
                        count = 0
                        for ep_name, plugin_obj in plugins:
                            if name and ep_name != name:
                                continue
                            if ep_name and (pm.is_blocked(ep_name) or pm.has_plugin(ep_name)):
                                continue
                            if ep_name:
                                pm.register(plugin_obj, name=ep_name)
                            else:
                                pm.register(plugin_obj)
                            count += 1
                        for ep_name, plugin_value in deferred_plugins:
                            if name and ep_name != name:
                                continue
                            if ep_name and (pm.is_blocked(ep_name) or pm.has_plugin(ep_name)):
                                continue
                            try:
                                plugin_obj = resolve_plugin(plugin_value)
                            except Exception as exc:
                                raise _TctxPluginImportError(
                                    f"ZYGOTE_PLUGIN_IMPORT_ERROR: {plugin_value}: {exc}"
                                )
                            if ep_name:
                                pm.register(plugin_obj, name=ep_name)
                            else:
                                pm.register(plugin_obj)
                            _dlist = _dist_by_ep.get((ep_name, plugin_value))
                            if _dlist and _DistFacade is not None and hasattr(pm, "_plugin_distinfo"):
                                pm._plugin_distinfo.append((plugin_obj, _DistFacade(_dlist.pop(0))))
                            count += 1
                        return count
                    _pytest_cfg_mod.PytestPluginManager.load_setuptools_entrypoints = _tctx_load_ep
                    patched_ep = True
                except Exception:
                    patched_ep = False
            if not _disable_for_run and not _plugin_discovery_unavailable and not patched_ep and deferred_plugins:
                # Fallback when `PytestPluginManager.load_setuptools_entrypoints` could not be patched.
                child_plugins = list(plugins)
                for ep_name, plugin_value in deferred_plugins:
                    try:
                        child_plugins.append((ep_name, resolve_plugin(plugin_value)))
                    except Exception as exc:
                        payload = json.dumps({
                            "pre_start_error": f"ZYGOTE_PLUGIN_IMPORT_ERROR: {plugin_value}: {exc}",
                            "error": f"ZYGOTE_PLUGIN_IMPORT_ERROR: {plugin_value}: {exc}",
                        }).encode("utf-8")
                        offset = 0
                        while offset < len(payload):
                            offset += os.write(w_fd, payload[offset:])
                        os._exit(0)
                run_plugins = [*[obj for _, obj in child_plugins], reporter]
            else:
                run_plugins = [reporter]
            try:
                code = int(pytest.main(args, plugins=run_plugins))
            except _TctxPluginImportError as exc:
                payload = json.dumps({
                    "pre_start_error": str(exc),
                    "error": str(exc),
                }).encode("utf-8")
                offset = 0
                while offset < len(payload):
                    offset += os.write(w_fd, payload[offset:])
                os._exit(0)
            except SystemExit as exc:
                code = int(exc.code) if isinstance(exc.code, int) else 1
            except Exception as exc:
                sys.stderr.write(f"zygote pytest exception: {exc}\n")
                code = 2
            sys.stdout.flush()
            sys.stderr.flush()
            _ext_mod_entries = {}
            if capture_dynamic_deps and not audit_incomplete:
                def _record_ext_entry(lex_p, real_p, is_dir):
                    global audit_incomplete
                    if lex_p in _ext_mod_entries:
                        return
                    if _ext_mod_pre_targets is not None:
                        pre_t = _ext_mod_pre_targets.get(lex_p)
                        if pre_t is not None and pre_t != real_p:
                            # Symlink was retargeted during test execution: never cache!
                            audit_incomplete = True
                            return
                    post_st = _startup_stamp(real_p)
                    if post_st is None:
                        audit_incomplete = True
                        return
                    pre_st = _ext_mod_pre_stamps.get(real_p) if _ext_mod_pre_stamps is not None else None
                    if pre_st is not None and pre_st != post_st:
                        # File or directory changed DURING test execution: never cache!
                        audit_incomplete = True
                        return
                    _ext_mod_entries[lex_p] = [lex_p, real_p, bool(is_dir), *post_st]
                    if len(_ext_mod_entries) > 1024:
                        audit_incomplete = True

                _candidate_ext_pairs = []
                for _m in list(sys.modules.values()):
                    _mf = getattr(_m, "__file__", None)
                    if isinstance(_mf, str) and _mf:
                        try:
                            _lf = os.path.abspath(_mf)
                            _candidate_ext_pairs.append((_lf, os.path.realpath(_lf)))
                        except Exception:
                            pass
                if _ext_mod_observed_files:
                    for _obs_lex, _obs_orig_real in _ext_mod_observed_files:
                        try:
                            _cur_real = os.path.realpath(_obs_lex)
                            if _cur_real != _obs_orig_real:
                                audit_incomplete = True
                                break
                            _candidate_ext_pairs.append((_obs_lex, _cur_real))
                        except Exception:
                            audit_incomplete = True
                            break

                for _lex_f, _rmf in _candidate_ext_pairs:
                    if audit_incomplete:
                        break
                    _lex_sl = _lex_f.replace("\\", "/")
                    _real_sl = _rmf.replace("\\", "/")
                    if (
                        "/site-packages/" not in _lex_sl
                        and "/dist-packages/" not in _lex_sl
                        and "/site-packages/" not in _real_sl
                        and "/dist-packages/" not in _real_sl
                    ):
                        continue
                    if _is_stdlib_file(_rmf):
                        continue
                    if _rmf.startswith(workspace_root + os.sep):
                        # Only skip tracked workspace source files; workspace-local virtualenvs
                        # (e.g. `.venv/lib/.../site-packages/...`) live under ignored directories
                        # omitted from the workspace AST graph and MUST be stamped externally!
                        _ws_rel_parts = os.path.relpath(_rmf, workspace_root).replace("\\", "/").split("/")
                        if not any(part in _AUDIT_IGNORED_PARTS for part in _ws_rel_parts):
                            continue
                    if _in_fingerprinted_plugin_tree(_rmf, _lex_f != _rmf):
                        continue
                    if _rmf.endswith((".pyc", ".pyo")):
                        # Retain BOTH the executed bytecode file AND its mapped `.py` source so
                        # in-place bytecode edits with a valid header ALSO invalidate cached passes!
                        _pairs_to_record = [(_lex_f, _rmf)]
                        _mapped = _map_pyc_to_source(_rmf)
                        if _mapped is not None and os.path.isfile(_mapped):
                            _pairs_to_record.append(
                                (os.path.abspath(_mapped), os.path.realpath(_mapped))
                            )
                    else:
                        _pairs_to_record = [(_lex_f, _rmf)]
                    for _rec_lex, _rec_real in _pairs_to_record:
                        if not os.path.isfile(_rec_real):
                            # An observed imported module disappeared during test execution: never cache!
                            audit_incomplete = True
                            break
                        _record_ext_entry(_rec_lex, _rec_real, False)
                        # Stamp all ancestor package directories up to `site-packages`/`dist-packages`
                        # so adding `pkg/helper/__init__.py` or `acme/__init__.py` invalidates!
                        for _base_p in (_rec_lex, _rec_real):
                            _cur_d = os.path.dirname(_base_p)
                            for _ in range(16):
                                if not _cur_d or _cur_d == os.path.dirname(_cur_d):
                                    break
                                _bn = os.path.basename(_cur_d)
                                if _bn in ("site-packages", "dist-packages"):
                                    break
                                _cd_sl = _cur_d.replace("\\", "/")
                                if "/site-packages/" not in _cd_sl and "/dist-packages/" not in _cd_sl:
                                    break
                                _record_ext_entry(_cur_d, os.path.realpath(_cur_d), True)
                                _cur_d = os.path.dirname(_cur_d)
            payload = json.dumps({
                "exit_code": code,
                "reports": reporter.reports,
                "collected_counts": reporter.collected_counts,
                "dynamic_deps": sorted(audit_paths or ()),
                "shared_dynamic_deps": sorted(audit_shared_paths or ()),
                "module_dynamic_deps": {k: sorted(v) for k, v in (audit_module_paths or {}).items()},
                "dynamic_deps_incomplete": bool(audit_incomplete),
                "external_module_entries": sorted(_ext_mod_entries.values()) if not audit_incomplete else [],
            }, separators=(",", ":")).encode("utf-8")
            if len(payload) > MAX_FRAME:
                payload = json.dumps({"error": "zygote result frame too large"}).encode("utf-8")
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
        # Ensure child is in its own process group (-pid) BEFORE reporting started_pid to Rust
        # so an immediate kill(-pid, SIGKILL) cannot race with child's os.setsid().
        try:
            os.setpgid(pid, pid)
        except OSError:
            pass
        os.set_blocking(r_fd, False)
        os.set_blocking(out_r, False)
        os.set_blocking(err_r, False)
        write_frame(proto_out, {"started_pid": pid})
        timed_out = False
        result_chunks = []
        stdout_chunks = []
        stderr_chunks = []
        retained = {r_fd: 0, out_r: 0, err_r: 0}
        result_done = False
        open_fds = {r_fd: result_chunks, out_r: stdout_chunks, err_r: stderr_chunks}
        proto_in_fd = proto_in.fileno()
        parent_dead = False
        deadline = time.monotonic() + timeout_secs
        while open_fds and not result_done and time.monotonic() < deadline:
            watch_fds = list(open_fds) + [proto_in_fd]
            ready, _, _ = select.select(watch_fds, [], [], max(0.0, deadline - time.monotonic()))
            if not ready:
                break
            if proto_in_fd in ready:
                # ZygoteManager serializes requests one at a time; readability on proto_in during
                # an active child run means the owning daemon closed the pipe (exited/crashed).
                parent_dead = True
                try:
                    os.killpg(pid, signal.SIGKILL)
                except Exception:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except Exception:
                        pass
                break
            for fd in ready:
                try:
                    chunk = os.read(fd, 65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    if fd == r_fd:
                        result_done = True
                    open_fds.pop(fd, None)
                    try:
                        os.close(fd)
                    except OSError:
                        pass
                    continue
                target = open_fds[fd]
                limit = MAX_FRAME if fd == r_fd else 2 * 1024 * 1024
                available = limit - retained[fd]
                if available > 0:
                    saved = chunk[:available]
                    target.append(saved)
                    retained[fd] += len(saved)
        wait_status = 0
        waited = False
        raw_result_bytes = b"".join(result_chunks)
        if not result_done or not raw_result_bytes:
            # Reap child FIRST so a killed child closing `w_fd` with 0 bytes is classified as
            # `cancelled` instead of a malformed-JSON error that would poison the circuit breaker.
            try:
                done_pid, wait_status = os.waitpid(pid, os.WNOHANG)
                if done_pid == pid:
                    waited = True
                elif result_done and not raw_result_bytes:
                    time.sleep(0.005)
                    done_pid, wait_status = os.waitpid(pid, os.WNOHANG)
                    if done_pid == pid:
                        waited = True
                    else:
                        _, wait_status = os.waitpid(pid, 0)
                        waited = True
            except Exception:
                waited = False
            if waited:
                was_sig = os.WIFSIGNALED(wait_status) and os.WTERMSIG(wait_status) in (signal.SIGKILL, signal.SIGTERM)
                if was_sig:
                    child_result = {"exit_code": 130, "reports": [], "dynamic_deps": []}
                elif result_done and not raw_result_bytes:
                    child_result = {"error": "invalid zygote result: empty payload"}
                else:
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
                child_result = json.loads(raw_result_bytes.decode("utf-8"))
            except Exception as exc:
                child_result = {"error": f"invalid zygote result: {exc}"}
        if not waited:
            try:
                done_pid, wait_status = os.waitpid(pid, os.WNOHANG)
                if done_pid == pid:
                    waited = True
                else:
                    time.sleep(0.005)
                    done_pid, wait_status = os.waitpid(pid, os.WNOHANG)
                    if done_pid == pid:
                        waited = True
                    else:
                        _, wait_status = os.waitpid(pid, 0)
            except Exception:
                pass
        try:
            os.killpg(pid, signal.SIGKILL)
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
                available = 2 * 1024 * 1024 - retained[fd]
                if available > 0:
                    saved = chunk[:available]
                    target.append(saved)
                    retained[fd] += len(saved)
            try:
                os.close(fd)
            except OSError:
                pass
        cancelled = (not raw_result_bytes) and (not timed_out) and os.WIFSIGNALED(wait_status) and os.WTERMSIG(wait_status) in (signal.SIGKILL, signal.SIGTERM)
        stdout_text = b"".join(stdout_chunks).decode("utf-8", errors="replace")
        stderr_text = b"".join(stderr_chunks).decode("utf-8", errors="replace")

        duration_ms = max(1, int(round((time.perf_counter() - started) * 1000.0)))
        if parent_dead:
            os._exit(0)
        write_frame(proto_out, {
            "exit_code": int(child_result.get("exit_code", 1)),
            "reports": child_result.get("reports", []),
            "collected_counts": child_result.get("collected_counts", {}),
            "dynamic_deps": child_result.get("dynamic_deps", []),
            "shared_dynamic_deps": child_result.get("shared_dynamic_deps", []),
            "module_dynamic_deps": child_result.get("module_dynamic_deps", {}),
            "dynamic_deps_incomplete": bool(child_result.get("dynamic_deps_incomplete", False)),
            "external_module_entries": child_result.get("external_module_entries", []),
            "stdout": stdout_text,
            "stderr": stderr_text,
            "duration_ms": duration_ms,
            "timed_out": timed_out,
            "cancelled": cancelled,
            "plugin_errors": plugin_errors,
            # Always propagate explicit child errors and `pre_start_error` so plugin import
            # failures trigger a clean subprocess pytest fallback without circuit-breaking.
            "pre_start_error": child_result.get("pre_start_error"),
            "error": child_result.get("error"),
        })
