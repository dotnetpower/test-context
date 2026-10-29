# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **867.57 ms without TCTX** vs **0.93 ms with TCTX** (**933.90× speedup**, 95% CI **634.80–1,551.61×**; median **863.67 ms → 0.46 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.54 ms** vs **872.84 ms** for direct `pytest` (**1,616.17× speedup**, 95% CI **1,275.73–1,963.22×**; median **871.29 ms → 0.46 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.47 ms** vs **875.33 ms** (**1,856.14× speedup**, 95% CI **1,782.80–1,931.41×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **66.83 ms** vs **862.82 ms** (**12.91× speedup**, 95% CI **12.47–13.39×**; median **854.94 ms → 64.42 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.53 ms** vs **842.05 ms** for `pytest --collect-only` (**1,585.86× speedup**, 95% CI **1,182.40–1,998.65×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **864.12 ms off → 13.86 ms on** (**62.35× mean speedup**, 95% CI **60.00–64.79×**, and **1,809.66× median speedup** from **858.42 ms to 0.47 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 867.57 ms | 0.93 ms | 863.67 ms | 0.46 ms | 933.90× [634.80, 1551.61] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 872.84 ms | 0.54 ms | 871.29 ms | 0.46 ms | 1616.17× [1275.73, 1963.22] |
| Reverted edit (`merkle_revert`) | 64 | 875.33 ms | 0.47 ms | 865.35 ms | 0.46 ms | 1856.14× [1782.80, 1931.41] |
| Immediate leaf delta (`leaf_delta`) | 64 | 862.82 ms | 66.83 ms | 854.94 ms | 64.42 ms | 12.91× [12.47, 13.39] |
| Impact query (`affected_graph`) | 64 | 842.05 ms | 0.53 ms | 836.27 ms | 0.42 ms | 1585.86× [1182.40, 1998.65] |
| Fixed mixed suite (`all`) | 320 | 864.12 ms | 13.86 ms | 858.42 ms | 0.47 ms | 62.35× [60.00, 64.79] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **768.84 ms** vs **1800.97 ms** for direct `pytest` (**2.34× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
