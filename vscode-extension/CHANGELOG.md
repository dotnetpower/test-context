# Changelog

## 0.1.0

- Initial release of Test Context Native (`moonchoi.test-context-native`) for VS Code and GitHub Copilot.
- Added 7 native Language Model Tools (`#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, `#tctxNativeStatus`, `#tctxNativeSync`, `#tctxNativeInvalidate`, `#tctxNativeWorkspaces`).
- Persistent framed-JSON Unix socket IPC (`TCTXJ1`) with the shared Rust `tctx daemon`.
- Native-first routing; MCP registration is an explicit fallback opt-in.
- Show the Test Context Native output channel once after first activation so it is discoverable in the Output picker.
