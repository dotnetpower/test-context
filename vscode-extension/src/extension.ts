import { execFile } from 'node:child_process';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve } from 'node:path';
import { promisify } from 'node:util';

import * as vscode from 'vscode';

import { DaemonClient, RequestCancelledError } from './daemonClient.js';
import { NATIVE_TOOL_NAMES, NativeToolBridge } from './nativeTools.js';
import { formatToolResult, validateDaemonStatus } from './resultFormatter.js';
import {
  assertWslDistributionMatches,
  extractWslUncDistribution,
  RootResolutionError,
  toPosixPathIfPossible,
} from './rootResolver.js';
import type { ClientSettings } from './types.js';
import { UsageSummary } from './usage.js';

const execFileAsync = promisify(execFile);
const OUTPUT_DISCOVERED_STATE_KEY = 'testContextNative.outputDiscovered';
const PERIODIC_STATUS_REFRESH_MS = 30_000;
const FOCUS_STATUS_THROTTLE_MS = 5_000;
const TOOLTIP_TRUSTED_COMMANDS = [
  'testContextNative.showStatus',
  'testContextNative.syncWorkspace',
  'testContextNative.selfTest',
  'testContextNative.showSessionRoi',
];

let client: DaemonClient | undefined;

function applyStatusBarUsage(target: vscode.StatusBarItem, usage: UsageSummary): void {
  if (!usage.hasRenderableData) {
    return;
  }
  target.text = usage.text();
  if (typeof vscode.MarkdownString === 'function') {
    const md = new vscode.MarkdownString(usage.markdownTooltip(), true);
    md.isTrusted = { enabledCommands: TOOLTIP_TRUSTED_COMMANDS };
    target.tooltip = md;
  } else {
    target.tooltip = usage.tooltip();
  }
}

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel('Test Context Native', {
    log: true,
  });
  client = new DaemonClient(readSettings);
  const usage = new UsageSummary();
  let renderUsage = (): void => {};
  let scheduleDaemonRefresh: (force?: boolean) => void = () => {};

  const bridge = new NativeToolBridge(
    client,
    output,
    async (missingRootPath: string, token?: vscode.CancellationToken) => {
      if (!readAutoRegister()) {
        throw new RootResolutionError(
          `Workspace ${missingRootPath} is not registered with Test Context.`,
        );
      }
      const regGen = client?.generation;
      await registerRootWithCli(missingRootPath, output, token);
      // Only refresh registry cache (preserving the current invocation's rootStateGeneration
      // so the newly registered root is recorded in sticky state and usage summary).
      if (client?.generation === regGen) {
        bridge.refreshRegistry();
      }
    },
    (event) => {
      usage.record(event);
      renderUsage();
      scheduleDaemonRefresh(false);
    },
  );
  bridge.register();

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 19);
  status.name = 'Test Context Native';
  status.command = 'testContextNative.showStatus';
  status.text = '$(beaker) TCTX Native';
  status.tooltip = 'Show Test Context native tool status';
  status.show();

  let needsManualRegistration = false;
  let probing: Promise<void> | undefined;
  let daemonRefreshing: Promise<void> | undefined;
  let lastDaemonRefreshAt = 0;
  let daemonRefreshTimer: NodeJS.Timeout | undefined;
  let periodicStatusTimer: NodeJS.Timeout | undefined;
  let folderProbeTimer: NodeJS.Timeout | undefined;

  const transportLabel = (): string =>
    client?.usesWslBridge ? 'WSL bridge' : 'Unix socket';

  renderUsage = (): void => {
    if (needsManualRegistration) {
      return;
    }
    applyStatusBarUsage(status, usage);
  };

  const refreshDaemonSnapshot = async (
    explicitRoot?: string | null,
    force = false,
  ): Promise<void> => {
    if (!vscode.workspace.isTrusted || !client) {
      return;
    }
    if (!force && Date.now() - lastDaemonRefreshAt < FOCUS_STATUS_THROTTLE_MS) {
      return;
    }
    if (daemonRefreshing) {
      return daemonRefreshing;
    }
    const task = (async (): Promise<void> => {
      const source = new vscode.CancellationTokenSource();
      try {
        let targetRoot: string | null =
          explicitRoot !== undefined ? explicitRoot : (usage.lastRoot ?? null);
        if (targetRoot === null && !client?.usesWslBridge) {
          try {
            targetRoot = await bridge.resolveRoot(undefined, source.token, false);
          } catch {
            targetRoot = null;
          }
        }
        const raw = await bridge.queryDaemonStatus(targetRoot, source.token);
        const snap = usage.ingestDaemonStatus(raw, transportLabel());
        // When no explicit root was known (e.g. Windows WSL Agents window before first call)
        // and the daemon already has a loaded root, pull its non-blocking cached status too.
        if (targetRoot === null && snap && !snap.workspace && snap.loadedRoots.length > 0) {
          const firstLoaded = snap.loadedRoots[0];
          if (firstLoaded) {
            try {
              const loadedRaw = await bridge.queryDaemonStatus(firstLoaded, source.token);
              usage.ingestDaemonStatus(loadedRaw, transportLabel());
            } catch {
              // Keep daemon-level snapshot if root-specific lookup fails.
            }
          }
        }
        lastDaemonRefreshAt = Date.now();
        renderUsage();
      } catch {
        // Never overwrite an active status indicator on a passive background status poll.
      } finally {
        source.dispose();
      }
    })().finally(() => {
      daemonRefreshing = undefined;
    });
    daemonRefreshing = task;
    return task;
  };

  scheduleDaemonRefresh = (force = false): void => {
    if (!vscode.workspace.isTrusted || !client) {
      return;
    }
    if (daemonRefreshTimer) {
      clearTimeout(daemonRefreshTimer);
    }
    daemonRefreshTimer = setTimeout(() => {
      daemonRefreshTimer = undefined;
      void refreshDaemonSnapshot(undefined, force);
    }, 500);
    daemonRefreshTimer.unref?.();
  };

  const currentFoldersFingerprint = (): string =>
    (vscode.workspace.workspaceFolders ?? [])
      .map((folder) => folder.uri.toString())
      .sort()
      .join('\n');
  let lastFoldersFingerprint = currentFoldersFingerprint();
  const probe = (): Promise<void> => {
    probing ??= runStartupProbe(bridge, status, output, usage, transportLabel, (manual) => {
      needsManualRegistration = manual;
    }).finally(() => {
      probing = undefined;
      renderUsage();
    });
    return probing;
  };

  periodicStatusTimer = setInterval(() => {
    if (vscode.workspace.isTrusted && vscode.window.state?.focused !== false && client) {
      void refreshDaemonSnapshot(undefined, false);
    }
  }, PERIODIC_STATUS_REFRESH_MS);
  periodicStatusTimer.unref?.();

  context.subscriptions.push(
    output,
    status,
    bridge,
    client,
    vscode.commands.registerCommand('testContextNative.gettingStarted', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openWalkthrough',
        `${context.extension.id}#getting-started`,
        false,
      );
    }),
    vscode.commands.registerCommand('testContextNative.configureConnection', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openSettings',
        `@ext:${context.extension.id}`,
      );
    }),
    vscode.commands.registerCommand('testContextNative.showStatus', async () => {
      await showStatus(bridge, status, output, usage, transportLabel);
    }),
    vscode.commands.registerCommand('testContextNative.showSessionRoi', () => {
      output.appendLine(usage.sessionRoiReport());
      output.show(true);
    }),
    vscode.commands.registerCommand('testContextNative.selfTest', async (root?: string) => {
      await runSelfTest(bridge, output, root);
    }),
    vscode.commands.registerCommand('testContextNative.registerWorkspace', async () => {
      const folders = vscode.workspace.workspaceFolders ?? [];
      if (folders.length === 0) {
        await vscode.window.showErrorMessage('Open a workspace folder before registering Test Context.');
        return;
      }
      let folder = folders[0];
      if (folders.length > 1) {
        const activeUri = vscode.window.activeTextEditor?.document.uri;
        const activeFolder = activeUri ? vscode.workspace.getWorkspaceFolder(activeUri) : undefined;
        const picked = await vscode.window.showQuickPick(
          folders.map((f) => ({
            label: f.name,
            description: f.uri.fsPath,
            folder: f,
          })),
          {
            placeHolder: activeFolder
              ? `Select workspace folder to register (active: ${activeFolder.name})`
              : 'Select workspace folder to register with Test Context',
          },
        );
        if (!picked) {
          return;
        }
        folder = picked.folder;
      }
      if (!folder) {
        return;
      }
      await registerRootWithCli(folder.uri.fsPath, output);
      bridge.invalidateRegistry();
      await probe();
      await vscode.window.showInformationMessage(
        `Registered with Test Context: ${folder.uri.fsPath}`,
      );
    }),
    vscode.commands.registerCommand('testContextNative.syncWorkspace', async () => {
      const source = new vscode.CancellationTokenSource();
      try {
        // Resolve the root INSIDE the generation-guarded `bridge.invoke` rather than before it
        // so workspace folder changes mid-resolution never turn a stale root into an explicit target.
        const value = await bridge.invoke(
          {
            name: 'command_sync',
            operation: 'test_sync',
            needsRoot: true,
            invocationMessage: () => 'Synchronizing Test Context AST graph',
            arguments: () => ({ wait: false }),
          },
          {},
          source.token,
        );
        output.appendLine(formatToolResult('test_sync', value));
        output.show(true);
      } finally {
        source.dispose();
      }
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('testContextNative')) {
        client?.reset();
        bridge.invalidateRegistry();
        usage.clearLastRoot();
        usage.clearDaemonStatus();
        void probe();
      }
    }),
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      void probe();
    }),
    {
      dispose: (): void => {
        if (folderProbeTimer) {
          clearTimeout(folderProbeTimer);
          folderProbeTimer = undefined;
        }
        if (daemonRefreshTimer) {
          clearTimeout(daemonRefreshTimer);
          daemonRefreshTimer = undefined;
        }
        if (periodicStatusTimer) {
          clearInterval(periodicStatusTimer);
          periodicStatusTimer = undefined;
        }
      },
    },
    ...(typeof (vscode.window as { onDidChangeWindowState?: unknown }).onDidChangeWindowState === 'function'
      ? [
          vscode.window.onDidChangeWindowState((state) => {
            if (state.focused && vscode.workspace.isTrusted) {
              void refreshDaemonSnapshot(undefined, false);
            }
          }),
        ]
      : []),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      const nextFingerprint = currentFoldersFingerprint();
      if (nextFingerprint === lastFoldersFingerprint) {
        return;
      }
      lastFoldersFingerprint = nextFingerprint;
      bridge.invalidateRegistry();
      usage.clearLastRoot();
      usage.clearDaemonStatus();
      renderUsage();
      if (folderProbeTimer) {
        clearTimeout(folderProbeTimer);
      }
      folderProbeTimer = setTimeout(() => {
        folderProbeTimer = undefined;
        if (vscode.workspace.isTrusted) {
          void probe();
        }
      }, 750);
    }),
  );

  if (!context.globalState.get<boolean>(OUTPUT_DISCOVERED_STATE_KEY, false)) {
    output.show(true);
    void context.globalState.update(OUTPUT_DISCOVERED_STATE_KEY, true);
  }

  try {
    output.appendLine(`[ready] native endpoint=${client.endpoint}`);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    output.appendLine(`[error] invalid native endpoint configuration: ${message}`);
    status.text = '$(warning) TCTX Native';
    status.tooltip = `Invalid Test Context native endpoint: ${message}`;
  }
  if (vscode.workspace.isTrusted) {
    // Defer startup daemon probe briefly so extension activation stays zero-IPC on startup
    const startupProbeTimer = setTimeout(() => {
      if (vscode.workspace.isTrusted && client) {
        void probe();
      }
    }, 1_500);
    startupProbeTimer.unref?.();
    context.subscriptions.push({
      dispose: () => clearTimeout(startupProbeTimer),
    });
  } else {
    status.text = '$(lock) TCTX Native';
    status.tooltip = 'Trust this workspace to enable native Test Context tools';
  }
}

