# Test Context Native for VS Code

Native VS Code Language Model Tools for **Test Context (`tctx`)** — incremental Python AST dependency analysis, speculative background `pytest` execution, and zero-startup cached test results for GitHub Copilot.

These Native tools are the default Test Context surface in VS Code. Use the optional MCP server only when Native extension dispatch is unavailable, such as an unattended background agent session.

For a Windows Agents window backed by WSL, install the VSIX in both the Windows FDAI profile and
the WSL extension host. The installer performs both installations and configures the Windows
extension to use `wsl.exe --exec tctx native-bridge`; it does not route Native tools through MCP or
a network port.

## Contributed Language Model Tools

- `#tctxNativeRun` (`test-context-native_testRun`): Run `pytest` incrementally with instant cache hits for tests already verified in the background.
- `#tctxNativeAffected` (`test-context-native_testAffected`): Inspect which test files are affected by modified Python source files or `git diff`, plus their live background cache state.
- `#tctxNativeGraph` (`test-context-native_testGraph`): Query direct imports, reverse importers, transitive dependencies, `conftest.py` chains, and Merkle `dep_hash` for any Python file.
- `#tctxNativeStatus` (`test-context-native_testStatus`): View daemon health, cached test counts, background worker queue, and total time saved.
- `#tctxNativeSync` (`test-context-native_testSync`): Force AST graph synchronization and schedule background test execution.
- `#tctxNativeInvalidate` (`test-context-native_testInvalidate`): Invalidate cached test results for specific files or the entire workspace.
- `#tctxNativeWorkspaces` (`test-context-native_testWorkspaces`): List registered `tctx` workspaces.
