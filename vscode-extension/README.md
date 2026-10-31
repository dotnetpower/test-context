<p align="center">
  <img src="resources/banner.png" alt="Test Context — instant AST test cache for AI agents" width="100%">
</p>

# Test Context by dotnetpower

**Native VS Code Language Model Tools for Test Context (`tctx`) — incremental Python (`pytest` + `ruff`) and Rust (`cargo test`) AST dependency analysis, speculative background execution, and sub-millisecond cached test results for GitHub Copilot.**
Publisher: **dotnetpower** (`dotnetpower.test-context-native`).

## Resources

[Repository](https://github.com/dotnetpower/test-context) ·
[Benchmark report](https://github.com/dotnetpower/test-context/blob/main/docs/BENCHMARKS.md) ·
[Publishing guide](docs/PUBLISHING.md) ·
[Issues](https://github.com/dotnetpower/test-context/issues) · [License](LICENSE)

## Performance

Across 8 independent daemon sessions (**320 paired observations**) on [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo) (`src/ai-service`):

| Task | TCTX off (`pytest`) | TCTX on | Speedup [95% CI] |
|---|---:|---:|---:|
| Warm suite (`warm_suite`) | 1000.90 ms | **0.56 ms** (median 0.53 ms) | **1796.00×** [1710.88, 1867.12] |
| Post-edit background pre-run (`bg_speculative`) | 994.27 ms | **0.81 ms** (median 0.51 ms) | **1230.61×** [817.21, 1915.37] |
| Reverted edit Merkle hit (`merkle_revert`) | 988.26 ms | **0.60 ms** (median 0.55 ms) | **1643.59×** [1413.96, 1850.33] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 981.56 ms | **78.14 ms** (median 74.92 ms) | **12.56×** [12.00, 13.18] |
| Impact query + Ruff pre-check (`affected_graph`) | 960.91 ms | **0.47 ms** (median 0.45 ms) | **2049.69×** [1957.13, 2155.51] |
| Fixed mixed suite (`all`, 320 pairs) | 985.18 ms | **16.11 ms** (median 0.53 ms) | **61.13×** [57.39, 64.72] |

![Test execution and query latency and paired-session speedup](resources/benchmark.png)

## Install

```sh
git clone https://github.com/dotnetpower/test-context.git && cd test-context
./scripts/install.sh
code --install-extension dotnetpower.test-context-native
```

For Windows Agents windows backed by WSL, run `./scripts/install-vscode-extension.sh` from WSL to configure the `wsl.exe --exec tctx native-bridge` transport automatically.

## Contributed Language Model Tools

- `#tctxNativeRun` (`test-context-native_testRun`): Run `pytest` and `cargo test` incrementally with instant cache hits for tests already verified in the background, plus pre-flight Ruff diagnostics and optional `ruff_fix: true` auto-remediation.
- `#tctxNativeAffected` (`test-context-native_testAffected`): Inspect which Python/Rust test files are affected by modified source files or `git diff`, along with their live background cache state and pre-checked Ruff errors.
- `#tctxNativeGraph` (`test-context-native_testGraph`): Query direct imports, reverse importers, transitive dependencies, `conftest.py` / `tests/common` chains, referenced env vars, and Merkle `dep_hash` for any Python or Rust file.
- `#tctxNativeStatus` (`test-context-native_testStatus`): View daemon health, indexed Python/Rust counts, cached test counts, Ruff diagnostic counts, background worker queue, and total time saved.
- `#tctxNativeSync` (`test-context-native_testSync`): Force AST graph synchronization and schedule background test execution.
- `#tctxNativeInvalidate` (`test-context-native_testInvalidate`): Invalidate cached test results for specific files or the entire workspace.
- `#tctxNativeWorkspaces` (`test-context-native_testWorkspaces`): List registered `tctx` workspaces.

