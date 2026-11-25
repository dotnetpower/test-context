import { basename, dirname, isAbsolute, posix, resolve } from 'node:path';

import {
  assertWslDistributionMatches,
  bestContainingPosixRoot,
  bestContainingRoot,
  canonicalizePathIfExists,
  canonicalizePosixPathIfPossible,
  extractWslUncDistribution,
  isExistingDirectory,
  isExistingFile,
  RootConflictError,
  RootResolutionError,
} from './pathContainment.js';
import {
  assertTargetsInsideRoot,
  cleanTargetString,
  disambiguateRootByRelativePaths,
  findCommonRootForInputHints,
  matchingRootsByRelativePaths,
  normalizeTargetForRoot,
  stripLineAndNodeSuffix,
  toPosixPathIfPossible,
  validateCrossRootInputHints,
} from './targetPath.js';
import {
  buildAffectedArguments,
  buildGraphArguments,
  buildInvalidateArguments,
  buildRunArguments,
  extractInputPathHints,
  extractRawRootInput,
} from './toolArgBuilder.js';
import type { WorkspaceSpec } from './types.js';

export {
  assertTargetsInsideRoot,
  assertWslDistributionMatches,
  buildAffectedArguments,
  buildGraphArguments,
  buildInvalidateArguments,
  buildRunArguments,
  cleanTargetString,
  disambiguateRootByRelativePaths,
  extractInputPathHints,
  extractRawRootInput,
  extractWslUncDistribution,
  findCommonRootForInputHints,
  matchingRootsByRelativePaths,
  normalizeTargetForRoot,
  RootConflictError,
  RootResolutionError,
  stripLineAndNodeSuffix,
  toPosixPathIfPossible,
  validateCrossRootInputHints,
};

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

