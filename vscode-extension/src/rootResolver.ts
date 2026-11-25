import { existsSync, lstatSync, realpathSync, statSync } from 'node:fs';
import { basename, dirname, isAbsolute, join, relative, resolve, sep, posix } from 'node:path';

import type { JsonValue, WorkspaceSpec } from './types.js';

const WSL_UNC_RE = /^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)(\\.+)$/iu;
const WSL_SLASH_UNC_RE = /^\/\/(?:wsl\.localhost|wsl\$)\/([^/]+)(\/.*)$/iu;
const WSL_FILE_URI_RE = /^file:\/\/(?:wsl\.localhost|wsl\$)\/([^/]+)(\/.*)$/iu;
const REMOTE_URI_RE =
  /^(?:vscode-remote|vscode-file|vscode-userdata|vscode-insiders|cursor|windsurf|vscodium|git):\/\/[^/]+(\/.*)$/iu;
const MD_LINK_RE = /^\[[^\]]*\]\((.+)\)$/u;
const COLON_LINE_RE = /^(.+?\.[^/:]+):(\d+)(?::\d+)?(?:-(\d+)(?::\d+)?)?(?::\s+.*|\s+.*)?$/u;

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

const WSL_REMOTE_URI_RE = /^vscode-remote:\/\/wsl(?:\+|%2B)([^/]+)(\/.*)$/iu;

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

function isExistingAbsolutePathForProbe(
  cand: string,
  wslDistribution?: string,
): boolean {
  const uncMatch = WSL_UNC_RE.exec(cand);
  if (uncMatch?.[1] && uncMatch[2]) {
    const uncPath = `\\\\wsl.localhost\\${uncMatch[1]}${uncMatch[2].replace(/\//gu, '\\')}`;
    return isExistingFile(uncPath) || isExistingDirectory(uncPath);
  }
  const slashUncMatch = WSL_SLASH_UNC_RE.exec(cand.replace(/\\/gu, '/'));
  if (slashUncMatch?.[1] && slashUncMatch[2]) {
    const uncPath = `\\\\wsl.localhost\\${slashUncMatch[1]}${slashUncMatch[2].replace(/\//gu, '\\')}`;
    return isExistingFile(uncPath) || isExistingDirectory(uncPath);
  }
  if (wslDistribution && process.platform === 'win32' && posix.isAbsolute(cand)) {
    const uncPath = `\\\\wsl.localhost\\${wslDistribution}${cand.replace(/\//gu, '\\')}`;
    if (isExistingFile(uncPath) || isExistingDirectory(uncPath)) {
      return true;
    }
  }
  return isAbsolute(cand) && (isExistingFile(cand) || isExistingDirectory(cand));
}

function decodeUriPathPreservingEncodedHash(
  rawUriPart: string,
  wslDistribution?: string,
): {
  filePart: string;
  nodePart: string | undefined;
} {
  // Strip raw `#fragment` and `?query` FIRST (before checking/splitting `::` and decoding)
  // so `file:///repo/test.py?version=1#L10` normalizes cleanly while percent-encoded `#` (`%23`)
  // and `?` (`%3F`) survive intact into the decoded filename.
  const rawHashIdx = rawUriPart.indexOf('#');
  const uriWithoutRawFrag = rawHashIdx >= 0 ? rawUriPart.slice(0, rawHashIdx) : rawUriPart;
  const rawQueryIdx = uriWithoutRawFrag.indexOf('?');
  const uriWithoutQueryAndFrag =
    rawQueryIdx >= 0 ? uriWithoutRawFrag.slice(0, rawQueryIdx) : uriWithoutRawFrag;
  const decodeSafe = (raw: string): string => {
    try {
      return decodeURIComponent(raw);
    } catch {
      return raw;
    }
  };
  // Strip raw unencoded `:line[:col]` suffixes BEFORE percent-decoding (unless the full decoded
  // path already exists on disk) so percent-encoded colons like `file:///repo/config.py%3A17`
  // survive intact even if that literal file was deleted, while encoded extensions like
  // `file:///repo/tests/test%2Epy:42` still strip their unencoded `:42` line suffix!
  const decodeFilePartPreservingEncodedColon = (rawFile: string): string => {
    const fullDecoded = decodeSafe(rawFile);
    if (isExistingAbsolutePathForProbe(fullDecoded, wslDistribution)) {
      return fullDecoded;
    }
    // Match longest `:line[:col][-endLine[:endCol]]` suffix FIRST so greedy `(.*)` never strips
    // only the trailing `:col` from `file:///repo/tests/test.py:10:2-20:3` or `test%2Epy:42:12`!
    const rawSuffixPatterns = [
      /^(.*):(\d+:\d+-\d+:\d+)$/u,
      /^(.*):(\d+:\d+-\d+)$/u,
      /^(.*):(\d+-\d+:\d+)$/u,
      /^(.*):(\d+-\d+)$/u,
      /^(.*):(\d+:\d+)$/u,
      /^(.*):(\d+)$/u,
    ];
    for (const pat of rawSuffixPatterns) {
      const m = pat.exec(rawFile);
      if (m?.[1] && m[2] && !/^\/?[a-zA-Z]$/u.test(m[1])) {
        const prefixDecoded = decodeSafe(m[1]);
        // Validate the suffix against the decoded prefix with any decoded `%3A` colons masked so
        // `file:///project/tests/config.py%3A17:42:3` strips `:42:3` while keeping `config.py:17`!
        if (COLON_LINE_RE.test(`${prefixDecoded.replace(/:/gu, '_')}:${m[2]}`)) {
          return prefixDecoded;
        }
      }
    }
    return fullDecoded;
  };
  if (uriWithoutQueryAndFrag.includes('::')) {
    const splitIndices: number[] = [];
    let pos = uriWithoutQueryAndFrag.indexOf('::');
    while (pos >= 0) {
      splitIndices.push(pos);
      pos = uriWithoutQueryAndFrag.indexOf('::', pos + 2);
    }
    const fullDecoded = decodeSafe(uriWithoutQueryAndFrag);
    if (isExistingAbsolutePathForProbe(fullDecoded, wslDistribution)) {
      return { filePart: fullDecoded, nodePart: undefined };
    }
    for (let i = splitIndices.length - 1; i >= 0; i -= 1) {
      const idx = splitIndices[i];
      if (idx !== undefined && idx > 0) {
        const candDecoded = decodeSafe(uriWithoutQueryAndFrag.slice(0, idx));
        if (isExistingAbsolutePathForProbe(candDecoded, wslDistribution)) {
          return {
            filePart: candDecoded,
            nodePart: decodeSafe(uriWithoutQueryAndFrag.slice(idx + 2)).trim() || undefined,
          };
        }
      }
    }
    const firstIdx = splitIndices[0]!;
    return {
      filePart: decodeFilePartPreservingEncodedColon(uriWithoutQueryAndFrag.slice(0, firstIdx)),
      nodePart: decodeSafe(uriWithoutQueryAndFrag.slice(firstIdx + 2)).trim() || undefined,
    };
  }
  return { filePart: decodeFilePartPreservingEncodedColon(uriWithoutQueryAndFrag), nodePart: undefined };
}

