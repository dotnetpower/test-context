import { isAbsolute, relative, resolve, sep, posix } from 'node:path';

import type { WorkspaceSpec } from './types.js';

export class RootResolutionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'RootResolutionError';
  }
}

export function resolveWslRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
): string {
  const target = explicitPath ?? (workspaces.length === 1 ? workspaces[0]?.root : undefined);
  if (!target) {
    throw new RootResolutionError(
      'Windows Agent Sessions require an explicit absolute Linux root when multiple roots are registered.',
    );
  }
  if (!posix.isAbsolute(target) || /[\\\0\r\n]/u.test(target)) {
    throw new RootResolutionError('WSL Test Context root must be an absolute Linux path.');
  }
  return posix.normalize(target);
}

export function resolveRegisteredRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[],
): string {
  const roots = workspaces.map((workspace) => resolve(workspace.root));
  if (explicitPath !== undefined && explicitPath.trim().length > 0) {
    if (!isAbsolute(explicitPath)) {
      throw new RootResolutionError('Test Context root must be an absolute path.');
    }
    const match = bestContainingRoot(roots, resolve(explicitPath));
    if (!match) {
      throw new RootResolutionError(
        `No registered Test Context workspace contains ${explicitPath}. Run tctx register first.`,
      );
    }
    return match;
  }
  for (const candidate of candidates) {
    const match = bestContainingRoot(roots, resolve(candidate));
    if (match) {
      return match;
    }
  }
  if (roots.length === 1) {
    return roots[0] as string;
  }
  if (roots.length === 0) {
    throw new RootResolutionError(
      'No Test Context workspaces are registered. Register the current workspace first.',
    );
  }
  throw new RootResolutionError(
    'The current VS Code workspace is not registered with Test Context.',
  );
}

export async function resolveRegisteredRootAfterRefresh(
  workspaces: readonly WorkspaceSpec[],
  refresh: () => Promise<readonly WorkspaceSpec[]>,
  explicitPath: string | undefined,
  candidates: readonly string[],
  registerMissingRoot?: (path: string) => Promise<void>,
): Promise<string> {
  try {
    return resolveRegisteredRoot(workspaces, explicitPath, candidates);
  } catch (error: unknown) {
    if (!(error instanceof RootResolutionError)) {
      throw error;
    }
    const refreshed = await refresh();
    try {
      return resolveRegisteredRoot(refreshed, explicitPath, candidates);
    } catch (refreshedError: unknown) {
      if (!(refreshedError instanceof RootResolutionError) || !registerMissingRoot) {
        throw refreshedError;
      }
      const target = explicitPath ?? candidates[0];
      if (!target || !isAbsolute(target)) {
        throw refreshedError;
      }
      await registerMissingRoot(target);
      return resolveRegisteredRoot(await refresh(), explicitPath, candidates);
    }
  }
}

function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => contains(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

function contains(root: string, candidate: string): boolean {
  const child = relative(root, candidate);
  return child === '' || (!child.startsWith(`..${sep}`) && child !== '..' && !isAbsolute(child));
}
