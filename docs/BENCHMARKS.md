# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **773.69 ms without TCTX** vs **0.45 ms with TCTX** (**1,720.96× speedup**, 95% CI **1,533.72–1,874.07×**; median **779.49 ms → 0.42 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.48 ms** vs **788.46 ms** for direct `pytest` (**1,633.09× speedup**, 95% CI **1,417.19–1,879.41×**; median **765.02 ms → 0.41 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.42 ms** vs **790.21 ms** (**1,862.04× speedup**, 95% CI **1,777.18–1,942.46×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **55.63 ms** vs **783.59 ms** (**14.09× speedup**, 95% CI **13.64–14.54×**; median **774.27 ms → 54.57 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.48 ms** vs **749.46 ms** for `pytest --collect-only` (**1,562.70× speedup**, 95% CI **1,235.58–1,845.61×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **777.08 ms off → 11.49 ms on** (**67.61× mean speedup**, 95% CI **65.79–69.35×**, and **1,761.3× median speedup** from **761.00 ms to 0.43 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 773.69 ms | 0.45 ms | 779.49 ms | 0.42 ms | 1720.96× [1533.72, 1874.07] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 788.46 ms | 0.48 ms | 765.02 ms | 0.41 ms | 1633.09× [1417.19, 1879.41] |
| Reverted edit (`merkle_revert`) | 64 | 790.21 ms | 0.42 ms | 763.95 ms | 0.41 ms | 1862.04× [1777.18, 1942.46] |
| Immediate leaf delta (`leaf_delta`) | 64 | 783.59 ms | 55.63 ms | 774.27 ms | 54.57 ms | 14.09× [13.64, 14.54] |
| Impact query (`affected_graph`) | 64 | 749.46 ms | 0.48 ms | 735.86 ms | 0.40 ms | 1562.70× [1235.58, 1845.61] |
| Fixed mixed suite (`all`) | 320 | 777.08 ms | 11.49 ms | 761.00 ms | 0.43 ms | 67.61× [65.79, 69.35] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. This full safety setup averaged **1,328.20 ms** vs **814.19 ms** for direct `pytest` (**1.63× slower on cold start**). No cold-cache speedup is claimed; the one-time cost buys stable environment identity and dynamic dependency evidence used by subsequent sub-millisecond cache decisions.

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
