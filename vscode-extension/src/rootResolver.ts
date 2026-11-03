import { existsSync } from 'node:fs';
import { basename, isAbsolute, join, relative, resolve, sep, posix } from 'node:path';

import type { JsonValue, WorkspaceSpec } from './types.js';

const WSL_UNC_RE = /^\\\\(?:wsl\.localhost|wsl\$)\\[^\\]+(\\.+)$/iu;
const WSL_SLASH_UNC_RE = /^\/\/(?:wsl\.localhost|wsl\$)\/[^/]+(\/.*)$/iu;
const WSL_FILE_URI_RE = /^file:\/\/(?:wsl\.localhost|wsl\$)\/[^/]+(\/.*)$/iu;
const REMOTE_URI_RE =
  /^(?:vscode-remote|vscode-file|vscode-userdata|vscode-insiders|cursor|windsurf|vscodium|git):\/\/[^/]+(\/.*)$/iu;
const MD_LINK_RE = /^\[[^\]]*\]\((.+)\)$/u;
const COLON_LINE_RE = /^(.+?\.[^/:]+):(\d+)(?::\d+)?(?:-(\d+)(?::\d+)?)?(?::\s+.*|\s+.*)?$/u;

export class RootResolutionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'RootResolutionError';
  }
}

export function cleanTargetString(rawPath: string): string {
  let s = rawPath.trim();
  while (
    s.length >= 2 &&
    ((s.startsWith('`') && s.endsWith('`')) ||
      (s.startsWith('"') && s.endsWith('"')) ||
      (s.startsWith("'") && s.endsWith("'")))
  ) {
    s = s.slice(1, -1).trim();
  }
  const mdMatch = MD_LINK_RE.exec(s);
  if (mdMatch?.[1]) {
    s = mdMatch[1].trim();
    if (s.startsWith('<') && s.endsWith('>') && s.length >= 2) {
      s = s.slice(1, -1).trim();
    }
  }
  const fileWslMatch = WSL_FILE_URI_RE.exec(s);
  if (fileWslMatch?.[1]) {
    try {
      return decodeURIComponent(fileWslMatch[1]);
    } catch {
      return fileWslMatch[1];
    }
  }
  if (s.startsWith('file://')) {
    s = s.slice('file://'.length);
    try {
      s = decodeURIComponent(s);
    } catch {
      // keep raw
    }
  } else {
    const remoteMatch = REMOTE_URI_RE.exec(s);
    if (remoteMatch?.[1]) {
      try {
        s = decodeURIComponent(remoteMatch[1]);
      } catch {
        s = remoteMatch[1];
      }
    }
  }
  const uncMatch = WSL_UNC_RE.exec(s);
  if (uncMatch?.[1]) {
    return uncMatch[1].replace(/\\/gu, '/');
  }
  const slashUncMatch = WSL_SLASH_UNC_RE.exec(s.replace(/\\/gu, '/'));
  if (slashUncMatch?.[1]) {
    return slashUncMatch[1];
  }
  return s;
}

export function stripLineAndNodeSuffix(rawPath: string): string {
  const cleaned = cleanTargetString(rawPath);
  const withoutNode = cleaned.split('::')[0]?.trim() ?? '';
  if (!withoutNode) {
    return '';
  }
  const hashIdx = withoutNode.lastIndexOf('#');
  const withoutFrag = hashIdx > 0 ? withoutNode.slice(0, hashIdx) : withoutNode;
  const colonMatch = COLON_LINE_RE.exec(withoutFrag);
  if (colonMatch?.[1] && !/^[a-zA-Z]:$/u.test(colonMatch[1])) {
    return colonMatch[1];
  }
  return withoutFrag;
}