export function deactivate(): void {
  client?.dispose();
  client = undefined;
}

async function registerRootWithCli(
  rootPath: string,
  output: vscode.OutputChannel,
  token?: vscode.CancellationToken,
): Promise<void> {
  if (token?.isCancellationRequested) {
    throw new RequestCancelledError();
  }
  const abortController = new AbortController();
  const cancelSub = token?.onCancellationRequested(() => {
    abortController.abort();
  });
  try {
    if (process.platform === 'win32') {
      const settings = readSettings();
      assertWslDistributionMatches(rootPath, settings.wslDistribution);
      const pathDistro = extractWslUncDistribution(rootPath);
      const effectiveDistro = settings.wslDistribution || pathDistro;
      const linuxRoot = toPosixPathIfPossible(rootPath, effectiveDistro);
      if (!linuxRoot) {
        throw new Error(`Cannot map workspace path '${rootPath}' to a WSL Linux path.`);
      }
      const wslArgs: string[] = [];
      if (effectiveDistro) {
        wslArgs.push('-d', effectiveDistro);
      }
      const wslExe = settings.wslExecutablePath || 'tctx';
      // Probe the actual Linux path inside WSL (`[ -f "$target" ]`) so a file target resolves to
      // its parent directory while dotted directories (e.g. `/home/dev/project.v2`) stay intact.
      wslArgs.push(
        '--',
        'sh',
        '-c',
        'target="$1"; [ -f "$target" ] && target="$(dirname "$target")"; exec "$2" --root "$target" register',
        'tctx-register',
        linuxRoot,
        wslExe,
      );
      await execFileAsync('wsl.exe', wslArgs, {
        timeout: 15_000,
        signal: abortController.signal,
      });
      output.appendLine(`[success] workspace_registered root=${linuxRoot}`);
      return;
    }
    const exe = executablePath();
    const resolved = resolve(rootPath);
    await execFileAsync(exe, ['--root', resolved, 'register'], {
      timeout: 15_000,
      signal: abortController.signal,
    });
    output.appendLine(`[success] workspace_registered root=${resolved}`);
  } catch (error: unknown) {
    if (token?.isCancellationRequested || abortController.signal.aborted) {
      throw new RequestCancelledError();
    }
    throw error;
  } finally {
    cancelSub?.dispose();
  }
}