function cleanTargetStringWithMeta(
  rawPath: string,
  wslDistribution?: string,
): {
  cleaned: string;
  isUri: boolean;
  uriFilePart?: string;
  uriNodePart?: string;
} {
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
  if (fileWslMatch?.[2]) {
    const uriDistro = fileWslMatch[1] || wslDistribution;
    const { filePart, nodePart } = decodeUriPathPreservingEncodedHash(fileWslMatch[2], uriDistro);
    return {
      cleaned: nodePart ? `${filePart}::${nodePart}` : filePart,
      isUri: true,
      uriFilePart: filePart,
      ...(nodePart ? { uriNodePart: nodePart } : {}),
    };
  }
  let isUri = false;
  let uriFilePart: string | undefined;
  let uriNodePart: string | undefined;
  if (s.startsWith('file://')) {
    const withoutScheme = s.replace(/^file:\/\/localhost(?=\/)/iu, 'file://').slice('file://'.length);
    const decoded = decodeUriPathPreservingEncodedHash(withoutScheme, wslDistribution);
    // Standard RFC 8089 Windows drive file URIs (`file:///C:/repo/test.py`) decode with a leading
    // `/` before the drive letter; strip it so they are recognized as Windows paths, not POSIX!
    const winDriveMatch = /^\/([A-Za-z]:\/.*)$/u.exec(decoded.filePart);
    uriFilePart = winDriveMatch?.[1] ?? decoded.filePart;
    uriNodePart = decoded.nodePart;
    s = uriNodePart ? `${uriFilePart}::${uriNodePart}` : uriFilePart;
    isUri = true;
  } else {
    const remoteMatch = REMOTE_URI_RE.exec(s);
    if (remoteMatch?.[1]) {
      const wslRemoteDistroMatch = /^vscode-remote:\/\/wsl(?:\+|%2B)([^/]+)\//iu.exec(s);
      let uriDistro = wslDistribution;
      if (wslRemoteDistroMatch?.[1]) {
        try {
          uriDistro = decodeURIComponent(wslRemoteDistroMatch[1]);
        } catch {
          uriDistro = wslRemoteDistroMatch[1];
        }
      }
      const decoded = decodeUriPathPreservingEncodedHash(remoteMatch[1], uriDistro);
      uriFilePart = decoded.filePart;
      uriNodePart = decoded.nodePart;
      s = uriNodePart ? `${uriFilePart}::${uriNodePart}` : uriFilePart;
      isUri = true;
    }
  }
  const uncMatch = WSL_UNC_RE.exec(s);
  if (uncMatch?.[2]) {
    const mapped = uncMatch[2].replace(/\\/gu, '/');
    return { cleaned: mapped, isUri };
  }
  const slashUncMatch = WSL_SLASH_UNC_RE.exec(s.replace(/\\/gu, '/'));
  if (slashUncMatch?.[2]) {
    return { cleaned: slashUncMatch[2], isUri };
  }
  return {
    cleaned: s,
    isUri,
    ...(uriFilePart !== undefined ? { uriFilePart } : {}),
    ...(uriNodePart !== undefined ? { uriNodePart } : {}),
  };
}

export function cleanTargetString(rawPath: string): string {
  return cleanTargetStringWithMeta(rawPath).cleaned;
}

const HASH_LINE_FRAGMENT_RE = /^L?\d+(?:C\d+)?(?:-L?\d+(?:C\d+)?)?$/u;
const FILE_EXT_BEFORE_HASH_RE = /\.[a-zA-Z0-9]{1,12}$/u;

function stripFragmentAndLineSuffix(
  filePart: string,
  isUri: boolean,
  rootDir?: string,
  wslDistro?: string,
): string {
  // If the literal path (including any `#L10` or `:10` characters in a real directory/file name
  // such as `app#L10`, `tests/foo:10.py`, or root-prefixed `repo/tests/foo.py#L10`) already exists
  // on disk (including via WSL UNC on Windows when a WSL distribution is configured), preserve it
  // verbatim before suffix stripping.
  if (filePart.includes('#') || filePart.includes(':')) {
    const probeExists = (probe: string): boolean => {
      if (isAbsolute(probe) && existsSync(probe)) {
        return true;
      }
      if (process.platform === 'win32' && wslDistro && probe.startsWith('/')) {
        const uncProbe = `\\\\wsl.localhost\\${wslDistro}${probe.replace(/\//gu, '\\')}`;
        if (existsSync(uncProbe)) {
          return true;
        }
      }
      return false;
    };
    const directProbe = rootDir && !isAbsolute(filePart) ? join(rootDir, filePart) : filePart;
    if (probeExists(directProbe)) {
      return filePart;
    }
    if (rootDir && !isAbsolute(filePart)) {
      const rootNorm = rootDir.replace(/\\/gu, '/').replace(/\/+$/u, '');
      const rootName = posix.basename(rootNorm);
      const fpNorm = filePart.replace(/\\/gu, '/').replace(/^(?:\.\/)+/u, '');
      if (rootName && fpNorm.startsWith(`${rootName}/`)) {
        const withoutRootName = fpNorm.slice(rootName.length + 1);
        if (
          withoutRootName &&
          containsPosix(
            rootNorm.startsWith('/') ? posix.normalize(rootNorm) : `/${rootName}`,
            posix.resolve(
              rootNorm.startsWith('/') ? posix.normalize(rootNorm) : `/${rootName}`,
              withoutRootName,
            ),
          ) &&
          probeExists(join(rootDir, withoutRootName))
        ) {
          return filePart;
        }
      }
    }
  }
  // For URI inputs, real `#fragment` suffixes were already stripped prior to percent-decoding,
  // so any `#` remaining in `filePart` came from `%23` in the filename and must be preserved.
  // For plain paths, only strip `#...` when it is clearly a line anchor (`#L10-L20`, `#42`) or
  // a doc/symbol fragment after an existing file extension (`guide.md#section`), preserving
  // literal `#` characters in directory/file names (`tests/foo#bar.test.ts`, `/repo#one/a.ts`).
  let withoutFrag = filePart;
  if (!isUri) {
    const hashIdx = filePart.lastIndexOf('#');
    if (hashIdx > 0) {
      const prefix = filePart.slice(0, hashIdx);
      const frag = filePart.slice(hashIdx + 1);
      const isLineFrag = HASH_LINE_FRAGMENT_RE.test(frag);
      const isSymbolOrHeadingFrag =
        frag.length > 0 &&
        !frag.includes('/') &&
        !frag.includes('\\') &&
        !frag.includes('.') &&
        FILE_EXT_BEFORE_HASH_RE.test(prefix);
      if (isLineFrag || isSymbolOrHeadingFrag) {
        withoutFrag = prefix;
      }
    }
  }
  if (!isUri) {
    const colonMatch = COLON_LINE_RE.exec(withoutFrag);
    if (colonMatch?.[1] && !/^[a-zA-Z]:$/u.test(colonMatch[1])) {
      return colonMatch[1];
    }
  }
  return withoutFrag;
}

export function stripLineAndNodeSuffix(rawPath: string, wslDistribution?: string): string {
  const effectiveDistro = wslDistribution ?? extractWslUncDistribution(rawPath);
  const meta = cleanTargetStringWithMeta(rawPath, effectiveDistro);
  if (meta.isUri && meta.uriFilePart !== undefined) {
    const withoutNode = meta.uriFilePart.trim();
    return withoutNode ? stripFragmentAndLineSuffix(withoutNode, true, undefined, effectiveDistro) : '';
  }
  const cleaned = meta.cleaned.trim();
  if (!cleaned) {
    return '';
  }
  if (cleaned.includes('::')) {
    // Check full path and right-to-left `::` prefixes for an existing literal file BEFORE
    // stripping a node selector so `foo::bar.py` / `\\wsl.localhost\Ubuntu\home\me\work::proj`
    // are never truncated.
    const splitIndices: number[] = [];
    let pos = cleaned.indexOf('::');
    while (pos >= 0) {
      splitIndices.push(pos);
      pos = cleaned.indexOf('::', pos + 2);
    }
    const candidates: string[] = [cleaned];
    for (let i = splitIndices.length - 1; i >= 0; i -= 1) {
      const idx = splitIndices[i];
      if (idx !== undefined && idx > 0) {
        candidates.push(cleaned.slice(0, idx).trim());
      }
    }
    for (const cand of candidates) {
      if (isAbsolute(cand) && (isExistingFile(cand) || isExistingDirectory(cand))) {
        return stripFragmentAndLineSuffix(cand, false, undefined, effectiveDistro);
      }
      if (process.platform === 'win32' && effectiveDistro && cand.startsWith('/')) {
        const uncCand = `\\\\wsl.localhost\\${effectiveDistro}${cand.replace(/\//gu, '\\')}`;
        if (isExistingFile(uncCand) || isExistingDirectory(uncCand)) {
          return stripFragmentAndLineSuffix(cand, false, undefined, effectiveDistro);
        }
      }
    }
    const first = cleaned.slice(0, splitIndices[0]).trim();
    return first ? stripFragmentAndLineSuffix(first, false, undefined, effectiveDistro) : '';
  }
  return stripFragmentAndLineSuffix(cleaned, false, undefined, effectiveDistro);
}

