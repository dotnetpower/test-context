import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

import * as vscode from 'vscode';

import { TracedRequestError, type ClientTrace, type DaemonClient } from './daemonClient.js';
import {
  formatToolResult,
  formatTraceLog,
  shouldReportTrace,
} from './resultFormatter.js';
import { resolveRegisteredRootAfterRefresh, resolveWslRoot } from './rootResolver.js';
import type { DaemonRequest, JsonValue, WorkspaceSpec } from './types.js';
import { classifyOutcome, type UsageEvent } from './usage.js';

const REGISTRY_CACHE_MS = 5_000;

export const NATIVE_TOOL_NAMES = [
  'test-context-native_testRun',
  'test-context-native_testAffected',
  'test-context-native_testGraph',
  'test-context-native_testStatus',
  'test-context-native_testSync',
  'test-context-native_testInvalidate',
  'test-context-native_testWorkspaces',
] as const;

interface RootInput {
  root?: string;
}

interface TestStatusInput extends RootInput {
  load?: boolean;
}

interface TestRunInput extends RootInput {
  targets?: string[];
  k?: string;
  mark?: string;
  rerun_failed?: boolean;
  no_cache?: boolean;
  pytest_args?: string[];
}

interface TestAffectedInput extends RootInput {
  files?: string[];
  git_diff?: boolean;
}

interface TestGraphInput extends RootInput {
  file: string;
}

interface TestSyncInput extends RootInput {
  wait?: boolean;
}

interface TestInvalidateInput extends RootInput {
  files?: string[];
}

type EmptyInput = Record<string, never>;

export interface ToolDefinition<T> {
  name: string;
  operation: string;
  invocationMessage(input: T): string;
  needsRoot: boolean;
  arguments(input: T): Record<string, JsonValue>;
}

class WorkspaceRegistry {
  private cached: WorkspaceSpec[] | undefined;
  private cachedAt = 0;

  public constructor(private readonly client: DaemonClient) {}

  public async list(
    token: vscode.CancellationToken,
    refresh = false,
  ): Promise<WorkspaceSpec[]> {
    if (!refresh && this.cached && Date.now() - this.cachedAt < REGISTRY_CACHE_MS) {
      return this.cached;
    }
    const result = await this.client.request<unknown>(
      request(null, 'test_workspaces', {}),
      token,
    );
    if (!Array.isArray(result)) {
      throw new Error('Test Context daemon returned an invalid workspace registry.');
    }
    const workspaces = result.map(parseWorkspaceSpec);
    this.cached = workspaces;
    this.cachedAt = Date.now();
    return workspaces;
  }

  public invalidate(): void {
    this.cached = undefined;
    this.cachedAt = 0;
  }
}

export class NativeToolBridge implements vscode.Disposable {
  private readonly registry: WorkspaceRegistry;
  private readonly disposables: vscode.Disposable[] = [];

  public constructor(
    private readonly client: DaemonClient,
    private readonly output: vscode.OutputChannel,
    private readonly registerMissingRoot?: (path: string) => Promise<void>,
    private readonly onUsage?: (event: UsageEvent) => void,
  ) {
    this.registry = new WorkspaceRegistry(client);
  }

  public register(): void {
    const definitions: Array<ToolDefinition<object>> = [
      testRunDefinition,
      testAffectedDefinition,
      testGraphDefinition,
      testStatusDefinition,
      testSyncDefinition,
      testInvalidateDefinition,
      testWorkspacesDefinition,
    ];
    for (const definition of definitions) {
      const disposable = vscode.lm.registerTool(
        definition.name,
        new NativeTool(definition, this),
      );
      this.disposables.push(disposable);
    }
  }

  public async invoke<T>(
    definition: ToolDefinition<T>,
    input: T,
    token: vscode.CancellationToken,
  ): Promise<JsonValue> {
    return (await this.invokeTraced(definition, input, token)).value;
  }