async function showStatus(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
  usage: UsageSummary,
  transportLabel: () => string,
  explicitRoot?: string,
): Promise<void> {
  const source = new vscode.CancellationTokenSource();
  try {
    // Resolve the root solely inside the generation-guarded `bridge.invoke` so workspace folder
    // changes mid-resolution cannot turn a stale root into an explicit target.
    const value = await bridge.invoke(
      {
        name: 'command_status',
        operation: 'test_status',
        needsRoot: true,
        invocationMessage: () => 'Reading Test Context status',
        arguments: () => ({ load: true }),
      },
      explicitRoot ? { root: explicitRoot } : {},
      source.token,
    );
    usage.ingestDaemonStatus(value, transportLabel());
    applyStatusBarUsage(statusBar, usage);
    output.appendLine(formatToolResult('test_status', value));
    output.show(true);
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    statusBar.text = '$(warning) TCTX Native';
    statusBar.tooltip = message;
    output.appendLine(`[error] status message=${message}`);
    output.show(true);
  } finally {
    source.dispose();
  }
}

async function runSelfTest(
  bridge: NativeToolBridge,
  output: vscode.OutputChannel,
  explicitRoot?: string,
): Promise<void> {
  const source = new vscode.CancellationTokenSource();
  try {
    await vscode.commands.executeCommand('setContext', 'testContextNative.selfTestPassed', false);
    const registered = registeredNativeTools();
    if (registered.length !== NATIVE_TOOL_NAMES.length) {
      throw new Error(
        `Expected ${NATIVE_TOOL_NAMES.length} native tools, found ${registered.length}.`,
      );
    }
    const { value: daemonStatus, trace } = await bridge.invokeTraced(
      {
        name: 'self_test_status',
        operation: 'test_status',
        needsRoot: true,
        invocationMessage: () => 'Running Test Context self-test',
        arguments: () => ({ load: false }),
      },
      explicitRoot ? { root: explicitRoot } : {},
      source.token,
    );
    validateDaemonStatus(daemonStatus);
    await vscode.commands.executeCommand('setContext', 'testContextNative.selfTestPassed', true);
    output.appendLine(formatToolResult('test_status', daemonStatus));
    output.show(true);
    await vscode.window.showInformationMessage(
      `Test Context self-test passed: ${trace.root ?? explicitRoot ?? ''}`,
    );
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    output.appendLine(`[error] self_test message=${message}`);
    await vscode.window.showErrorMessage(`Test Context self-test failed: ${message}`);
  } finally {
    source.dispose();
  }
}

