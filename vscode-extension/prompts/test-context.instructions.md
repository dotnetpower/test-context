---
description: "Use when running tests/linters (pytest, cargo test, vitest, jest, node --test, ruff, eslint) or checking affected tests in a registered workspace"
applyTo: "**/*.{py,rs,ts,tsx,mts,cts,js,jsx,mjs,cjs,vue,svelte,css,scss,sass,less,proto,graphql,gql,c,cc,cpp,h,hpp,cs,toml,json,yaml,yml,ini,cfg,sh,mk}"
---

# Test Context (`tctx`) — Incremental Test & Lint Routing

- In foreground VS Code Editor Chat, MUST use `#tctxNativeRun` (`#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`) FIRST before MCP or shell runners. Do not choose MCP when the matching Native tool is callable.
- In AgentHost / Copilot SDK / background / subagent sessions, prefer backend MCP (`test-context-test_run`, `test-context-test_affected`, `test-context-test_graph`, `test-context-test_status`) FIRST so Reload Window or disconnected UI clients never stall execution.
- Always pass explicit absolute `root` when known. Pass modified files, test paths, nodeids (`path::test`), or `["."]` for full suite (omitting `targets` without `k`/`mark` in >64-test workspaces scopes to git-dirty/recent-commit affected tests and falls back to all tests if none). Unchanged cached passes return in `<5ms`; only uncached deltas run.
- Results with `skipped > 0` are never cached or verified: if skips cite missing env vars/services, run those tests via the project's own wrapper. Resolve reported `[ruff]` / lint errors (or pass `ruff_fix: true` for `[fixable]` rules).
- When delegating to subagents, pass explicit absolute `root` and instruct them to use `test-context-test_*` MCP (or `#tctxNative*`).
- If tools are deferred, discover them in small pairs via `tool_search` (e.g. `"test-context-test_run test-context-test_affected"` + `"test-context-test_graph test-context-test_status"`, or `"tctxNativeRun tctxNativeAffected"` + `"tctxNativeGraph tctxNativeStatus"`).
- On `"No matching client connected"` / `"Client ... disconnected"`, immediately switch to `test-context-test_*` MCP or `tctx --root <root> run|affected|graph` CLI.
