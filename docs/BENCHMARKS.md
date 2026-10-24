# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **723.97 ms without TCTX** vs **0.43 ms with TCTX** (**1,677.00× speedup**, 95% CI **1,591.30–1,759.58×**; median **704.13 ms → 0.42 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.43 ms** vs **728.55 ms** for direct `pytest` (**1,712.74× speedup**, 95% CI **1,630.81–1,802.29×**; median **693.88 ms → 0.42 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.43 ms** vs **725.51 ms** (**1,702.89× speedup**, 95% CI **1,618.66–1,797.86×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **54.88 ms** vs **738.12 ms** (**13.45× speedup**, 95% CI **13.03–13.93×**; median **722.51 ms → 53.13 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.39 ms** vs **689.88 ms** for `pytest --collect-only` (**1,777.89× speedup**, 95% CI **1,699.82–1,873.27×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **721.20 ms off → 11.31 ms on** (**63.76× mean speedup**, 95% CI **61.85–65.82×**, and **1,650.91× median speedup** from **701.94 ms to 0.43 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 723.97 ms | 0.43 ms | 704.13 ms | 0.42 ms | 1677.00× [1591.30, 1759.58] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 728.55 ms | 0.43 ms | 693.88 ms | 0.42 ms | 1712.74× [1630.81, 1802.29] |
| Reverted edit (`merkle_revert`) | 64 | 725.51 ms | 0.43 ms | 703.16 ms | 0.42 ms | 1702.89× [1618.66, 1797.86] |
| Immediate leaf delta (`leaf_delta`) | 64 | 738.12 ms | 54.88 ms | 722.51 ms | 53.13 ms | 13.45× [13.03, 13.93] |
| Impact query (`affected_graph`) | 64 | 689.88 ms | 0.39 ms | 671.98 ms | 0.38 ms | 1777.89× [1699.82, 1873.27] |
| Fixed mixed suite (`all`) | 320 | 721.20 ms | 11.31 ms | 701.94 ms | 0.43 ms | 63.76× [61.85, 65.82] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **669.79 ms** vs **922.09 ms** for direct `pytest` (**1.38× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
