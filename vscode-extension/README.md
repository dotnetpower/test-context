<p align="center">
  <img src="resources/banner.png" alt="Test Context — instant AST test cache for AI agents" width="100%">
</p>

# Test Context by dotnetpower

**Native VS Code Language Model Tools for Test Context (`tctx`) — incremental Python (`pytest` + `ruff`), Rust (`cargo test`), and Frontend (`React`, `Vite`, `TypeScript`, `CSS/SCSS`, `Vitest`/`Jest`/`Node test`, `ESLint`) AST dependency analysis, speculative background execution, and sub-millisecond cached test results for GitHub Copilot.**
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
| Warm suite (`warm_suite`) | 892.16 ms | **0.72 ms** (median 0.70 ms) | **1230.88×** [1179.49, 1286.36] |
| Post-edit background pre-run (`bg_speculative`) | 878.06 ms | **0.83 ms** (median 0.70 ms) | **1055.49×** [926.13, 1185.02] |
| Reverted edit Merkle hit (`merkle_revert`) | 911.60 ms | **0.75 ms** (median 0.73 ms) | **1221.47×** [1180.54, 1255.10] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 894.47 ms | **106.51 ms** (median 103.37 ms) | **8.40×** [8.20, 8.59] |
| Impact query + Ruff pre-check (`affected_graph`) | 871.40 ms | **0.58 ms** (median 0.56 ms) | **1492.77×** [1424.36, 1571.28] |
| Fixed mixed suite (`all`, 320 pairs) | 889.54 ms | **21.88 ms** (median 0.71 ms) | **40.65×** [39.50, 41.75] |

![Test execution and query latency and paired-session speedup](resources/benchmark.png)

## Install

```sh
git clone https://github.com/dotnetpower/test-context.git && cd test-context
./scripts/install.sh
code --install-extension dotnetpower.test-context-native
```

For Windows Agents windows backed by WSL, run `./scripts/install-vscode-extension.sh` from WSL to configure the `wsl.exe --exec tctx native-bridge` transport automatically.

## Contributed Language Model Tools

- `#tctxNativeRun` (`test-context-native_testRun`): Run `pytest`, `cargo test`, `vitest`, `jest`, and `node --test` incrementally with `< 1ms` instant cache hits for tests already verified against the current semantic Merkle `dep_hash`, plus pre-flight Ruff/ESLint/Biome/Clippy diagnostics and optional `ruff_fix: true` auto-remediation. Automatically normalizes absolute target paths and scopes large workspaces (`> 64` tests) to uncommitted `git diff` changes when `targets` is omitted.
- `#tctxNativeAffected` (`test-context-native_testAffected`): Inspect which Python, Rust, Frontend, C/C++, or C# test files are transitively affected by modified source/stylesheet files or `git diff`, ranked by shortest dependency hop distance with live background cache state, untested modified file alerts, and pre-checked lint diagnostics.
- `#tctxNativeGraph` (`test-context-native_testGraph`): Query direct imports, reverse importers, transitive dependencies, `conftest.py` / `tests/common` / `setupTests.ts` chains, cross-language bridges (PyO3, Wasm, Full-Stack `/api/...` routes, Protobuf), referenced env vars, and semantic Merkle `dep_hash` for any indexed file.
- `#tctxNativeStatus` (`test-context-native_testStatus`): View daemon health, indexed Python/Rust/Frontend file counts, cached test counts, background worker queue, Warm Pytest Zygote status, and cumulative time saved.
- `#tctxNativeSync` (`test-context-native_testSync`): Synchronize the polyglot AST dependency graph with disk and schedule background test execution.
- `#tctxNativeInvalidate` (`test-context-native_testInvalidate`): Invalidate cached test results for specific files or the entire workspace.
- `#tctxNativeWorkspaces` (`test-context-native_testWorkspaces`): List registered `tctx` workspaces.

## Enterprise Scale & Copilot UX Highlights

- **Monorepo Speed (`9,000+` files)**: Parallel multi-core AST indexing loads a 9,042-file / 3,434-test repository in **`~670ms`**, and `u32` integer adjacency + parent-pointer BFS resolves reverse test impact in **`~5ms`**.
- **Semantic AST Hashing**: Comment-only and blank-line formatting edits preserve `semantic_sha256` without invalidating cached test passes, while directive comments (`noqa`, `type:`, `eslint`, `@ts-`, Rust doctest fences) are preserved.
- **Zero-Config WSL & Multi-Root Resolution**: Automatically resolves workspace roots from target file hints, `vscode-remote://` URIs, and Windows WSL UNC paths (`\\wsl.localhost\...`), with transparent Unix socket auto-reconnect inside `tctx native-bridge`.
- **Context Token Compaction (92.7% Reduction)**: Compacts passing test boilerplate and bounds large dependency lists so Copilot responses stay inline (`< 8 KB`) without `/tmp` file offloading.
