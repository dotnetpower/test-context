# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **940.39 ms without TCTX** vs **12.43 ms with TCTX** (**75.63× speedup**, 95% CI **50.14–126.56×**; median **916.23 ms → 6.95 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **10.56 ms** vs **938.13 ms** for direct `pytest` (**88.85× speedup**, 95% CI **64.65–137.32×**; median **903.35 ms → 6.58 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **11.27 ms** vs **938.37 ms** (**83.27× speedup**, 95% CI **58.01–135.06×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file (**967.76 ms → 920.48 ms**, **1.05×**, 95% CI **1.03–1.07×**). Because Python interpreter + `fastapi`/`openai`/`azure.identity` import startup accounts for ~880 ms of a ~940 ms run on a 4-file microservice, immediate on-demand delta execution saves test-body execution time while still paying Python startup—demonstrating why **background speculative execution (`bg_speculative`, 88.85×)** is critical for AI agent workflows.
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.474 ms** vs **899.91 ms** for `pytest --collect-only` (**1,899.28× speedup**, 95% CI **1,814.61–1,978.40×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **936.91 ms off → 191.04 ms on** (**4.90× mean speedup**, 95% CI **4.78–5.02×**, and **132× median speedup** from **916.67 ms to 6.93 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 940.39 ms | 12.43 ms | 916.23 ms | 6.95 ms | 75.63× [50.14, 126.56] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 938.13 ms | 10.56 ms | 903.35 ms | 6.58 ms | 88.85× [64.65, 137.32] |
| Reverted edit (`merkle_revert`) | 64 | 938.37 ms | 11.27 ms | 926.71 ms | 7.14 ms | 83.27× [58.01, 135.06] |
| Immediate leaf delta (`leaf_delta`) | 64 | 967.76 ms | 920.48 ms | 934.83 ms | 906.78 ms | 1.05× [1.03, 1.07] |
| Impact query (`affected_graph`) | 64 | 899.91 ms | 0.47 ms | 886.20 ms | 0.44 ms | 1899.28× [1814.61, 1978.40] |
| Fixed mixed suite (`all`) | 320 | 936.91 ms | 191.04 ms | 916.67 ms | 6.93 ms | 4.90× [4.78, 5.02] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX parses the workspace Python AST graph, executes all 4 test files via `pytest` with `--junit-xml`, and persists the Merkle cache records. Cold execution averaged **1,033.2 ms off** vs **1,023.9 ms on** (parity; no cold-cache speedup is claimed).

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
