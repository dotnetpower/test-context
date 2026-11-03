# AGENTS.md — Test Context (`tctx`)

## Overview

`test-context` (`tctx`) is a shared local polyglot (**Python + Rust + Frontend TS/JS/JSX/TSX/Vue/Svelte/CSS/SCSS**) AST dependency graph, speculative background test runner (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`), content-addressed **Ruff / ESLint** pre-flight checker, and incremental SHA-256 Merkle (`dep_hash`) test cache for AI coding agents, paired with a native VS Code Language Model Tools extension (`dotnetpower.test-context-native`) and an MCP server (`tctx shared-mcp`).

- **Rust backend** ([src/](src)): Polyglot AST reverse dependency graph ([src/ast_graph.rs](src/ast_graph.rs)), background worker & incremental delta runner ([src/runner.rs](src/runner.rs)), Python Zygote fork server ([src/zygote.rs](src/zygote.rs)), WAL-backed Merkle result cache ([src/cache.rs](src/cache.rs)), and daemon/IPC/MCP transports ([src/daemon.rs](src/daemon.rs), [src/shared_mcp.rs](src/shared_mcp.rs)).
- **VS Code extension** ([vscode-extension/](vscode-extension)): Contributes seven native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`) and automatic agent instructions ([vscode-extension/prompts/test-context.instructions.md](vscode-extension/prompts/test-context.instructions.md) with `applyTo: "**"`).

## Recommended Tool Usage for Agents

1. **Native-first priority in VS Code**: In foreground VS Code sessions, ALWAYS prefer `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` (`test-context-native_*`) FIRST before MCP (`test_*`) or shell `pytest` / `cargo test` / `vitest` / `jest` / `node --test` / `ruff` / `eslint`. Pass `root` explicitly when the project or worktree path is known.
2. **Incremental test & lint execution**: Use `#tctxNativeRun` (or MCP `test-context-test_run` / `test_run`) with modified source files or test targets in `targets`. Unchanged test files that already passed against their current `dep_hash` return in `< 5ms` (`mode=instant_cache_hit`), and only uncached/modified test files execute (`mode=hybrid_delta`). Pass `ruff_fix: true` to auto-remediate safe Ruff/ESLint issues before running tests.
3. **Affected test & lint discovery**: Use `#tctxNativeAffected` (or MCP `test-context-test_affected` / `test_affected`) to inspect which test files are impacted by modified source files or `git diff`, their background cache states, and pre-checked Ruff/ESLint diagnostics.
4. **AST dependency graph inspection**: Use `#tctxNativeGraph` (or MCP `test-context-test_graph` / `test_graph`) to inspect direct imports, reverse importers, transitive dependencies, `conftest.py` / `tests/common` / `setupTests.ts` chains, test symbols, and `dep_hash`.
5. **Subagents & deferred tool discovery**: When delegating to subagents (`task`, `general-purpose`, `explore`, `code-review`, `rubber-duck`, `security-review`), pass the explicit absolute `root` and instruct the subagent to use `#tctxNative*` or `test-context-test_*` first. When discovering deferred tools via `tool_search`, include exact names for both Native and MCP surfaces (`"tctxNativeRun tctxNativeAffected tctxNativeGraph tctxNativeStatus tctxNativeSync tctxNativeInvalidate tctxNativeWorkspaces test-context-test_run test-context-test_affected test-context-test_graph test-context-test_status test-context-test_sync test-context-test_invalidate test-context-test_workspaces"`) so discovery succeeds even when no UI client is connected.
6. **Headless / disconnected fallback**: If a native `#tctxNative*` call fails with `"No matching client connected"` or `"Client ... disconnected"`, immediately switch to `test-context-test_*` MCP tools or `tctx --root /path/to/repo run` / `affected` / `graph` CLI instead of falling back to raw shell test runners.

## Build, Test, and Verification Commands

```sh
# Fast incremental verification via Test Context
tctx --root . run

# Full Rust backend unit & integration tests and clippy lint
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings

# Python configuration & benchmark tests
python3 -m unittest discover -s tests -p 'test_*.py'

# VS Code extension typecheck, lint, unit tests, and VSIX packaging
cd vscode-extension
npm run check
npm run package
cd ..
python3 scripts/verify_vsix.py vscode-extension/dist/test-context-native.vsix
```
