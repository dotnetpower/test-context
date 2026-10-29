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

---

## Additional Hardening Rounds 12–16

### Round 12 — Linearizable freshness and sync safety (10 criteria)

1. Restored a target-dependency metadata barrier on every `test_run`; watcher delivery is no longer assumed to precede an immediate request.
2. Kept the 60-second full metadata safety scan for missed membership events.
3. Added `ctime_sec` and `ctime_nsec` to file stamps, complementing mtime, size, and inode.
4. Kept live SHA-256 verification when a stamp changes.
5. Prevented `test_sync` from replacing a `ManagedRoot` while a test is executing.
6. Reused the existing root for sync so in-flight singleflight/resource state is preserved.
7. Changed `test_cancel` to act only on an already loaded root; cancellation never cold-loads a workspace.
8. Retained post-execution `dep_hash` verification and explicit `SOURCE_CHANGED_DURING_TEST`.
9. Added environment identity revalidation every 60 seconds.
10. Added generation, watcher event, safety-scan age, and environment-check age diagnostics.

### Round 13 — Zygote lifecycle and bounded output (11 criteria)

1. Replaced unbounded `memfd` output growth with concurrently drained bounded stdout/stderr pipes.
2. Bounded each captured stream at 2 MiB while continuing to drain overflow to avoid child deadlock.
3. Added `PR_SET_PDEATHSIG=SIGKILL` to the Zygote launched by Rust.
4. Added `PR_SET_PDEATHSIG` to each pytest fork child.
5. Closed the parent-death race by checking `getppid()` immediately after `prctl`.
6. Added an RLIMIT_CPU safety ceiling derived from the configured timeout.
7. Added active-PGID RAII cleanup on every Rust error path.
8. Added Zygote liveness checks through `try_wait()`.
9. Added automatic Zygote recycling after 200 child runs.
10. Added cancellation tracking for subprocess fallback, not just fork children.
11. Added a multi-megabyte stdout integration test proving bounded response size and no pipe deadlock.

### Round 14 — WAL crash recovery and cache semantics (10 criteria)

1. Bumped cache schema to v5.
2. Added monotonic WAL sequence numbers.
3. Added SHA-256 checksum verification for every WAL event.
4. Isolated WAL filenames by schema version.
5. Preserved the valid prefix of a crash-truncated final line.
6. Truncated a damaged tail under exclusive lock before appending new events.
7. Added directory fsync after atomic snapshot rename.
8. Added WAL sequence, bytes, snapshot bytes, and record counts to status.
9. Prevented caching files containing any skipped tests, not only all-skipped files.
10. Added a test that recovers a valid prefix, repairs the tail, appends a new record, reloads both records, and compacts atomically.

### Round 15 — Daemon concurrency and operational controls (10 criteria)

1. Added starvation prevention: shortest-first scheduling uses every fourth batch for the longest queued tests.
2. Preserved recent-edit LIFO priority before duration scheduling.
3. Added watcher generation and drained-event counters.
4. Added background batch counters and load-deferral diagnostics.
5. Added connection-slot RAII unit coverage.
6. Added module-level deny-marker policy tests.
7. Added module-level resource-key extraction tests.
8. Added a configurable `--background-max-load-ratio`; deterministic benchmarks explicitly use 200%.
9. Removed eager initial full-suite background warming; speculative execution now follows actual edits or explicit invalidation/sync.
10. Added process-group registry support so opt-in parallel delta children can all be cancelled.

### Round 16 — Final 12+ criteria audit (13 criteria)

The final repeated audit covered immediate-save freshness, sync/run concurrency, parent/child lifecycle, bounded output, timeout/cancellation distinction, WAL prefix recovery, WAL repair-and-append, background authorization, resource conflicts, starvation, plugin identity caching, large-repository cache-hit cost, and exact-binary benchmark provenance.

It found and fixed these additional defects:

1. A killed child whose result pipe closed before `waitpid` could be mislabeled as timeout.
2. Subprocess fallback was not included in active process-group cancellation.
3. Parallel opt-in workers needed a multi-PGID cancellation registry.
4. A truncated WAL tail was detected but not removed before later appends.
5. Module-level deny/resource markers were initially read only from function markers.
6. Eager small-suite warmup made cold benchmark behavior nondeterministic.
7. Runtime identity and plugin entry-point discovery used duplicate Python subprocesses; they now share a 60-second identity cache.
8. Large-workspace cache hits unnecessarily recomputed preload module lists; that work now happens only when tests execute.