  public async invokeTraced<T>(
    definition: ToolDefinition<T>,
    input: T,
    token: vscode.CancellationToken,
  ): Promise<{ value: JsonValue; trace: ClientTrace }> {
    ensureTrustedWorkspace();
    const started = performance.now();
    const startedUnixMs = Date.now();
    const traceId = randomUUID();
    let root: string | null = null;
    try {
      root = definition.needsRoot
        ? await this.resolveRoot((input as RootInput).root, token)
        : null;
      const result = await this.client.requestTraced<JsonValue>(
        request(root, definition.operation, definition.arguments(input)),
        token,
        traceId,
        performance.now() - started,
      );
      result.trace.total_ms = performance.now() - started;
      result.trace.started_unix_ms = startedUnixMs;
      result.trace.completed_unix_ms = Date.now();
      result.trace.outcome = 'success';
      this.onUsage?.({ operation: definition.operation, root });
      this.output.appendLine(
        `[success] operation=${definition.operation} root=${root ?? '-'} duration=${result.trace.total_ms.toFixed(3)}ms trace=${traceId}`,
      );
      if (definition.operation === 'test_sync') {
        this.registry.invalidate();
      }
      return result;
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : String(error);
      this.onUsage?.({ operation: definition.operation, root, error: message });
      if (error instanceof TracedRequestError) {
        error.trace.outcome = classifyOutcome(message);
        error.trace.started_unix_ms = startedUnixMs;
        error.trace.completed_unix_ms = Date.now();
        this.recordTrace(error.trace, token);
      }
      this.output.appendLine(
        `[error] operation=${definition.operation} trace=${traceId} message=${message}`,
      );
      throw new Error(
        message.includes(`[tctx_trace=${traceId}]`)
          ? message
          : `${message} [tctx_trace=${traceId}]`,
        { cause: error },
      );
    }
  }

  public recordTrace(trace: ClientTrace, token: vscode.CancellationToken): void {
    this.output.appendLine(formatTraceLog(trace));
    if (!shouldReportTrace(trace)) {
      return;
    }
    void this.client.request(request(null, 'test_trace', trace), token).catch((error: unknown) => {
      this.output.appendLine(
        `[warning] trace_report_failed id=${trace.trace_id} message=${String(error)}`,
      );
    });
  }

  public async listWorkspaces(token: vscode.CancellationToken): Promise<WorkspaceSpec[]> {
    ensureTrustedWorkspace();
    return this.registry.list(token, true);
  }

  public async resolveRoot(
    explicit: string | undefined,
    token: vscode.CancellationToken,
    allowPreparation = true,
  ): Promise<string> {
    ensureTrustedWorkspace();
    const candidates = workspaceCandidates();
    const workspaces = await this.registry.list(token);
    if (this.client.usesWslBridge) {
      return resolveWslRoot(workspaces, explicit);
    }
    return resolveRegisteredRootAfterRefresh(
      workspaces,
      () => this.registry.list(token, true),
      explicit,
      candidates,
      allowPreparation ? this.registerMissingRoot : undefined,
    );
  }

  public dispose(): void {
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.disposables.length = 0;
  }

  public invalidateRegistry(): void {
    this.registry.invalidate();
  }
}

class NativeTool<T> implements vscode.LanguageModelTool<T> {
  public constructor(
    private readonly definition: ToolDefinition<T>,
    private readonly bridge: NativeToolBridge,
  ) {}

  public async invoke(
    options: vscode.LanguageModelToolInvocationOptions<T>,
    token: vscode.CancellationToken,
  ): Promise<vscode.LanguageModelToolResult> {
    const result = await this.bridge.invokeTraced(this.definition, options.input, token);
    const started = performance.now();
    const text = formatToolResult(this.definition.operation, result.value);
    result.trace.serialize_ms = performance.now() - started;
    result.trace.total_ms += result.trace.serialize_ms;
    result.trace.completed_unix_ms = Date.now();
    this.bridge.recordTrace(result.trace, token);
    return new vscode.LanguageModelToolResult([
      new vscode.LanguageModelTextPart(text),
      new vscode.LanguageModelTextPart(`TCTX_TRACE ${formatTraceLog(result.trace)}`),
    ]);
  }

  public prepareInvocation(
    options: vscode.LanguageModelToolInvocationPrepareOptions<T>,
    _token: vscode.CancellationToken,
  ): vscode.ProviderResult<vscode.PreparedToolInvocation> {
    void _token;
    return {
      invocationMessage: this.definition.invocationMessage(options.input),
    };
  }
}

