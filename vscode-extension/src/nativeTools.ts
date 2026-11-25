import { randomUUID } from 'node:crypto';

import * as vscode from 'vscode';

import {
  ConnectionResetError,
  RequestCancelledError,
  TracedRequestError,
  type ClientTrace,
  type DaemonClient,
} from './daemonClient.js';
import {
  formatToolResult,
  formatTraceLog,
  shouldReportTrace,
} from './resultFormatter.js';
import {
  assertTargetsInsideRoot,
  buildAffectedArguments,
  buildGraphArguments,
  buildInvalidateArguments,
  buildRunArguments,
  extractInputPathHints,
  extractRawRootInput,
  extractWslUncDistribution,
  findCommonRootForInputHints,
  resolveRegisteredRootAfterRefresh,
  resolveWslDirForRegistration,
  resolveWslRoot,
  RootConflictError,
  RootResolutionError,
  toPosixPathIfPossible,
} from './rootResolver.js';
import type { JsonValue, WorkspaceSpec } from './types.js';
import { classifyOutcome, type UsageEvent } from './usage.js';
import {
  buildDaemonRequest as request,
  uriCandidatePaths,
  workspaceCandidates,
  WorkspaceRegistry,
} from './workspaceRegistry.js';

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
  ruff_fix?: boolean;
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
  arguments(input: T, root?: string | null, wslDistribution?: string): Record<string, JsonValue>;
}

export class NativeToolBridge implements vscode.Disposable {
  private readonly registry: WorkspaceRegistry;
  private readonly disposables: vscode.Disposable[] = [];
  private lastResolvedRoot: string | undefined;
  private lastResolvedDistro: string | undefined;
  private rootStateGeneration = 0;

