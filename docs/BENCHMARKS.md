# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

Measured with the `v0.1.2` release build. Exact source-tree and binary hashes are
recorded in [results.json](../benchmarks/results/aks-store-demo/results.json).

- **Warm suite re-execution (`warm_suite`)**: **1036.64 ms without TCTX** vs **0.61 ms with TCTX** (**1,693.30× speedup**, 95% CI **1,483.04–1,866.03×**; median **977.66 ms → 0.55 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests and Ruff rules while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.57 ms** (median **0.53 ms**) vs **1004.61 ms** for direct `pytest` (**1,763.31× speedup**, 95% CI **1,594.86–1,934.22×**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.85 ms** (median **0.54 ms**) vs **1009.18 ms** (**1,183.76× speedup**, 95% CI **764.93–1,824.79×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **82.83 ms** (median **76.75 ms**) vs **1031.66 ms** (**12.46× speedup**, 95% CI **11.93–13.03×**).
- **Impact analysis + Ruff pre-check (`affected_graph`)**: Querying affected tests and cached Ruff diagnostics via the in-memory Rust AST reverse dependency graph takes **0.52 ms** (median **0.49 ms**) vs **968.01 ms** for `pytest --collect-only` (**1,850.15× speedup**, 95% CI **1,798.47–1,915.56×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **1010.02 ms off → 17.08 ms on** (**59.15× mean speedup**, 95% CI **56.74–61.75×**, and **1,746.34× median speedup** from **974.02 ms to 0.56 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 1036.64 ms | 0.61 ms | 977.66 ms | 0.55 ms | 1693.30× [1483.04, 1866.03] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 1004.61 ms | 0.57 ms | 974.90 ms | 0.53 ms | 1763.31× [1594.86, 1934.22] |
| Reverted edit (`merkle_revert`) | 64 | 1009.18 ms | 0.85 ms | 978.08 ms | 0.54 ms | 1183.76× [764.93, 1824.79] |
| Immediate leaf delta (`leaf_delta`) | 64 | 1031.66 ms | 82.83 ms | 999.60 ms | 76.75 ms | 12.46× [11.93, 13.03] |
| Impact query (`affected_graph`) | 64 | 968.01 ms | 0.52 ms | 936.47 ms | 0.49 ms | 1850.15× [1798.47, 1915.56] |
| Fixed mixed suite (`all`) | 320 | 1010.02 ms | 17.08 ms | 974.02 ms | 0.56 ms | 59.15× [56.74, 61.75] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **829.92 ms** vs **1089.31 ms** for direct `pytest` (**1.31× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