export function normalizeTargetForRoot(
  root: string | null | undefined,
  rawPath: string,
): string {
  const cleaned = cleanTargetString(rawPath);
  const doubleColonIdx = cleaned.indexOf('::');
  const rawFile =
    doubleColonIdx >= 0 ? cleaned.slice(0, doubleColonIdx).trim() : cleaned.trim();
  const nodePart =
    doubleColonIdx >= 0 ? cleaned.slice(doubleColonIdx + 2).trim() : undefined;
  const filePart = stripLineAndNodeSuffix(rawFile).replace(/\\/gu, '/');
  let relFile = filePart.replace(/^(?:\.\/)+/u, '');
  if (root) {
    const rootTrimmed = root.replace(/\\/gu, '/').replace(/\/+$/u, '');
    if (filePart === rootTrimmed) {
      relFile = '.';
    } else if (filePart.startsWith(`${rootTrimmed}/`)) {
      relFile = filePart.slice(rootTrimmed.length + 1).replace(/^(?:\.\/)+/u, '');
    } else if (!relFile.startsWith('/')) {
      const rootName = posix.basename(rootTrimmed);
      if (rootName && relFile.startsWith(`${rootName}/`)) {
        const strippedBase = relFile.slice(rootName.length + 1);
        if (
          strippedBase &&
          (existsSync(join(root, strippedBase)) || !existsSync(join(root, relFile)))
        ) {
          relFile = strippedBase;
        }
      }
    }
  }
  return nodePart ? `${relFile}::${nodePart}` : relFile;
}

export function toPosixPathIfPossible(rawPath: string): string | undefined {
  const trimmed = stripLineAndNodeSuffix(rawPath);
  if (!trimmed || /[\0\r\n]/u.test(trimmed)) {
    return undefined;
  }
  if (posix.isAbsolute(trimmed) && !trimmed.includes('\\')) {
    return posix.normalize(trimmed);
  }
  return undefined;
}

export function disambiguateRootByRelativePaths(
  roots: readonly string[],
  relativeTargets: readonly string[],
  usePosix = false,
): string | undefined {
  if (roots.length <= 1 || relativeTargets.length === 0) {
    return undefined;
  }
  for (const rawTarget of relativeTargets) {
    const cleaned = stripLineAndNodeSuffix(rawTarget)
      .replace(/\\/gu, '/')
      .replace(/^(?:\.\/)+/u, '')
      .replace(/\/+$/u, '');
    if (!cleaned || cleaned.startsWith('/') || cleaned.startsWith('..')) {
      continue;
    }
    const firstSegment = cleaned.split('/')[0];
    if (firstSegment && cleaned.includes('/')) {
      const rest = cleaned.slice(firstSegment.length + 1);
      const basenameMatches = roots.filter(
        (root) => (usePosix ? posix.basename(root) : basename(root)) === firstSegment,
      );
      if (basenameMatches.length === 1 && basenameMatches[0] !== undefined) {
        const matchedRoot = basenameMatches[0];
        if (!rest || existsSync(usePosix ? posix.join(matchedRoot, rest) : join(matchedRoot, rest))) {
          return matchedRoot;
        }
      }
    }
    const existingMatches = roots.filter((root) =>
      existsSync(usePosix ? posix.join(root, cleaned) : join(root, cleaned)),
    );
    if (existingMatches.length === 1 && existingMatches[0] !== undefined) {
      return existingMatches[0];
    }
  }
  return undefined;
}

