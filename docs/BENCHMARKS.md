# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **991.64 ms without TCTX** vs **0.49 ms with TCTX** (**2,027.40× speedup**, 95% CI **1,890.99–2,188.77×**; median **963.53 ms → 0.47 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.47 ms** vs **988.90 ms** for direct `pytest` (**2,104.72× speedup**, 95% CI **2,034.71–2,178.60×**; median **995.99 ms → 0.46 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.46 ms** vs **988.69 ms** (**2,152.02× speedup**, 95% CI **2,052.92–2,249.68×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **134.31 ms** vs **991.53 ms** (**7.38× speedup**, 95% CI **6.30–8.49×**; median **977.24 ms → 120.97 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.49 ms** vs **952.14 ms** for `pytest --collect-only` (**1,962.92× speedup**, 95% CI **1,882.12–2,049.91×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **982.58 ms off → 27.24 ms on** (**36.07× mean speedup**, 95% CI **30.66–41.50×**, and **2,004.5× median speedup** from **965.13 ms to 0.48 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 991.64 ms | 0.49 ms | 963.53 ms | 0.47 ms | 2027.40× [1890.99, 2188.77] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 988.90 ms | 0.47 ms | 995.99 ms | 0.46 ms | 2104.72× [2034.71, 2178.60] |
| Reverted edit (`merkle_revert`) | 64 | 988.69 ms | 0.46 ms | 974.78 ms | 0.45 ms | 2152.02× [2052.92, 2249.68] |
| Immediate leaf delta (`leaf_delta`) | 64 | 991.53 ms | 134.31 ms | 977.24 ms | 120.97 ms | 7.38× [6.30, 8.49] |
| Impact query (`affected_graph`) | 64 | 952.14 ms | 0.49 ms | 940.30 ms | 0.47 ms | 1962.92× [1882.12, 2049.91] |
| Fixed mixed suite (`all`) | 320 | 982.58 ms | 27.24 ms | 965.13 ms | 0.48 ms | 36.07× [30.66, 41.50] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX parses the workspace Python AST graph and executes all 4 test files through the pre-warmed `PytestZygote` fork server, averaging **897.16 ms** vs **1,044.90 ms** for a cold `pytest` subprocess (**1.16× faster** even on the initial uncached run).

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
