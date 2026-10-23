# Test Context (`tctx`) — Hardening & Verification Report

## Overview

`test-context` (`tctx`) was audited and hardened across 5 iterative hardening rounds (each evaluating $\ge 10$ technical criteria) until **0 Critical, 0 High, and 0 Medium** findings remained.

---

## Measured Before vs. After Hardening (FDAI Repository: 8,163 Python files, 3,223 test files)

| Metric | Initial Prototype | After 5 Hardening Rounds | Improvement |
| :--- | :--- | :--- | :--- |
| **Idle Daemon CPU (`notify-rs`)** | `109.0%` (feedback loop) | **`0.0%` (`do_epoll_wait`)** | Eliminated self-induced inotify `Access(_)` storm |
| **Daemon Resident Memory (RSS)** | `624.0 MB` | **`87.9 MB`** | **86% reduction** via `u32` file ID transitive tables & on-demand root loading |
| **Body-Only Edit Graph Update** | `~220 ms` (full BFS rebuild) | **`< 1 ms` ($O(\text{affected})$)** | Skips `ModuleIndex` & BFS when AST imports/fixtures are unchanged |
| **Unchanged File Cache Check I/O** | Full file reads + SHA-256 | **`0` disk reads (`FileStamp`)** | Compares `(mtime_sec, mtime_nsec, size, ino)` before reading bytes |
| **Warm Cache Hit Latency (`tctx run`)** | `~6,072 ms` (cold pytest) | **`10 ms` (`instant_cache_hit`)** | **~600x speedup** when target tests are already cached green |

---

## Hardening Rounds Summary

### Round 1: Python AST Lexer & Dependency Graph Engine (12 Criteria)
1. **[CRITICAL - Fixed]** Compacted `transitive_deps_for_test` from `BTreeMap<String, BTreeSet<String>>` to `u32` file-index vectors (`transitive_dep_ids_for_test`), cutting RSS from 624 MB to 87.9 MB.
2. **[HIGH - Fixed]** Implemented $O(\text{affected})$ `recompute_hashes_for_changed` when a file's body changes but its `ParsedPythonFile` structure (`imports`, `fixtures`, `defined_symbols`, `data_refs`) is unchanged.
3. **[HIGH - Fixed]** Added Monorepo compartment locality filtering (`filter_by_package_locality`) so services under `services/<svc_a>` do not false-alias identically named modules in `services/<svc_b>`.
4. **[HIGH - Fixed]** Added two-pass `__init__.py` symbol re-export resolution plus direct `defined_symbols` matching (`def`, `class`, module-level assignments) to prevent barrel `__init__.py` fan-out explosions.
5. **[HIGH - Fixed]** Added static data/schema file reference tracking (`data_refs` for `.json`, `.yaml`, `.yml`, `.toml`, `.ini`, `.cfg`, `.sql`, `.rego`, `.csv`) and automatic graph ingestion (`discover_referenced_data_files`).
6. **[HIGH - Fixed]** Added `uv.lock`, `poetry.lock`, and `requirements.txt` to `PYTEST_CONFIG_FILES` so dependency upgrades automatically invalidate cached test results.
7. **[MEDIUM - Fixed]** Added `@pytest.fixture(name="alias")` custom fixture name extraction.
8. **[MEDIUM - Fixed]** Added `@pytest.mark.usefixtures(...)` and `pytestmark = pytest.mark.usefixtures(...)` extraction.
9. **[MEDIUM - Fixed]** Excluded `*args` and `**kwargs` vararg identifiers from `parse_function_params` while preserving keyword-only parameters after bare `*`.
10. **[MEDIUM - Fixed]** Hardened `scan_string_literal` to track `{ ... }` expression nesting depth inside Python 3.12+ f-strings so nested quotes do not prematurely close string tokens.
11. **[LOW - Verified]** Preserved `pending_fixture_decorator` across stacked multi-line decorators.
12. **[LOW - Verified]** Verified `importlib.import_module("...")`, `pytest.importorskip("...")`, and `pytest_plugins = [...]` dynamic dependency edges.

