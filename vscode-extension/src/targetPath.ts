import { existsSync, lstatSync } from 'node:fs';
import { isAbsolute, join, posix, resolve, sep } from 'node:path';

import {
  allContainingPosixRoots,
  allContainingRoots,
  assertWslDistributionMatches,
  canonicalizePathIfExists,
  canonicalizePathStrictDotDot,
  canonicalizePosixPathIfPossible,
  COLON_LINE_RE,
  contains,
  containsPosix,
  extractWslUncDistribution,
  isExistingDirectory,
  isExistingFile,
  MD_LINK_RE,
  REMOTE_URI_RE,
  RootConflictError,
  WSL_FILE_URI_RE,
  WSL_SLASH_UNC_RE,
  WSL_UNC_RE,
} from './pathContainment.js';
import type { WorkspaceSpec } from './types.js';

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

export function cleanTargetStringWithMeta(
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