All strict Clippy, Rust unit/integration/doc tests, Python benchmark artifact tests, TypeScript compile, ESLint, Node tests, clean-target checks, and paired benchmark parity checks pass. No known Critical, High, or Medium issue remains.

---

## Additional Hardening Rounds 17–21

### Round 17 — Pytest collection semantics (10 criteria)

1. Added multiline `python_files` parsing for TOML and INI-style configuration.
2. Added `testpaths` parsing and safe relative-path validation.
3. Restricted test-file classification to configured `testpaths` when present.
4. Preserved default `test_*.py` and `*_test.py` behavior when no override exists.
5. Added support for both `=` and `:` configuration delimiters.
6. Added quoted list and whitespace-separated pattern parsing.
7. Added tests proving matching files outside configured testpaths are not treated as tests.
8. Changed explicit unmatched targets from success-shaped `no_tests` to `NO_TESTS_MATCHED`.
9. Preserved parameterized nodeid execution and safe exact node-result reuse.
10. Kept broad fallback invalidation for unresolved wildcard/package re-exports.

### Round 18 — Cache memory and disk bounds (10 criteria)

1. Added a 128 MiB in-memory cache budget.
2. Added a 10,000 node-results-per-file limit.
3. Capped file failure snippets at 8 KiB.
4. Capped node failure snippets at 2 KiB.
5. Capped failed node IDs at 10,000.
6. Deduplicated and capped dynamic dependencies at 4,096 entries.
7. Evicted oldest records until both count and byte limits are met.
8. Removed matching latest/quarantine entries when history is evicted.
9. Reapplied byte and count limits after snapshot/WAL replay.
10. Added estimated memory usage to cache status and a stress test with Unicode traces and oversized node maps.

### Round 19 — Graceful shutdown and recovery (10 criteria)

1. Installed SIGTERM/SIGINT stop handling.
2. Replaced blocking `incoming()` with `poll()`-driven acceptance that wakes immediately on new connections.
3. Added graceful root/Zygote shutdown before process exit.
4. Added a bounded connection drain deadline.
5. Added maintenance-thread termination and join.
6. Removed the Unix socket after graceful shutdown.
7. Refused to replace stale runtime paths unless they are owned Unix sockets.
8. Reduced persistent connection read timeout and checked stop state between frames.
9. Added daemon shutdown integration coverage proving zero exit and socket cleanup.
10. Added stale regular-file rejection coverage.

### Round 20 — Native IPC cancellation and protocol hardening (10 criteria)

1. Added daemon `test_cancel` capability reporting.
2. Added fire-and-forget cancellation over a second native connection.
3. Added an active request cancellation test proving the second connection carries `test_cancel`.
4. Added protocol desynchronization coverage for multiple response frames in one chunk.
5. Added timeout/reconnect/recovery coverage.
6. Preserved application errors without unnecessary reconnects.
7. Added a fixed connection worker pool to remove per-request thread creation.
8. Preserved the 64-connection admission bound and RAII release.
9. Added `poll()` acceptance to avoid the nonblocking-loop latency regression.
10. Added multi-PGID cancellation for opt-in parallel execution.

### Round 21 — Final audit and performance gates (12 criteria)

1. Added source-tree SHA-256 provenance to benchmark records.
2. Added test enforcement that published results match the current `src/*.rs`, Cargo.toml, and Cargo.lock.
3. Added lower-bound speedup gates for warm, delta, affected, and mixed workloads.
4. Added upper-bound warm latency gates.
5. Added exact record/session/task count validation.
6. Added exact installed/release binary SHA-256 verification.
7. Re-ran 8 independent sessions and 320 paired observations.
8. Inspected both final PNG charts for overlap and label clipping.
9. Verified `aks-store-demo` remains clean.
10. Re-ran strict Clippy, Rust, Python, TypeScript, ESLint, and Node suites.
11. Verified graceful service restart and idle resource use.
12. Repeated the final review after every discovered defect until no known Critical/High/Medium finding remained.

