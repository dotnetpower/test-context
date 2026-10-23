# Test Context (`tctx`) — Architecture & Design

## Purpose and Scope

Eliminate the `pytest` execution and collection bottleneck for Python projects and AI coding agents (such as GitHub Copilot) by combining:

1. **Fast Rust Python AST & Dependency Graph Analysis**: Parses Python `import` / `from ... import ...` statements (including relative imports, parenthesized multi-line imports, and in-function imports), `conftest.py` directory hierarchies, and `pyproject.toml`/`pytest.ini` configurations.
2. **Transitive Reverse Dependency Graph & Merkle Dependency Digests**: Computes a deterministic SHA-256 Merkle dependency hash (`dep_hash`) for every test file from its own source hash, all transitively imported workspace Python modules, applicable `conftest.py` files, and pytest configuration files.
3. **Speculative Background Test Execution**: Watches workspace files via `notify`. When Python source or test files change, affected tests are marked stale and queued for background pytest execution while the developer or AI agent is still editing or reasoning.
4. **Zero-Startup Instant Cache Return & Singleflight Delta Execution**:
   - When GitHub Copilot (via Native VS Code Language Model Tools, MCP, or `tctx run`/`tctx pytest`) requests test execution:
     - **100% Cache Hit**: If all targeted test files already have a `Passed` record matching their live `dep_hash`, `tctx` returns the cached pass result immediately in **< 5ms** without spawning Python or `pytest`.
     - **Partial Hit / Delta Run**: Already-passed test files with matching `dep_hash` are skipped, in-flight background runs for the same `dep_hash` are joined (singleflight), and only uncached/changed/failed test files are executed in a single delta `pytest` invocation.

## Surfaces for GitHub Copilot

1. **Native VS Code Extension (`moonchoi.test-context-native`)**:
   - Contributes 7 Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeWorkspaces`, `#tctxNativeInvalidate`) and automatic `chatInstructions`.
   - Uses a persistent framed-JSON (`TCTXJ1\n`) Unix-socket connection (or WSL `native-bridge` on Windows hosts) to the shared `tctx daemon`.
2. **Shared MCP Server (`tctx shared-mcp` / `tctx mcp`)**:
   - Exposes `test_run`, `test_affected`, `test_graph`, `test_status`, `test_sync`, `test_workspaces`, and `test_invalidate` over JSON-RPC 2.0 stdio, backed by the same Unix-socket daemon (`TCTXB1\n` binary protocol).
3. **CLI & Drop-in Pytest Wrapper (`tctx run`, `tctx pytest`, `tctx affected`, `tctx graph`, `tctx status`, `tctx sync`)**:
   - `tctx pytest [ARGS...]` intercepts standard `pytest` invocations, resolves target files or directories, checks live Merkle dependency hashes, skips cached-passing files, runs only the uncached delta, and prints a standard pytest-compatible summary with the exact exit code (`0` when all pass, non-zero on failure).

## Runtime & Security Boundaries

- State directory (`TCTX_HOME` or `~/.local/state/test-context`) is enforced at mode `0700` owned by the current UID.
- Unix domain socket (`daemon.sock`) and process lock (`daemon.lock`) are enforced at mode `0600`.
- Live file SHA-256 verification before any cache hit guarantees that stale or modified files on disk never produce a false cached pass.
