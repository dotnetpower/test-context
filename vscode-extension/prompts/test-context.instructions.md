---
description: "Use when: running pytest, finding tests affected by modified Python files, or checking incremental background test status in a registered Python workspace"
---

# Test Context (`tctx`) incremental pytest execution

TCTX (`test-context`) maintains an in-memory Rust Python AST reverse dependency graph, computes live SHA-256 Merkle dependency hashes (`dep_hash`) for every test file, and speculatively runs affected tests in the background when files are saved.

- Use `#tctxNativeRun` (or MCP `test_run`) FIRST before running shell `pytest` in a registered Python workspace.
  - You can pass modified Python source files, test file paths, or test directories in `targets`. If you pass a modified Python source file (e.g. `src/pkg/service.py`), TCTX automatically expands it to all transitively affected `test_*.py` files via the AST reverse dependency graph.
  - When all targeted test files have already passed against their current live `dep_hash` (in the background worker or a prior run), `#tctxNativeRun` returns the cached green result in **< 5ms** (`mode=instant_cache_hit`, `executed_files=0`) without spawning `pytest`.
  - When some test files are uncached or modified, `#tctxNativeRun` reuses cached passes for unchanged files, joins any in-flight background execution, and runs `pytest` **only on the uncached delta** (`mode=hybrid_delta`).
- Use `#tctxNativeAffected` (or MCP `test_affected`) to inspect which test files are affected by specific source files or current `git diff` changes, along with their dependency chains and live background cache states (`cached_passed`, `cached_failed`, `running_in_background`, `queued_in_background`, `uncached`).
- Use `#tctxNativeGraph` (or MCP `test_graph`) to inspect direct imports, reverse importers, transitive dependencies, `conftest.py` chains, and test symbols for a Python file.
- If native extension tools are unavailable in a background session, use the MCP `test_run` / `test_affected` tools or the `tctx --root /path/to/repo run` / `tctx --root /path/to/repo pytest` CLI, which talk to the same shared `tctx daemon`.