  public constructor(
    private readonly client: DaemonClient,
    private readonly output: vscode.OutputChannel,
    private readonly registerMissingRoot?: (
      path: string,
      token?: vscode.CancellationToken,
    ) => Promise<void>,
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
    const startGen = this.client.generation;
    const startRootGen = this.rootStateGeneration;
    try {
      if (token.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      // Bind root resolution, auto-registration, and request dispatch to one settings generation
      // so switching WSL distributions mid-preparation never sends the old root to the new distro.
      const rawRoot = extractRawRootInput(input);
      const hints = extractInputPathHints(input);
      let resolveRootGen = startRootGen;
      root = definition.needsRoot
        ? await this.resolveRoot(rawRoot, token, true, hints, startGen, resolveRootGen)
        : null;
      if (token.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      if (this.client.generation !== startGen) {
        throw new ConnectionResetError(
          'Test Context settings changed during workspace resolution; retry request.',
        );
      }
      if (definition.needsRoot && this.rootStateGeneration !== resolveRootGen) {
        // Workspace folders changed mid-resolution: re-resolve once against the updated folders so
        // candidates from the previous workspace are never dispatched.
        resolveRootGen = this.rootStateGeneration;
        root = await this.resolveRoot(rawRoot, token, true, hints, startGen, resolveRootGen);
        if (token.isCancellationRequested) {
          throw new RequestCancelledError();
        }
        if (this.client.generation !== startGen || this.rootStateGeneration !== resolveRootGen) {
          throw new ConnectionResetError(
            'Workspace folders changed during root resolution; retry request.',
          );
        }
      }
      if (root && this.rootStateGeneration === resolveRootGen) {
        this.lastResolvedRoot = root;
        this.lastResolvedDistro = this.client.wslDistribution;
      }
      const capturedRootGen = resolveRootGen;
      const result = await this.client.requestTraced<JsonValue>(
        request(
          root,
          definition.operation,
          definition.arguments(input, root, this.client.wslDistribution),
        ),
        token,
        traceId,
        performance.now() - started,
        startGen,
        () => {
          if (definition.needsRoot && this.rootStateGeneration !== capturedRootGen) {
            throw new ConnectionResetError(
              'Workspace folders changed while request was queued; retry request.',
            );
          }
        },
      );
      result.trace.total_ms = performance.now() - started;
      result.trace.started_unix_ms = startedUnixMs;
      result.trace.completed_unix_ms = Date.now();
      result.trace.outcome = 'success';
      const valRec =
        typeof result.value === 'object' && result.value !== null && !Array.isArray(result.value)
          ? (result.value as Record<string, JsonValue>)
          : undefined;
      const savedMs = typeof valRec?.saved_ms === 'number' ? valRec.saved_ms : 0;
      const cachedFiles = typeof valRec?.cached_files_count === 'number' ? valRec.cached_files_count : 0;
      const executedFiles =
        typeof valRec?.executed_files_count === 'number' ? valRec.executed_files_count : 0;
      const passed = typeof valRec?.passed === 'number' ? valRec.passed : undefined;
      const failed = typeof valRec?.failed === 'number' ? valRec.failed : undefined;
      const skipped = typeof valRec?.skipped === 'number' ? valRec.skipped : undefined;
      const mode = typeof valRec?.mode === 'string' ? valRec.mode : undefined;
      const ruffIssues = Array.isArray(valRec?.ruff_diagnostics)
        ? valRec.ruff_diagnostics.length
        : undefined;
      const rootStillCurrent =
        this.client.generation === startGen && this.rootStateGeneration === startRootGen;
      const currentGenRoot = rootStillCurrent ? root : null;
      this.onUsage?.({
        operation: definition.operation,
        root: currentGenRoot,
        durationMs: result.trace.total_ms,
        savedMs,
        cachedFiles,
        executedFiles,
        ...(passed !== undefined ? { passed } : {}),
        ...(failed !== undefined ? { failed } : {}),
        ...(skipped !== undefined ? { skipped } : {}),
        ...(mode !== undefined ? { mode } : {}),
        ...(ruffIssues !== undefined ? { ruffIssues } : {}),
      });
      this.output.appendLine(
        `[success] operation=${definition.operation} root=${root ?? '-'} duration=${result.trace.total_ms.toFixed(3)}ms trace=${traceId}`,
      );
      if (definition.operation === 'test_sync') {
        this.registry.invalidate();
      }
      return result;
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : String(error);
      // Never publish a root from a superseded settings or workspace-folder generation back into
      // UsageSummary (which would restore a `lastRoot` that folder/config listeners just cleared).
      const errorRoot =
        error instanceof ConnectionResetError ||
        this.client.generation !== startGen ||
        this.rootStateGeneration !== startRootGen
          ? null
          : root;
      this.onUsage?.({ operation: definition.operation, root: errorRoot, error: message });
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
    if (trace.success && trace.daemon !== null && trace.daemon !== undefined) {
      return;
    }
    void this.client.request(request(null, 'test_trace', trace), token).catch((error: unknown) => {
      this.output.appendLine(
        `[warning] trace_report_failed id=${trace.trace_id} message=${String(error)}`,
      );
    });
  }

  public async queryDaemonStatus(
    root: string | null,
    token: vscode.CancellationToken,
  ): Promise<JsonValue> {
    return this.client.request<JsonValue>(
      request(root, 'test_status', { load: false }),
      token,
    );
  }

  public async listWorkspaces(token: vscode.CancellationToken): Promise<WorkspaceSpec[]> {
    ensureTrustedWorkspace();
    return this.registry.list(token, true);
  }

  public async resolveRoot(
    explicit: string | undefined,
    token: vscode.CancellationToken,
    allowPreparation = true,
    inputHints: readonly string[] = [],
    expectedGen?: number,
    expectedRootGen?: number,
  ): Promise<string> {
    const resolved = await this.resolveRootInner(
      explicit,
      token,
      allowPreparation,
      inputHints,
      expectedGen,
      expectedRootGen,
    );
    // Validate EVERY absolute target against the final selected root (even when `root` was
    // passed explicitly) so a cross-workspace target is never silently dropped by the daemon.
    assertTargetsInsideRoot(
      resolved,
      inputHints,
      this.client.usesWslBridge,
      this.client.wslDistribution,
    );
    return resolved;
  }

  private async resolveRootInner(
    explicit: string | undefined,
    token: vscode.CancellationToken,
    allowPreparation = true,
    inputHints: readonly string[] = [],
    expectedGen?: number,
    expectedRootGen?: number,
  ): Promise<string> {
    ensureTrustedWorkspace();
    const resolveGen = expectedGen ?? this.client.generation;
    const resolveRootGen = expectedRootGen ?? this.rootStateGeneration;
    const assertGen = (): void => {
      if (this.client.generation !== resolveGen) {
        throw new ConnectionResetError(
          'Test Context settings changed during workspace resolution; retry request.',
        );
      }
      if (this.rootStateGeneration !== resolveRootGen) {
        throw new ConnectionResetError(
          'Workspace folders changed during workspace resolution; retry request.',
        );
      }
    };
    const guardedRegisterMissingRoot =
      allowPreparation && this.registerMissingRoot
        ? async (pathToReg: string): Promise<void> => {
            if (token.isCancellationRequested) {
              throw new RequestCancelledError();
            }
            assertGen();
            await this.registerMissingRoot?.(pathToReg, token);
            if (token.isCancellationRequested) {
              throw new RequestCancelledError();
            }
            assertGen();
          }
        : undefined;
    const relativeTargets = inputHints.filter(
      (h) => !h.startsWith('/') && !h.startsWith('\\\\') && !/^[a-zA-Z]:[\\/]/u.test(h) && !/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//u.test(h),
    );
    const absoluteInputHints = inputHints.filter((h) => !relativeTargets.includes(h));
    // Only reuse `lastResolvedRoot` when its WSL distribution matches the current configuration
    // so switching from Debian to Ubuntu never injects an unqualified root from the old distro.
    const effectiveStickyRoot =
      this.lastResolvedDistro === this.client.wslDistribution ? this.lastResolvedRoot : undefined;
    const folderCandidates = (vscode.workspace.workspaceFolders ?? []).flatMap((f) =>
      uriCandidatePaths(f.uri),
    );
    // When explicit absolute target hints are present without an explicit `root`, do NOT fall back
    // to unrelated ambient open workspaces/tabs (which would silently route an unregistered target
    // in B to an unrelated registered workspace A instead of auto-registering B).
    const candidates =
      absoluteInputHints.length > 0 && explicit === undefined
        ? absoluteInputHints
        : workspaceCandidates(
            inputHints,
            relativeTargets.length === 0 ? effectiveStickyRoot : undefined,
          );
    const workspaces = await this.registry.list(token);
    if (this.client.usesWslBridge) {
      const distro = this.client.wslDistribution;
      const filterDistro = (c: string): boolean => {
        if (!distro) {
          return true;
        }
        const d = extractWslUncDistribution(c);
        return !d || d.toLowerCase() === distro.toLowerCase();
      };
      const wslFolderPosixCandidates = folderCandidates
        .filter(filterDistro)
        .map((c) => toPosixPathIfPossible(c, distro))
        .filter((p): p is string => Boolean(p));
      const ensureRegisteredWslRoot = async (
        currentWorkspaces: readonly WorkspaceSpec[],
        candidateRoot: string,
      ): Promise<string> => {
        if (!guardedRegisterMissingRoot) {
          return candidateRoot;
        }
        const registered = currentWorkspaces.some(
          (w) => candidateRoot === w.root || candidateRoot.startsWith(`${w.root}/`),
        );
        if (registered) {
          return candidateRoot;
        }
        const dirToRegister = resolveWslDirForRegistration(
          candidateRoot,
          wslFolderPosixCandidates,
          distro,
        );
        await guardedRegisterMissingRoot(dirToRegister);
        const reloaded = await this.registry.list(token, true);
        assertGen();
        return resolveWslRoot(reloaded, dirToRegister, candidates, relativeTargets, distro);
      };
      if (
        explicit === undefined &&
        (absoluteInputHints.length > 0 || wslFolderPosixCandidates.length === 0)
      ) {
        try {
          const commonWslRoot = findCommonRootForInputHints(workspaces, inputHints, true, distro);
          if (commonWslRoot) {
            return await ensureRegisteredWslRoot(workspaces, commonWslRoot);
          }
        } catch (conflictErr: unknown) {
          if (conflictErr instanceof RootConflictError) {
            // Refresh the registry ONCE before rejecting so a newly registered parent workspace
            // (e.g. `/repo` registered after `/repo/a` and `/repo/b`) resolves cleanly!
            const refreshedConflict = await this.registry.list(token, true);
            assertGen();
            const retriedWsl = findCommonRootForInputHints(
              refreshedConflict,
              inputHints,
              true,
              distro,
            );
            if (retriedWsl) {
              return await ensureRegisteredWslRoot(refreshedConflict, retriedWsl);
            }
          }
          throw conflictErr;
        }
      }
      try {
        const resolvedWsl = resolveWslRoot(
          workspaces,
          explicit,
          candidates,
          relativeTargets,
          distro,
          wslFolderPosixCandidates,
        );
        return await ensureRegisteredWslRoot(workspaces, resolvedWsl);
      } catch (error: unknown) {
        if (error instanceof RootConflictError || error instanceof ConnectionResetError) {
          throw error;
        }
        if (!(error instanceof Error) || error.name !== 'RootResolutionError') {
          throw error;
        }
        const refreshed = await this.registry.list(token, true);
        assertGen();
        if (
          explicit === undefined &&
          (absoluteInputHints.length > 0 || wslFolderPosixCandidates.length === 0)
        ) {
          const commonRefreshedWsl = findCommonRootForInputHints(
            refreshed,
            inputHints,
            true,
            distro,
          );
          if (commonRefreshedWsl) {
            return await ensureRegisteredWslRoot(refreshed, commonRefreshedWsl);
          }
        }
        try {
          const resolvedRefreshed = resolveWslRoot(
            refreshed,
            explicit,
            candidates,
            relativeTargets,
            distro,
            wslFolderPosixCandidates,
          );
          return await ensureRegisteredWslRoot(refreshed, resolvedRefreshed);
        } catch (refreshError: unknown) {
          if (
            refreshError instanceof RootConflictError ||
            refreshError instanceof ConnectionResetError
          ) {
            throw refreshError;
          }
          // When WSL registry is empty or missing a folder/target, invoke auto-registration
          // resolving any file path to its containing workspace folder or parent directory.
          if (guardedRegisterMissingRoot) {
            const suggestedWslDir =
              refreshError instanceof RootResolutionError
                ? refreshError.suggestedRegistrationDir
                : undefined;
            const rawWslTarget =
              suggestedWslDir ??
              (explicit ? toPosixPathIfPossible(explicit, distro) : undefined) ??
              (absoluteInputHints[0]
                ? toPosixPathIfPossible(absoluteInputHints[0], distro)
                : undefined) ??
              wslFolderPosixCandidates[0];
            const toRegister = rawWslTarget
              ? resolveWslDirForRegistration(rawWslTarget, wslFolderPosixCandidates, distro)
              : undefined;
            if (toRegister) {
              await guardedRegisterMissingRoot(toRegister);
              const postReg = await this.registry.list(token, true);
              assertGen();
              if (
                explicit === undefined &&
                (absoluteInputHints.length > 0 || wslFolderPosixCandidates.length === 0)
              ) {
                const commonPostWsl = findCommonRootForInputHints(
                  postReg,
                  inputHints,
                  true,
                  distro,
                );
                if (commonPostWsl) {
                  return commonPostWsl;
                }
              }
              return resolveWslRoot(
                postReg,
                explicit,
                candidates,
                relativeTargets,
                distro,
                wslFolderPosixCandidates,
              );
            }
          }
          if (
            refreshError instanceof RootResolutionError &&
            effectiveStickyRoot &&
            explicit === undefined &&
            absoluteInputHints.length === 0
          ) {
            const stickyPosix = toPosixPathIfPossible(effectiveStickyRoot, distro);
            if (stickyPosix) {
              return stickyPosix;
            }
          }
          throw refreshError;
        }
      }
    }
    if (explicit === undefined && (absoluteInputHints.length > 0 || folderCandidates.length === 0)) {
      try {
        const commonRoot = findCommonRootForInputHints(workspaces, inputHints, false);
        if (commonRoot) {
          return commonRoot;
        }
      } catch (conflictErr: unknown) {
        if (conflictErr instanceof RootConflictError) {
          // Refresh the registry ONCE before rejecting so a newly registered parent workspace
          // (e.g. `/repo` registered after `/repo/a` and `/repo/b`) resolves cleanly!
          const refreshedConflict = await this.registry.list(token, true);
          assertGen();
          const retried = findCommonRootForInputHints(refreshedConflict, inputHints, false);
          if (retried) {
            return retried;
          }
        }
        throw conflictErr;
      }
    }
    try {
      return await resolveRegisteredRootAfterRefresh(
        workspaces,
        () => this.registry.list(token, true),
        explicit,
        candidates,
        guardedRegisterMissingRoot,
        relativeTargets,
        folderCandidates,
        inputHints,
      );
    } catch (error: unknown) {
      if (error instanceof RootConflictError || error instanceof ConnectionResetError) {
        throw error;
      }
      if (
        error instanceof RootResolutionError &&
        relativeTargets.length > 0 &&
        absoluteInputHints.length === 0 &&
        effectiveStickyRoot &&
        explicit === undefined
      ) {
        return await resolveRegisteredRootAfterRefresh(
          await this.registry.list(token),
          () => this.registry.list(token, true),
          explicit,
          [...candidates, effectiveStickyRoot],
          guardedRegisterMissingRoot,
          relativeTargets,
          folderCandidates,
          inputHints,
        );
      }
      throw error;
    }
  }

  public dispose(): void {
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.disposables.length = 0;
  }

  public refreshRegistry(): void {
    // Refresh cached workspace list without invalidating workspace-folder generation or sticky
    // root (used after in-flight auto-registration completes for the current invocation).
    this.registry.invalidate();
  }

  public invalidateRegistry(): void {
    this.rootStateGeneration += 1;
    this.lastResolvedRoot = undefined;
    this.lastResolvedDistro = undefined;
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
    const parts = [new vscode.LanguageModelTextPart(text)];
    if (process.env.TCTX_INCLUDE_TRACE === '1') {
      parts.push(new vscode.LanguageModelTextPart(`TCTX_TRACE ${formatTraceLog(result.trace)}`));
    }
    return new vscode.LanguageModelToolResult(parts);
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
  arguments: (input, root, wslDistro) => buildRunArguments(input, root, wslDistro),
};

const testAffectedDefinition: ToolDefinition<TestAffectedInput> = {
  name: 'test-context-native_testAffected',
  operation: 'test_affected',
  needsRoot: true,
  invocationMessage: () => 'Finding affected pytest tests via AST reverse dependency graph',
  arguments: (input, root, wslDistro) => buildAffectedArguments(input, root, wslDistro),
};

const testGraphDefinition: ToolDefinition<TestGraphInput> = {
  name: 'test-context-native_testGraph',
  operation: 'test_graph',
  needsRoot: true,
  invocationMessage: (input) => `Inspecting Python AST dependency graph for ${input.file}`,
  arguments: (input, root, wslDistro) => buildGraphArguments(input, root, wslDistro),
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
  arguments: (input, root, wslDistro) => buildInvalidateArguments(input, root, wslDistro),
};

const testWorkspacesDefinition: ToolDefinition<EmptyInput> = {
  name: 'test-context-native_testWorkspaces',
  operation: 'test_workspaces',
  needsRoot: false,
  invocationMessage: () => 'Listing registered Test Context workspaces',
  arguments: () => ({}),
};

function ensureTrustedWorkspace(): void {
  if (!vscode.workspace.isTrusted) {
    throw new Error('Trust this workspace before using Test Context native tools.');
  }
}

