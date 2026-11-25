# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

Measured with the `v0.1.5` release build. Exact source-tree and binary hashes are
recorded in [results.json](../benchmarks/results/aks-store-demo/results.json).

- **Warm suite re-execution (`warm_suite`)**: **951.63 ms without TCTX** vs **0.74 ms with TCTX** (**1,290.06× speedup**, 95% CI **1,242.85–1,336.89×**; median **950.48 ms → 0.74 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests and Ruff rules while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.77 ms** (median **0.73 ms**) vs **945.82 ms** for direct `pytest` (**1,230.27× speedup**, 95% CI **1,208.76–1,253.39×**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.80 ms** (median **0.76 ms**) vs **951.10 ms** (**1,185.17× speedup**, 95% CI **1,123.35–1,245.98×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, full runtime `sys.addaudithook` dynamic dependency capture, and source-stability verification)** in **107.29 ms** (median **104.93 ms**) vs **958.55 ms** (**8.93× speedup**, 95% CI **8.78–9.07×**).
- **Impact analysis + Ruff pre-check (`affected_graph`)**: Querying affected tests and cached Ruff diagnostics via the in-memory Rust AST reverse dependency graph takes **0.58 ms** (median **0.57 ms**) vs **916.08 ms** for `pytest --collect-only` (**1,591.31× speedup**, 95% CI **1,544.12–1,643.99×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **944.64 ms off → 22.04 ms on** (**42.87× mean speedup**, 95% CI **42.22–43.48×**, and **1,255.06× median speedup** from **940.33 ms to 0.75 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 951.63 ms | 0.74 ms | 950.48 ms | 0.74 ms | 1290.06× [1242.85, 1336.89] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 945.82 ms | 0.77 ms | 943.81 ms | 0.73 ms | 1230.27× [1208.76, 1253.39] |
| Reverted edit (`merkle_revert`) | 64 | 951.10 ms | 0.80 ms | 941.54 ms | 0.76 ms | 1185.17× [1123.35, 1245.98] |
| Immediate leaf delta (`leaf_delta`) | 64 | 958.55 ms | 107.29 ms | 955.26 ms | 104.93 ms | 8.93× [8.78, 9.07] |
| Impact query (`affected_graph`) | 64 | 916.08 ms | 0.58 ms | 909.11 ms | 0.57 ms | 1591.31× [1544.12, 1643.99] |
| Fixed mixed suite (`all`) | 320 | 944.64 ms | 22.04 ms | 940.33 ms | 0.75 ms | 42.87× [42.22, 43.48] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, preloads the Warm Pytest Zygote parent, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **1,120.45 ms** (median **1,116.80 ms**) vs **1,008.38 ms** (median **1,015.86 ms**) for direct `pytest` (+112 ms one-time startup overhead, amortized on the very next call). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