export function resolveWslRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[] = [],
  relativeTargets: readonly string[] = [],
): string {
  const posixRoots = workspaces
    .map((w) => w.root)
    .filter((r) => posix.isAbsolute(r))
    .map((r) => posix.normalize(r));

  if (explicitPath !== undefined && explicitPath.trim().length > 0) {
    const cleanedExplicit = stripLineAndNodeSuffix(explicitPath);
    if (
      cleanedExplicit &&
      !posix.isAbsolute(cleanedExplicit) &&
      !/[\\/:\0\r\n]/u.test(cleanedExplicit)
    ) {
      const byName = posixRoots.filter((r) => posix.basename(r) === cleanedExplicit);
      if (byName.length === 1 && byName[0] !== undefined) {
        return byName[0];
      }
    }
    const uncConverted = toPosixPathIfPossible(explicitPath);
    const rawTarget = uncConverted ?? cleanedExplicit;
    if (!posix.isAbsolute(rawTarget) || /[\\\0\r\n]/u.test(rawTarget)) {
      throw new RootResolutionError('WSL Test Context root must be an absolute Linux path.');
    }
    const normalized = posix.normalize(rawTarget);
    return bestContainingPosixRoot(posixRoots, normalized) ?? normalized;
  }

  for (const candidate of candidates) {
    const posixCand = toPosixPathIfPossible(candidate);
    if (posixCand) {
      const match = bestContainingPosixRoot(posixRoots, posixCand);
      if (match) {
        return match;
      }
    }
  }

  const byRelative = disambiguateRootByRelativePaths(posixRoots, relativeTargets, true);
  if (byRelative) {
    return byRelative;
  }

  if (workspaces.length === 1 && workspaces[0]?.root) {
    return posix.normalize(workspaces[0].root);
  }

  const listed = posixRoots.slice(0, 5).join(', ');
  const hint = listed ? ` Registered roots: ${listed}` : '';
  throw new RootResolutionError(
    `Windows Agent Sessions require an explicit absolute Linux root when multiple roots are registered.${hint}`,
  );
}

export function resolveRegisteredRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[],
  relativeTargets: readonly string[] = [],
): string {
  const roots = workspaces.map((workspace) => resolve(workspace.root));
  if (explicitPath !== undefined && explicitPath.trim().length > 0) {
    const cleanedExplicit = stripLineAndNodeSuffix(explicitPath);
    if (
      cleanedExplicit &&
      !isAbsolute(cleanedExplicit) &&
      !/[\\/:\0\r\n]/u.test(cleanedExplicit)
    ) {
      const byName = roots.filter((root) => basename(root) === cleanedExplicit);
      if (byName.length === 1 && byName[0] !== undefined) {
        return byName[0];
      }
      const lowerExplicit = cleanedExplicit.toLowerCase();
      const byCiName = roots.filter(
        (root) => basename(root).toLowerCase() === lowerExplicit,
      );
      if (byCiName.length === 1 && byCiName[0] !== undefined) {
        return byCiName[0];
      }
    }
    if (!isAbsolute(cleanedExplicit)) {
      throw new RootResolutionError('Test Context root must be an absolute path.');
    }
    const match = bestContainingRoot(roots, resolve(cleanedExplicit));
    if (!match) {
      throw new RootResolutionError(
        `No registered Test Context workspace contains ${cleanedExplicit}. Run tctx register first.`,
      );
    }
    return match;
  }
  for (const candidate of candidates) {
    const cleanedCand = stripLineAndNodeSuffix(candidate);
    if (cleanedCand && isAbsolute(cleanedCand)) {
      const match = bestContainingRoot(roots, resolve(cleanedCand));
      if (match) {
        return match;
      }
    }
  }
  const byRelative = disambiguateRootByRelativePaths(roots, relativeTargets, false);
  if (byRelative) {
    return byRelative;
  }
  if (roots.length === 1) {
    return roots[0] as string;
  }
  if (roots.length === 0) {
    throw new RootResolutionError(
      'No Test Context workspaces are registered. Register the current workspace first.',
    );
  }
  const listed = roots.slice(0, 5).join(', ');
  throw new RootResolutionError(
    `The current VS Code workspace is not registered with Test Context. Registered roots: ${listed}`,
  );
}

