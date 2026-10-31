# Verify native language model tools

The native self-test verifies:

- all 7 stable Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`);
- framed-JSON IPC connectivity to the local `tctx daemon`;
- workspace registration without cold-loading inactive AST graphs.