const testRunDefinition: ToolDefinition<TestRunInput> = {
  name: 'test-context-native_testRun',
  operation: 'test_run',
  needsRoot: true,
  invocationMessage: (input) =>
    input.targets && input.targets.length > 0
      ? `Running incremental pytest via Test Context for ${input.targets.join(', ')}`
      : 'Running incremental pytest via Test Context',
  arguments: (input) => {
    const args: Record<string, JsonValue> = {
      targets: input.targets ?? [],
      rerun_failed: input.rerun_failed ?? true,
      no_cache: input.no_cache ?? false,
      pytest_args: input.pytest_args ?? [],
    };
    if (input.k !== undefined) {
      args.k = input.k;
    }
    if (input.mark !== undefined) {
      args.mark = input.mark;
    }
    return args;
  },
};

const testAffectedDefinition: ToolDefinition<TestAffectedInput> = {
  name: 'test-context-native_testAffected',
  operation: 'test_affected',
  needsRoot: true,
  invocationMessage: () => 'Finding affected pytest tests via AST reverse dependency graph',
  arguments: (input) => ({
    files: input.files ?? [],
    git_diff: input.git_diff ?? true,
  }),
};

const testGraphDefinition: ToolDefinition<TestGraphInput> = {
  name: 'test-context-native_testGraph',
  operation: 'test_graph',
  needsRoot: true,
  invocationMessage: (input) => `Inspecting Python AST dependency graph for ${input.file}`,
  arguments: (input) => ({
    file: input.file,
  }),
};

const testStatusDefinition: ToolDefinition<TestStatusInput> = {
  name: 'test-context-native_testStatus',
  operation: 'test_status',
  needsRoot: true,
  invocationMessage: () => 'Reading Test Context status and background cache statistics',
  arguments: (input) => ({ load: input.load ?? false }),
};

const testSyncDefinition: ToolDefinition<TestSyncInput> = {
  name: 'test-context-native_testSync',
  operation: 'test_sync',
  needsRoot: true,
  invocationMessage: () => 'Synchronizing Python AST graph and background test queue',
  arguments: (input) => ({
    wait: input.wait ?? false,
  }),
};

const testInvalidateDefinition: ToolDefinition<TestInvalidateInput> = {
  name: 'test-context-native_testInvalidate',
  operation: 'test_invalidate',
  needsRoot: true,
  invocationMessage: () => 'Invalidating cached test results and queueing background re-verification',
  arguments: (input) => ({
    files: input.files ?? [],
  }),
};

const testWorkspacesDefinition: ToolDefinition<EmptyInput> = {
  name: 'test-context-native_testWorkspaces',
  operation: 'test_workspaces',
  needsRoot: false,
  invocationMessage: () => 'Listing registered Test Context workspaces',
  arguments: () => ({}),
};

function request(
  root: string | null,
  name: string,
  args: Record<string, JsonValue>,
): DaemonRequest {
  return {
    root,
    name,
    arguments: args,
    compact: false,
  };
}

function ensureTrustedWorkspace(): void {
  if (!vscode.workspace.isTrusted) {
    throw new Error('Trust this workspace before using Test Context native tools.');
  }
}

function workspaceCandidates(): string[] {
  const candidates: string[] = [];
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  if (activeUri?.scheme === 'file') {
    const folder = vscode.workspace.getWorkspaceFolder(activeUri);
    if (folder) {
      candidates.push(resolve(folder.uri.fsPath));
    }
  }
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    if (folder.uri.scheme === 'file') {
      const path = resolve(folder.uri.fsPath);
      if (!candidates.includes(path)) {
        candidates.push(path);
      }
    }
  }
  return candidates;
}

function parseWorkspaceSpec(value: unknown): WorkspaceSpec {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error('Invalid workspace specification entry.');
  }
  const record = value as Record<string, unknown>;
  if (typeof record.root !== 'string' || !Array.isArray(record.scopes) || !Array.isArray(record.globs)) {
    throw new Error('Workspace specification is missing root, scopes, or globs.');
  }
  return {
    root: record.root,
    scopes: record.scopes.filter((s): s is string => typeof s === 'string'),
    globs: record.globs.filter((g): g is string => typeof g === 'string'),
  };
}
