# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **952.22 ms without TCTX** vs **0.50 ms with TCTX** (**1,896.87× speedup**, 95% CI **1,815.99–1,969.40×**; median **913.72 ms → 0.48 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.54 ms** vs **961.24 ms** for direct `pytest` (**1,795.59× speedup**, 95% CI **1,602.31–1,993.67×**; median **910.60 ms → 0.47 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.54 ms** vs **937.14 ms** (**1,739.34× speedup**, 95% CI **1,488.15–1,946.73×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **80.04 ms** vs **936.17 ms** (**11.70× speedup**, 95% CI **9.04–14.61×**; median **903.96 ms → 63.73 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.47 ms** vs **896.10 ms** for `pytest --collect-only` (**1,904.19× speedup**, 95% CI **1,806.70–2,002.64×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **936.57 ms off → 16.42 ms on** (**57.05× mean speedup**, 95% CI **44.90–70.54×**, and **1,825.9× median speedup** from **903.54 ms to 0.49 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 952.22 ms | 0.50 ms | 913.72 ms | 0.48 ms | 1896.87× [1815.99, 1969.40] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 961.24 ms | 0.54 ms | 910.60 ms | 0.47 ms | 1795.59× [1602.31, 1993.67] |
| Reverted edit (`merkle_revert`) | 64 | 937.14 ms | 0.54 ms | 918.43 ms | 0.48 ms | 1739.34× [1488.15, 1946.73] |
| Immediate leaf delta (`leaf_delta`) | 64 | 936.17 ms | 80.04 ms | 903.96 ms | 63.73 ms | 11.70× [9.04, 14.61] |
| Impact query (`affected_graph`) | 64 | 896.10 ms | 0.47 ms | 877.85 ms | 0.45 ms | 1904.19× [1806.70, 2002.64] |
| Fixed mixed suite (`all`) | 320 | 936.57 ms | 16.42 ms | 903.54 ms | 0.49 ms | 57.05× [44.90, 70.54] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. This full safety setup averaged **1,617.63 ms** vs **986.49 ms** for direct `pytest` (**1.64× slower on cold start**). No cold-cache speedup is claimed; the one-time cost buys stable environment identity and dynamic dependency evidence used by subsequent 0.5 ms cache decisions.

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
