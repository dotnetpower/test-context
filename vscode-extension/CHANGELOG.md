# Changelog

## 0.1.0

- Initial release of Test Context Native (`moonchoi.test-context-native`) for VS Code and GitHub Copilot.
- Added 7 native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`).
- Persistent framed-JSON Unix socket IPC (`TCTXJ1`) with the shared Rust `tctx daemon`.
- Native-first routing; MCP registration is an explicit fallback opt-in.
- Show the Test Context Native output channel once after first activation so it is discoverable in the Output picker.
- Install into the Windows FDAI profile from WSL and configure the direct `tctx native-bridge` transport for Agents windows.
- Reconnect the WSL native bridge after daemon idle expiry instead of repeating broken-pipe errors.
- Render status, workspace, operation, and trace logs as concise text rather than JSON dumps.
- Treat Windows multi-root selection as a ready state and wait briefly for the Language Model Tool registry during startup.
- Return registered/unloaded workspace status without cold-loading a large graph; detailed status is opt-in with `load=true`.
