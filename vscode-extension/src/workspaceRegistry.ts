import { resolve } from 'node:path';

import * as vscode from 'vscode';

import { RequestCancelledError, type DaemonClient } from './daemonClient.js';
import { toPosixPathIfPossible } from './rootResolver.js';
import type { DaemonRequest, JsonValue, WorkspaceSpec } from './types.js';

const REGISTRY_CACHE_MS = 15_000;

export function buildDaemonRequest(
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

interface InFlightFetch {
  promise: Promise<WorkspaceSpec[]>;
  cancel: () => void;
  waiters: number;
}

export class WorkspaceRegistry {
  private cached: WorkspaceSpec[] | undefined;
  private cachedAt = 0;
  private inFlight: InFlightFetch | undefined;
  private generation = 0;
  private readonly genWaiters = new Set<() => void>();

  public constructor(private readonly client: DaemonClient) {}

  private bumpGeneration(): void {
    this.generation += 1;
    // Cancel any superseded in-flight fetch so a replacement refresh is never blocked behind a
    // stalled fetch in the control lane.
    const oldFetch = this.inFlight;
    this.inFlight = undefined;
    oldFetch?.cancel();
    for (const wake of [...this.genWaiters]) {
      wake();
    }
    this.genWaiters.clear();
  }

  public async list(
    token: vscode.CancellationToken,
    refresh = false,
  ): Promise<WorkspaceSpec[]> {
    let forceRefresh = refresh;
    for (let attempt = 0; attempt < 16; attempt += 1) {
      if (token.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      if (forceRefresh) {
        this.bumpGeneration();
        this.cached = undefined;
        this.cachedAt = 0;
        forceRefresh = false;
      } else if (this.cached && Date.now() - this.cachedAt < REGISTRY_CACHE_MS) {
        return this.cached;
      }
      const startGen = this.generation;
      let fetchState = this.inFlight;
      if (!fetchState) {
        const fetchGen = startGen;
        // Decouple the shared in-flight fetch from any individual caller's CancellationToken so
        // cancelling caller A never fails caller B, while still cancelling when superseded or
        // when the last waiter leaves.
        const listeners = new Set<() => void>();
        let fetchCancelled = false;
        const sharedToken = {
          get isCancellationRequested(): boolean {
            return fetchCancelled;
          },
          onCancellationRequested: (listener: () => void): { dispose: () => void } => {
            listeners.add(listener);
            return { dispose: (): void => { listeners.delete(listener); } };
          },
        };
        const cancelThisFetch = (): void => {
          if (fetchCancelled) {
            return;
          }
          fetchCancelled = true;
          for (const listener of [...listeners]) {
            listener();
          }
          listeners.clear();
        };
        const created: InFlightFetch = {
          promise: Promise.resolve([]),
          cancel: cancelThisFetch,
          waiters: 0,
        };
        const fetchPromise = (async (): Promise<WorkspaceSpec[]> => {
          const result = await this.client.request<unknown>(
            buildDaemonRequest(null, 'test_workspaces', {}),
            sharedToken,
          );
          if (!Array.isArray(result)) {
            throw new Error('Test Context daemon returned an invalid workspace registry.');
          }
          const workspaces = result.map(parseWorkspaceSpec);
          // Only publish into cache if no invalidation or newer refresh superseded this fetch.
          if (this.generation === fetchGen) {
            this.cached = workspaces;
            this.cachedAt = Date.now();
          }
          return workspaces;
        })().finally(() => {
          if (this.inFlight === created) {
            this.inFlight = undefined;
          }
        });
        // Prevent unhandled rejection if every waiter cancels before `fetchPromise` rejects
        fetchPromise.catch(() => undefined);
        created.promise = fetchPromise;
        this.inFlight = created;
        fetchState = created;
      }
      let cancelSub: vscode.Disposable | undefined;
      let wakeGen: (() => void) | undefined;
      let superseded = false;
      let result: WorkspaceSpec[] | undefined;
      fetchState.waiters += 1;
      try {
        result = await Promise.race([
          fetchState.promise,
          new Promise<never>((_resolve, reject) => {
            cancelSub = token.onCancellationRequested(() => {
              reject(new RequestCancelledError());
            });
          }),
          new Promise<undefined>((resolve) => {
            wakeGen = (): void => {
              superseded = true;
              resolve(undefined);
            };
            this.genWaiters.add(wakeGen);
          }),
        ]);
      } catch (err: unknown) {
        if (err instanceof RequestCancelledError) {
          throw err;
        }
        // If an invalidated/superseded fetch rejected, retry against the newer generation.
        if (this.generation !== startGen) {
          continue;
        }
        throw err;
      } finally {
        cancelSub?.dispose();
        if (wakeGen) {
          this.genWaiters.delete(wakeGen);
        }
        fetchState.waiters = Math.max(0, fetchState.waiters - 1);
        // If every waiter has left (e.g. cancelled) before this specific fetch settled, cancel
        // and discard it so later callers never join an orphaned stalled fetch.
        if (fetchState.waiters === 0 && this.inFlight === fetchState) {
          this.inFlight = undefined;
          fetchState.cancel();
        }
      }
      // If an invalidation or newer refresh occurred while awaiting, retry immediately without
      // waiting for the stalled older fetch to finish.
      if (!superseded && this.generation === startGen && result !== undefined) {
        return result;
      }
    }
    throw new Error('Test Context workspace registry changed concurrently; retry request.');
  }

  public invalidate(): void {
    this.bumpGeneration();
    this.cached = undefined;
    this.cachedAt = 0;
    this.inFlight = undefined;
  }
}


export function uriCandidatePaths(uri: vscode.Uri): string[] {
  const out: string[] = [];
  // Preserve `vscode-remote://wsl+<distro>/...` authority so distribution filtering can
  // distinguish e.g. Debian from Ubuntu instead of losing the distro in bare `uri.path`.
  if (uri.scheme === 'vscode-remote' && uri.authority.toLowerCase().startsWith('wsl+')) {
    out.push(uri.toString());
    return out;
  }
  // Preserve distro-qualified WSL UNC `file://wsl.localhost/<distro>/...` URIs without also
  // emitting an unqualified `uri.path` that would bypass WSL distribution validation.
  if (
    uri.scheme === 'file' &&
    (uri.authority.toLowerCase() === 'wsl.localhost' ||
      uri.authority.toLowerCase() === 'wsl$' ||
      (uri.fsPath && /^\\\\(?:wsl\.localhost|wsl\$)\\/iu.test(uri.fsPath)))
  ) {
    out.push(uri.fsPath || uri.toString());
    return out;
  }
  if (uri.scheme === 'file' && uri.fsPath) {
    out.push(resolve(uri.fsPath));
  }
  if (uri.path && uri.path.startsWith('/')) {
    out.push(uri.path);
  }
  return out;
}

export function workspaceCandidates(
  inputHints: readonly string[] = [],
  lastResolvedRoot?: string,
): string[] {
  const candidates: string[] = [];
  const seen = new Set<string>();
  const pushUnique = (p: string): void => {
    if (p && !seen.has(p)) {
      seen.add(p);
      candidates.push(p);
    }
  };
  for (const hint of inputHints) {
    // Classify using both raw prefix and normalized POSIX/WSL URI form while retaining the raw
    // string so `vscode-remote://wsl+Ubuntu/home/A/...` is kept as a candidate with its distro.
    if (
      hint.startsWith('/') ||
      hint.startsWith('\\\\') ||
      /^[a-zA-Z]:[\\/]/u.test(hint) ||
      toPosixPathIfPossible(hint) !== undefined
    ) {
      pushUnique(hint);
    }
  }
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  if (activeUri) {
    const folder = vscode.workspace.getWorkspaceFolder(activeUri);
    if (folder) {
      for (const p of uriCandidatePaths(folder.uri)) {
        pushUnique(p);
      }
    }
    for (const p of uriCandidatePaths(activeUri)) {
      pushUnique(p);
    }
  }
  for (const editor of vscode.window.visibleTextEditors ?? []) {
    const folder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
    if (folder) {
      for (const p of uriCandidatePaths(folder.uri)) {
        pushUnique(p);
      }
    }
    for (const p of uriCandidatePaths(editor.document.uri)) {
      pushUnique(p);
    }
  }
  // Prioritize open workspace folders BEFORE inactive tab groups so an old open tab from
  // another repository never overrides a newly opened workspace when no editor is active!
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    for (const p of uriCandidatePaths(folder.uri)) {
      pushUnique(p);
    }
  }
  for (const group of vscode.window.tabGroups?.all ?? []) {
    for (const tab of group.tabs) {
      const input = tab.input as { uri?: vscode.Uri } | undefined;
      if (input?.uri) {
        for (const p of uriCandidatePaths(input.uri)) {
          pushUnique(p);
        }
      }
    }
  }
  if (lastResolvedRoot) {
    pushUnique(lastResolvedRoot);
  }
  return candidates;
}

export function parseWorkspaceSpec(value: unknown): WorkspaceSpec {
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
