# Changelog

## 0.1.0

- First Marketplace release of **Test Context by dotnetpower** (`dotnetpower.test-context-native`) for VS Code and GitHub Copilot.
- Added 7 native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`).
- Dual-language Python (`pytest`) and Rust (`cargo test`) AST dependency graph, PyO3/Maturin cross-language native module tracking, and static environment variable / data file hashing.
- Speculative background Ruff pre-flight diagnostics (`ruff_diagnostics`) and automatic remediation via `ruff_fix: true`.
- Persistent framed-JSON Unix socket IPC (`TCTXJ1`) with the shared Rust `tctx daemon` and WSL native bridge for Windows Agents.
- Packaged walkthrough media, benchmark chart (`resources/benchmark.png`), and Marketplace metadata.

