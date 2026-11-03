# Changelog

## 0.1.2

- Release watches and graph entries for deleted or moved-out directory subtrees, recover watch capacity, and rewatch recreated paths.
- Preserve nested watches across directory renames and clean up missing watches during overflow recovery, safety reconciliation, and sync.
- Surface watch registration/removal failures and watched-directory counts in workspace status.
- Normalize native and MCP argument aliases, Markdown links, line suffixes, file URIs, and WSL paths for reliable workspace and test-target resolution.
- Improve Node.js discovery for daemon environments and enable MCP fallback and subagent tool configuration during installation.
- Refresh the 8-session, 320-pair Python benchmark using the release binary and assert the exact fixture-aware affected-test set.

## 0.1.1

- Resolved multi-byte UTF-8 escape char-boundary panics across Python, Rust, and Frontend semantic normalizers and tokenizers.
- Added poison auto-recovery and `catch_unwind` isolation across `DaemonHub`, `ManagedRoot`, and background workers, plus per-root load gates and lock-free linter/git execution.
- Accelerated AST graph build and reverse-dependency queries via parallel multi-core indexing, compact `u32` forward/reverse adjacency lists, parent-pointer BFS, and `O(1)` cross-language bridge indexing.
- Optimized Rust incremental builds and test execution (`--no-run` multi-target precompilation, `--lib` module harness filtering, and non-barrel `lib.rs` decoupling).
- Enhanced VS Code Copilot compatibility with workspace-root path normalization, WSL/multi-root automatic root inference, singleflight workspace registry caching, and inline response bounding.

## 0.1.0

- First Marketplace release of **Test Context by dotnetpower** (`dotnetpower.test-context-native`) for VS Code and GitHub Copilot.
- Added 7 native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`).
- Dual-language Python (`pytest`) and Rust (`cargo test`) AST dependency graph, PyO3/Maturin cross-language native module tracking, and static environment variable / data file hashing.
- Speculative background Ruff pre-flight diagnostics (`ruff_diagnostics`) and automatic remediation via `ruff_fix: true`.
- Persistent framed-JSON Unix socket IPC (`TCTXJ1`) with the shared Rust `tctx daemon` and WSL native bridge for Windows Agents.
- Packaged walkthrough media, benchmark chart (`resources/benchmark.png`), and Marketplace metadata.
