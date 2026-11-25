# Changelog

## 0.1.5

- Complete multi-round hardening across the Rust backend, polyglot AST dependency graph, Warm Pytest Zygote, CLI drop-in runners, and VS Code native extension, and decompose monolithic Rust/TypeScript source files (`ast_graph`, `runner`, `zygote`, `cli`, `daemon`, `cache`, `workspace`, `rootResolver`, `nativeTools`) into cohesive SRP submodules (extracting `zygote_worker.py` via `include_str!`).
- Focus polyglot support on **Python, Rust, Frontend (TS/JS/Vue/Svelte/CSS), C/C++, and C#** (plus Protobuf/GraphQL): add C# `namespace` / `using` / `global using` / `using static` / alias resolution, live incremental namespace updates, `.sln`/`.csproj`/`Directory.Build.props` isolation, and C/C++ `include/`/`inc/`/`src/` & nonstandard `.inc` header discovery with `CMakeLists.txt` scoping.
- Fix real-world FastAPI/Zygote regressions (multiline `os.environ.get`/`os.getenv` parsing, scoped `.env` handling, benign stdlib/wheel audit probes, immutable wheel stamp deduplication, and hot-path env probe throttling) and refresh the 8-session, 320-pair `aks-store-demo` benchmark suite.
- Upgrade the VS Code status bar with a `cctx`-style rich `MarkdownString` hover tooltip (KPI summary table, per-ecosystem AST/test breakdown, Background Worker & Warm Pytest Zygote telemetry, Native tool metrics, clickable quick-action links, and automatic non-blocking daemon status polling).

## 0.1.4

- Allow safe Rust unit tests (`src/**/*.rs` without deny markers) to run speculatively in the background worker even under `background_safe_only`, while keeping foreground `cargo test` builds uncapped and scoping `CARGO_BUILD_JOBS=4` / `RUST_TEST_THREADS=4` to background worker subprocesses.
- Isolate pure Rust, Frontend, and Go `dep_hash` calculations from Python virtual-environment/plugin fingerprints (`non_python_environment_hash`) and non-Maturin `pyproject.toml` edits, while tracking `Cargo.lock` changes and post-build lockfile updates without false `SOURCE_CHANGED_DURING_TEST` discards.
- Batch multi-crate Cargo workspace `--lib` test precompilation in a single `cargo test --no-run` pass, map `src/main.rs` directly to its `[[bin]]` executable instead of rebuilding `--bins` twice, attribute precompile duration into `saved_ms`, and fix two-token Cargo flags (`-p`, `--bin`, `--features`, `-j`, `-- --skip`) in the `tctx-cargo-test` drop-in parser.
- Migrate legacy `pytest_timeout_secs: 180` workspace policies to `300`, fall back to `HEAD` commit files when `targets` is omitted on clean >64-test repositories, add configurable daemon root retention (`TCTX_IDLE_TIMEOUT_SECS`, `TCTX_LARGE_ROOT_IDLE_TIMEOUT_SECS`, `TCTX_MAX_RESIDENT_ROOTS`), and split integration tests into parallel binaries.

## 0.1.3

- Add persisted `bincode` AST + edge snapshots (`.v4.ast.bin`), multi-core directory scanning, `O(1)` scoped config hashing, and persisted Python runtime metadata caching for up to 6–25× faster cold workspace loads.
- Support root `.gitignore` DFA filtering (including negation and directory-only rules) and restrict non-test Rust string `data_refs` to static fixtures/compile-time includes so mutable runtime state files never cause false invalidations.
- Split Rust `api_semantic_sha256` from inline `#[cfg(test)]` blocks so editing unit tests inside a module preserves downstream importer caches, and isolate monorepo sub-crate `Cargo.toml` invalidation and target expansion.
- Preserve verified test results in `test_run` when unrelated files are edited concurrently, execute precompiled Cargo test binaries concurrently in parallel, wire `cargo-nextest` support, and separate Cargo compilation timeouts from execution timeouts.
- Add structured daemon/MCP operational logging and bounded `metrics.jsonl` telemetry, automatic zombie `.worktrees/` and legacy/orphan cache GC, and `test_status(load=true)` `loaded_roots` consistency.
- Add a dedicated read-only fast IPC lane in `DaemonClient` to eliminate Head-of-Line blocking during long `test_run` calls, debounce workspace-folder probes, and recognize short-flag WSL MCP configurations.

## 0.1.2

- Release watches and graph entries for deleted or moved-out directory subtrees, recover watch capacity, and rewatch recreated paths.
- Preserve nested watches across directory renames and clean up missing watches during overflow recovery, safety reconciliation, and sync.
- Surface watch registration/removal failures and watched-directory counts in workspace status.
- Normalize native and MCP argument aliases, Markdown links, line suffixes, file URIs, and WSL paths for reliable workspace and test-target resolution.
- Improve Node.js discovery for daemon environments and enable MCP fallback and subagent tool configuration during installation.
- Refresh the 8-session, 320-pair Python benchmark using the release binary and assert the exact fixture-aware affected-test set.

## 0.1.1

- Resolved multi-byte UTF-8 escape char-boundary panics across Python, Rust, and Frontend semantic normalizers and tokenizers.
- Added poison auto-recovery and `catch_unwind` isolation across `DaemonHub`, `ManagedRoot`, and background workers, plus per-root load gates and lock-free linter/git execution.
- Accelerated AST graph build and reverse-dependency queries via parallel multi-core indexing, compact `u32` forward/reverse adjacency lists, parent-pointer BFS, and `O(1)` cross-language bridge indexing.
- Optimized Rust incremental builds and test execution (`--no-run` multi-target precompilation, `--lib` module harness filtering, and non-barrel `lib.rs` decoupling).
- Enhanced VS Code Copilot compatibility with workspace-root path normalization, WSL/multi-root automatic root inference, singleflight workspace registry caching, and inline response bounding.

## 0.1.0

- First Marketplace release of **Test Context by dotnetpower** (`dotnetpower.test-context-native`) for VS Code and GitHub Copilot.
- Added 7 native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`).
- Dual-language Python (`pytest`) and Rust (`cargo test`) AST dependency graph, PyO3/Maturin cross-language native module tracking, and static environment variable / data file hashing.
- Speculative background Ruff pre-flight diagnostics (`ruff_diagnostics`) and automatic remediation via `ruff_fix: true`.
- Persistent framed-JSON Unix socket IPC (`TCTXJ1`) with the shared Rust `tctx daemon` and WSL native bridge for Windows Agents.
- Packaged walkthrough media, benchmark chart (`resources/benchmark.png`), and Marketplace metadata.
