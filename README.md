# Test Context (`tctx`)

**Test Context (`tctx`)** is a local Rust accelerator for **Python (`pytest` + `ruff`)** and **Rust (`cargo test`)** workflows, designed as the companion to `code-context` (`cctx`) for GitHub Copilot and AI coding agents.

Instead of running the entire test suite or paying interpreter/compiler startup costs on every test or lint call, `tctx` maintains an in-memory **Python + Rust AST reverse dependency graph**, pre-checks Python files with **Ruff**, and runs affected tests **speculatively in the background** as files are saved.

When GitHub Copilot (or a developer) invokes tests:
- **100% Cache Hit**: If all targeted or affected tests have already passed against the current Merkle dependency hash (`dep_hash`), `tctx` returns the result **instantly (< 1ms)** without spawning `pytest` or `cargo test`.
- **Delta Execution**: If some tests are uncached or modified, `tctx` reuses cached results for unchanged tests, joins any in-flight background test run, prioritizes previously failed tests first (**Fail-First scheduling**), and runs `pytest` / `cargo test` **only on the uncached delta**.
- **Pre-Flight Ruff Diagnostics & Auto-Fix**: Modified Python files are pre-checked by `ruff` in the background worker and cached by content `sha256`. `#tctxNativeAffected` and `#tctxNativeRun` surface exact `file:row:col` Ruff diagnostics immediately, and passing `ruff_fix: true` (`--ruff-fix`) auto-remediates safe lint/import issues before tests run.

## Features

- **Dual-Language Python & Rust AST Dependency Graph**:
  - **Python**: Extracts `import x`, `from x import y`, relative imports, multi-line parenthesized imports, in-function lazy imports, `conftest.py` hierarchies, and `pyproject.toml`/`pytest.ini` configs while excluding `if TYPE_CHECKING:`-only imports from false test invalidation.
  - **Rust**: Parses `mod foo;`, `#[path = "..."] mod`, nested `use crate::{...}` trees, `#[test]` / `#[tokio::test]` / `#[rstest]` symbols, `#[ignore]` / `#[serial]` attributes, `tests/common/mod.rs` shared helpers, `build.rs`, and `include_str!` / `include_bytes!` compile-time data dependencies.
  - **PyO3 / Maturin Cross-Language Graph**: Links Rust `cdylib` / `[tool.maturin]` / `#[pymodule]` crates to Python importers so editing a `.rs` kernel automatically marks dependent Python `pytest` files as affected alongside Rust unit tests.
  - **Static Environment Variable & Data Tracking**: Tracks `os.getenv`, `os.environ`, `env!`, and `env::var` keys as well as referenced `.json`, `.yaml`, `.toml`, `.sql`, `.snap`, `.golden`, and `.proto` files in each test's Merkle `dep_hash`.
- **Speculative Background Daemon (`tctx daemon`)**:
  - Watches workspace changes through `notify` (Linux `inotify`), filters access-only callbacks, and immediately queues affected test files and Ruff pre-checks in a background worker upon file changes.
  - Batches multiple Rust `--lib` unit test modules per crate into a single `cargo test --lib` invocation and demultiplexes output per source file and per test node (`src/calc.rs::test_mul_unit`).
  - Supports function/node-level cache hits (`file.py::test_fn` and `src/file.rs::test_fn`), fuzzy `did_you_mean` suggestions on target typos, and single-writer WAL persistence with `BufWriter` and $O(1)$ incremental memory accounting.
  - Creates the warm pytest Zygote lazily, preloads only imports needed by the requested tests, reuses compatible parents across narrowed selections, and reclaims idle Zygotes after 60 seconds.
