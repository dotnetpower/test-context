# Test Context (`tctx`)

**Test Context (`tctx`)** is a local Rust accelerator for Python `pytest` workflows, designed as the companion to `code-context` (`cctx`) for GitHub Copilot and AI coding agents.

Instead of running the entire test suite or paying Python interpreter/collection startup costs on every `pytest` call, `tctx` maintains an in-memory **Python AST reverse dependency graph** and runs affected tests **speculatively in the background** as files are saved.

When GitHub Copilot (or a developer) invokes tests:
- **100% Cache Hit**: If all targeted or affected tests have already passed against the current Merkle dependency hash (`dep_hash`), `tctx` returns the result **instantly (< 5ms)** without spawning `pytest`.
- **Delta Execution**: If some tests are uncached or modified, `tctx` reuses cached results for unchanged tests, joins any in-flight background test run, and runs `pytest` **only on the uncached delta**.

## Features

- **Rust Python AST & Import Graph Analyzer**:
  - Extracts `import x`, `from x import y`, relative imports (`from . import z`), multi-line parenthesized imports, and in-function lazy imports.
  - Automatically links `conftest.py` hierarchies and `pyproject.toml`/`pytest.ini` configurations into each test file's Merkle dependency hash (`dep_hash`).
  - Supports root packages, `src/` layouts, and monorepo layouts (`services/*/src`, `packages/*/src`).
- **Speculative Background Daemon (`tctx daemon`)**:
  - Watches workspace changes through `notify` (Linux `inotify` in the validated environment), filters access-only callbacks, and immediately queues affected test files in a background worker upon file changes.
  - Learns bounded, successful whole-file `test_run` invocation patterns and prioritizes those affected tests on later edits; filtered, node-level, failed, and arbitrary-argument runs are not learned.
  - Persists content-addressed test results (`dep_hash -> pass/fail/counts/duration/output`) per workspace root.
  - Creates the warm pytest Zygote lazily, preloads only imports needed by the requested tests, and reclaims it after 60 seconds of inactivity.
- **Native GitHub Copilot Integration (`moonchoi.test-context-native`)**:
  - 7 Native VS Code Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeWorkspaces`, `#tctxNativeInvalidate`).
  - Shared MCP Server (`tctx shared-mcp`) for background/CLI agent sessions (`test_run`, `test_affected`, `test_graph`, `test_status`, `test_sync`, `test_workspaces`, `test_invalidate`).
  - Drop-in CLI (`tctx run`, `tctx pytest`, `tctx affected`, `tctx graph`, `tctx status`, `tctx sync`).

`tctx` can learn only invocations routed through its Native, MCP, or CLI surfaces. A direct shell
command such as `pytest tests/test_api.py` is not intercepted; use `tctx pytest
tests/test_api.py` when invocation learning and cache reuse are desired.

## Measured Performance (`aks-store-demo` Python AI Service)

Across 8 independent daemon sessions (**320 paired observations**) on [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo) (`src/ai-service`):

![Test execution and query latency and paired-session speedup with 95% confidence intervals](benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | TCTX off (`pytest`) | TCTX on | Speedup [95% CI] |
|---|---:|---:|---:|
| Warm suite (`warm_suite`) | 738.01 ms | **0.50 ms** (median 0.41 ms) | **1489.42×** [1190.31, 1755.25] |
| Post-edit background pre-run (`bg_speculative`) | 741.39 ms | **0.43 ms** (median 0.40 ms) | **1741.14×** [1683.01, 1795.17] |
| Reverted edit Merkle hit (`merkle_revert`) | 742.74 ms | **0.42 ms** (median 0.40 ms) | **1750.57×** [1686.97, 1814.47] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 735.33 ms | **56.59 ms** (median 53.33 ms) | **12.99×** [12.26, 13.57] |
| Impact query (`affected_graph`) | 714.43 ms | **0.40 ms** (median 0.38 ms) | **1802.81×** [1678.57, 1921.45] |
| Fixed mixed suite (`all`, 320 pairs) | 734.38 ms | **11.67 ms** (median 0.42 ms) | **62.95×** [59.18, 66.23] |

See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, cold-start measurements, raw samples, and reproduction instructions.

## Quick Start

```bash
# 1. Build and install tctx + user systemd service
./scripts/install.sh

# 2. Register a Python workspace
tctx --root /path/to/repo register

# 3. Inspect AST dependency graph or affected tests
tctx --root /path/to/repo status
tctx --root /path/to/repo affected --file src/my_pkg/core.py

# 4. Run tests with incremental AST caching (or drop-in pytest wrapper)
tctx --root /path/to/repo run
tctx --root /path/to/repo pytest -q

# 5. Package and install the VS Code Native Extension
./scripts/install-vscode-extension.sh
```