export function normalizeTargetForRoot(
  root: string | null | undefined,
  rawPath: string,
  wslDistribution?: string,
  includeNodeId = true,
): string {
  assertWslDistributionMatches(rawPath, wslDistribution);
  const firstPass = cleanTargetStringWithMeta(rawPath, wslDistribution);
  let rawFile: string;
  let nodePart: string | undefined;
  let isUri = firstPass.isUri;
  if (firstPass.isUri && firstPass.uriFilePart !== undefined) {
    // Preserve decoded `%3A%3A` colons inside URI file paths rather than splitting them as node IDs.
    rawFile = firstPass.uriFilePart.trim();
    nodePart = firstPass.uriNodePart;
  } else {
    // Prefer the longest existing literal file prefix before splitting pytest/cargo `::` selectors
    // so filenames like `tests/foo::bar.py` or `tests/foo::bar.py::test_a` are never truncated.
    const cleaned = firstPass.cleaned.trim();
    const isPosixRoot = Boolean(root && root.replace(/\\/gu, '/').startsWith('/'));
    let matchedSplit: { file: string; node?: string } | undefined;
    if (cleaned.includes('::')) {
      const splitIndices: number[] = [];
      let searchPos = cleaned.indexOf('::');
      while (searchPos >= 0) {
        splitIndices.push(searchPos);
        searchPos = cleaned.indexOf('::', searchPos + 2);
      }
      // Check full literal path first, then longest `::` prefixes from right to left.
      const candidates: Array<{ file: string; node?: string }> = [{ file: cleaned }];
      for (let i = splitIndices.length - 1; i >= 0; i -= 1) {
        const idx = splitIndices[i];
        if (idx !== undefined && idx > 0) {
          candidates.push({
            file: cleaned.slice(0, idx).trim(),
            node: cleaned.slice(idx + 2).trim(),
          });
        }
      }
      const rootTrimmedForSplit = root
        ? (isPosixRoot ? root : root.replace(/\\/gu, '/')).replace(/\/+$/u, '')
        : '';
      const rootNameForSplit = rootTrimmedForSplit ? posix.basename(rootTrimmedForSplit) : '';
      for (const cand of candidates) {
        const candLooksWin =
          /^[a-zA-Z]:[\\/]/u.test(cand.file) ||
          cand.file.startsWith('\\\\') ||
          (!isPosixRoot && process.platform === 'win32');
        const candNorm = candLooksWin ? cand.file.replace(/\\/gu, '/') : cand.file;
        const candRel = candNorm.replace(/^(?:\.\/)+/u, '');
        let existsDirect =
          isAbsolute(cand.file) && (isExistingFile(cand.file) || isExistingDirectory(cand.file));
        if (
          !existsDirect &&
          process.platform === 'win32' &&
          wslDistribution &&
          candNorm.startsWith('/')
        ) {
          const uncCand = `\\\\wsl.localhost\\${wslDistribution}${candNorm.replace(/\//gu, '\\')}`;
          existsDirect = isExistingFile(uncCand) || isExistingDirectory(uncCand);
        }
        const existsUnderRoot =
          Boolean(root) &&
          !candNorm.startsWith('/') &&
          checkRootRelativeExists(root!, candRel, isPosixRoot, wslDistribution);
        const existsUnderRootStrippedPrefix =
          !existsUnderRoot &&
          Boolean(root) &&
          Boolean(rootNameForSplit) &&
          !candNorm.startsWith('/') &&
          candRel.startsWith(`${rootNameForSplit}/`) &&
          checkRootRelativeExists(
            root!,
            candRel.slice(rootNameForSplit.length + 1),
            isPosixRoot,
            wslDistribution,
          );
        if (existsDirect || existsUnderRoot || existsUnderRootStrippedPrefix) {
          matchedSplit = cand;
          break;
        }
      }
    }
    if (matchedSplit) {
      rawFile = matchedSplit.file;
      nodePart = matchedSplit.node;
    } else {
      const doubleColonIdx = cleaned.indexOf('::');
      rawFile = doubleColonIdx >= 0 ? cleaned.slice(0, doubleColonIdx).trim() : cleaned;
      nodePart = doubleColonIdx >= 0 ? cleaned.slice(doubleColonIdx + 2).trim() : undefined;
    }
    if (nodePart !== undefined && !firstPass.isUri) {
      const filePass = cleanTargetStringWithMeta(rawFile);
      rawFile =
        filePass.isUri && filePass.uriFilePart !== undefined
          ? filePass.uriFilePart
          : filePass.cleaned;
      isUri = filePass.isUri;
    }
  }
  const strippedFilePart = stripFragmentAndLineSuffix(
    rawFile,
    isUri,
    root ?? undefined,
    wslDistribution,
  );
  const looksLikeWindowsPath =
    /^[a-zA-Z]:[\\/]/u.test(strippedFilePart) ||
    strippedFilePart.startsWith('\\\\') ||
    (process.platform === 'win32' && (!root || !root.startsWith('/'))) ||
    Boolean(root && !root.startsWith('/'));
  const filePart = looksLikeWindowsPath
    ? strippedFilePart.replace(/\\/gu, '/')
    : strippedFilePart;
  let relFile = filePart.replace(/^(?:\.\/)+/u, '');
  if (root) {
    const isPosixRoot = root.startsWith('/');
    const rootTrimmed = (isPosixRoot ? root : root.replace(/\\/gu, '/')).replace(/\/+$/u, '');
    let matchedAbsRoot = false;
    const hasDotDotInFilePart = /(?:^|[\\/])\.\.(?:$|[\\/])/u.test(filePart);
    if (isPosixRoot && filePart.startsWith('/')) {
      const canonRoot = canonicalizePosixPathIfPossible(rootTrimmed, wslDistribution).replace(
        /\/+$/u,
        '',
      );
      const canonFile = canonicalizePosixPathIfPossible(
        filePart,
        wslDistribution,
        hasDotDotInFilePart,
      );
      if (canonFile === canonRoot) {
        relFile = '.';
        matchedAbsRoot = true;
      } else if (canonFile.startsWith(`${canonRoot}/`)) {
        relFile = canonFile.slice(canonRoot.length + 1).replace(/^(?:\.\/)+/u, '');
        matchedAbsRoot = true;
      }
    } else if (isAbsolute(filePart) && !(isPosixRoot && process.platform === 'win32')) {
      const canonRoot = canonicalizePathIfExists(rootTrimmed)
        .replace(/\\/gu, '/')
        .replace(/\/+$/u, '');
      const canonFile = hasDotDotInFilePart
        ? canonicalizePathStrictDotDot(filePart)?.replace(/\\/gu, '/')
        : canonicalizePathIfExists(filePart).replace(/\\/gu, '/');
      if (canonFile !== undefined) {
        if (canonFile === canonRoot) {
          relFile = '.';
          matchedAbsRoot = true;
        } else if (canonFile.startsWith(`${canonRoot}/`)) {
          relFile = canonFile.slice(canonRoot.length + 1).replace(/^(?:\.\/)+/u, '');
          matchedAbsRoot = true;
        }
      }
    } else if (filePart === rootTrimmed) {
      relFile = '.';
      matchedAbsRoot = true;
    } else if (filePart.startsWith(`${rootTrimmed}/`)) {
      relFile = filePart.slice(rootTrimmed.length + 1).replace(/^(?:\.\/)+/u, '');
      matchedAbsRoot = true;
    }
    if (!matchedAbsRoot && !relFile.startsWith('/')) {
      // Canonicalize safe relative paths containing `..` (e.g. `src/../tests/test_a.py`) against
      // `root` ONLY when every intermediate component before `..` actually exists on disk (so
      // `src/no-such-directory/../..` is NOT collapsed to `.` and instead stays un-rewritten for
      // rejection!), while resolving symlinks BEFORE `..`!
      if (/(?:^|\/)\.\.(?:$|\/)/u.test(relFile)) {
        if (isPosixRoot) {
          const canonRoot = canonicalizePosixPathIfPossible(rootTrimmed, wslDistribution).replace(
            /\/+$/u,
            '',
          );
          const canonRel = canonicalizePosixPathIfPossible(
            `${rootTrimmed}/${relFile}`,
            wslDistribution,
            true,
          );
          if (canonRel !== undefined) {
            if (canonRel === canonRoot) {
              relFile = '.';
            } else if (canonRel.startsWith(`${canonRoot}/`)) {
              relFile = canonRel.slice(canonRoot.length + 1).replace(/^(?:\.\/)+/u, '');
            }
          }
        } else {
          const canonRoot = canonicalizePathIfExists(rootTrimmed)
            .replace(/\\/gu, '/')
            .replace(/\/+$/u, '');
          const canonRel = canonicalizePathStrictDotDot(`${rootTrimmed}${sep}${relFile}`)?.replace(
            /\\/gu,
            '/',
          );
          if (canonRel !== undefined) {
            if (canonRel === canonRoot) {
              relFile = '.';
            } else if (canonRel.startsWith(`${canonRoot}/`)) {
              relFile = canonRel.slice(canonRoot.length + 1).replace(/^(?:\.\/)+/u, '');
            }
          }
        }
      }
      // Never strip a leading `<rootName>/` from a relative path: in `/work/repo/repo/...`
      // layouts (or when `repo/src/a.ts` was deleted while `src/a.ts` exists), stripping
      // `repo/` silently rewrites a valid workspace-relative path to the wrong file!
    }
  }
  return includeNodeId && nodePart ? `${relFile}::${nodePart}` : relFile;
}