- **Native GitHub Copilot Integration (`dotnetpower.test-context-native`)**:
  - 7 Native VS Code Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeWorkspaces`, `#tctxNativeInvalidate`).
  - Native tools are the default in VS Code foreground sessions; optional Shared MCP Server (`tctx shared-mcp`) is available for unattended background/CLI agent sessions.
  - Drop-in CLI (`tctx run [--ruff-fix]`, `tctx pytest`, `tctx cargo-test`, `tctx ruff [--fix]`, `tctx affected`, `tctx graph`, `tctx status`, `tctx sync`).

`tctx` can learn only invocations routed through its Native, MCP, or CLI surfaces. A direct shell
command such as `pytest tests/test_api.py` is not intercepted; use `tctx pytest
tests/test_api.py` or `tctx cargo-test` when invocation learning and cache reuse are desired.

## Measured Performance

### 1. Python FastAPI Suite (`aks-store-demo`, 320 Paired Observations)

Across 8 independent daemon sessions (**320 paired observations**) on [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo) (`src/ai-service`):

![Test execution and query latency and paired-session speedup with 95% confidence intervals](benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | TCTX off (`pytest`) | TCTX on | Speedup [95% CI] |
|---|---:|---:|---:|
| Warm suite (`warm_suite`) | 1000.90 ms | **0.56 ms** (median 0.53 ms) | **1796.00×** [1710.88, 1867.12] |
| Post-edit background pre-run (`bg_speculative`) | 994.27 ms | **0.81 ms** (median 0.51 ms) | **1230.61×** [817.21, 1915.37] |
| Reverted edit Merkle hit (`merkle_revert`) | 988.26 ms | **0.60 ms** (median 0.55 ms) | **1643.59×** [1413.96, 1850.33] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 981.56 ms | **78.14 ms** (median 74.92 ms) | **12.56×** [12.00, 13.18] |
| Impact query + Ruff pre-check (`affected_graph`) | 960.91 ms | **0.47 ms** (median 0.45 ms) | **2049.69×** [1957.13, 2155.51] |
| Fixed mixed suite (`all`, 320 pairs) | 985.18 ms | **16.11 ms** (median 0.53 ms) | **61.13×** [57.39, 64.72] |

### 2. Rust (`cargo test`) & Multi-File Demux Benchmarks (`test-context` Workspace)

| Scenario | Cold / Unbatched | With `tctx` | Speedup / Improvement |
|---|---:|---:|---:|
| 4 Rust modules (`25 unit tests`) cold batch | ~5,800 ms (4× `cargo test`) | **1,254.64 ms** (1× `cargo test --lib` demux) | **4.6× faster cold batch** |
| 4 Rust modules (`25 unit tests`) warm cache hit | 1,254.64 ms | **0.41 ms** (`instant_cache_hit`) | **3,060.1× speedup** |
| Single Rust test node (`src/runner.rs::demux_...`) | 1,254.64 ms | **0.23 ms** (`cached_node`) | **5,455.0× speedup** |
| Rust AST graph query (`test_graph` on `src/ast_graph.rs`) | — | **0.19 ms** | **< 0.2 ms instant** |
| Rust reverse impact query (`test_affected` on `src/cache.rs`) | — | **0.24 ms** | **< 0.3 ms instant** |

See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, cold-start measurements, raw samples, and reproduction instructions.

## Quick Start

```bash
# 1. Build and install tctx + user systemd service
./scripts/install.sh

# 2. Register a Python, Rust, or PyO3 hybrid workspace
tctx --root /path/to/repo register

# 3. Inspect AST dependency graph, affected tests, or pre-flight Ruff diagnostics
tctx --root /path/to/repo status
tctx --root /path/to/repo affected --file src/my_pkg/core.py
tctx --root /path/to/repo ruff --fix

# 4. Run Python (pytest) and Rust (cargo test) tests with incremental AST caching
tctx --root /path/to/repo run --ruff-fix
tctx --root /path/to/repo pytest -q
tctx --root /path/to/repo cargo-test

# 5. Package and install the VS Code Native Extension
./scripts/install-vscode-extension.sh
```
