import { execFile } from 'node:child_process';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve } from 'node:path';
import { promisify } from 'node:util';

import * as vscode from 'vscode';

import { DaemonClient } from './daemonClient.js';
import { NATIVE_TOOL_NAMES, NativeToolBridge } from './nativeTools.js';
import { validateDaemonStatus } from './resultFormatter.js';
import { RootResolutionError } from './rootResolver.js';
import type { ClientSettings, JsonValue } from './types.js';
import { UsageSummary } from './usage.js';

const execFileAsync = promisify(execFile);
const OUTPUT_DISCOVERED_STATE_KEY = 'testContextNative.outputDiscovered';

let client: DaemonClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel('Test Context Native', {
    log: true,
  });
  client = new DaemonClient(readSettings);
  const usage = new UsageSummary();
  let renderUsage = (): void => {};

  const bridge = new NativeToolBridge(
    client,
    output,
    async (missingRootPath: string) => {
      if (!readAutoRegister()) {
        throw new RootResolutionError(
          `Workspace ${missingRootPath} is not registered with Test Context.`,
        );
      }
      await registerRootWithCli(missingRootPath, output);
      bridge.invalidateRegistry();
    },
    (event) => {
      usage.record(event);
      renderUsage();
    },
  );
  bridge.register();

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 19);
  status.name = 'Test Context Native';
  status.command = 'testContextNative.showStatus';
  status.text = '$(beaker) TCTX Native';
  status.tooltip = 'Show Test Context native tool status';
  status.show();

  renderUsage = (): void => {
    if (usage.last) {
      status.text = usage.text();
      status.tooltip = usage.tooltip();
    }
  };

  let probing: Promise<void> | undefined;
  const probe = (): Promise<void> => {
    probing ??= runStartupProbe(bridge, status, output).finally(() => {
      probing = undefined;
      renderUsage();
    });
    return probing;
  };

  context.subscriptions.push(
    output,
    status,
    bridge,
    client,
    vscode.commands.registerCommand('testContextNative.gettingStarted', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openWalkthrough',
        'moonchoi.test-context-native#getting-started',
        false,
      );
    }),
    vscode.commands.registerCommand('testContextNative.configureConnection', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openSettings',
        '@ext:moonchoi.test-context-native',
      );
    }),
    vscode.commands.registerCommand('testContextNative.showStatus', async () => {
      await showStatus(bridge, status, output, usage.lastRoot);
    }),
    vscode.commands.registerCommand('testContextNative.selfTest', async (root?: string) => {
      await runSelfTest(bridge, output, root);
    }),
    vscode.commands.registerCommand('testContextNative.registerWorkspace', async () => {
      const folder = vscode.workspace.workspaceFolders?.[0];
      if (!folder) {
        await vscode.window.showErrorMessage('Open a workspace folder before registering Test Context.');
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
        const root = await bridge.resolveRoot(usage.lastRoot, source.token, true);
        const value = await bridge.invoke(
          {
            name: 'command_sync',
            operation: 'test_sync',
            needsRoot: true,
            invocationMessage: () => 'Synchronizing Test Context AST graph',
            arguments: () => ({ wait: false }),
          },
          { root },
          source.token,
        );
        output.appendLine(JSON.stringify(value, null, 2));
        output.show(true);
      } finally {
        source.dispose();
      }
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('testContextNative')) {
        client?.reset();
        bridge.invalidateRegistry();

        if (!context.globalState.get<boolean>(OUTPUT_DISCOVERED_STATE_KEY, false)) {
          output.show(true);
          void context.globalState.update(OUTPUT_DISCOVERED_STATE_KEY, true);
        }

        void probe();
      }
    }),
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      void probe();
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      bridge.invalidateRegistry();
      if (vscode.workspace.isTrusted) {
        void probe();
      }
    }),
  );

  output.appendLine(`Activated Test Context native tools; endpoint=${client.endpoint}`);
  if (vscode.workspace.isTrusted) {
    void probe();
  } else {
    status.text = '$(lock) TCTX Native';
    status.tooltip = 'Trust this workspace to enable native Test Context tools';
  }
}

export function deactivate(): void {
  client?.dispose();
  client = undefined;
}

async function registerRootWithCli(rootPath: string, output: vscode.OutputChannel): Promise<void> {
  const exe = executablePath();
  const resolved = resolve(rootPath);
  const { stdout } = await execFileAsync(exe, ['--root', resolved, 'register'], {
    timeout: 15_000,
  });
  output.appendLine(`TCTX_REGISTERED root=${resolved} ${stdout.trim()}`);
}

