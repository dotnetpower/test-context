import { extractWslUncDistribution } from './pathContainment.js';
import {
  cleanTargetString,
  normalizeTargetForRoot,
  stripLineAndNodeSuffix,
} from './targetPath.js';
import type { JsonValue } from './types.js';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const PLURAL_LIST_KEYS = new Set(['targets', 'files', 'pytest_args', 'args', 'extra_args', 'extraArgs']);

function extractStringArray(value: unknown, allowCommaSplit = true): string[] {
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
    if (allowCommaSplit && trimmed.includes(',') && !trimmed.startsWith('[')) {
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
  deduplicate = true,
): string[] {
  const out: string[] = [];
  for (const key of keys) {
    // Only split scalar strings on commas for plural keys (`targets`, `files`, `args`),
    // never for singular file-path keys (`file`, `path`) so filenames containing commas work.
    const allowCommaSplit = PLURAL_LIST_KEYS.has(key);
    for (const item of extractStringArray(rec[key], allowCommaSplit)) {
      if (!deduplicate || !out.includes(item)) {
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
    // Preserve distro-qualified WSL URIs/UNC paths, percent-encoded `#` URIs (`%23`), AND
    // relative paths containing `::`, `#`, or `:` (which could be literal filenames such as
    // `tests/foo::bar.py`, `tests/foo.py#L10`, or `tests/foo.py:10` that need root-relative
    // probing against candidate roots before suffix stripping).
    const trimmed = item.trim();
    const isColonPath =
      trimmed.includes(':') &&
      !/^[A-Za-z]:[\\/]/u.test(trimmed) &&
      !/^[A-Za-z][A-Za-z0-9+.-]*:\/\//u.test(trimmed);
    const keepRawUri =
      Boolean(extractWslUncDistribution(trimmed)) ||
      trimmed.includes('::') ||
      trimmed.includes('#') ||
      isColonPath ||
      (/^(?:file|vscode-remote|vscode-file|vscode-userdata|vscode-insiders|cursor|windsurf|vscodium|git):\/\//iu.test(
        trimmed,
      ) &&
        trimmed.toLowerCase().includes('%23'));
    const hint = keepRawUri ? trimmed : stripLineAndNodeSuffix(trimmed);
    if (hint && !hints.includes(hint)) {
      hints.push(hint);
    }
  }
  return hints;
}

function inferSubpathTargetFromRoot(
  input: unknown,
  root: string | null | undefined,
  preserveNodeId: boolean,
  wslDistribution?: string,
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
  const rel = normalizeTargetForRoot(root, rawRoot, wslDistribution, preserveNodeId);
  if (!rel || rel === '.' || rel.startsWith('/')) {
    return undefined;
  }
  return rel;
}

export function buildRunArguments(
  input: unknown,
  root?: string | null,
  wslDistribution?: string,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  const rawTargets = collectAliasStrings(rec, [
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
  let targets: string[];
  if (rawTargets.length > 0) {
    targets = rawTargets.map((t) => normalizeTargetForRoot(root, t, wslDistribution));
  } else {
    const inferred = inferSubpathTargetFromRoot(rec, root, true, wslDistribution);
    targets = inferred ? [inferred] : [];
  }
  // Never deduplicate CLI arguments (`["-o", "a=1", "-o", "b=2"]` requires repeated flags).
  const pytestArgs = collectAliasStrings(
    rec,
    ['pytest_args', 'args', 'extra_args', 'extraArgs'],
    false,
  );
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
  wslDistribution?: string,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  const rawFiles = collectAliasStrings(rec, [
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
  let files: string[];
  if (rawFiles.length > 0) {
    files = rawFiles.map((f) => normalizeTargetForRoot(root, f, wslDistribution, false));
  } else {
    const inferred = inferSubpathTargetFromRoot(rec, root, false, wslDistribution);
    files = inferred ? [inferred] : [];
  }
  return {
    files,
    git_diff: typeof rec.git_diff === 'boolean' ? rec.git_diff : true,
  };
}

export function buildGraphArguments(
  input: unknown,
  root?: string | null,
  wslDistribution?: string,
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
  const firstCandidate = candidates[0];
  const normalized =
    firstCandidate !== undefined
      ? normalizeTargetForRoot(root, firstCandidate, wslDistribution, false)
      : (inferSubpathTargetFromRoot(rec, root, false, wslDistribution) ?? '');
  return {
    file: normalized,
  };
}

export function buildInvalidateArguments(
  input: unknown,
  root?: string | null,
  wslDistribution?: string,
): Record<string, JsonValue> {
  const rec = isRecord(input) ? input : {};
  const rawFiles = collectAliasStrings(rec, [
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
  let files: string[];
  if (rawFiles.length > 0) {
    files = rawFiles.map((f) => normalizeTargetForRoot(root, f, wslDistribution, false));
  } else {
    const inferred = inferSubpathTargetFromRoot(rec, root, false, wslDistribution);
    files = inferred ? [inferred] : [];
  }
  return {
    files,
  };
}
