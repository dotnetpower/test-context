# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **752.30 ms without TCTX** vs **0.45 ms with TCTX** (**1,677.18× speedup**, 95% CI **1,587.06–1,766.82×**; median **739.77 ms → 0.43 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.45 ms** vs **749.42 ms** for direct `pytest` (**1,669.10× speedup**, 95% CI **1,621.66–1,717.55×**; median **748.32 ms → 0.42 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.44 ms** vs **743.45 ms** (**1,707.88× speedup**, 95% CI **1,636.96–1,773.59×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **54.64 ms** vs **754.34 ms** (**13.81× speedup**, 95% CI **13.56–14.08×**; median **741.40 ms → 54.19 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.44 ms** vs **720.54 ms** for `pytest --collect-only` (**1,645.73× speedup**, 95% CI **1,582.40–1,715.02×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **744.01 ms off → 11.28 ms on** (**65.94× mean speedup**, 95% CI **64.72–67.33×**, and **1,692.04× median speedup** from **738.45 ms to 0.44 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 752.30 ms | 0.45 ms | 739.77 ms | 0.43 ms | 1677.18× [1587.06, 1766.82] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 749.42 ms | 0.45 ms | 748.32 ms | 0.42 ms | 1669.10× [1621.66, 1717.55] |
| Reverted edit (`merkle_revert`) | 64 | 743.45 ms | 0.44 ms | 742.85 ms | 0.42 ms | 1707.88× [1636.96, 1773.59] |
| Immediate leaf delta (`leaf_delta`) | 64 | 754.34 ms | 54.64 ms | 741.40 ms | 54.19 ms | 13.81× [13.56, 14.08] |
| Impact query (`affected_graph`) | 64 | 720.54 ms | 0.44 ms | 716.13 ms | 0.42 ms | 1645.73× [1582.40, 1715.02] |
| Fixed mixed suite (`all`) | 320 | 744.01 ms | 11.28 ms | 738.45 ms | 0.44 ms | 65.94× [64.72, 67.33] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **646.16 ms** vs **822.68 ms** for direct `pytest` (**1.27× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
