# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **738.01 ms without TCTX** vs **0.50 ms with TCTX** (**1,489.42× speedup**, 95% CI **1,190.31–1,755.25×**; median **731.43 ms → 0.41 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.43 ms** vs **741.39 ms** for direct `pytest` (**1,741.14× speedup**, 95% CI **1,683.01–1,795.17×**; median **717.15 ms → 0.40 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.42 ms** vs **742.74 ms** (**1,750.57× speedup**, 95% CI **1,686.97–1,814.47×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with safe preloads, explicit plugin manifest, bounded output pipes, process-group cancellation, dynamic data-dependency capture, and source-stability verification)** in **56.59 ms** vs **735.33 ms** (**12.99× speedup**, 95% CI **12.26–13.57×**; median **716.60 ms → 53.33 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.40 ms** vs **714.43 ms** for `pytest --collect-only` (**1,802.81× speedup**, 95% CI **1,678.57–1,921.45×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **734.38 ms off → 11.67 ms on** (**62.95× mean speedup**, 95% CI **59.18–66.23×**, and **1,705.02× median speedup** from **711.63 ms to 0.42 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 738.01 ms | 0.50 ms | 731.43 ms | 0.41 ms | 1489.42× [1190.31, 1755.25] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 741.39 ms | 0.43 ms | 717.15 ms | 0.40 ms | 1741.14× [1683.01, 1795.17] |
| Reverted edit (`merkle_revert`) | 64 | 742.74 ms | 0.42 ms | 731.44 ms | 0.40 ms | 1750.57× [1686.97, 1814.47] |
| Immediate leaf delta (`leaf_delta`) | 64 | 735.33 ms | 56.59 ms | 716.60 ms | 53.33 ms | 12.99× [12.26, 13.57] |
| Impact query (`affected_graph`) | 64 | 714.43 ms | 0.40 ms | 696.34 ms | 0.38 ms | 1802.81× [1678.57, 1921.45] |
| Fixed mixed suite (`all`) | 320 | 734.38 ms | 11.67 ms | 711.63 ms | 0.42 ms | 62.95× [59.18, 66.23] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX performs Python/pytest/plugin environment fingerprinting, builds the AST graph, captures initial runtime data dependencies, and executes all 4 test files. In this run, the full safety setup averaged **662.06 ms** vs **961.74 ms** for direct `pytest` (**1.45× faster**). Cold-start results remain environment-sensitive; the sustained claim is the content-addressed reuse demonstrated by the paired warm and edit scenarios.

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