export async function resolveRegisteredRootAfterRefresh(
  workspaces: readonly WorkspaceSpec[],
  refresh: () => Promise<readonly WorkspaceSpec[]>,
  explicitPath: string | undefined,
  candidates: readonly string[],
  registerMissingRoot?: (path: string) => Promise<void>,
  relativeTargets: readonly string[] = [],
): Promise<string> {
  try {
    return resolveRegisteredRoot(workspaces, explicitPath, candidates, relativeTargets);
  } catch (error: unknown) {
    if (!(error instanceof RootResolutionError)) {
      throw error;
    }
    const refreshed = await refresh();
    try {
      return resolveRegisteredRoot(refreshed, explicitPath, candidates, relativeTargets);
    } catch (refreshedError: unknown) {
      if (!(refreshedError instanceof RootResolutionError) || !registerMissingRoot) {
        throw refreshedError;
      }
      const rawTarget = explicitPath ?? candidates[0];
      const target = rawTarget ? stripLineAndNodeSuffix(rawTarget) : undefined;
      if (!target || !isAbsolute(target)) {
        throw refreshedError;
      }
      await registerMissingRoot(target);
      return resolveRegisteredRoot(
        await refresh(),
        explicitPath,
        candidates,
        relativeTargets,
      );
    }
  }
}