export function toPosixPathIfPossible(
  rawPath: string,
  wslDistribution?: string,
): string | undefined {
  const trimmed = stripLineAndNodeSuffix(rawPath, wslDistribution);
  if (!trimmed || /[\0\r\n]/u.test(trimmed)) {
    return undefined;
  }
  if (posix.isAbsolute(trimmed)) {
    return canonicalizePosixPathIfPossible(trimmed, wslDistribution);
  }
  return undefined;
}

function checkRootRelativeExists(
  root: string,
  relPath: string,
  usePosix: boolean,
  wslDistribution?: string,
): boolean {
  // Concatenate without collapsing `..` lexically first so symlinks like `link/../test.ts`
  // resolve `link` before applying `..` (matching `normalizeTargetForRoot`'s strict semantics!).
  const rawJoined = usePosix
    ? `${root.replace(/\/+$/u, '')}/${relPath.replace(/^\/+/u, '')}`
    : `${root.replace(/[\\/]+$/u, '')}/${relPath.replace(/^[\\/]+/u, '')}`;
  if (usePosix && process.platform === 'win32') {
    if (!wslDistribution) {
      return false;
    }
    const rawUnc = `\\\\wsl.localhost\\${wslDistribution}${rawJoined.replace(/\//gu, '\\')}`;
    const resolvedUnc = canonicalizePathStrictDotDot(rawUnc);
    return Boolean(resolvedUnc && existsSync(resolvedUnc));
  }
  const resolved = canonicalizePathStrictDotDot(rawJoined);
  return Boolean(resolved && existsSync(resolved));
}

function candidateRootsForRelativeTarget(
  roots: readonly string[],
  rawTarget: string,
  usePosix: boolean,
  wslDistribution?: string,
): Set<string> {
  const perTargetMatches = new Set<string>();
  const meta = cleanTargetStringWithMeta(rawTarget, wslDistribution);
  const rawInput = (
    meta.isUri && meta.uriFilePart !== undefined ? meta.uriFilePart : meta.cleaned
  ).trim();
  const normalizeRelSep = (value: string): string => {
    const isWinLike =
      /^[a-zA-Z]:[\\/]/u.test(value) || value.startsWith('\\\\') || (!usePosix && process.platform === 'win32');
    const sepNorm = isWinLike ? value.replace(/\\/gu, '/') : value;
    return sepNorm.replace(/^(?:\.\/)+/u, '').replace(/\/+$/u, '');
  };
  const isParentTraversal = (value: string): boolean => value === '..' || value.startsWith('../');
  const rawCleaned = normalizeRelSep(rawInput);
  if (!rawCleaned || rawCleaned.startsWith('/') || isParentTraversal(rawCleaned)) {
    return perTargetMatches;
  }
  // Build right-to-left `::` candidates (including the full path first) so relative literal
  // `::` filenames (`tests/foo::bar.py` or `tests/foo::bar.py::test_a`) match their owning root.
  const relCandidates: string[] = [];
  const pushUnique = (cand: string): void => {
    const literal = normalizeRelSep(cand.trim());
    if (
      !meta.isUri &&
      literal &&
      !literal.startsWith('/') &&
      !isParentTraversal(literal) &&
      !relCandidates.includes(literal)
    ) {
      relCandidates.push(literal);
    }
    const stripped = normalizeRelSep(stripFragmentAndLineSuffix(cand.trim(), meta.isUri, undefined, wslDistribution));
    if (stripped && !stripped.startsWith('/') && !isParentTraversal(stripped) && !relCandidates.includes(stripped)) {
      relCandidates.push(stripped);
    }
  };
  if (rawCleaned.includes('::') && !(meta.isUri && meta.uriFilePart !== undefined)) {
    const splitIndices: number[] = [];
    let pos = rawCleaned.indexOf('::');
    while (pos >= 0) {
      splitIndices.push(pos);
      pos = rawCleaned.indexOf('::', pos + 2);
    }
    pushUnique(rawCleaned);
    for (let i = splitIndices.length - 1; i >= 0; i -= 1) {
      const idx = splitIndices[i];
      if (idx !== undefined && idx > 0) {
        pushUnique(rawCleaned.slice(0, idx));
      }
    }
  } else {
    pushUnique(rawCleaned);
  }
  for (const cleaned of relCandidates) {
    // Match roots using the EXACT same unstripped relative-path semantics as
    // `normalizeTargetForRoot` (never stripping a leading root basename segment, which would
    // misroute `vscode-extension/src/rootResolver.ts` to nested `/repo/vscode-extension`!).
    for (const root of roots) {
      if (checkRootRelativeExists(root, cleaned, usePosix, wslDistribution)) {
        perTargetMatches.add(root);
      }
    }
    if (perTargetMatches.size > 0) {
      break;
    }
  }
  return perTargetMatches;
}

export function matchingRootsByRelativePaths(
  roots: readonly string[],
  relativeTargets: readonly string[],
  usePosix = false,
  wslDistribution?: string,
): string[] | undefined {
  if (roots.length <= 1 || relativeTargets.length === 0) {
    return undefined;
  }
  // Intersect candidate root sets across all relative targets so incompatible target sets
  // (e.g. `only.ts` in root A + `shared.ts` in roots B & C) raise an explicit conflict instead
  // of discarding the second target and selecting A.
  let commonRoots: Set<string> | undefined;
  for (const rawTarget of relativeTargets) {
    const perTargetMatches = candidateRootsForRelativeTarget(
      roots,
      rawTarget,
      usePosix,
      wslDistribution,
    );
    if (perTargetMatches.size > 0) {
      if (commonRoots === undefined) {
        commonRoots = new Set(perTargetMatches);
      } else {
        for (const existing of [...commonRoots]) {
          if (!perTargetMatches.has(existing)) {
            commonRoots.delete(existing);
          }
        }
        if (commonRoots.size === 0) {
          throw new RootConflictError(
            `Targets span multiple incompatible registered workspaces; pass an explicit root or separate requests per workspace.`,
          );
        }
      }
    }
  }
  if (commonRoots && commonRoots.size > 0) {
    return [...commonRoots];
  }
  return undefined;
}

