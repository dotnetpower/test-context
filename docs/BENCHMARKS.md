# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

Measured with the current `v0.1.4` hardened & modularized release build. Exact source-tree and binary hashes are
recorded in [results.json](../benchmarks/results/aks-store-demo/results.json).

- **Warm suite re-execution (`warm_suite`)**: **763.03 ms without TCTX** vs **0.69 ms with TCTX** (**1,113.23× speedup**, 95% CI **1,062.72–1,168.37×**; median **757.17 ms → 0.66 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests and Ruff rules while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.67 ms** (median **0.64 ms**) vs **754.23 ms** for direct `pytest` (**1,127.44× speedup**, 95% CI **1,080.98–1,171.86×**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.69 ms** (median **0.68 ms**) vs **759.33 ms** (**1,095.76× speedup**, 95% CI **1,071.13–1,121.72×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, full runtime `sys.addaudithook` dynamic dependency capture, and source-stability verification)** in **102.21 ms** (median **100.50 ms**) vs **764.76 ms** (**7.48× speedup**, 95% CI **7.32–7.65×**).
- **Impact analysis + Ruff pre-check (`affected_graph`)**: Querying affected tests and cached Ruff diagnostics via the in-memory Rust AST reverse dependency graph takes **0.51 ms** (median **0.50 ms**) vs **729.86 ms** for `pytest --collect-only` (**1,439.47× speedup**, 95% CI **1,392.80–1,486.37×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **754.24 ms off → 20.95 ms on** (**36.00× mean speedup**, 95% CI **35.46–36.52×**, and **1,125.52× median speedup** from **751.59 ms to 0.67 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 763.03 ms | 0.69 ms | 757.17 ms | 0.66 ms | 1113.23× [1062.72, 1168.37] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 754.23 ms | 0.67 ms | 758.91 ms | 0.64 ms | 1127.44× [1080.98, 1171.86] |
| Reverted edit (`merkle_revert`) | 64 | 759.33 ms | 0.69 ms | 753.65 ms | 0.68 ms | 1095.76× [1071.13, 1121.72] |
| Immediate leaf delta (`leaf_delta`) | 64 | 764.76 ms | 102.21 ms | 757.77 ms | 100.50 ms | 7.48× [7.32, 7.65] |
| Impact query (`affected_graph`) | 64 | 729.86 ms | 0.51 ms | 728.95 ms | 0.50 ms | 1439.47× [1392.80, 1486.37] |
| Fixed mixed suite (`all`) | 320 | 754.24 ms | 20.95 ms | 751.59 ms | 0.67 ms | 36.00× [35.46, 36.52] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, preloads the Warm Pytest Zygote parent, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **952.36 ms** (median **934.53 ms**) vs **812.99 ms** (median **774.31 ms**) for direct `pytest` (+139 ms one-time startup overhead, amortized on the very next call). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

## Cache-Disabled Execution Review

A separate local before/after probe compared build `d53b4a6` with the optimized runner.
Background execution was disabled and every request used `no_cache=true`; assertions checked
the number of executed files and passing tests. A two-file fixture imported Pydantic and HTTPX
without making network requests. Each of three private daemon sessions performed six
broad/repeated-broad/narrow cycles. The first cycle was excluded, leaving 15 observations per row.

| Executed selection | Previous median | Optimized median | Reduction |
|---|---:|---:|---:|
| Broad selection after a narrow selection | 266.59 ms | 56.64 ms | 78.8% |
| Repeated broad selection | 64.41 ms | 54.74 ms | 15.0% |
| Narrow selection after a broad selection | 229.60 ms | 55.07 ms | 76.0% |

The previous build created 36 parent processes across 54 requests. The optimized runner used
three parents, one per session. A parent is reused only when its interpreter, root, Python path,
plugin manifest and lifetime remain compatible and it already contains every requested preload.
New preloads or changed configuration still require a new parent. Test modules and fixtures run
in fresh forked children, not in the reusable parent.

A separate 1,200-case parametrized fixture exposed a result-pipe stall: the old reader stopped
after the first chunk and waited for the child, while the child was blocked writing the rest.
With a deliberately short three-second pytest limit and the ten-second response grace, two cold
runs took a median **13.89 s** and fell back to subprocess execution. The optimized reader
drained the complete bounded result and finished in **0.63 s**, with all 1,200 cases passing and
no fallback. These timings describe this reproduction, not a universal suite speedup.

Zygote runs now use structured reports whether or not dynamic dependency capture is needed,
avoiding the internal JUnit write/read/parse path. User-requested JUnit output and subprocess
fallback remain available. Payload limits, cancellation, timeouts, skip counts, fixture errors,
source-stability checks and the 200-run/idle parent recycling rules remain enforced. The
environment fingerprint was advanced to invalidate results produced by the old reporter.

## Reproduce

```bash
cargo build --locked --release
uv venv --python 3.12 .venv-benchmark
uv pip install --python .venv-benchmark/bin/python -r benchmarks/requirements.txt
.venv-benchmark/bin/python benchmarks/benchmark.py \
  --root ../aks-store-demo \
  --tctx target/release/tctx \
  --pytest .venv-benchmark/bin/pytest \
  --output /tmp/tctx-benchmark-run \
  --sessions 8 --rounds 8 --seed 20261023
```
