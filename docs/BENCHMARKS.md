# Pytest Efficacy Benchmark: TCTX Off versus On (`aks-store-demo`)

## Result and Scope

Across **8 independent daemon sessions** (320 paired observations) targeting the Python FastAPI service (`src/ai-service`) of [`aks-store-demo`](https://github.com/dotnetpower/aks-store-demo):

- **Warm suite re-execution (`warm_suite`)**: **1,035.19 ms without TCTX** vs **0.53 ms with TCTX** (**1,958.57× speedup**, 95% CI **1,877.34–2,036.13×**; median **1,020.90 ms → 0.50 ms**).
- **Post-edit speculative background run (`bg_speculative`)**: When a router (`src/ai-service/routers/description_generator.py`) is edited and the background worker verifies affected tests while the developer or AI coding agent is reasoning, the subsequent `test_run` call takes **0.54 ms** vs **1,043.13 ms** for direct `pytest` (**1,923.44× speedup**, 95% CI **1,824.29–2,014.54×**; median **1,010.97 ms → 0.51 ms**).
- **Reverted edit Merkle cache hit (`merkle_revert`)**: Reverting a source edit back to a previously verified state hits the content-addressed SHA-256 Merkle `dep_hash` history in **0.57 ms** vs **1,075.63 ms** (**1,876.19× speedup**, 95% CI **1,604.49–2,112.41×**).
- **Immediate 0ms-delay leaf edit (`leaf_delta`)**: Calling `test_run` immediately (0 ms delay) after editing an isolated leaf helper skips 3 cached test files and runs only the 1 affected test file via the **Warm Pytest Fork Zygote (`os.fork()` with pre-imported 3rd-party SDKs and ephemeral-child GC elision)** in **86.04 ms** vs **1,079.16 ms** (**12.54× speedup**, 95% CI **12.25–12.81×**; median **1,023.34 ms → 81.36 ms**).
- **Impact analysis (`affected_graph`)**: Querying affected tests via the in-memory Rust AST reverse dependency graph takes **0.50 ms** vs **989.91 ms** for `pytest --collect-only` (**1,968.25× speedup**, 95% CI **1,881.70–2,039.39×**).
- **Fixed mixed suite (`all`, 320 pairs)**: **1,044.60 ms off → 17.64 ms on** (**59.23× mean speedup**, 95% CI **57.96–60.47×**, and **1,902.7× median speedup** from **1,022.45 ms to 0.54 ms**).

![Test execution and query latency and paired-session speedup](../benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | Pairs | TCTX off mean | TCTX on mean | Off median | On median | Speedup [95% CI] |
|---|---:|---:|---:|---:|---:|---:|
| Warm suite (`warm_suite`) | 64 | 1035.19 ms | 0.53 ms | 1020.90 ms | 0.50 ms | 1958.57× [1877.34, 2036.13] |
| Post-edit bg pre-run (`bg_speculative`) | 64 | 1043.13 ms | 0.54 ms | 1010.97 ms | 0.51 ms | 1923.44× [1824.29, 2014.54] |
| Reverted edit (`merkle_revert`) | 64 | 1075.63 ms | 0.57 ms | 1069.44 ms | 0.51 ms | 1876.19× [1604.49, 2112.41] |
| Immediate leaf delta (`leaf_delta`) | 64 | 1079.16 ms | 86.04 ms | 1023.34 ms | 81.36 ms | 12.54× [12.25, 12.81] |
| Impact query (`affected_graph`) | 64 | 989.91 ms | 0.50 ms | 991.18 ms | 0.47 ms | 1968.25× [1881.70, 2039.39] |
| Fixed mixed suite (`all`) | 320 | 1044.60 ms | 17.64 ms | 1022.45 ms | 0.54 ms | 59.23× [57.96, 60.47] |

## Cold Start Behavior

![Cold initial suite run](../benchmarks/results/aks-store-demo/cold-start.png)

On a cold cache (first run in a brand-new daemon session with no cached `dep_hash` entries), TCTX parses the workspace Python AST graph and executes all 4 test files through the pre-warmed `PytestZygote` fork server, averaging **687.4 ms** vs **1,142.8 ms** for a cold `pytest` subprocess (**1.66× faster** even on the initial uncached run).

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