async function runStartupProbe(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
  usage: UsageSummary,
  transportLabel: () => string,
  setManualRegistration: (needed: boolean) => void,
): Promise<void> {
  const source = new vscode.CancellationTokenSource();
  try {
    const registered = await waitForNativeTools();
    if (registered.length !== NATIVE_TOOL_NAMES.length) {
      throw new Error(
        `Language Model Tool registry contains ${registered.length}/${NATIVE_TOOL_NAMES.length} Test Context tools.`,
      );
    }
    const workspaces = await bridge.listWorkspaces(source.token);
    setManualRegistration(false);
    if (client?.usesWslBridge) {
      statusBar.text = '$(beaker) TCTX Available';
      statusBar.tooltip = `${registered.length} Test Context native tools registered through the WSL bridge; pass an explicit Linux root when multiple roots are registered`;
      try {
        const raw = await bridge.queryDaemonStatus(usage.lastRoot ?? null, source.token);
        const snap = usage.ingestDaemonStatus(raw, transportLabel());
        if (!snap?.workspace && snap && snap.loadedRoots.length > 0 && snap.loadedRoots[0]) {
          try {
            const loadedRaw = await bridge.queryDaemonStatus(snap.loadedRoots[0], source.token);
            usage.ingestDaemonStatus(loadedRaw, transportLabel());
          } catch {
            // Keep daemon-level snapshot.
          }
        }
        applyStatusBarUsage(statusBar, usage);
      } catch {
        // Keep fallback tooltip when passive status lookup fails.
      }
      output.appendLine(
        `[ready] tools=${registered.length} workspaces=${workspaces.length} transport=wsl-native-bridge root=explicit-when-ambiguous`,
      );
      return;
    }
    const root = await bridge.resolveRoot(undefined, source.token, false);
    statusBar.text = '$(beaker) TCTX Available';
    statusBar.tooltip = `${registered.length} Test Context native tools registered and callable for ${root}`;
    try {
      const raw = await bridge.queryDaemonStatus(root, source.token);
      usage.ingestDaemonStatus(raw, transportLabel());
      applyStatusBarUsage(statusBar, usage);
    } catch {
      // Keep fallback tooltip when passive status lookup fails.
    }
    output.appendLine(
      `[ready] tools=${registered.length} workspaces=${workspaces.length} transport=unix-socket root=${root}`,
    );
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    if (error instanceof RootResolutionError) {
      setManualRegistration(true);
      statusBar.text = '$(add) TCTX Register';
      statusBar.tooltip = 'Workspace will be registered on first Test Context tool call; click for status';
      output.appendLine(`[info] workspace_registration_deferred message=${message}`);
    } else {
      setManualRegistration(false);
      statusBar.text = '$(warning) TCTX Native';
      statusBar.tooltip = message;
      output.appendLine(`[error] native_not_ready message=${message}`);
    }
  } finally {
    source.dispose();
  }
}

