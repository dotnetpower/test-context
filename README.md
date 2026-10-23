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
  - Watches workspace files with `notify` and immediately queues affected test files in a background worker upon file changes.
  - Persists content-addressed test results (`dep_hash -> pass/fail/counts/duration/output`) per workspace root.
- **Native GitHub Copilot Integration (`moonchoi.test-context-native`)**:
  - 7 Native VS Code Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeWorkspaces`, `#tctxNativeInvalidate`).
  - Shared MCP Server (`tctx shared-mcp`) for background/CLI agent sessions (`test_run`, `test_affected`, `test_graph`, `test_status`, `test_sync`, `test_workspaces`, `test_invalidate`).
  - Drop-in CLI (`tctx run`, `tctx pytest`, `tctx affected`, `tctx graph`, `tctx status`, `tctx sync`).

## Measured Performance (`aks-store-demo` Python AI Service)

Across 8 independent daemon sessions (**320 paired observations**) on [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo) (`src/ai-service`):

![Test execution and query latency and paired-session speedup with 95% confidence intervals](benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | TCTX off (`pytest`) | TCTX on | Speedup [95% CI] |
|---|---:|---:|---:|
| Warm suite (`warm_suite`) | 1035.19 ms | **0.53 ms** (median 0.50 ms) | **1958.57×** [1877.34, 2036.13] |
| Post-edit background pre-run (`bg_speculative`) | 1043.13 ms | **0.54 ms** (median 0.51 ms) | **1923.44×** [1824.29, 2014.54] |
| Reverted edit Merkle hit (`merkle_revert`) | 1075.63 ms | **0.57 ms** (median 0.51 ms) | **1876.19×** [1604.49, 2112.41] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 1079.16 ms | **86.04 ms** (median 81.36 ms) | **12.54×** [12.25, 12.81] |
| Impact query (`affected_graph`) | 989.91 ms | **0.50 ms** (median 0.47 ms) | **1968.25×** [1881.70, 2039.39] |
| Fixed mixed suite (`all`, 320 pairs) | 1044.60 ms | **17.64 ms** (median 0.54 ms) | **59.23×** [57.96, 60.47] |

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
