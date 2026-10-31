# Connect to the shared `tctx` daemon

## Linux & WSL workspace windows

Leave `testContextNative.socketPath` empty to use the default local runtime (`~/.local/state/test-context/daemon.sock`).

## Windows Agent Sessions

When running in a Windows Agents window backed by WSL, configure your WSL distribution and Linux `tctx` executable:

```json
{
  "testContextNative.wslDistribution": "Ubuntu-22.04",
  "testContextNative.wslExecutablePath": "/path/to/.local/bin/tctx"
}
```

Running `./scripts/install-vscode-extension.sh` from WSL configures both WSL and Windows profiles automatically.