function registeredNativeTools(): string[] {
  const available = new Set(vscode.lm.tools.map((tool) => tool.name));
  return NATIVE_TOOL_NAMES.filter((name) => available.has(name));
}

async function waitForNativeTools(): Promise<string[]> {
  const deadline = performance.now() + 2_000;
  let registered = registeredNativeTools();
  while (registered.length !== NATIVE_TOOL_NAMES.length && performance.now() < deadline) {
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 50));
    registered = registeredNativeTools();
  }
  return registered;
}

function readSettings(): ClientSettings {
  const config = vscode.workspace.getConfiguration('testContextNative');
  const wslDistribution = config.get<string>('wslDistribution', '').trim();
  const wslExecutablePath = config.get<string>('wslExecutablePath', '').trim();
  return {
    socketPath: config.get<string>('socketPath', ''),
    autoStartService: config.get<boolean>('autoStartService', true),
    requestTimeoutMs: config.get<number>('requestTimeoutMs', 600_000),
    ...(wslDistribution ? { wslDistribution } : {}),
    ...(wslExecutablePath ? { wslExecutablePath } : {}),
  };
}

function readAutoRegister(): boolean {
  return vscode.workspace
    .getConfiguration('testContextNative')
    .get<boolean>('autoRegisterWorkspace', true);
}

function executablePath(): string {
  const configured = vscode.workspace
    .getConfiguration('testContextNative')
    .get<string>('executablePath', '')
    .trim();
  if (configured.length === 0) {
    return join(homedir(), '.local/bin/tctx');
  }
  if (configured.startsWith('~/')) {
    return join(homedir(), configured.slice(2));
  }
  if (!isAbsolute(configured)) {
    throw new Error('testContextNative.executablePath must be an absolute path.');
  }
  return resolve(configured);
}