export function disambiguateRootByRelativePaths(
  roots: readonly string[],
  relativeTargets: readonly string[],
  usePosix = false,
  wslDistribution?: string,
): string | undefined {
  const matched = matchingRootsByRelativePaths(
    roots,
    relativeTargets,
    usePosix,
    wslDistribution,
  );
  if (matched && matched.length === 1) {
    return matched[0];
  }
  return undefined;
}

export function findCommonRootForInputHints(
  workspaces: readonly WorkspaceSpec[],
  inputHints: readonly string[],
  usePosix: boolean,
  wslDistribution?: string,
): string | undefined {
  if (inputHints.length === 0) {
    return undefined;
  }
  const roots = usePosix
    ? workspaces
        .map((w) => w.root)
        .filter((r) => posix.isAbsolute(r))
        .map((r) => posix.normalize(r))
    : workspaces.map((w) => resolve(w.root));
  // Intersect candidate root sets across ALL absolute AND relative targets so nested roots
  // (e.g. `/repo` + `/repo/sub` with targets `/repo/sub/a.ts` and `/repo/b.ts`) select their
  // deepest common containing root (`/repo`) instead of failing or dropping a target.
  let commonRoots: Set<string> | undefined;
  let hasUnmatchedAbsolute = false;
  const intersectWith = (candidates: Set<string>): void => {
    if (candidates.size === 0) {
      return;
    }
    if (commonRoots === undefined) {
      commonRoots = new Set(candidates);
      return;
    }
    for (const existing of [...commonRoots]) {
      if (!candidates.has(existing)) {
        commonRoots.delete(existing);
      }
    }
    if (commonRoots.size === 0) {
      throw new RootConflictError(
        `Targets span multiple incompatible registered workspaces; pass an explicit root or separate requests per workspace.`,
      );
    }
  };
  for (const raw of inputHints) {
    if (usePosix) {
      assertWslDistributionMatches(raw, wslDistribution);
    }
    const cleaned = stripLineAndNodeSuffix(raw, wslDistribution);
    if (!cleaned) {
      continue;
    }
    const posixRaw = usePosix ? toPosixPathIfPossible(raw, wslDistribution) : undefined;
    const isAbs = usePosix ? Boolean(posixRaw) : isAbsolute(cleaned);
    if (isAbs) {
      const containing = usePosix
        ? allContainingPosixRoots(roots, posixRaw as string, wslDistribution)
        : allContainingRoots(roots, cleaned);
      if (containing.length > 0) {
        intersectWith(new Set(containing));
      } else {
        hasUnmatchedAbsolute = true;
      }
    } else if (roots.length > 1) {
      intersectWith(candidateRootsForRelativeTarget(roots, raw, usePosix, wslDistribution));
    }
  }
  if (hasUnmatchedAbsolute) {
    if (commonRoots && commonRoots.size > 0) {
      throw new RootConflictError(
        `Targets span both registered and unregistered workspaces; pass an explicit root or separate requests per workspace.`,
      );
    }
    return undefined;
  }
  if (!commonRoots || commonRoots.size === 0) {
    return undefined;
  }
  const sorted = [...commonRoots].sort((left, right) => right.length - left.length);
  const deepest = sorted[0];
  if (!deepest) {
    return undefined;
  }
  // Only return a resolved root when `commonRoots` is unambiguous (size 1) OR all remaining
  // roots are strict ancestors of `deepest` (nested workspaces like `/repo` + `/repo/sub`).
  // For ambiguous disjoint sibling roots (e.g. both `/repoA` and `/repoB` have `shared.ts`),
  // return `undefined` so ambient VS Code workspace/editor candidates disambiguate cleanly.
  const allContainDeepest = sorted.every((root) =>
    usePosix ? containsPosix(root, deepest, wslDistribution) : contains(root, deepest),
  );
  return allContainDeepest ? deepest : undefined;
}

export function assertTargetsInsideRoot(
  resolvedRoot: string,
  inputHints: readonly string[],
  usePosix: boolean,
  wslDistribution?: string,
): void {
  if (inputHints.length === 0) {
    return;
  }
  const normRoot = usePosix ? posix.normalize(resolvedRoot) : resolve(resolvedRoot);
  for (const raw of inputHints) {
    if (usePosix) {
      assertWslDistributionMatches(raw, wslDistribution);
    }
    const cleaned = stripLineAndNodeSuffix(raw, wslDistribution);
    if (!cleaned) {
      continue;
    }
    // Also validate literal `::` filenames AND all right-to-left `::` prefixes (e.g. a symlink
    // named `escape::foo.py` invoked as `escape::foo.py::test_a`!) BEFORE `stripLineAndNodeSuffix`
    // splits on `::`.
    const rawMeta = cleanTargetStringWithMeta(raw, wslDistribution);
    const literalFull = (
      rawMeta.isUri && rawMeta.uriFilePart !== undefined ? rawMeta.uriFilePart : rawMeta.cleaned
    ).trim();
    if (literalFull.includes('::') && literalFull !== cleaned) {
      const pathExistsOrSymlink = (p: string): boolean => {
        if (existsSync(p)) {
          return true;
        }
        try {
          lstatSync(p);
          return true;
        } catch {
          return false;
        }
      };
      const litPrefixes: string[] = [];
      let curLit = literalFull;
      while (curLit && curLit !== cleaned) {
        litPrefixes.push(curLit);
        const sepPos = curLit.lastIndexOf('::');
        if (sepPos <= 0) {
          break;
        }
        curLit = curLit.slice(0, sepPos).trim();
      }
      for (const litCand of litPrefixes) {
        if (usePosix) {
          const litTarget = litCand.startsWith('/')
            ? litCand
            : `${normRoot.replace(/\/+$/u, '')}/${litCand}`;
          const litExists =
            process.platform !== 'win32'
              ? pathExistsOrSymlink(litTarget)
              : Boolean(
                  wslDistribution &&
                    pathExistsOrSymlink(
                      `\\\\wsl.localhost\\${wslDistribution}${litTarget.replace(/\//gu, '\\')}`,
                    ),
                );
          if (litExists && !containsPosix(normRoot, litTarget, wslDistribution)) {
            throw new RootConflictError(
              `Target '${litCand}' escapes selected workspace root '${normRoot}'.`,
            );
          }
        } else {
          const litTarget = isAbsolute(litCand)
            ? litCand
            : `${normRoot.replace(/[\\/]+$/u, '')}${sep}${litCand}`;
          if (pathExistsOrSymlink(litTarget) && !contains(normRoot, litTarget)) {
            throw new RootConflictError(
              `Target '${litCand}' escapes selected workspace root '${normRoot}'.`,
            );
          }
        }
      }
    }
    if (usePosix) {
      // Strictly validate raw POSIX absolute paths containing `..` BEFORE calling
      // `toPosixPathIfPossible` (which canonicalizes non-strictly), so `<root>/src/ast_graph.rs/../..`
      // or `<root>/src/no-such-dir/../..` is rejected immediately!
      if (cleaned.startsWith('/') && /(?:^|\/)\.\.(?:$|\/)/u.test(cleaned)) {
        const strictAbs = canonicalizePosixPathIfPossible(cleaned, wslDistribution, true);
        if (strictAbs === cleaned) {
          throw new RootConflictError(
            `Target '${cleaned}' traverses through a missing or non-directory component.`,
          );
        }
      }
      const mappedPosix = toPosixPathIfPossible(raw, wslDistribution);
      if (mappedPosix) {
        if (!containsPosix(normRoot, mappedPosix, wslDistribution)) {
          throw new RootConflictError(
            `Target '${cleaned}' is outside selected workspace root '${normRoot}'; pass matching targets or separate requests per workspace.`,
          );
        }
        continue;
      }
      // Reject unmappable Windows drive/UNC absolute paths in WSL mode (e.g. `C:\other\test.py`)
      // rather than silently omitting them.
      if (/^[a-zA-Z]:[\\/]/u.test(cleaned) || cleaned.startsWith('\\\\') || cleaned.startsWith('//')) {
        throw new RootConflictError(
          `Target '${cleaned}' cannot be mapped inside WSL workspace root '${normRoot}'; pass a Linux/WSL path under '${normRoot}'.`,
        );
      }
      // Reject invalid `..` traversals through missing or non-directory components
      // (e.g. `src/no-such-directory/../..` or `src/ast_graph.rs/../..`)!
      const rawRelTarget = `${normRoot.replace(/\/+$/u, '')}/${cleaned}`;
      if (
        /(?:^|\/)\.\.(?:$|\/)/u.test(cleaned) &&
        canonicalizePosixPathIfPossible(rawRelTarget, wslDistribution, true) === rawRelTarget
      ) {
        throw new RootConflictError(
          `Relative target '${cleaned}' traverses through a missing or non-directory component under '${normRoot}'.`,
        );
      }
      if (!containsPosix(normRoot, rawRelTarget, wslDistribution)) {
        throw new RootConflictError(
          `Relative target '${cleaned}' escapes selected workspace root '${normRoot}'.`,
        );
      }
      // Also validate the post-normalization target so basename-stripping (`repo/../other/test.py`)
      // can never produce a path escaping `normRoot`.
      const normTarget = normalizeTargetForRoot(normRoot, raw, wslDistribution).split('::')[0] ?? '';
      if (
        normTarget &&
        !containsPosix(
          normRoot,
          `${normRoot.replace(/\/+$/u, '')}/${normTarget}`,
          wslDistribution,
        )
      ) {
        throw new RootConflictError(
          `Normalized target '${normTarget}' escapes selected workspace root '${normRoot}'.`,
        );
      }
    } else {
      const rootPrefix = normRoot.replace(/[\\/]+$/u, '');
      const rawTargetPath = isAbsolute(cleaned) ? cleaned : `${rootPrefix}${sep}${cleaned}`;
      if (
        /(?:^|[\\/])\.\.(?:$|[\\/])/u.test(rawTargetPath) &&
        canonicalizePathStrictDotDot(rawTargetPath) === undefined
      ) {
        throw new RootConflictError(
          `Target '${cleaned}' traverses through a missing or non-directory component.`,
        );
      }
      if (!contains(normRoot, rawTargetPath)) {
        throw new RootConflictError(
          `Target '${cleaned}' is outside selected workspace root '${normRoot}'; pass matching targets or separate requests per workspace.`,
        );
      }
      const normTarget = normalizeTargetForRoot(normRoot, raw, wslDistribution).split('::')[0] ?? '';
      if (normTarget && !contains(normRoot, `${rootPrefix}${sep}${normTarget}`)) {
        throw new RootConflictError(
          `Normalized target '${normTarget}' escapes selected workspace root '${normRoot}'.`,
        );
      }
    }
  }
}

