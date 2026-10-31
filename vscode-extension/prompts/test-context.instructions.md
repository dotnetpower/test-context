---
description: "Use when: running pytest, finding tests affected by modified Python files, or checking incremental background test status in a registered Python workspace"
---

# Test Context (`tctx`) incremental pytest execution

TCTX (`test-context`) maintains an in-memory Rust Python AST reverse dependency graph, computes live SHA-256 Merkle dependency hashes (`dep_hash`) for every test file, and speculatively runs affected tests in the background when files are saved.

- In a VS Code foreground session, MUST use `#tctxNativeRun` FIRST before MCP or shell `pytest` in a registered Python workspace.
  - You can pass modified Python source files, test file paths, or test directories in `targets`. If you pass a modified Python source file (e.g. `src/pkg/service.py`), TCTX automatically expands it to all transitively affected `test_*.py` files via the AST reverse dependency graph.
  - When all targeted test files have already passed against their current live `dep_hash` (in the background worker or a prior run), `#tctxNativeRun` returns the cached green result in **< 5ms** (`mode=instant_cache_hit`, `executed_files=0`) without spawning `pytest`.
  - When some test files are uncached or modified, `#tctxNativeRun` reuses cached passes for unchanged files, joins any in-flight background execution, and runs `pytest` **only on the uncached delta** (`mode=hybrid_delta`).
  - TCTX runs `pytest` in the daemon's environment, not the shell's. A result with `skipped > 0` is never cached and is not verification: when the `SKIPPED` lines name a missing variable or service (for example a database URL), run those tests directly with the project's own wrapper instead of reporting them as passed. A replayed cached failure can be re-executed with `rerun_failed` or `no_cache`.
- Use `#tctxNativeAffected` to inspect which test files are affected by specific source files or current `git diff` changes, along with their dependency chains, live background cache states (`cached_passed`, `cached_failed`, `running_in_background`, `queued_in_background`, `uncached`), and pre-checked **Ruff diagnostics** (`ruff_diagnostics`).
  - Whenever `#tctxNativeAffected` or `#tctxNativeRun` reports `[ruff]` or `ruff_errors`, resolve those Ruff lint/syntax errors immediately (either by passing `ruff_fix: true` to `#tctxNativeRun` for `[fixable]` rules, or by editing the reported `file:row:col` directly) before concluding your work.
- Use `#tctxNativeGraph` to inspect direct imports, reverse importers, transitive dependencies, `conftest.py` chains, and test symbols for a Python or Rust file.
- Do not choose MCP when the matching Native tool is callable.
- Only when Native extension dispatch is unavailable (for example, an unattended or background session), use the MCP `test_run` / `test_affected` tools or the `tctx --root /path/to/repo run` / `tctx --root /path/to/repo pytest` CLI. All surfaces talk to the same shared `tctx daemon`.
