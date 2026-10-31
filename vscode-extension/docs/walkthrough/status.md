# Instant cache hits & delta execution

- Use `#tctxNativeRun` before shell `pytest` or `cargo test`.
- Use `#tctxNativeAffected` to inspect affected Python/Rust test files, live background cache state, and pre-checked Ruff diagnostics.
- Pass `ruff_fix: true` to `#tctxNativeRun` to auto-remediate safe Ruff rules before running tests.
