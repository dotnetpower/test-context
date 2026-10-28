# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **926.26 ms without TCTX** vs **0.45 ms with TCTX** (**2,035.98× speedup**, 95% CI **1,918.06–2,153.73×**; median **922.23 ms → 0.44 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.46 ms** vs **921.12 ms** for direct `pytest` (**1,996.27× speedup**, 95% CI **1,824.10–2,145.22×**; median **902.37 ms → 0.45 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.46 ms** vs **919.47 ms** (**1,992.66× speedup**, 95% CI **1,860.90–2,135.04×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **63.16 ms** vs **926.45 ms** (**14.67× speedup**, 95% CI **13.77–15.49×**; median **900.16 ms → 59.79 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.42 ms** vs **877.79 ms** for `pytest --collect-only` (**2,070.62× speedup**, 95% CI **1,969.21–2,176.17×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **914.22 ms off → 12.99 ms on** (**70.37× mean speedup**, 95% CI **66.32–74.21×**, and **1,973.00× median speedup** from **893.43 ms to 0.45 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 926.26 ms | 0.45 ms | 922.23 ms | 0.44 ms | 2035.98× [1918.06, 2153.73] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 921.12 ms | 0.46 ms | 902.37 ms | 0.45 ms | 1996.27× [1824.10, 2145.22] |
| Reverted edit (`merkle_revert`) | 64 | 919.47 ms | 0.46 ms | 884.53 ms | 0.43 ms | 1992.66× [1860.90, 2135.04] |
| Immediate leaf delta (`leaf_delta`) | 64 | 926.45 ms | 63.16 ms | 900.16 ms | 59.79 ms | 14.67× [13.77, 15.49] |
| Impact query (`affected_graph`) | 64 | 877.79 ms | 0.42 ms | 856.53 ms | 0.40 ms | 2070.62× [1969.21, 2176.17] |
| Fixed mixed suite (`all`) | 320 | 914.22 ms | 12.99 ms | 893.43 ms | 0.45 ms | 70.37× [66.32, 74.21] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **778.91 ms** vs **1041.03 ms** for direct `pytest` (**1.34× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
