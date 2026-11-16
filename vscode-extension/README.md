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
| Warm suite (`warm_suite`) | 763.96 ms | **0.58 ms** (median 0.53 ms) | **1312.68×** [1130.23, 1460.62] |
| Post-edit background pre-run (`bg_speculative`) | 774.77 ms | **0.56 ms** (median 0.52 ms) | **1380.99×** [1198.13, 1528.39] |
| Reverted edit Merkle hit (`merkle_revert`) | 768.29 ms | **0.68 ms** (median 0.53 ms) | **1124.04×** [986.07, 1308.67] |
| Immediate 0ms-delay leaf edit (`leaf_delta`) | 765.98 ms | **69.54 ms** (median 66.66 ms) | **11.02×** [10.67, 11.35] |
| Impact query + Ruff pre-check (`affected_graph`) | 746.59 ms | **0.47 ms** (median 0.45 ms) | **1582.62×** [1506.33, 1655.94] |
| Fixed mixed suite (`all`, 320 pairs) | 763.92 ms | **14.37 ms** (median 0.54 ms) | **53.17×** [51.92, 54.34] |

![Test execution and query latency and paired-session speedup](resources/benchmark.png)

## Install

```sh
git clone https://github.com/dotnetpower/test-context.git && cd test-context
./scripts/install.sh
code --install-extension dotnetpower.test-context-native
```

For Windows Agents windows backed by WSL, run `./scripts/install-vscode-extension.sh` from WSL to configure the `wsl.exe --exec tctx native-bridge` transport automatically.

## Contributed Language Model Tools

- `#tctxNativeRun` (`test-context-native_testRun`): Run `pytest`, `cargo test`, `vitest`, `jest`, `node --test`, and `go test` incrementally with `< 1ms` instant cache hits for tests already verified against the current semantic Merkle `dep_hash`, plus pre-flight Ruff/ESLint/Clippy diagnostics and optional `ruff_fix: true` auto-remediation. Automatically normalizes absolute target paths and scopes large workspaces (`> 64` tests) to uncommitted `git diff` changes when `targets` is omitted.
- `#tctxNativeAffected` (`test-context-native_testAffected`): Inspect which Python, Rust, Frontend, or Go test files are transitively affected by modified source/stylesheet files or `git diff`, ranked by shortest dependency hop distance with live background cache state, untested modified file alerts, and pre-checked lint diagnostics.
- `#tctxNativeGraph` (`test-context-native_testGraph`): Query direct imports, reverse importers, transitive dependencies, `conftest.py` / `tests/common` / `setupTests.ts` chains, cross-language bridges (PyO3, Wasm, Full-Stack `/api/...` routes, Protobuf), referenced env vars, and semantic Merkle `dep_hash` for any indexed file.
- `#tctxNativeStatus` (`test-context-native_testStatus`): View daemon health, indexed Python/Rust/Frontend file counts, cached test counts, background worker queue, Warm Pytest Zygote status, and cumulative time saved.
- `#tctxNativeSync` (`test-context-native_testSync`): Synchronize the polyglot AST dependency graph with disk and schedule background test execution.
- `#tctxNativeInvalidate` (`test-context-native_testInvalidate`): Invalidate cached test results for specific files or the entire workspace.
- `#tctxNativeWorkspaces` (`test-context-native_testWorkspaces`): List registered `tctx` workspaces.

## Enterprise Scale & Copilot UX Highlights

- **Monorepo Speed (`9,000+` files)**: Parallel multi-core AST indexing loads a 9,042-file / 3,434-test repository in **`~670ms`**, and `u32` integer adjacency + parent-pointer BFS resolves reverse test impact in **`~5ms`**.
- **Semantic AST Hashing**: Comment-only and blank-line formatting edits preserve `semantic_sha256` without invalidating cached test passes, while directive comments (`noqa`, `type:`, `eslint`, `@ts-`, `go:embed`, Rust doctest fences) are preserved.
- **Zero-Config WSL & Multi-Root Resolution**: Automatically resolves workspace roots from target file hints, `vscode-remote://` URIs, and Windows WSL UNC paths (`\\wsl.localhost\...`), with transparent Unix socket auto-reconnect inside `tctx native-bridge`.
- **Context Token Compaction (92.7% Reduction)**: Compacts passing test boilerplate and bounds large dependency lists so Copilot responses stay inline (`< 8 KB`) without `/tmp` file offloading.
