import { realpathSync, statSync } from 'node:fs';
import { dirname, isAbsolute, join, posix, relative, resolve, sep } from 'node:path';

export const WSL_UNC_RE = /^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)(\\.+)$/iu;
export const WSL_SLASH_UNC_RE = /^\/\/(?:wsl\.localhost|wsl\$)\/([^/]+)(\/.*)$/iu;
export const WSL_FILE_URI_RE = /^file:\/\/(?:wsl\.localhost|wsl\$)\/([^/]+)(\/.*)$/iu;
export const REMOTE_URI_RE =
  /^(?:vscode-remote|vscode-file|vscode-userdata|vscode-insiders|cursor|windsurf|vscodium|git):\/\/[^/]+(\/.*)$/iu;
export const MD_LINK_RE = /^\[[^\]]*\]\((.+)\)$/u;
export const COLON_LINE_RE = /^(.+?\.[^/:]+):(\d+)(?::\d+)?(?:-(\d+)(?::\d+)?)?(?::\s+.*|\s+.*)?$/u;
export const WSL_REMOTE_URI_RE = /^vscode-remote:\/\/wsl(?:\+|%2B)([^/]+)(\/.*)$/iu;

export class RootResolutionError extends Error {
  public readonly suggestedRegistrationDir: string | undefined;

  public constructor(message: string, suggestedRegistrationDir?: string) {
    super(message);
    this.name = 'RootResolutionError';
    this.suggestedRegistrationDir = suggestedRegistrationDir;
  }
}

export class RootConflictError extends RootResolutionError {
  public constructor(message: string) {
    super(message);
    this.name = 'RootConflictError';
  }
}