The last five rounds found and repaired: success-shaped no-test targets, multiline pytest config loss, testpaths false positives, cache memory amplification, unbounded child output, SIGTERM socket leakage, unsafe stale socket replacement, timeout/cancellation misclassification, fixed-poll accept latency, nested native tests, WAL append after truncated tails, and eager connection-thread creation.

### Round 22 — Predictive invocation and long-running resources (12 criteria)

1. Added bounded learning for successful whole-file `test_run` patterns.
2. Excluded failed, filtered, node-level, and arbitrary-argument invocations from prediction.
3. Ranked affected tests by observed frequency and recency before background batching.
4. Persisted invocation patterns through the checksummed WAL and snapshot format.
5. Coalesced repeated pending invocation events so cache-hit traffic cannot grow memory linearly.
6. Bounded learned patterns to 128 entries and re-applied the bound after WAL recovery.
7. Filtered access-only watcher callbacks before they reach the bounded event queue.
8. Preserved overflow detection and dynamic directory watch registration.
9. Removed eager Zygote creation and limited preload discovery to requested tests.
10. Added idle Zygote reclamation and parent/child RSS diagnostics.
11. Added a resource soak covering 500 cache hits, 50 edit/delta cycles, repeated cancellation, FD/thread bounds, RSS growth, idle CPU, and idle child reclamation.
12. Documented that direct shell `pytest` commands are not intercepted and must use `tctx pytest` to participate in learning.

### Rounds 23–32 — Additional stability hardening

23. Drained subprocess stdout and stderr to EOF while retaining only bounded bytes, preventing `SIGPIPE` from changing test outcomes.
24. Classified externally terminated subprocess groups as cancellation instead of cacheable test failure.
25. Added a 10-second Zygote readiness-handshake deadline.
26. Added bounded child-start and final-result protocol deadlines and kill-on-stall recovery.
27. Added a daemon shutdown gate that rejects queued work and cancels active roots before worker drain.
28. Retained and joined background-worker handles before final cache persistence and Zygote teardown.
29. Reloaded and merged durable cache state under the exclusive lock before WAL append or snapshot compaction.
30. Made watcher creation and initial watch registration fail closed; runtime watcher errors now request a full rescan.
31. Enforced registered include globs in inventory and watcher filtering while always retaining pytest configuration files.
32. Replaced stable empty Python runtime identity on inspection failure with a unique unavailable identity, preventing cache reuse.

Post-round validation: strict Clippy passed; 21 Rust unit tests, 2 Rust integration tests, and 11 native extension tests passed. The resource soak completed 500 cache hits, 10 cancellations, and 50 edit/delta cycles with stable FD/thread counts and zero idle CPU ticks. The `aks-store-demo` benchmark completed 8 isolated daemon sessions and 320 paired observations with test parity in every session.

### Rounds 33–37 — Five monitored resource-optimization cycles

33. Confirmed 391 MiB anonymous RSS with zero loaded roots was released-object allocator retention, then invoked glibc `malloc_trim` from the maintenance cycle. A 30-minute replay removed swap growth and reduced final RSS from 469 MiB to 217 MiB under concurrent external root activity.
34. Added a 192 MiB daemon RSS pressure ceiling that evicts the oldest inactive root. Two intentionally loaded FDAI graphs converged from 186 MiB and two roots to 100 MiB and one root, with no swap or ping failure.
35. Reduced inactive retention for graphs with at least 5,000 indexed files from 15 minutes to 5 minutes. One 8,440-file graph returned from 97 MiB to an 8–9 MiB baseline before a separate external session loaded another root.
36. Capped connection workers at eight. Active-root threads fell from 20 to 12 and idle threads from 18 to 10; the 30-minute run ended at 12.7 MiB RSS, five FDs, zero swap, and 0.225% mean CPU.
37. Expired idle persistent Native/MCP clients after 30 seconds. Twelve deliberately stalled clients raised FDs from 5 to 25, then all FDs returned to 5; subsequent pings all succeeded. A concurrently loaded large root rose to 103 MiB and was reclaimed to a 13.3 MiB baseline.

Final validation after all five cycles: strict Clippy passed; 26 Rust unit tests, 2 Rust integration tests, and 11 native extension tests passed. The resource soak completed 500 cache hits, 10 cancellations, and 50 edit/delta cycles with RSS ending at 7.1 MiB, stable FD/thread counts, and zero idle CPU ticks. The final `aks-store-demo` benchmark completed 8 isolated daemon sessions and 320 paired observations with parity in every session.

