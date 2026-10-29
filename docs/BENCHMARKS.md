# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **911.38 ms without TCTX** vs **0.47 ms with TCTX** (**1,948.53× speedup**, 95% CI **1,897.96–2,005.05×**; median **908.18 ms → 0.46 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.46 ms** vs **920.08 ms** for direct `pytest` (**1,983.64× speedup**, 95% CI **1,924.26–2,052.37×**; median **910.53 ms → 0.46 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.48 ms** vs **907.72 ms** (**1,873.61× speedup**, 95% CI **1,800.33–1,943.23×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **62.52 ms** vs **915.21 ms** (**14.64× speedup**, 95% CI **14.24–14.96×**; median **903.52 ms → 61.77 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.44 ms** vs **883.79 ms** for `pytest --collect-only` (**2,019.97× speedup**, 95% CI **1,962.72–2,092.94×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **907.64 ms off → 12.87 ms on** (**70.50× mean speedup**, 95% CI **69.04–71.75×**, and **1,905.35× median speedup** from **903.91 ms to 0.47 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 911.38 ms | 0.47 ms | 908.18 ms | 0.46 ms | 1948.53× [1897.96, 2005.05] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 920.08 ms | 0.46 ms | 910.53 ms | 0.46 ms | 1983.64× [1924.26, 2052.37] |
| Reverted edit (`merkle_revert`) | 64 | 907.72 ms | 0.48 ms | 907.10 ms | 0.48 ms | 1873.61× [1800.33, 1943.23] |
| Immediate leaf delta (`leaf_delta`) | 64 | 915.21 ms | 62.52 ms | 903.52 ms | 61.77 ms | 14.64× [14.24, 14.96] |
| Impact query (`affected_graph`) | 64 | 883.79 ms | 0.44 ms | 874.40 ms | 0.43 ms | 2019.97× [1962.72, 2092.94] |
| Fixed mixed suite (`all`) | 320 | 907.64 ms | 12.87 ms | 903.91 ms | 0.47 ms | 70.50× [69.04, 71.75] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **742.36 ms** vs **964.46 ms** for direct `pytest` (**1.30× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
