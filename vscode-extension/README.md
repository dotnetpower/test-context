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
| Warm suite (`warm_suite`) | 763.03 ms | **0.69 ms** (median 0.66 ms) | **1113.23×** [1062.72, 1168.37] |
| Post-edit background pre-run (`bg_speculative`) | 754.23 ms | **0.67 ms** (median 0.64 ms) | **1127.44×** [1080.98, 1171.86] |
| Reverted edit Merkle hit (`merkle_revert`) | 759.33 ms | **0.69 ms** (median 0.68 ms) | **1095.76×** [1071.13, 1121.72] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 764.76 ms | **102.21 ms** (median 100.50 ms) | **7.48×** [7.32, 7.65] |
| Impact query + Ruff pre-check (`affected_graph`) | 729.86 ms | **0.51 ms** (median 0.50 ms) | **1439.47×** [1392.80, 1486.37] |
| Fixed mixed suite (`all`, 320 pairs) | 754.24 ms | **20.95 ms** (median 0.67 ms) | **36.00×** [35.46, 36.52] |

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