function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => contains(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

function bestContainingPosixRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => containsPosix(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

function containsPosix(root: string, candidate: string): boolean {
  const child = posix.relative(root, candidate);
  return child === '' || (!child.startsWith('../') && child !== '..' && !posix.isAbsolute(child));
}

function contains(root: string, candidate: string): boolean {
  const child = relative(root, candidate);
  return child === '' || (!child.startsWith(`..${sep}`) && child !== '..' && !isAbsolute(child));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function extractStringArray(value: unknown): string[] {
  if (Array.isArray(value)) {
    return value
      .filter((item): item is string => typeof item === 'string')
      .map((item) => item.trim())
      .filter((item) => item.length > 0);
  }
  if (typeof value === 'string') {
    const trimmed = value.trim();
    if (!trimmed) {
      return [];
    }
    if (trimmed.includes(',') && !trimmed.startsWith('[')) {
      return trimmed
        .split(',')
        .map((part) => part.trim())
        .filter((part) => part.length > 0);
    }
    return [trimmed];
  }
  return [];
}

function collectAliasStrings(
  rec: Record<string, unknown>,
  keys: readonly string[],
): string[] {
  const out: string[] = [];
  for (const key of keys) {
    for (const item of extractStringArray(rec[key])) {
      if (!out.includes(item)) {
        out.push(item);
      }
    }
  }
  return out;
}

export function extractRawRootInput(input: unknown): string | undefined {
  if (!isRecord(input)) {
    return undefined;
  }
  for (const key of [
    'root',
    'workspace',
    'workspaceRoot',
    'workspace_root',
    'cwd',
    'project',
    'repo',
  ] as const) {
    const val = input[key];
    if (typeof val === 'string' && val.trim().length > 0) {
      return val.trim();
    }
  }
  return undefined;
}

export function extractInputPathHints(input: unknown): string[] {
  if (!isRecord(input)) {
    return [];
  }
  const raw = collectAliasStrings(input, [
    'file',
    'path',
    'filePath',
    'file_path',
    'filepath',
    'relative_path',
    'relativePath',
    'target',
    'target_file',
    'targetFile',
    'uri',
    'targets',
    'files',
  ]);
  const hints: string[] = [];
  for (const item of raw) {
    const stripped = stripLineAndNodeSuffix(item);
    if (stripped && !hints.includes(stripped)) {
      hints.push(stripped);
    }
  }
  return hints;
}

function inferSubpathTargetFromRoot(
  input: unknown,
  root: string | null | undefined,
  preserveNodeId: boolean,
): string | undefined {
  const rawRoot = extractRawRootInput(input);
  if (!rawRoot || !root) {
    return undefined;
  }
  const cleanedRaw = cleanTargetString(rawRoot).replace(/\\/gu, '/').replace(/\/+$/u, '');
  const rootTrimmed = root.replace(/\\/gu, '/').replace(/\/+$/u, '');
  if (!cleanedRaw.startsWith(`${rootTrimmed}/`)) {
    return undefined;
  }
  const rel = normalizeTargetForRoot(root, rawRoot);
  if (!rel || rel === '.' || rel.startsWith('/')) {
    return undefined;
  }
  return preserveNodeId ? rel : (rel.split('::')[0] ?? rel);
}

export function buildRunArguments(
  input: unknown,
  root?: string | null,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  let rawTargets = collectAliasStrings(rec, [
    'targets',
    'files',
    'file',
    'target',
    'path',
    'filePath',
    'file_path',
    'filepath',
    'relative_path',
    'relativePath',
    'target_file',
    'targetFile',
    'uri',
  ]);
  if (rawTargets.length === 0) {
    const inferred = inferSubpathTargetFromRoot(rec, root, true);
    if (inferred) {
      rawTargets = [inferred];
    }
  }
  const targets = rawTargets.map((t) => normalizeTargetForRoot(root, t));
  const pytestArgs = collectAliasStrings(rec, [
    'pytest_args',
    'args',
    'extra_args',
    'extraArgs',
  ]);
  const args: Record<string, JsonValue> = {
    targets,
    rerun_failed: typeof rec.rerun_failed === 'boolean' ? rec.rerun_failed : true,
    no_cache: typeof rec.no_cache === 'boolean' ? rec.no_cache : false,
    pytest_args: pytestArgs,
  };
  const ruffFix = rec.ruff_fix ?? rec.fix ?? rec.lint_fix ?? rec.lintFix;
  if (ruffFix === true) {
    args.ruff_fix = true;
  }
  const kVal = rec.k ?? rec.filter ?? rec.test_filter ?? rec.testFilter ?? rec.pattern;
  if (typeof kVal === 'string' && kVal.trim().length > 0) {
    args.k = kVal;
  }
  const markVal = rec.mark ?? rec.marker ?? rec.markers;
  if (typeof markVal === 'string' && markVal.trim().length > 0) {
    args.mark = markVal;
  }
  return args;
}

export function buildAffectedArguments(
  input: unknown,
  root?: string | null,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  let rawFiles = collectAliasStrings(rec, [
    'files',
    'targets',
    'target',
    'file',
    'path',
    'filePath',
    'file_path',
    'filepath',
    'relative_path',
    'relativePath',
    'target_file',
    'targetFile',
    'uri',
  ]);
  if (rawFiles.length === 0) {
    const inferred = inferSubpathTargetFromRoot(rec, root, false);
    if (inferred) {
      rawFiles = [inferred];
    }
  }
  const files = rawFiles.map(
    (f) => normalizeTargetForRoot(root, f).split('::')[0] ?? f,
  );
  return {
    files,
    git_diff: typeof rec.git_diff === 'boolean' ? rec.git_diff : true,
  };
}

export function buildGraphArguments(
  input: unknown,
  root?: string | null,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  const candidates = collectAliasStrings(rec, [
    'file',
    'path',
    'filePath',
    'file_path',
    'filepath',
    'relative_path',
    'relativePath',
    'target',
    'target_file',
    'targetFile',
    'uri',
    'files',
    'targets',
  ]);
  const rawFile = candidates[0] ?? inferSubpathTargetFromRoot(rec, root, false) ?? '';
  const normalized = normalizeTargetForRoot(root, rawFile).split('::')[0] ?? rawFile;
  return {
    file: normalized,
  };
}

export function buildInvalidateArguments(
  input: unknown,
  root?: string | null,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  let rawFiles = collectAliasStrings(rec, [
    'files',
    'targets',
    'target',
    'file',
    'path',
    'filePath',
    'file_path',
    'filepath',
    'relative_path',
    'relativePath',
    'target_file',
    'targetFile',
    'uri',
  ]);
  if (rawFiles.length === 0) {
    const inferred = inferSubpathTargetFromRoot(rec, root, false);
    if (inferred) {
      rawFiles = [inferred];
    }
  }
  return {
    files: rawFiles.map((f) => normalizeTargetForRoot(root, f).split('::')[0] ?? f),
  };
}