### Round 2: Watcher, Runner, JUnit XML Parser & Cache Integrity (11 Criteria)
1. **[CRITICAL - Fixed]** Filtered `notify::EventKind::Access(_)` and `ModifyKind::Metadata(AccessTime)` in the watcher callback, eliminating the self-induced read feedback storm.
2. **[HIGH - Fixed]** Decoupled the `notify` callback from `Mutex<RootState>` using a bounded `sync_channel(4096)` + `fs_overflow: AtomicBool` + 40ms debounce window.
3. **[HIGH - Fixed]** Fixed `extract_xml_attr` word-boundary check so `name="..."` never matches the suffix of `classname="..."`.
4. **[HIGH - Fixed]** Replaced naive `find('>')` in `parse_junit_xml_per_file` with quote-aware `find_xml_tag_end` so `>` inside failure messages or parameterized test names cannot corrupt XML parsing.
5. **[HIGH - Fixed]** Isolated multi-file batch collection errors so unmentioned sibling files are not poisoned as `Failed` when one file fails collection.
6. **[HIGH - Fixed]** Protected `RootCache::load_or_new` and `RootCache::save` with `fs2::FileExt` (`lock_shared` / `lock_exclusive`) plus atomic temp-file rename (`0600`).
7. **[MEDIUM - Fixed]** Added `FileStamp` (`mtime_sec`, `mtime_nsec`, `size`, `ino`) fast-path in `reconcile_live_files_locked` and `drain_fs_events_locked`.
8. **[MEDIUM - Fixed]** Added dynamic directory watch registration (`watcher.watch`) when new directories or files in new directories appear at runtime, plus a 25ms retry window for atomic editor rename saves.
9. **[MEDIUM - Fixed]** Changed background queue insertion for newly edited files to LIFO priority (`push_front`) and reduced default `background_batch_size` to `4` to minimize singleflight head-of-line blocking.
10. **[MEDIUM - Fixed]** Preserved `output_snippet` in `stdout_summary` when returning cached failures (`rerun_failed = false`), and verified `file.py::symbol` nodeid existence before serving whole-file cache hits.
11. **[MEDIUM - Fixed]** Hardened `run_pytest_dropin` to consume value arguments for `-c`, `-o`, `-p`, `-W`, `-n`, `--maxfail`, `--tb`, `--rootdir`, `--confcutdir`, `--import-mode`, `--basetemp`, `--durations`, `--cov`, `--cov-report`, `--cov-config`, `--log-level`, `--color`, and `--capture`, and placed child `pytest` processes in a dedicated process group (`process_group(0)`) killed via `kill(-pgid, SIGKILL)` on timeout.

### Round 3: Daemon Security, Concurrency & Resource Bounds (10 Criteria)
1. **[HIGH - Fixed]** Serialized cold workspace loads with `load_gate: Mutex<()>` in `DaemonHub::get_or_load_root` to prevent duplicate concurrent loads and leaked background threads.
2. **[HIGH - Fixed]** Removed unconditional startup preloading of all registered workspaces; roots now load on demand.
3. **[HIGH - Fixed]** Added `RuntimeIdentity` verification checking ownership (`geteuid`), permissions (`0700` state dir, `0600` socket/lock), and `(dev, ino)` identity every 5 seconds.
4. **[MEDIUM - Fixed]** Added `ActiveConnection` RAII guard enforcing `MAX_ACTIVE_CONNECTIONS = 64`.
5. **[MEDIUM - Fixed]** Added `IDLE_ROOT_TIMEOUT` (15 minutes) and `MAX_RESIDENT_ROOTS = 8` LRU eviction with clean `ManagedRoot::shutdown()`.
6. **[MEDIUM - Fixed]** Hardened `request_or_local` to attempt `systemctl --user start test-context.service` before falling back to a one-shot local execution with `background_enabled = false` and explicit `shutdown_all()`.
7. **[MEDIUM - Fixed]** Added strict validation for `_tctx_trace` correlation IDs (`1..=96` ASCII alphanumeric/`-`/`_`/`.`).
8. **[MEDIUM - Fixed]** Added `ingest_explicit_paths_locked` so newly created files passed directly to `test_run`, `test_affected`, or `test_graph` before inotify delivery are immediately indexed.
9. **[LOW - Fixed]** Added `MemoryHigh=512M` and `MemoryMax=1G` guardrails to `deploy/test-context.service`.
10. **[LOW - Fixed]** Handled `BrokenPipe` cleanly in CLI JSON output when piped to `head` or `jq`.

