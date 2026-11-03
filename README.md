# Test Context (`tctx`)

**Test Context (`tctx`)** is a local Rust accelerator for **Python (`pytest` + `ruff`)**, **Rust (`cargo test` + `clippy`)**, **Frontend (`React`, `Vite`, `TypeScript`/`JavaScript`, `Vue`/`Svelte`, `CSS`/`SCSS`, `Vitest`/`Jest`/`Node test`, `ESLint`/`Biome`)**, and **Polyglot (`Go`, `C/C++`, `C#`, `Java`/`Kotlin`, `Protobuf`, `GraphQL`)** workflows, designed as the companion to `code-context` (`cctx`) for GitHub Copilot and AI coding agents.

Instead of running the entire test suite or paying interpreter/compiler startup costs on every test or lint call, `tctx` maintains an in-memory **polyglot AST reverse dependency graph**, computes comment/whitespace-normalized **Merkle dependency hashes (`dep_hash`)**, pre-checks files with **Ruff / ESLint / Biome / Clippy**, and runs affected tests **speculatively in the background** as files are saved.

When GitHub Copilot (or a developer) invokes tests:
- **100% Cache Hit (`< 1ms`)**: If all targeted or affected tests have already passed against the current live Merkle `dep_hash`, `tctx` returns the result **instantly (< 1ms)** without spawning a test runner.
- **Delta Execution (`hybrid_delta`)**: If some tests are uncached or modified, `tctx` reuses cached results for unchanged tests, joins any in-flight background test run (**Singleflight join**), prioritizes previously failed tests first (**Fail-First scheduling**), and executes `pytest` / `cargo test` / `vitest` / `jest` / `node --test` / `go test` **only on the uncached delta**.
- **Pre-Flight Lint Diagnostics & Auto-Fix**: Modified files are pre-checked by `ruff`, `eslint`/`biome`, or `cargo clippy --no-deps` in the background worker and cached by content `sha256`. `#tctxNativeAffected` and `#tctxNativeRun` surface exact `file:row:col` diagnostics immediately, and passing `ruff_fix: true` (`--ruff-fix`) auto-remediates safe lint/import issues before tests run.

---

## Key Features

### 1. Polyglot AST Reverse Dependency Graph & Semantic Merkle Hashing
- **Python (`pytest`)**: Extracts `import x`, `from x import y`, relative imports, multi-line parenthesized imports, in-function lazy imports, `conftest.py` fixture hierarchies (connecting only tests that actually use a defined `@pytest.fixture` or `autouse`/hook), and `pyproject.toml`/`pytest.ini` configs while excluding `if TYPE_CHECKING:`-only imports from false test invalidation.
- **Rust (`cargo test`)**: Parses `mod foo;`, `#[path = "..."] mod`, nested `use crate::{...}` trees, `#[test]` / `#[tokio::test]` / `#[rstest]` / `#[test_case]` symbols, `#[ignore]` / `#[serial]` attributes, `tests/common/mod.rs` helpers, `build.rs`, and `include_str!` / `include_bytes!` compile-time data dependencies. Decouples non-barrel `lib.rs` / `mod.rs` module mounts (`pub mod <child>;`) and tree-shakes `pub use` re-exports so shared types (`use crate::Result`) never couple unrelated sibling modules.
- **Frontend (`React` / `Vite` / `TS` / `JS` / `CSS` / `SCSS`)**: Resolves ES/CJS/dynamic `import(...)`, excludes `import type` / `export type`, tree-shakes barrel (`index.ts` / `index.tsx`) named and wildcard re-exports, and supports `tsconfig.json` (`paths` & `extends`), `package.json` (`imports` `#subpath` & `exports`), Vite asset queries (`?raw`, `?url`, `?worker`), `import.meta.glob(...)`, Web Workers, CSS/SCSS `@import`/`@use`/`@forward`/partials (`_tokens.scss`), Next.js App Router (`app/**/layout.tsx -> page.tsx`), and Storybook `*.stories.tsx` play interaction tests.
- **Go / C++ / C# / Java / Protobuf / GraphQL**: Tracks Go packages (`go.mod`, `//go:embed`, `*_test.go`), C/C++ `#include "..."` & GoogleTest (`TEST`/`TEST_F`), C# `using` & `[Fact]`/`[Theory]`/`[TestMethod]`, Java/Kotlin `import` & `@Test`/`@ParameterizedTest`, `.proto` definitions, and `.graphql` `#import` edges.
- **Cross-Language & Full-Stack Bridges (`O(1)` `BridgeProviderIndex`)**:
  - **PyO3 / Maturin**: Links Rust `cdylib` / `[tool.maturin]` / `#[pymodule]` crates to Python importers so editing a `.rs` kernel marks dependent Python `pytest` files as affected alongside Rust unit tests.
  - **Full-Stack API Routes**: Links backend route definitions (`@app.get('/api/v1/orders')`) to frontend tests calling `fetch('/api/v1/orders')` or `axios`.
  - **WebAssembly & Protobuf & React Context**: Links Rust `#[wasm_bindgen]` exports to frontend Wasm imports, `.proto` schemas to generated `_pb2.py`/`.pb.go` consumers, and React `createContext` providers to `useContext` consumers.