### Log-driven Native bridge and readability hardening

The Windows and WSL output logs identified six actionable issues:

1. Daemon idle expiry left the long-lived WSL bridge holding a dead binary connection, producing 127 `Broken pipe` messages.
2. The startup probe classified transport failures as `NATIVE_NOT_READY` 43 times.
3. Windows multi-root sessions logged four expected explicit-root selections as deferred registration warnings.
4. The Output channel emitted 46 raw `TCTX_CLIENT_TRACE` JSON records.
5. Failed transport traces attempted 42 redundant reports over the same disconnected path.
6. Status, self-test, sync, and workspace output used JSON dumps rather than operator-readable text.

The bridge now exits on transport disconnect so the extension's bounded reconnect path creates a fresh bridge. Startup waits up to two seconds for the native tool registry and reports Windows multi-root mode as ready. System-error traces are not reported back over a failed transport. Output records now use stable `[ready]`, `[success]`, `[info]`, `[warning]`, `[error]`, and `[trace]` lines; status and workspace objects have bounded text summaries. Nested daemon trace payloads and request arguments are excluded from Output logs.

### Current-session Native feedback review

The current session selected `tctxNativeStatus` after Native installation and did not return to the Test Context MCP surface. The first status request still took 3.34 seconds: 3.02 seconds were daemon IPC while an inactive 8,440-file FDAI graph was loaded only to report status. `test_status` now returns registered/unloaded state without loading the graph by default. Agents can request `load=true` for detailed counts, while the interactive CLI and explicit **Show Native Tool Status** command retain detailed loading. This preserves the 5-minute large-root memory policy and removes unnecessary cold-load latency from routine agent health checks.

Installed-session verification reduced the same Native status request from **3,340.16 ms to 2.09 ms** (about **1,596×**) and left the target graph unloaded. An explicit `load=true` request still loaded all 8,440 Python files and returned detailed graph/cache/Zygote counts in 3,602.94 ms.

### Source-change stability retry

A Native `tctxNativeRun` returned `SOURCE_CHANGED_DURING_TEST` because the target test file was edited while pytest was running. The discard is correct: a result computed for content that no longer exists must never be cached as a pass. The status probe showed a healthy daemon with no background or persistence error, so the cause was a genuine concurrent edit, not environment-hash churn.

Discarding left the agent with no result, though, so `run_tests` now re-runs up to two times (250 ms and 500 ms backoff) when the content changed mid-run, only when the failed attempt took under 60 seconds and the daemon is not shutting down. Continuous edits still end in an error, now worded `result discarded (N requeued for background); the file was edited while tests ran, retry once edits settle`. The integration test covers both a single mid-run edit (absorbed by the retry, current result returned) and continuous churn (discarded, never cached). The benchmark was re-run for the changed source tree (8 sessions, 320 pairs, parity in every session).

### Skipped-test cache poisoning

Another session reported pytest results that disagreed with direct runs. Reproduced on a FDAI worktree: `lifecycle/hub/tests/test_hubs_postgres.py` skips all six tests when `FDAI_DATABASE_URL` is unset, which is the daemon's environment. The first on-demand run reported `1 passed, 0 skipped` and cached the file as a pass, so later calls returned an instant green while the same file run with the project's database wrapper executed six real tests.

Two defects combined. The Zygote reporter recorded only `call` reports and failed `setup`/`teardown` reports, so a skip raised in fixture setup produced no per-file evidence. The missing-evidence fallback then converted a clean exit into `passed=1` and cached it. The reporter now records setup-phase skips, and a clean exit without per-file reports is reported as unverified and never cached. Skipped results already bypass the cache, so skip-only and mixed files are re-executed and reported with accurate counts.

The run summary now states that skipped tests are not verification and that TCTX executes pytest in the daemon environment, and a replayed cached failure points to `rerun_failed`/`no_cache`. The Copilot instructions say the same. The environment fingerprint tag moved from `tctx-env-v2` to `tctx-env-v3`, which invalidates every record cached by earlier builds, including poisoned ones. A regression test covers a fixture-setup skip across repeated runs and fails without the fix. The benchmark was re-run for the changed source tree.