### Round 4: VS Code Native Extension & Test Suite (10 Criteria)
1. **[HIGH - Fixed]** Added trailing-buffer protocol desynchronization check (`this.receiveBuffer.byteLength > 0`) in `DaemonClient.onData`.
2. **[HIGH - Fixed]** Updated `runStartupProbe` to verify actual Language Model Tool dispatch through `vscode.lm.invokeTool('test-context-native_testWorkspaces', ...)`.
3. **[MEDIUM - Fixed]** Updated `runSelfTest` to set the `testContextNative.selfTestPassed` context key upon verification.
4. **[MEDIUM - Fixed]** Added `getting-started` Walkthrough contribution to `package.json` and wired `testContextNative.gettingStarted` to open it.
5. **[MEDIUM - Fixed]** Shortened extension `description` to $\le 140$ characters for Marketplace compliance.
6. **[MEDIUM - Fixed]** Added `test/daemonClient.test.ts` covering persistent framed IPC (`TCTXJ1\n`), trace negotiation, application errors, and cancellation tokens.
7. **[MEDIUM - Fixed]** Added `test/manifest.test.ts` verifying all 7 `languageModelTools`, `toolReferenceName`s, PNG dimensions (`256x256` icon, `1440x480` banner), and walkthrough schema.
8. **[LOW - Verified]** Verified strict TypeScript compiler flags (`strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`) and ESLint type-checked rules (`0` warnings/errors).
9. **[LOW - Verified]** Verified WSL bridge readiness frame (`TCTXN1\n`) and service auto-start fallback.
10. **[LOW - Verified]** Packaged and installed `dist/test-context-native.vsix` into VS Code (`moonchoi.test-context-native@0.1.0`).

### Round 5: Final 12-Criteria Audit (Residual Risk Assessment)
All 12 audit dimensions (Watcher safety, AST soundness, Monorepo isolation, Data-file tracking, Incremental hash complexity, JUnit XML resilience, Cache atomicity, Subprocess cleanup, Daemon identity/bounds, MCP JSON-RPC compliance, Native VSIX callability, and Live resource footprint) passed with **0 Critical, 0 High, and 0 Medium** issues remaining. Only documented Low/architectural trade-offs remain (e.g., arbitrary runtime reflection or external network state outside the workspace requires `--no-cache` or `test_invalidate`).

---

## Deep Hardening Rounds 6–11

### Round 6 — Cache correctness and execution identity (12 criteria)

1. Added before/after dependency snapshot verification; source changes during a test return `SOURCE_CHANGED_DURING_TEST`, discard the result, and requeue only eligible tests.
2. Added environment fingerprinting for TCTX version, Python executable/version/platform, pytest version, plugin entry points, command line, selected environment variables, and lock files.
3. Bumped cache schema to v4 and isolated WAL names by schema.
4. Added provenance fields to every cache record: environment hash, TCTX version, runner mode, start/end timestamps.
5. Stopped caching all-skipped files by default.
6. Added same-`dep_hash` pass/fail conflict detection and cache quarantine.
7. Added `ctime` to `FileStamp` to catch equal-size edits whose `mtime` is restored.
8. Added configured `python_files` parsing from `pyproject.toml`, `pytest.ini`, `setup.cfg`, and `tox.ini`.
9. Added integration coverage for source mutation during execution.
10. Added WAL restart persistence coverage.
11. Added node-level safe cache coverage.
12. Added flaky/quarantine and skip-policy unit coverage.

### Round 7 — Zygote safety and result transport (12 criteria)

