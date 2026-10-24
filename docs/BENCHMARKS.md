# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **793.30 ms without TCTX** vs **0.47 ms with TCTX** (**1,684.73× speedup**, 95% CI **1,555.75–1,823.59×**; median **784.39 ms → 0.44 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.47 ms** vs **795.75 ms** for direct `pytest` (**1,705.35× speedup**, 95% CI **1,561.01–1,843.64×**; median **779.07 ms → 0.45 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.61 ms** vs **804.94 ms** (**1,309.13× speedup**, 95% CI **895.11–1,780.40×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **57.66 ms** vs **800.22 ms** (**13.88× speedup**, 95% CI **13.57–14.23×**; median **794.01 ms → 56.86 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.43 ms** vs **789.93 ms** for `pytest --collect-only` (**1,825.49× speedup**, 95% CI **1,725.93–1,928.52×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **796.83 ms off → 11.93 ms on** (**66.80× mean speedup**, 95% CI **64.78–68.79×**, and **1,715.62× median speedup** from **786.76 ms to 0.46 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 793.30 ms | 0.47 ms | 784.39 ms | 0.44 ms | 1684.73× [1555.75, 1823.59] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 795.75 ms | 0.47 ms | 779.07 ms | 0.45 ms | 1705.35× [1561.01, 1843.64] |
| Reverted edit (`merkle_revert`) | 64 | 804.94 ms | 0.61 ms | 800.71 ms | 0.45 ms | 1309.13× [895.11, 1780.40] |
| Immediate leaf delta (`leaf_delta`) | 64 | 800.22 ms | 57.66 ms | 794.01 ms | 56.86 ms | 13.88× [13.57, 14.23] |
| Impact query (`affected_graph`) | 64 | 789.93 ms | 0.43 ms | 782.67 ms | 0.40 ms | 1825.49× [1725.93, 1928.52] |
| Fixed mixed suite (`all`) | 320 | 796.83 ms | 11.93 ms | 786.76 ms | 0.46 ms | 66.80× [64.78, 68.79] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **679.41 ms** vs **862.93 ms** for direct `pytest` (**1.27× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