export function validateCrossRootInputHints(
  workspaces: readonly WorkspaceSpec[],
  inputHints: readonly string[],
  usePosix: boolean,
  wslDistribution?: string,
): void {
  findCommonRootForInputHints(workspaces, inputHints, usePosix, wslDistribution);
}

export function resolveWslRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[] = [],
  relativeTargets: readonly string[] = [],
  wslDistribution?: string,
  openFolderCandidates?: readonly string[],
): string {
  const posixRoots = workspaces
    .map((w) => w.root)
    .filter((r) => posix.isAbsolute(r))
    .map((r) => posix.normalize(r));

  if (explicitPath !== undefined && explicitPath.trim().length > 0) {
    assertWslDistributionMatches(explicitPath, wslDistribution);
    const cleanedExplicit = stripLineAndNodeSuffix(
      explicitPath,
      wslDistribution ?? extractWslUncDistribution(explicitPath),
    );
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
    const uncConverted = toPosixPathIfPossible(
      explicitPath,
      wslDistribution ?? extractWslUncDistribution(explicitPath),
    );
    const rawTarget = uncConverted ?? cleanedExplicit;
    if (!posix.isAbsolute(rawTarget) || /[\0\r\n]/u.test(rawTarget)) {
      throw new RootResolutionError('WSL Test Context root must be an absolute Linux path.');
    }
    const normalized = posix.normalize(rawTarget);
    return bestContainingPosixRoot(posixRoots, normalized, wslDistribution) ?? normalized;
  }

  // Validate ambient WSL distribution in strict candidate priority order BEFORE relative-target
  // selection so an active Debian workspace with Ubuntu configured raises a conflict immediately
  // even when an older Ubuntu tab also exists in `candidates`!
  let mismatchedAmbientDistro: string | undefined;
  let hasValidPosixCandidate = false;
  for (const candidate of candidates) {
    if (wslDistribution) {
      const candDistro = extractWslUncDistribution(candidate);
      if (candDistro && candDistro.toLowerCase() !== wslDistribution.toLowerCase()) {
        if (!hasValidPosixCandidate) {
          // The FIRST authoritative WSL candidate belongs to a conflicting distribution!
          throw new RootConflictError(
            `Active workspace belongs to WSL distribution '${candDistro}', which conflicts with configured distribution '${wslDistribution}'.`,
          );
        }
        mismatchedAmbientDistro ??= candDistro;
        continue;
      }
    }
    if (toPosixPathIfPossible(candidate, wslDistribution)) {
      hasValidPosixCandidate = true;
    }
  }

  // In a VS Code window with one or more open folders (whether registered OR unregistered!),
  // match `relativeTargets` against ALL open folders FIRST so:
  // - Two open folders route to the folder actually owning the target (not folder 0).
  // - A sole nested open folder `/repo/app` inside a registered parent `/repo` does NOT silently
  //   misroute `tests/a.py` to `/repo/tests/a.py`!
  // - A symlinked open folder pointing to an already-registered canonical root matches via
  //   canonical equality instead of being rejected by lexical comparison.
  const sameCanonicalPosix = (a: string, b: string): boolean =>
    canonicalizePosixPathIfPossible(a, wslDistribution) ===
    canonicalizePosixPathIfPossible(b, wslDistribution);
  if (
    relativeTargets.length > 0 &&
    openFolderCandidates &&
    openFolderCandidates.length >= 1
  ) {
    const allOpenPosix: string[] = [];
    for (const folder of openFolderCandidates) {
      const pf = toPosixPathIfPossible(folder, wslDistribution);
      if (pf) {
        const normPf = posix.normalize(pf);
        if (!allOpenPosix.includes(normPf)) {
          allOpenPosix.push(normPf);
        }
      }
    }
    if (allOpenPosix.length === 1) {
      const soleOpen = allOpenPosix[0] as string;
      const regMatch = bestContainingPosixRoot(posixRoots, soleOpen, wslDistribution);
      if (regMatch && sameCanonicalPosix(regMatch, soleOpen)) {
        return regMatch;
      }
      throw new RootResolutionError(
        `Relative target belongs to unregistered open WSL folder ${soleOpen}.`,
        soleOpen,
      );
    }
    if (allOpenPosix.length > 1) {
      const matchedOpenPosix = matchingRootsByRelativePaths(
        allOpenPosix,
        relativeTargets,
        true,
        wslDistribution,
      );
      if (matchedOpenPosix && matchedOpenPosix.length === 1) {
        const ownerOpen = matchedOpenPosix[0] as string;
        const regMatch = bestContainingPosixRoot(posixRoots, ownerOpen, wslDistribution);
        if (regMatch && sameCanonicalPosix(regMatch, ownerOpen)) {
          return regMatch;
        }
        throw new RootResolutionError(
          `Relative target belongs to unregistered open WSL folder ${ownerOpen}.`,
          ownerOpen,
        );
      }
      if (matchedOpenPosix && matchedOpenPosix.length > 1) {
        // Restrict fallback strictly to the open WSL folders that actually contain the relative
        // targets so an unrelated active folder C is NEVER selected when the targets exist only
        // in open folders A and B!
        for (const candidate of candidates) {
          if (wslDistribution) {
            const candDistro = extractWslUncDistribution(candidate);
            if (candDistro && candDistro.toLowerCase() !== wslDistribution.toLowerCase()) {
              continue;
            }
          }
          const posixCand = toPosixPathIfPossible(candidate, wslDistribution);
          if (posixCand) {
            const ownerFromCand = bestContainingPosixRoot(
              matchedOpenPosix,
              posixCand,
              wslDistribution,
            );
            if (ownerFromCand) {
              const regMatch = bestContainingPosixRoot(
                posixRoots,
                ownerFromCand,
                wslDistribution,
              );
              if (regMatch && sameCanonicalPosix(regMatch, ownerFromCand)) {
                return regMatch;
              }
              throw new RootResolutionError(
                `Relative target belongs to unregistered open WSL folder ${ownerFromCand}.`,
                ownerFromCand,
              );
            }
          }
        }
        throw new RootConflictError(
          `Relative target(s) match multiple WSL workspace folders (${matchedOpenPosix.join(', ')}); pass an explicit root.`,
        );
      }
    }
  }

  // Check open workspace/editor candidates in priority order FIRST! If the FIRST authoritative
  // open workspace candidate is NOT registered (e.g. open unregistered workspace B with an older
  // open tab from registered workspace A), throw immediately so auto-registration registers B
  // instead of skipping past B and hijacking to older tab A!
  const matchedWslCandidateRoots: string[] = [];
  let sawFirstPosixCandidate = false;
  for (const candidate of candidates) {
    // Skip ambient editor/tab candidates from a different WSL distribution so an open tab in
    // e.g. Debian never silently selects an identically-named root in Ubuntu.
    if (wslDistribution) {
      const candDistro = extractWslUncDistribution(candidate);
      if (candDistro && candDistro.toLowerCase() !== wslDistribution.toLowerCase()) {
        continue;
      }
    }
    const posixCand = toPosixPathIfPossible(candidate, wslDistribution);
    if (posixCand) {
      const match = bestContainingPosixRoot(posixRoots, posixCand, wslDistribution);
      if (!sawFirstPosixCandidate) {
        sawFirstPosixCandidate = true;
        if (!match) {
          break;
        }
      }
      if (match && !matchedWslCandidateRoots.includes(match)) {
        matchedWslCandidateRoots.push(match);
      }
    }
  }
  if (matchedWslCandidateRoots.length > 0) {
    return matchedWslCandidateRoots[0] as string;
  }
  if (!hasValidPosixCandidate) {
    const byRelative = disambiguateRootByRelativePaths(
      posixRoots,
      relativeTargets,
      true,
      wslDistribution,
    );
    if (byRelative) {
      return byRelative;
    }
  }

  if (mismatchedAmbientDistro && wslDistribution) {
    throw new RootConflictError(
      `Active workspace belongs to WSL distribution '${mismatchedAmbientDistro}', which conflicts with configured distribution '${wslDistribution}'.`,
    );
  }

  // Only fall back to a sole registered workspace when VS Code has NO open unmatched POSIX
  // workspace candidate; otherwise throw so auto-registration can register the open workspace.
  if (workspaces.length === 1 && workspaces[0]?.root && !hasValidPosixCandidate) {
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
  openFolderCandidates?: readonly string[],
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
    const match = bestContainingRoot(roots, cleanedExplicit);
    if (!match) {
      throw new RootResolutionError(
        `No registered Test Context workspace contains ${cleanedExplicit}. Run tctx register first.`,
      );
    }
    return match;
  }
  // In a VS Code window with one or more open folders (whether registered OR unregistered!),
  // match `relativeTargets` against ALL open folders FIRST so:
  // - Multi-root windows route to the folder actually owning the target (not folder 0).
  // - A sole nested open folder `/repo/app` inside a registered parent `/repo` does NOT silently
  //   misroute `tests/a.py` to `/repo/tests/a.py`!
  // - Symlinked open folders pointing to an already-registered canonical root match via canonical
  //   equality instead of failing lexical equality.
  const sameCanonicalNative = (a: string, b: string): boolean =>
    canonicalizePathIfExists(resolve(a)) === canonicalizePathIfExists(resolve(b));
  if (
    relativeTargets.length > 0 &&
    openFolderCandidates &&
    openFolderCandidates.length >= 1
  ) {
    const allOpenFolders: string[] = [];
    for (const folder of openFolderCandidates) {
      const cf = stripLineAndNodeSuffix(folder);
      if (cf && isAbsolute(cf)) {
        const resolvedF = resolve(cf);
        if (!allOpenFolders.includes(resolvedF)) {
          allOpenFolders.push(resolvedF);
        }
      }
    }
    if (allOpenFolders.length === 1) {
      const soleOpen = allOpenFolders[0] as string;
      const regMatch = bestContainingRoot(roots, soleOpen);
      if (regMatch && sameCanonicalNative(regMatch, soleOpen)) {
        return regMatch;
      }
      throw new RootResolutionError(
        `Relative target belongs to unregistered open workspace folder ${soleOpen}.`,
        soleOpen,
      );
    }
    if (allOpenFolders.length > 1) {
      const matchedOpenFolders = matchingRootsByRelativePaths(
        allOpenFolders,
        relativeTargets,
        false,
      );
      if (matchedOpenFolders && matchedOpenFolders.length === 1) {
        const ownerOpen = matchedOpenFolders[0] as string;
        const regMatch = bestContainingRoot(roots, ownerOpen);
        if (regMatch && sameCanonicalNative(regMatch, ownerOpen)) {
          return regMatch;
        }
        throw new RootResolutionError(
          `Relative target belongs to unregistered open workspace folder ${ownerOpen}.`,
          ownerOpen,
        );
      }
      if (matchedOpenFolders && matchedOpenFolders.length > 1) {
        // Restrict fallback strictly to the open folders that actually contain the relative
        // targets so an unrelated active folder C is NEVER selected when the targets exist only
        // in open folders A and B!
        for (const candidate of candidates) {
          const cleanedCand = stripLineAndNodeSuffix(candidate);
          if (cleanedCand && isAbsolute(cleanedCand)) {
            const ownerFromCand = bestContainingRoot(matchedOpenFolders, cleanedCand);
            if (ownerFromCand) {
              const regMatch = bestContainingRoot(roots, ownerFromCand);
              if (regMatch && sameCanonicalNative(regMatch, ownerFromCand)) {
                return regMatch;
              }
              throw new RootResolutionError(
                `Relative target belongs to unregistered open workspace folder ${ownerFromCand}.`,
                ownerFromCand,
              );
            }
          }
        }
        throw new RootConflictError(
          `Relative target(s) match multiple workspace folders (${matchedOpenFolders.join(', ')}); pass an explicit root.`,
        );
      }
    }
  }
  // Check open workspace/editor candidates in priority order FIRST! If the FIRST authoritative
  // open workspace candidate is NOT registered (e.g. open unregistered workspace B with an older
  // open tab from registered workspace A), stop and throw so auto-registration registers B!
  let hasAbsoluteCandidate = false;
  const matchedCandidateRoots: string[] = [];
  for (const candidate of candidates) {
    const cleanedCand = stripLineAndNodeSuffix(candidate);
    if (cleanedCand && isAbsolute(cleanedCand)) {
      const match = bestContainingRoot(roots, cleanedCand);
      if (!hasAbsoluteCandidate) {
        hasAbsoluteCandidate = true;
        if (!match) {
          break;
        }
      }
      if (match && !matchedCandidateRoots.includes(match)) {
        matchedCandidateRoots.push(match);
      }
    }
  }
  if (matchedCandidateRoots.length > 0) {
    return matchedCandidateRoots[0] as string;
  }
  if (!hasAbsoluteCandidate) {
    const byRelative = disambiguateRootByRelativePaths(roots, relativeTargets, false);
    if (byRelative) {
      return byRelative;
    }
  }
  // Only fall back to a sole registered workspace when VS Code has NO open unmatched workspace
  // candidate; otherwise throw so auto-registration can register the open workspace.
  if (roots.length === 1 && !hasAbsoluteCandidate) {
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

export function resolveWslDirForRegistration(
  posixTarget: string,
  folderPosixCandidates: readonly string[],
  wslDistribution?: string,
): string {
  const normalized = posix.normalize(posixTarget);
  const normalizedFolders = folderPosixCandidates
    .filter((f) => posix.isAbsolute(f))
    .map((f) => posix.normalize(f));
  const containingFolder = bestContainingPosixRoot(normalizedFolders, normalized, wslDistribution);
  if (containingFolder) {
    return containingFolder;
  }
  if (wslDistribution) {
    const uncPath = `\\\\wsl.localhost\\${wslDistribution}${normalized.replace(/\//gu, '\\')}`;
    if (isExistingDirectory(uncPath)) {
      return normalized;
    }
    if (isExistingFile(uncPath)) {
      return posix.dirname(normalized);
    }
  }
  // When no open folder contains the path and UNC probing is unavailable (no configured distro),
  // preserve the path verbatim so `registerRootWithCli` probes the real Linux filesystem inside
  // WSL (`[ -f "$target" ] && target="$(dirname "$target")"`) instead of guessing from dots.
  return normalized;
}

export async function resolveRegisteredRootAfterRefresh(
  workspaces: readonly WorkspaceSpec[],
  refresh: () => Promise<readonly WorkspaceSpec[]>,
  explicitPath: string | undefined,
  candidates: readonly string[],
  registerMissingRoot?: (path: string) => Promise<void>,
  relativeTargets: readonly string[] = [],
  registrationFolderCandidates?: readonly string[],
  inputHints?: readonly string[],
): Promise<string> {
  const hasAbsoluteInputHint =
    inputHints !== undefined &&
    inputHints.some((hint) => {
      const cleaned = stripLineAndNodeSuffix(hint);
      return Boolean(cleaned) && isAbsolute(cleaned);
    });
  const hasOpenFolders =
    registrationFolderCandidates !== undefined && registrationFolderCandidates.length > 0;
  try {
    return resolveRegisteredRoot(
      workspaces,
      explicitPath,
      candidates,
      relativeTargets,
      registrationFolderCandidates,
    );
  } catch (error: unknown) {
    if (error instanceof RootConflictError || !(error instanceof RootResolutionError)) {
      throw error;
    }
    const refreshed = await refresh();
    if (
      explicitPath === undefined &&
      inputHints &&
      inputHints.length > 0 &&
      (hasAbsoluteInputHint || !hasOpenFolders)
    ) {
      const commonRefreshed = findCommonRootForInputHints(refreshed, inputHints, false);
      if (commonRefreshed) {
        return commonRefreshed;
      }
    }
    try {
      return resolveRegisteredRoot(
        refreshed,
        explicitPath,
        candidates,
        relativeTargets,
        registrationFolderCandidates,
      );
    } catch (refreshedError: unknown) {
      if (
        refreshedError instanceof RootConflictError ||
        !(refreshedError instanceof RootResolutionError) ||
        !registerMissingRoot
      ) {
        throw refreshedError;
      }
      const target =
        refreshedError.suggestedRegistrationDir ??
        pickDirectoryTargetForRegistration(
          explicitPath,
          candidates,
          registrationFolderCandidates,
        );
      if (!target || !isAbsolute(target)) {
        throw refreshedError;
      }
      await registerMissingRoot(target);
      const postReg = await refresh();
      if (
        explicitPath === undefined &&
        inputHints &&
        inputHints.length > 0 &&
        (hasAbsoluteInputHint || !hasOpenFolders)
      ) {
        const commonPostReg = findCommonRootForInputHints(postReg, inputHints, false);
        if (commonPostReg) {
          return commonPostReg;
        }
      }
      return resolveRegisteredRoot(
        postReg,
        explicitPath,
        candidates,
        relativeTargets,
        registrationFolderCandidates,
      );
    }
  }
}

function isExistingDirectory(pathStr: string): boolean {
  try {
    return statSync(pathStr).isDirectory();
  } catch {
    return false;
  }
}

function isExistingFile(pathStr: string): boolean {
  try {
    return statSync(pathStr).isFile();
  } catch {
    return false;
  }
}

function pickDirectoryTargetForRegistration(
  explicitPath: string | undefined,
  candidates: readonly string[],
  extraFolderCandidates?: readonly string[],
): string | undefined {
  const cleanedCandidates = candidates
    .map((c) => stripLineAndNodeSuffix(c))
    .filter((c) => Boolean(c) && isAbsolute(c));
  const cleanedExtraFolders = (extraFolderCandidates ?? [])
    .map((c) => stripLineAndNodeSuffix(c))
    .filter((c) => Boolean(c) && isAbsolute(c));
  const existingDirs = [...cleanedCandidates, ...cleanedExtraFolders].filter((c) =>
    isExistingDirectory(c),
  );
  const primary = explicitPath
    ? stripLineAndNodeSuffix(explicitPath)
    : cleanedCandidates[0];
  if (primary && isAbsolute(primary)) {
    const resolvedPrimary = resolve(primary);
    // Prefer a workspace folder directory candidate that contains the target file.
    const containingDir = bestContainingRoot(
      existingDirs.map((d) => resolve(d)),
      resolvedPrimary,
    );
    if (containingDir) {
      return containingDir;
    }
    if (isExistingDirectory(resolvedPrimary)) {
      return resolvedPrimary;
    }
    if (isExistingFile(resolvedPrimary)) {
      // No open workspace folder contains this file; register its parent directory rather than
      // an unrelated open workspace folder.
      return dirname(resolvedPrimary);
    }
  }
  if (existingDirs[0]) {
    return resolve(existingDirs[0]);
  }
  return primary;
}

function allContainingRoots(roots: readonly string[], candidate: string): string[] {
  return roots.filter((root) => contains(root, candidate));
}

function canonicalizePosixPathIfPossible(
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

function allContainingPosixRoots(
  roots: readonly string[],
  candidate: string,
  wslDistribution?: string,
): string[] {
  return roots.filter((root) => containsPosix(root, candidate, wslDistribution));
}

function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => contains(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

function bestContainingPosixRoot(
  roots: readonly string[],
  candidate: string,
  wslDistribution?: string,
): string | undefined {
  return roots
    .filter((root) => containsPosix(root, candidate, wslDistribution))
    .sort((left, right) => right.length - left.length)[0];
}

function containsPosix(root: string, candidate: string, wslDistribution?: string): boolean {
  const canonRoot = canonicalizePosixPathIfPossible(root, wslDistribution);
  const canonCand = canonicalizePosixPathIfPossible(candidate, wslDistribution);
  const child = posix.relative(canonRoot, canonCand);
  return child === '' || (!child.startsWith('../') && child !== '..' && !posix.isAbsolute(child));
}

function canonicalizePathStrictDotDot(rawPath: string): string | undefined {
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

function canonicalizePathIfExists(rawPath: string): string {
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

function containsLexical(root: string, candidate: string): boolean {
  const child = relative(root, candidate);
  return child === '' || (!child.startsWith(`..${sep}`) && child !== '..' && !isAbsolute(child));
}

function contains(root: string, candidate: string): boolean {
  return containsLexical(
    canonicalizePathIfExists(root),
    canonicalizePathIfExists(candidate),
  );
}

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