1. Restricted third-party preload to a bounded safe allowlist.
2. Added active-thread count guard before `fork()`; unsafe parents fall back to subprocess.
3. Closed inherited child file descriptors except bounded protocol/output descriptors.
4. Added Zygote crash circuit breaker (three failures, 60-second fallback).
5. Replaced newline JSON IPC with length-prefixed frames.
6. Replaced temporary stdout/stderr files with Linux `memfd_create` where available.
7. Added explicit pytest plugin manifest discovery using `pytest11` entry points.
8. Disabled implicit plugin autoload inside Zygote and loaded the fingerprinted manifest explicitly.
9. Added pytest parser/plugin warm-up before readiness.
10. Added direct in-memory pytest report capture for dynamic dependency runs.
11. Kept hardened JUnit fallback for low-overhead subsequent delta runs.
12. Added plugin errors, active process group, failures, and circuit state to status diagnostics.

### Round 8 — Background authority, scheduling, and cancellation (12 criteria)

1. Changed speculative execution default to module-level `pytestmark = pytest.mark.background_safe`.
2. Added explicit `--background-all` opt-in for controlled benchmarks/development.
3. Denied background execution for live, integration, destructive, network, external, Azure, Docker, and serial markers.
4. Added module-level marker parsing so one function decorator cannot authorize an entire file.
5. Added resource-key locks for database, Docker, network, serial, port, and `resource_*` markers.
6. Added shortest-first duration scheduling within the priority queue.
7. Preserved recent-edit LIFO priority before duration selection.
8. Added adaptive `/proc/loadavg` throttling and `TCTX_PAUSE_BACKGROUND`.
9. Added background deferral counters to status.
10. Added process-group cancellation through `test_cancel`.
11. Propagated VS Code cancellation over a second native IPC connection.
12. Prevented externally cancelled children from producing cached failures.

### Round 9 — Persistent cache and graph scaling (10 criteria)

1. Replaced whole-cache rewrites with append-only, lock-protected WAL events.
2. Added atomic snapshot compaction at a 16 MiB WAL threshold.
3. Added partial upsert, invalidate, reset, quarantine, eviction, and metrics events.
4. Bounded WAL replay to 64 MiB.
5. Enforced cache record limits again after replay.
6. Preserved atomic snapshot rename and `sync_all` at compaction boundaries.
7. Exposed persistence errors in status instead of silently ignoring them.
8. Added incremental structural edge replacement for ordinary import changes.
9. Recomputed transitive closure/hash only for tests affected before or after the edge change.
10. Preserved full rebuild fallback for new/deleted files, config, `conftest.py`, and package `__init__.py`.

### Round 10 — Dynamic dependencies and safe fine-grained reuse (10 criteria)

1. Added Python audit-hook capture for workspace data files opened at runtime.
2. Associated runtime data dependencies with the active pytest nodeid instead of the entire batch.
3. Kept Python module dependencies in the AST graph to avoid batch-wide dynamic import false positives.
4. Made learned runtime dependencies monotonic and persisted them in cache records.
5. Rebuilt graph hashes when new runtime data dependencies are learned.
6. Added per-node outcomes, durations, and failure snippets to cache records.
7. Restricted node-level reuse to module-level `pytestmark = pytest.mark.node_cache_safe`.
8. Prevented node selector requests from returning whole-file test counts.
9. Added opt-in module-level `parallel_safe` multi-file parallel Zygote execution.
10. Kept parallel execution disabled by default and gated behind `--parallel-delta`.

### Round 11 — Final repeated audit (14 criteria)

The final audit checked snapshot stability, environment identity, fork safety, inherited file descriptors, cancellation semantics, background authorization, resource conflicts, load throttling, cache WAL crash behavior, cache schema isolation, dynamic dependency ownership, node cache safety, strict Clippy, and end-to-end benchmark parity. During this audit, four additional defects were found and fixed:

1. Module markers were initially parsed inside the `usefixtures` branch; moved to a correct top-level parser branch.
2. Function-level markers could authorize file-level background/node/parallel behavior; changed to module-level markers only.
3. Cancelled Zygote children could be interpreted as failures; added signal-aware cancellation response.
4. Runtime audit dependencies were initially assigned batch-wide; changed to nodeid-specific attribution.

After repair, all Rust tests, strict Clippy (`-D warnings`), TypeScript compilation, ESLint, Node tests, WAL restart, source-race rejection, process-group cancellation, and 320 paired AKS Store Demo observations pass. No known Critical, High, or Medium finding remains.