- **Semantic AST Hashing (`semantic_sha256`)**: Strips ordinary comments and non-semantic blank lines via SIMD `memchr` fast paths so comment/formatting edits preserve `dep_hash` and never invalidate cached test passes, while preserving directive comments (`noqa`, `type:`, `pragma`, `eslint`, `@ts-`, `biome-`, `go:embed`, and Rust doctest fences).
- **Environment Variable & Data File Tracking**: Tracks referenced `os.getenv`, `os.environ`, `env!`, `env::var`, `import.meta.env`, `process.env`, and `os.Getenv` keys plus `.json`, `.yaml`, `.toml`, `.sql`, `.snap`, `.golden`, `.csv`, and `.proto` data files in each test's Merkle `dep_hash`.

### 2. High-Throughput Speculative Background Daemon (`tctx daemon`)
- **Multi-Core Parallel Indexing & Compact Integer Graph**: Parallelizes file reading, SHA-256 hashing, AST parsing, direct dependency resolution, and per-test Merkle BFS across worker threads (`std::thread::scope`), maintaining compact `u32` forward/reverse adjacency lists (`forward_adj` / `reverse_adj`) and pre-computed per-file Merkle digest buffers.
- **Zero-Clone Parent-Pointer BFS**: Reverse impact analysis (`affected_tests`, `affected_modules`, `untested_modified_files`) traverses contiguous `u32` adjacency arrays with flat `parent_id` pointers, reconstructing dependency chains only for matched test files.
- **Rust Multi-Target Precompilation & Module Filtering**: Pre-compiles `--lib` and `--test <integration>` targets in a single multi-core `cargo test --no-run` pass and passes `-- <mod_filters...>` to `cargo test --lib` so only affected Rust modules run.
- **Warm Pytest Zygote**: Pre-imports safe third-party packages (`fastapi`, `pydantic`, `numpy`, `pandas`, `sqlalchemy`, etc.) in a single-threaded fork-safe parent process with `PR_SET_PDEATHSIG` and process-group reaping, eliminating Python startup overhead.
- **Self-Healing Concurrency & Fault Tolerance**: Per-root cold load gates (`root_load_gate`), lock-free linter/Git subprocess execution, automatic `PoisonError` recovery and `catch_unwind` isolation across all daemon and runner threads, `O(1)` incremental cache memory accounting, and automatic idle Zygote reclamation under RSS pressure (`> 64 MiB`).
- **Watch Lifecycle Cleanup**: Deleted or moved-out directory subtrees are unwatched and removed from the dependency graph, freeing watch capacity for recreated paths. Overflow recovery, periodic safety reconciliation, and explicit sync also prune missing watch directories. Watch registration/removal failures are exposed as `last_watcher_error` in loaded workspace status.

### 3. VS Code GitHub Copilot Native Integration (`dotnetpower.test-context-native`)
- **7 Native VS Code Language Model Tools**: `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, and `#tctxNativeWorkspaces`.
- **Agent-Friendly Path & Root Resolution**: Automatically strips workspace-root prefixes from absolute target paths (`/path/to/repo/src/daemon.rs::test_fn -> src/daemon.rs::test_fn`) and infers the active workspace root from target path hints, `vscode-remote://` URIs, Windows WSL UNC paths (`\\wsl.localhost\...`), and session history.
- **Smart Git-Dirty Delta Scoping**: In large repositories (`> 64` test files), omitting `targets` in `#tctxNativeRun` automatically scopes execution to tests affected by uncommitted `git status` changes rather than running thousands of unrelated tests.
- **Context Window Token Optimization (92.7% Reduction)**: Strips passing test boilerplate (`test ... ok`, compiler progress lines) while preserving 100% of warnings, failures, and skips, and bounds large `#tctxNativeAffected` / `#tctxNativeGraph` outputs to the closest dependency hop chains so responses stay inline (`< 8 KB`) without `/tmp` file offloading.
- **Culprit Attribution & CI Cache Sharing**: Extracts workspace-local stack frames (`structured_failures`), cross-references `git diff` with transitive dependencies to identify `suspected_culprit_files`, exports JUnit XML / SARIF v2.1.0 reports, and supports portable CI cache bundles (`cache-export` / `cache-import`).

`tctx` learns invocation patterns routed through its Native, MCP, or CLI surfaces. Use `tctx pytest`, `tctx cargo-test`, `tctx vitest`, or `tctx jest` (or their symlinks `tctx-pytest`, `tctx-cargo-test`, `tctx-vitest`, `tctx-jest`) for drop-in CLI acceleration.