async function showStatus(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
  explicitRoot?: string,
): Promise<void> {
  const source = new vscode.CancellationTokenSource();
  try {
    const root = await bridge.resolveRoot(explicitRoot, source.token, true);
    const value = await bridge.invoke(
      {
        name: 'command_status',
        operation: 'test_status',
        needsRoot: true,
        invocationMessage: () => 'Reading Test Context status',
        arguments: () => ({}),
      },
      { root },
      source.token,
    );
    output.appendLine(JSON.stringify(value, null, 2));
    output.show(true);
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    statusBar.text = '$(warning) TCTX Native';
    statusBar.tooltip = message;
    output.appendLine(`STATUS_ERROR ${message}`);
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
    const root = await bridge.resolveRoot(explicitRoot, source.token, true);
    const registered = registeredNativeTools();
    if (registered.length !== NATIVE_TOOL_NAMES.length) {
      throw new Error(
        `Expected ${NATIVE_TOOL_NAMES.length} native tools, found ${registered.length}.`,
      );
    }
    const toolResult = await vscode.lm.invokeTool(
      'test-context-native_testStatus',
      {
        input: { root },
        toolInvocationToken: undefined,
      },
      source.token,
    );
    const statusText = toolResultText(toolResult);
    const daemonStatus = JSON.parse(statusText) as JsonValue;
    validateDaemonStatus(daemonStatus);
    await vscode.commands.executeCommand('setContext', 'testContextNative.selfTestPassed', true);
    output.appendLine(JSON.stringify(daemonStatus, null, 2));
    output.show(true);
    await vscode.window.showInformationMessage(`Test Context self-test passed: ${root}`);
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    output.appendLine(`SELF_TEST_FAILED ${message}`);
    await vscode.window.showErrorMessage(`Test Context self-test failed: ${message}`);
  } finally {
    source.dispose();
  }
}

async function runStartupProbe(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
): Promise<void> {
  const source = new vscode.CancellationTokenSource();
  try {
    const registered = registeredNativeTools();
    if (registered.length !== NATIVE_TOOL_NAMES.length) {
      throw new Error(
        `Language Model Tool registry contains ${registered.length}/${NATIVE_TOOL_NAMES.length} Test Context tools.`,
      );
    }
    const workspaceResult = await vscode.lm.invokeTool(
      'test-context-native_testWorkspaces',
      {
        input: {},
        toolInvocationToken: undefined,
      },
      source.token,
    );
    const workspaces = JSON.parse(toolResultText(workspaceResult)) as JsonValue;
    if (!Array.isArray(workspaces)) {
      throw new Error('Native workspace probe returned a non-array result.');
    }
    const root = await bridge.resolveRoot(undefined, source.token, false);
    statusBar.text = '$(beaker) TCTX Available';
    statusBar.tooltip = `${registered.length} Test Context native tools registered and callable for ${root}`;
    output.appendLine(
      `EXTENSION_CALLABLE tools=${registered.join(',')} workspaces=${workspaces.length} root=${root}`,
    );
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    if (error instanceof RootResolutionError) {
      statusBar.text = '$(add) TCTX Register';
      statusBar.tooltip = 'Workspace will be registered on first Test Context tool call; click for status';
      output.appendLine(`NATIVE_UNREGISTERED_DEFERRED ${message}`);
    } else {
      statusBar.text = '$(warning) TCTX Native';
      statusBar.tooltip = message;
      output.appendLine(`NATIVE_NOT_READY ${message}`);
    }
  } finally {
    source.dispose();
  }
}

function toolResultText(result: vscode.LanguageModelToolResult): string {
  return result.content
    .filter(
      (part): part is vscode.LanguageModelTextPart =>
        part instanceof vscode.LanguageModelTextPart,
    )
    .map((part) => part.value)
    .filter((text) => !text.startsWith('TCTX_TRACE '))
    .join('');
}

function registeredNativeTools(): string[] {
  const available = new Set(vscode.lm.tools.map((tool) => tool.name));
  return NATIVE_TOOL_NAMES.filter((name) => available.has(name));
}

function readSettings(): ClientSettings {
  const config = vscode.workspace.getConfiguration('testContextNative');
  const wslDistribution = config.get<string>('wslDistribution', '').trim();
  const wslExecutablePath = config.get<string>('wslExecutablePath', '').trim();
  return {
    socketPath: config.get<string>('socketPath', ''),
    autoStartService: config.get<boolean>('autoStartService', true),
    requestTimeoutMs: config.get<number>('requestTimeoutMs', 180_000),
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