export function extractWslUncDistribution(rawPath: string): string | undefined {
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
  const remoteWslMatch = WSL_REMOTE_URI_RE.exec(s);
  if (remoteWslMatch?.[1]) {
    try {
      return decodeURIComponent(remoteWslMatch[1]);
    } catch {
      return remoteWslMatch[1];
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
  const uncMatch = WSL_UNC_RE.exec(s);
  if (uncMatch?.[1]) {
    return uncMatch[1];
  }
  const slashUncMatch = WSL_SLASH_UNC_RE.exec(s.replace(/\\/gu, '/'));
  if (slashUncMatch?.[1]) {
    return slashUncMatch[1];
  }
  return undefined;
}

export function assertWslDistributionMatches(
  rawPath: string,
  configuredDistribution?: string,
): void {
  if (!configuredDistribution) {
    return;
  }
  const pathDistro = extractWslUncDistribution(rawPath);
  if (pathDistro && pathDistro.toLowerCase() !== configuredDistribution.toLowerCase()) {
    throw new RootConflictError(
      `Path belongs to WSL distribution '${pathDistro}', which conflicts with configured distribution '${configuredDistribution}'.`,
    );
  }
}


export function isExistingDirectory(pathStr: string): boolean {
  try {
    return statSync(pathStr).isDirectory();
  } catch {
    return false;
  }
}

export function isExistingFile(pathStr: string): boolean {
  try {
    return statSync(pathStr).isFile();
  } catch {
    return false;
  }
}


export function allContainingRoots(roots: readonly string[], candidate: string): string[] {
  return roots.filter((root) => contains(root, candidate));
}

export function canonicalizePosixPathIfPossible(
  posixPath: string,
  wslDistribution?: string,
  requireExistingBeforeDotDot = false,
): string {
  if (!posixPath.startsWith('/')) {
    return posix.normalize(posixPath);
  }
  if (process.platform !== 'win32') {
    if (requireExistingBeforeDotDot) {
      return canonicalizePathStrictDotDot(posixPath) ?? posixPath;
    }
    // Resolve symlinks BEFORE applying `..` so `/repo/link/../test.py` (where `link` points
    // outside `/repo`) follows `link` first rather than lexically collapsing to `/repo/test.py`!
    return canonicalizePathIfExists(posixPath);
  }
  if (wslDistribution) {
    // Preserve raw `..` components when mapping into `\\wsl.localhost\<distro>\...` so
    // `canonicalizePathIfExists` resolves symlinks component-by-component before `..`!
    const uncPath = `\\\\wsl.localhost\\${wslDistribution}${posixPath.replace(/\//gu, '\\')}`;
    const canonUnc = requireExistingBeforeDotDot
      ? canonicalizePathStrictDotDot(uncPath)
      : canonicalizePathIfExists(uncPath);
    if (canonUnc === undefined) {
      return posixPath;
    }
    const uncMatch = WSL_UNC_RE.exec(canonUnc);
    if (
      uncMatch?.[1] &&
      uncMatch[2] &&
      uncMatch[1].toLowerCase() === wslDistribution.toLowerCase()
    ) {
      return posix.normalize(uncMatch[2].replace(/\\/gu, '/'));
    }
  }
  return requireExistingBeforeDotDot ? posixPath : posix.normalize(posixPath);
}

export function allContainingPosixRoots(
  roots: readonly string[],
  candidate: string,
  wslDistribution?: string,
): string[] {
  return roots.filter((root) => containsPosix(root, candidate, wslDistribution));
}

export function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => contains(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

export function bestContainingPosixRoot(
  roots: readonly string[],
  candidate: string,
  wslDistribution?: string,
): string | undefined {
  return roots
    .filter((root) => containsPosix(root, candidate, wslDistribution))
    .sort((left, right) => right.length - left.length)[0];
}

export function containsPosix(root: string, candidate: string, wslDistribution?: string): boolean {
  const canonRoot = canonicalizePosixPathIfPossible(root, wslDistribution);
  const canonCand = canonicalizePosixPathIfPossible(candidate, wslDistribution);
  const child = posix.relative(canonRoot, canonCand);
  return child === '' || (!child.startsWith('../') && child !== '..' && !posix.isAbsolute(child));
}

export function canonicalizePathStrictDotDot(rawPath: string): string | undefined {
  if (!isAbsolute(rawPath)) {
    return undefined;
  }
  const realpathNative = (p: string): string =>
    typeof realpathSync.native === 'function' ? realpathSync.native(p) : realpathSync(p);
  const isWinUnc = rawPath.startsWith('\\\\');
  const driveHead = /^([A-Za-z]:[\\/])(.*)$/u.exec(rawPath);
  const isWinPath = isWinUnc || Boolean(driveHead?.[1]) || process.platform === 'win32';
  let cur: string;
  let rest: string;
  if (isWinUnc) {
    const uncHead = /^(\\\\[^\\]+\\[^\\]+)(.*)$/u.exec(rawPath);
    if (!uncHead?.[1]) {
      return undefined;
    }
    cur = uncHead[1];
    rest = uncHead[2] ?? '';
  } else if (driveHead?.[1]) {
    cur = driveHead[1];
    rest = driveHead[2] ?? '';
  } else {
    cur = sep;
    rest = isWinPath ? rawPath.replace(/^[\\/]+/u, '') : rawPath.replace(/^\/+/u, '');
  }
  const parts = (isWinPath ? rest.split(/[\\/]+/u) : rest.split(/\/+/u)).filter(
    (seg) => seg.length > 0 && seg !== '.',
  );
  let canProbe = true;
  for (const seg of parts) {
    if (seg === '..') {
      if (!canProbe) {
        // An intermediate directory before `..` did not exist on disk (e.g. `src/no-such-dir/../..`):
        // refuse to collapse `..` past a missing component!
        return undefined;
      }
      try {
        cur = realpathNative(cur);
        // Require EVERY component before `..` to actually be a directory (so `src/ast_graph.rs/../..`
        // where `ast_graph.rs` is a regular file NEVER collapses to `.`!)
        if (!statSync(cur).isDirectory()) {
          return undefined;
        }
      } catch {
        return undefined;
      }
      cur = dirname(cur);
      continue;
    }
    cur = join(cur, seg);
    if (canProbe) {
      try {
        cur = realpathNative(cur);
      } catch {
        canProbe = false;
      }
    }
  }
  return cur;
}

export function canonicalizePathIfExists(rawPath: string): string {
  const realpathNative = (p: string): string =>
    typeof realpathSync.native === 'function' ? realpathSync.native(p) : realpathSync(p);
  if (isAbsolute(rawPath)) {
    // Node's `realpathSync` normalizes `..` lexically before resolving symlinks, so ONLY take the
    // fast path when `rawPath` contains no `..` segments; otherwise walk left-to-right resolving
    // every existing symlink component BEFORE applying `..` (matching OS/kernel path lookup for
    // paths like `/dev/fd/..` -> `/proc/<pid>`, NOT `/dev`)!
    const hasDotDot = /(?:^|[\\/])\.\.(?:$|[\\/])/u.test(rawPath);
    if (!hasDotDot) {
      try {
        return realpathNative(rawPath);
      } catch {
        // fall through to component-by-component canonicalization
      }
    }
    const isWinUnc = rawPath.startsWith('\\\\');
    let cur: string;
    let rest: string;
    if (isWinUnc) {
      const uncHead = /^(\\\\[^\\]+\\[^\\]+)(.*)$/u.exec(rawPath);
      if (!uncHead?.[1]) {
        return resolve(rawPath);
      }
      cur = uncHead[1];
      rest = uncHead[2] ?? '';
    } else {
      const driveHead = /^([A-Za-z]:[\\/])(.*)$/u.exec(rawPath);
      if (driveHead?.[1]) {
        cur = driveHead[1];
        rest = driveHead[2] ?? '';
      } else {
        cur = sep;
        rest = rawPath.replace(/^[\\/]+/u, '');
      }
    }
    const parts = rest.split(/[\\/]+/u).filter((seg) => seg.length > 0 && seg !== '.');
    let canProbe = true;
    for (const seg of parts) {
      if (seg === '..') {
        if (canProbe) {
          try {
            cur = realpathNative(cur);
          } catch {
            canProbe = false;
          }
        }
        cur = dirname(cur);
        continue;
      }
      cur = join(cur, seg);
      if (canProbe) {
        try {
          cur = realpathNative(cur);
        } catch {
          canProbe = false;
        }
      }
    }
    return cur;
  }
  const abs = resolve(rawPath);
  try {
    return realpathNative(abs);
  } catch {
    return abs;
  }
}

export function containsLexical(root: string, candidate: string): boolean {
  const child = relative(root, candidate);
  return child === '' || (!child.startsWith(`..${sep}`) && child !== '..' && !isAbsolute(child));
}

export function contains(root: string, candidate: string): boolean {
  return containsLexical(
    canonicalizePathIfExists(root),
    canonicalizePathIfExists(candidate),
  );
}