---

## Measured Performance

### 1. Python FastAPI Suite (`aks-store-demo`, 320 Paired Observations)

Across 8 independent daemon sessions (**320 paired observations**) on [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo) (`src/ai-service`):

![Test execution and query latency and paired-session speedup with 95% confidence intervals](benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | TCTX off (`pytest`) | TCTX on | Speedup [95% CI] |
|---|---:|---:|---:|
| Warm suite (`warm_suite`) | 1036.64 ms | **0.61 ms** (median 0.55 ms) | **1693.30×** [1483.04, 1866.03] |
| Post-edit background pre-run (`bg_speculative`) | 1004.61 ms | **0.57 ms** (median 0.53 ms) | **1763.31×** [1594.86, 1934.22] |
| Reverted edit Merkle hit (`merkle_revert`) | 1009.18 ms | **0.85 ms** (median 0.54 ms) | **1183.76×** [764.93, 1824.79] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 1031.66 ms | **82.83 ms** (median 76.75 ms) | **12.46×** [11.93, 13.03] |
| Impact query + Ruff pre-check (`affected_graph`) | 968.01 ms | **0.52 ms** (median 0.49 ms) | **1850.15×** [1798.47, 1915.56] |
| Fixed mixed suite (`all`, 320 pairs) | 1010.02 ms | **17.08 ms** (median 0.56 ms) | **59.15×** [56.74, 61.75] |

### 2. Rust (`cargo test`) & Enterprise Monorepo (`9,042 files / 3,434 tests`) Benchmarks

| Scenario | Baseline / Unoptimized | With `tctx` (`v0.1.1`) | Speedup / Improvement |
|---|---:|---:|---:|
| Enterprise monorepo (`9,042 files, 3,434 tests`) cold AST load | 6,047.38 ms | **670.33 ms** (parallel multi-core build) | **9.0× faster cold load (-88.9%)** |
| Enterprise monorepo core module `test_affected` (`67 affected tests`) | 256.80 ms | **5.17 ms** (`u32` `reverse_adj` parent-pointer BFS) | **49.7× faster impact query** |
| Enterprise monorepo `test_affected` (`2,363 affected tests`) payload | 1,400,000 B (`/tmp` offloaded) | **5,320 B** (hop-ranked inline `< 8 KB`) | **263× smaller inline payload** |
| 4 Rust modules (`25 unit tests`) cold batch | ~5,800 ms (4× `cargo test`) | **1,254.64 ms** (`cargo test --lib` demux) | **4.6× faster cold batch** |
| 2 Rust modules (`27 unit tests`) targeted harness filter | 1,110.00 ms (full crate 53 tests) | **90.00 ms** (`-- <mod_filters...>`) | **12.3× faster module run** |
| 6 Rust files (`53 unit tests`) warm cache hit | 6,606.00 ms | **0.87 ms** (`instant_cache_hit`) | **7,593× speedup** |
| Single Rust test node (`src/runner.rs::demux_...`) | 1,254.64 ms | **0.23 ms** (`cached_node`) | **5,455× speedup** |

See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, cold-start measurements, raw samples, and reproduction instructions.

---

## Quick Start & CLI Reference

```bash
# 1. Build and install tctx + user systemd service (or macOS LaunchAgent)
./scripts/install.sh

# 2. Register a Python, Rust, Frontend, or Polyglot workspace
tctx --root /path/to/repo register

# 3. Inspect daemon health, AST dependency graph, affected tests, or lint diagnostics
tctx --root /path/to/repo doctor
tctx --root /path/to/repo status
tctx --root /path/to/repo affected --file src/my_pkg/core.py
tctx --root /path/to/repo graph src/my_pkg/core.py
tctx --root /path/to/repo ruff --fix

# 4. Run tests incrementally across Python, Rust, Frontend, and Go
tctx --root /path/to/repo run --ruff-fix
tctx --root /path/to/repo pytest -q
tctx --root /path/to/repo cargo-test
tctx --root /path/to/repo vitest
tctx --root /path/to/repo jest

# 5. Diagnose failures, export JUnit/SARIF reports, or share cache bundles in CI/CD
tctx --root /path/to/repo explain
tctx --root /path/to/repo run --report-format=sarif --report-out=results.sarif
tctx --root /path/to/repo cache-export --out=tctx-cache.json
tctx --root /path/to/repo cache-import --input=tctx-cache.json

# 6. Install Git pre-commit/pre-push hooks or authorize Git worktrees
tctx --root /path/to/repo hook-install
tctx --root /path/to/repo authorize-worktrees

# 7. Package and install the VS Code Native Extension (Linux & Windows WSL)
./scripts/install-vscode-extension.sh
```
