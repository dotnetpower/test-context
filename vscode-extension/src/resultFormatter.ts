import type { JsonValue } from './types.js';
import type { ClientTrace } from './daemonClient.js';

const MAX_TOOL_OUTPUT_CHARS = 12_000;
const MAX_FAILED_NODEIDS = 30;
const MAX_INLINE_LIST = 20;
const MAX_READABLE_DEPTH = 4;
const MAX_READABLE_LINES = 120;

function safeSlicePrefix(text: string, maxUnits: number): string {
  if (maxUnits <= 0) {
    return '';
  }
  if (text.length <= maxUnits) {
    return text;
  }
  let end = maxUnits;
  // Never split a UTF-16 surrogate pair
  const lastCode = text.charCodeAt(end - 1);
  if (lastCode >= 0xd800 && lastCode <= 0xdbff) {
    end -= 1;
  }
  return text.slice(0, end);
}

function safeSliceSuffix(text: string, maxUnits: number): string {
  if (maxUnits <= 0) {
    return '';
  }
  if (text.length <= maxUnits) {
    return text;
  }
  let start = text.length - maxUnits;
  // Never start on a trailing low surrogate
  const firstCode = text.charCodeAt(start);
  if (firstCode >= 0xdc00 && firstCode <= 0xdfff) {
    start += 1;
  }
  return text.slice(start);
}

function capOutput(text: string, maxChars = MAX_TOOL_OUTPUT_CHARS): string {
  if (text.length <= maxChars) {
    return text;
  }
  const probeMarker = `\n... [${text.length} chars omitted]`;
  const keep = Math.max(0, maxChars - probeMarker.length);
  const head = safeSlicePrefix(text, keep);
  const omitted = text.length - head.length;
  const result = `${head}\n... [${omitted} chars omitted]`;
  return result.length <= maxChars ? result : safeSlicePrefix(result, maxChars);
}

function capHeadTail(text: string, maxChars: number): string {
  if (text.length <= maxChars) {
    return text;
  }
  const probeMarker = `\n... [${text.length} chars omitted; rerun via tctx CLI for full log] ...\n`;
  const available = Math.max(0, maxChars - probeMarker.length);
  if (available < 32) {
    return capOutput(text, maxChars);
  }
  const headChars = Math.floor(available * 0.35);
  const tailChars = available - headChars;
  const head = safeSlicePrefix(text, headChars);
  const tail = safeSliceSuffix(text, tailChars);
  const omitted = Math.max(0, text.length - head.length - tail.length);
  const result = `${head}\n... [${omitted} chars omitted; rerun via tctx CLI for full log] ...\n${tail}`;
  return result.length <= maxChars ? result : safeSlicePrefix(result, maxChars);
}

function capList(items: string[], maxItems: number): string {
  if (items.length <= maxItems) {
    return items.join(', ');
  }
  return `${items.slice(0, maxItems).join(', ')} (+${items.length - maxItems} more)`;
}

function packCompleteList(
  label: string,
  entries: string[],
  maxItems: number,
  maxChars: number,
): string {
  const prefix = `${label}=`;
  const reserveSuffix = 20;
  const budget = Math.max(80, maxChars - prefix.length - reserveSuffix);
  const kept: string[] = [];
  let used = 0;
  for (const entry of entries) {
    if (kept.length >= maxItems) {
      break;
    }
    const sep = kept.length > 0 ? 2 : 0;
    if (used + sep + entry.length > budget) {
      continue;
    }
    kept.push(entry);
    used += sep + entry.length;
  }
  const omitted = Math.max(0, entries.length - kept.length);
  const suffix = omitted > 0 ? ` (+${omitted} omitted)` : '';
  const body = kept.join(', ');
  return `${prefix}${body.length > 0 && suffix.length > 0 ? `${body}${suffix}` : body || suffix.trim()}`;
}

export function formatToolResult(operation: string, value: JsonValue): string {
  switch (operation) {
    case 'test_run': {
      const result = recordValue(value, 'test_run result');
      const status = stringValue(result.status, 'status');
      const mode = stringValue(result.mode, 'mode');
      const totalFiles = integerValue(result.total_test_files, 'total_test_files');
      const cachedFiles = integerValue(result.cached_files_count, 'cached_files_count');
      const executedFiles = integerValue(result.executed_files_count, 'executed_files_count');
      const passed = integerValue(result.passed, 'passed');
      const failed = integerValue(result.failed, 'failed');
      const skipped = integerValue(result.skipped, 'skipped');
      const errors = typeof result.errors === 'number' ? integerValue(result.errors, 'errors') : 0;
      const durationMs = integerValue(result.duration_ms, 'duration_ms');
      const savedMs = integerValue(result.saved_ms, 'saved_ms');
      const summary = stringValue(result.stdout_summary, 'stdout_summary');
      const failedNodeids = arrayValue(result.failed_nodeids, 'failed_nodeids').map((x) =>
        stringValue(x, 'failed nodeid'),
      );
      const errorsField = errors > 0 ? ` errors=${errors}` : '';
      const headerLine = capOutput(
        `status=${status} mode=${mode} total_files=${totalFiles} cached_files=${cachedFiles} executed_files=${executedFiles} passed=${passed} failed=${failed}${errorsField} skipped=${skipped} duration_ms=${durationMs} saved_ms=${savedMs}`,
        400,
      );
      // Budget each metadata field independently so their sum is strictly <= 4,000 chars and
      // untested_modified_files / flaky_tests can never be dropped by long failed_nodeids or failures.
      const metaLines: string[] = [];
      if (failedNodeids.length > 0) {
        // Scan all failed IDs (keeping up to MAX_FAILED_NODEIDS complete IDs within budget)
        // so oversized early IDs never hide shorter later IDs, and keep the omission count outside the budget.
        const idBudget = 1_100;
        const keptIds: string[] = [];
        let usedChars = 0;
        for (const id of failedNodeids) {
          if (keptIds.length >= MAX_FAILED_NODEIDS) {
            break;
          }
          const sep = keptIds.length > 0 ? 2 : 0;
          if (usedChars + sep + id.length > idBudget) {
            continue;
          }
          keptIds.push(id);
          usedChars += sep + id.length;
        }
        const omittedIds = Math.max(0, failedNodeids.length - keptIds.length);
        const suffix = omittedIds > 0 ? `(+${omittedIds} omitted; inspect stdout or rerun target)` : '';
        const listPart = keptIds.join(', ');
        const combined = listPart.length > 0 && suffix.length > 0 ? `${listPart} ${suffix}` : listPart || suffix;
        metaLines.push(`failed_nodeids=${combined}`);
      }
      if (Array.isArray(result.suspected_culprit_files) && result.suspected_culprit_files.length > 0) {
        const culprits = result.suspected_culprit_files.map((c) => stringValue(c, 'culprit file'));
        metaLines.push(`suspected_culprit_files=${capOutput(capList(culprits, MAX_INLINE_LIST), 500)}`);
      }
      if (Array.isArray(result.structured_failures) && result.structured_failures.length > 0) {
        for (const [idx, rawFail] of result.structured_failures.slice(0, 5).entries()) {
          const fail = recordValue(rawFail, `structured failure ${idx}`);
          const rawF = stringValue(fail.file, 'file');
          const fPath = rawF.length <= 70 ? rawF : `...${safeSliceSuffix(rawF, 67)}`;
          const rawNode = stringValue(fail.nodeid, 'nodeid');
          const fNode = rawNode.length <= 80 ? rawNode : `...${safeSliceSuffix(rawNode, 77)}`;
          const rawMsg = stringValue(fail.message, 'message');
          const fMsg = rawMsg.length <= 120 ? rawMsg : `${safeSlicePrefix(rawMsg, 117)}...`;
          metaLines.push(`failure ${fPath}:${integerValue(fail.line, 'line')} ${fNode} ${fMsg}`);
        }
        if (result.structured_failures.length > 5) {
          metaLines.push(`... (+${result.structured_failures.length - 5} more failures)`);
        }
      }
      if (Array.isArray(result.untested_modified_files) && result.untested_modified_files.length > 0) {
        const untested = result.untested_modified_files.map((u) => stringValue(u, 'untested file'));
        metaLines.push(packCompleteList('untested_modified_files', untested, MAX_INLINE_LIST, 450));
      }
      if (Array.isArray(result.flaky_tests) && result.flaky_tests.length > 0) {
        const flaky = result.flaky_tests.map((f) => stringValue(f, 'flaky test'));
        metaLines.push(packCompleteList('flaky_tests', flaky, MAX_INLINE_LIST, 450));
      }
      if (Array.isArray(result.missing_prerequisites) && result.missing_prerequisites.length > 0) {
        const prereqs = result.missing_prerequisites.map((p) => stringValue(p, 'missing prerequisite'));
        metaLines.push(`missing_prerequisites=${capOutput(capList(prereqs, 10), 400)}`);
      }
      if (Array.isArray(result.ruff_diagnostics) && result.ruff_diagnostics.length > 0) {
        const diagLines = [`ruff_errors=${result.ruff_diagnostics.length}`];
        for (const [idx, rawDiag] of result.ruff_diagnostics.slice(0, 5).entries()) {
          const diag = recordValue(rawDiag, `ruff diagnostic ${idx}`);
          const fixTag = diag.fixable === true ? ' [fixable]' : '';
          const rawF = stringValue(diag.file, 'file');
          const fPath = rawF.length <= 70 ? rawF : `...${safeSliceSuffix(rawF, 67)}`;
          const rawMsg = stringValue(diag.message, 'message');
          const msg = rawMsg.length <= 80 ? rawMsg : `${safeSlicePrefix(rawMsg, 77)}...`;
          diagLines.push(
            `ruff ${fPath}:${integerValue(diag.row, 'row')}:${integerValue(diag.column, 'column')} ${stringValue(diag.code, 'code')}${fixTag} ${msg}`,
          );
        }
        if (result.ruff_diagnostics.length > 5) {
          diagLines.push(`... (+${result.ruff_diagnostics.length - 5} more diagnostics)`);
        }
        metaLines.push(capOutput(diagLines.join('\n'), 1_000));
      }
      const prefix = metaLines.length > 0 ? `${headerLine}\n${metaLines.join('\n')}` : headerLine;
      const remainingForSummary = Math.max(2_000, MAX_TOOL_OUTPUT_CHARS - prefix.length - 1);
      return `${prefix}\n${capHeadTail(summary, remainingForSummary)}`;
    }
    case 'test_affected': {
      const result = recordValue(value, 'test_affected result');
      const total = integerValue(result.total_affected_test_files, 'total_affected_test_files');
      const items = arrayValue(result.affected_tests, 'affected_tests');
      // Budget untested files, suggested tests, and ruff diagnostics independently so none can hide another
      const footerParts: string[] = [];
      if (Array.isArray(result.untested_modified_files) && result.untested_modified_files.length > 0) {
        const untested = result.untested_modified_files.map((u) => stringValue(u, 'untested file'));
        footerParts.push(packCompleteList('untested_modified_files', untested, MAX_INLINE_LIST, 750));
      }
      if (Array.isArray(result.suggested_tests) && result.suggested_tests.length > 0) {
        const suggested = result.suggested_tests.map((s) => stringValue(s, 'suggested test'));
        footerParts.push(packCompleteList('suggested_tests', suggested, MAX_INLINE_LIST, 750));
      }
      if (Array.isArray(result.ruff_diagnostics) && result.ruff_diagnostics.length > 0) {
        const ruffFooterLines = [`ruff_errors=${result.ruff_diagnostics.length}`];
        for (const [idx, rawDiag] of result.ruff_diagnostics.slice(0, 10).entries()) {
          const diag = recordValue(rawDiag, `ruff diagnostic ${idx}`);
          const fixTag = diag.fixable === true ? ' [fixable]' : '';
          const rawDiagFile = stringValue(diag.file, 'file');
          const diagFile =
            rawDiagFile.length <= 80 ? rawDiagFile : `...${safeSliceSuffix(rawDiagFile, 77)}`;
          const diagHeader = `ruff ${diagFile}:${integerValue(diag.row, 'row')}:${integerValue(diag.column, 'column')} ${stringValue(diag.code, 'code')}`;
          const msgBudget = Math.max(40, 220 - diagHeader.length - fixTag.length - 1);
          const rawMsg = stringValue(diag.message, 'message');
          const boundedMsg =
            rawMsg.length <= msgBudget
              ? rawMsg
              : `${safeSlicePrefix(rawMsg, Math.max(10, msgBudget - 3))}...`;
          ruffFooterLines.push(`${diagHeader} ${boundedMsg}${fixTag}`);
        }
        if (result.ruff_diagnostics.length > 10) {
          ruffFooterLines.push(`... (+${result.ruff_diagnostics.length - 10} more diagnostics)`);
        }
        footerParts.push(capOutput(ruffFooterLines.join('\n'), 2_500));
      }
      if (typeof result.rust_lint_status === 'string' && result.rust_lint_status.length > 0) {
        footerParts.push(`rust_lint_status=${result.rust_lint_status}`);
      }
      const footerText = footerParts.join('\n');
      const bodyBudget = Math.max(1_500, MAX_TOOL_OUTPUT_CHARS - footerText.length - 1);

      const bodyLines = [`affected_test_files=${total}`];
      const maxInlineAffected = 20;
      // Preserve complete (unmodified) test paths so every path stays directly runnable;
      // bound only the dependency chain (or omit it when a path is very long).
      const perItemCap = Math.max(120, Math.floor((bodyBudget * 0.6) / maxInlineAffected));
      for (const [idx, raw] of items.slice(0, maxInlineAffected).entries()) {
        const item = recordValue(raw, `affected test ${idx}`);
        const file = stringValue(item.test_file, 'test_file');
        const state = stringValue(item.cache_state, 'cache_state');
        const chainSteps = arrayValue(item.reason_chain, 'reason_chain').map((c) =>
          stringValue(c, 'chain step'),
        );
        const chain =
          chainSteps.length > 6
            ? `${chainSteps.slice(0, 2).join(' -> ')} -> ... -> ${chainSteps.slice(-2).join(' -> ')}`
            : chainSteps.join(' -> ');
        const linePrefix = `${file} state=${state}`;
        const chainBudget = perItemCap - linePrefix.length - 9;
        if (chainBudget < 24) {
          bodyLines.push(`${linePrefix} chain=[omitted]`);
        } else {
          let boundedChain = chain;
          if (chain.length > chainBudget) {
            const half = Math.max(8, Math.floor((chainBudget - 5) / 2));
            boundedChain = `${safeSlicePrefix(chain, half)} ... ${safeSliceSuffix(chain, half)}`;
          }
          bodyLines.push(`${linePrefix} chain=[${boundedChain}]`);
        }
      }
      const serverOmissionNotice =
        total > items.length
          ? `... (${total - items.length} more affected test files omitted from preview; run test_run with targets=["."] for full suite verification)`
          : '';
      const reservedForNotice = serverOmissionNotice.length > 0 ? serverOmissionNotice.length + 1 : 0;
      const bodyListBudget = Math.max(1_200, bodyBudget - reservedForNotice);
      // Pack complete inline lines first, tracking exact omitted test counts (never cutting paths mid-string)
      const packedBodyLines: string[] = [];
      let packedChars = 0;
      let omittedInlineTests = 0;
      const reserveBodyOmit = 64;
      for (const [idx, line] of bodyLines.entries()) {
        const sep = packedBodyLines.length > 0 ? 1 : 0;
        if (packedChars + sep + line.length + reserveBodyOmit > bodyListBudget && packedBodyLines.length > 0) {
          // Index 0 is the summary header; subsequent entries are individual test lines
          if (idx > 0) {
            omittedInlineTests += 1;
          }
          continue;
        }
        packedBodyLines.push(line);
        packedChars += sep + line.length;
      }
      let omittedExtraTests = 0;
      if (items.length > maxInlineAffected) {
        const extraSlice = items.slice(maxInlineAffected);
        const extraPrefix = 'additional_affected_tests(chains_omitted)=';
        const remainingForExtra = bodyListBudget - packedChars - 1 - reserveBodyOmit;
        const extraBudget = Math.min(1_700, remainingForExtra - extraPrefix.length - 28);
        if (extraBudget >= 40) {
          const keptExtra: string[] = [];
          let extraChars = 0;
          for (const [i, raw] of extraSlice.entries()) {
            if (keptExtra.length >= 100) {
              break;
            }
            const item = recordValue(raw, `affected test ${i + maxInlineAffected}`);
            const entry = `${stringValue(item.test_file, 'test_file')}(${stringValue(item.cache_state, 'cache_state')})`;
            const sep = keptExtra.length > 0 ? 2 : 0;
            if (extraChars + sep + entry.length > extraBudget) {
              continue;
            }
            keptExtra.push(entry);
            extraChars += sep + entry.length;
          }
          const omittedHere = Math.max(0, extraSlice.length - keptExtra.length);
          if (keptExtra.length > 0) {
            const extraSuffix = omittedHere > 0 ? ` (+${omittedHere} omitted)` : '';
            const extraLine = `${extraPrefix}${keptExtra.join(', ')}${extraSuffix}`;
            packedBodyLines.push(extraLine);
          } else {
            omittedExtraTests = extraSlice.length;
          }
        } else {
          omittedExtraTests = extraSlice.length;
        }
      }
      const totalOmittedTests = omittedInlineTests + omittedExtraTests;
      if (totalOmittedTests > 0) {
        packedBodyLines.push(`... (+${totalOmittedTests} affected tests omitted to fit budget)`);
      }
      const boundedBody = packedBodyLines.join('\n');
      const bodyWithNotice =
        serverOmissionNotice.length > 0 ? `${boundedBody}\n${serverOmissionNotice}` : boundedBody;
      return footerText.length > 0 ? `${bodyWithNotice}\n${footerText}` : bodyWithNotice;
    }
    case 'test_status':
      return capOutput(formatStatus(value));
    case 'test_workspaces':
      return capOutput(formatWorkspaces(value));
    case 'test_graph':
      return capOutput(formatGraph(value));
    case 'test_sync':
    case 'test_invalidate':
      return capOutput(formatReadableValue(value));
    default:
      return capOutput(formatReadableValue(value));
  }
}

function formatGraph(value: JsonValue): string {
  const rec = recordValue(value, 'test_graph result');
  const lines = [
    capOutput(
      `file=${logScalar(rec.path)} lang=${logScalar(rec.language)} is_test=${logScalar(rec.is_test)} is_conftest=${logScalar(rec.is_conftest)}`,
      600,
    ),
  ];
  if (typeof rec.sha256 === 'string') {
    lines.push(`sha256=${rec.sha256}`);
  }
  if (typeof rec.semantic_sha256 === 'string') {
    lines.push(`semantic_sha256=${rec.semantic_sha256}`);
  }
  if (typeof rec.dep_hash === 'string') {
    lines.push(`dep_hash=${rec.dep_hash}`);
  }
  // Budget each graph field independently (max 1,200 chars each across 8 fields <= 9,600 chars)
  // filling complete entries up to the field budget with an explicit omission hint.
  for (const field of [
    'affected_tests',
    'direct_imports',
    'direct_imported_by',
    'conftest_chain',
    'test_symbols',
    'fixtures_defined',
    'env_vars_referenced',
    'transitive_dependencies',
  ]) {
    const arr = rec[field];
    if (Array.isArray(arr) && arr.length > 0) {
      const itemsStr = arr.map(String);
      const fieldPrefix = `${field}(${itemsStr.length})=`;
      const reserveSuffix = 24;
      const itemBudget = Math.max(120, 1_200 - fieldPrefix.length - reserveSuffix);
      const kept: string[] = [];
      let used = 0;
      for (const s of itemsStr) {
        const sep = kept.length > 0 ? 2 : 0;
        if (used + sep + s.length > itemBudget) {
          continue;
        }
        kept.push(s);
        used += sep + s.length;
      }
      const omitted = Math.max(0, itemsStr.length - kept.length);
      const suffix = omitted > 0 ? ` (+${omitted} omitted)` : '';
      const body = kept.join(', ');
      lines.push(`${fieldPrefix}${body.length > 0 && suffix.length > 0 ? `${body}${suffix}` : body || suffix.trim()}`);
    }
  }
  return lines.join('\n');
}

export function formatTraceLog(trace: ClientTrace): string {
  const outcome =
    typeof trace.outcome === 'string'
      ? trace.outcome
      : trace.success
        ? 'success'
        : 'error';
  return [
    '[trace]',
    `operation=${trace.operation}`,
    `outcome=${outcome}`,
    `duration=${trace.total_ms.toFixed(3)}ms`,
    `queue=${trace.queue_ms.toFixed(3)}ms`,
    `prepare=${trace.prepare_ms.toFixed(3)}ms`,
    `ipc=${trace.ipc_ms.toFixed(3)}ms`,
    `serialize=${trace.serialize_ms.toFixed(3)}ms`,
    `root=${trace.root ?? '-'}`,
    `id=${trace.trace_id}`,
  ].join(' ');
}

export function shouldReportTrace(trace: ClientTrace): boolean {
  return trace.available === true && trace.outcome !== 'system_error';
}

export function formatReadableValue(value: JsonValue): string {
  return readableLines(value, 0).join('\n');
}

export function validateDaemonStatus(value: JsonValue): void {
  const status = recordValue(value, 'daemon status');
  const capabilities = recordValue(status.capabilities, 'daemon capabilities');
  if (
    integerValue(capabilities.schema, 'capability schema') < 1 ||
    capabilities.test_run !== true ||
    capabilities.test_affected !== true ||
    capabilities.test_graph !== true ||
    capabilities.request_tracing !== true
  ) {
    throw new Error(
      'Test Context daemon capabilities are outdated; update tctx and reconnect tools.',
    );
  }
}

function recordValue(
  value: JsonValue | undefined,
  label: string,
): { [key: string]: JsonValue } {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error(`Invalid Test Context ${label}: expected an object.`);
  }
  return value;
}

function arrayValue(value: JsonValue | undefined, label: string): JsonValue[] {
  if (!Array.isArray(value)) {
    throw new Error(`Invalid Test Context ${label}: expected an array.`);
  }
  return value;
}

function stringValue(value: JsonValue | undefined, label: string): string {
  if (typeof value !== 'string') {
    throw new Error(`Invalid Test Context ${label}: expected a string.`);
  }
  return value;
}

function integerValue(value: JsonValue | undefined, label: string, minimum = 0): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < minimum) {
    throw new Error(
      `Invalid Test Context ${label}: expected an integer greater than or equal to ${minimum}.`,
    );
  }
  return value;
}

function formatStatus(value: JsonValue): string {
  const status = recordValue(value, 'daemon status');
  const daemon = recordValue(status.daemon, 'daemon');
  const lines = [
    `daemon status=${daemon.ok === true ? 'ready' : 'unavailable'} version=${logScalar(daemon.version)} pid=${logScalar(daemon.pid)} uptime=${logScalar(daemon.uptime_secs)}s registered_roots=${logScalar(daemon.registered_roots)} loaded_roots=${arrayValue(daemon.loaded_roots, 'loaded_roots').length}`,
  ];
  if (status.workspace && typeof status.workspace === 'object' && !Array.isArray(status.workspace)) {
    const workspace = status.workspace;
    if (workspace.loaded === false) {
      lines.push(
        `workspace root=${logScalar(workspace.root)} state=${logScalar(workspace.state)} pytest=${stringList(workspace.pytest_cmd)}`,
      );
      return lines.join('\n');
    }
    const cache =
      workspace.cache_storage &&
      typeof workspace.cache_storage === 'object' &&
      !Array.isArray(workspace.cache_storage)
        ? workspace.cache_storage
        : undefined;
    const zygote =
      workspace.zygote && typeof workspace.zygote === 'object' && !Array.isArray(workspace.zygote)
        ? workspace.zygote
        : undefined;
    lines.push(
      `workspace root=${logScalar(workspace.root)} python_files=${logScalar(workspace.indexed_python_files)} rust_files=${logScalar(workspace.indexed_rust_files, '0')} frontend_files=${logScalar(workspace.indexed_frontend_files, '0')} test_files=${logScalar(workspace.total_test_files)} cached_passed=${logScalar(workspace.cached_passed_files)} stale=${logScalar(workspace.uncached_or_stale_files)} queue=${logScalar(workspace.background_pending_queue)} in_flight=${logScalar(workspace.background_in_flight)}`,
    );
    if (workspace.watched_directories !== undefined) {
      lines.push(`watcher directories=${logScalar(workspace.watched_directories)}`);
    }
    if (typeof workspace.last_watcher_error === 'string' && workspace.last_watcher_error) {
      lines.push(`watcher error=${workspace.last_watcher_error}`);
    }
    if (cache) {
      lines.push(
        `cache history=${logScalar(cache.history_records)} latest=${logScalar(cache.latest_records)} memory=${logScalar(cache.estimated_memory_bytes)}B wal=${logScalar(cache.wal_bytes)}B patterns=${logScalar(cache.invocation_patterns)}`,
      );
    }
    if (zygote) {
      lines.push(
        `zygote ready=${logScalar(zygote.ready, 'false')} pid=${logScalar(zygote.pid)} rss=${logScalar(zygote.rss_bytes)}B runs=${logScalar(zygote.runs, '0')} active_pgid=${logScalar(zygote.active_pgid, '0')}`,
      );
    }
  }
  return lines.join('\n');
}

function formatWorkspaces(value: JsonValue): string {
  const workspaces = arrayValue(value, 'workspaces');
  const lines = [`workspaces count=${workspaces.length}`];
  for (const [index, raw] of workspaces.entries()) {
    const workspace = recordValue(raw, `workspace ${index}`);
    lines.push(
      `workspace root=${logScalar(workspace.root)} scopes=${stringList(workspace.scopes)} pytest=${stringList(workspace.pytest_cmd)}`,
    );
  }
  return lines.join('\n');
}

function stringList(value: JsonValue | undefined): string {
  if (!Array.isArray(value)) {
    return '-';
  }
  return value.map(String).join(',');
}

function logScalar(value: JsonValue | undefined, fallback = '-'): string {
  return value === null || value === undefined || typeof value === 'object'
    ? fallback
    : String(value);
}

function readableLines(
  value: JsonValue,
  depth: number,
  label?: string,
  budget: { remaining: number } = { remaining: MAX_READABLE_LINES },
): string[] {
  if (budget.remaining <= 0) {
    return [];
  }
  const indent = '  '.repeat(depth);
  const prefix = label ? `${label}: ` : '';
  if (value === null || typeof value !== 'object') {
    budget.remaining -= 1;
    return [`${indent}${prefix}${String(value)}`];
  }
  if (depth >= MAX_READABLE_DEPTH) {
    budget.remaining -= 1;
    return [`${indent}${prefix}[max depth reached]`];
  }
  if (Array.isArray(value)) {
    if (value.length === 0) {
      budget.remaining -= 1;
      return [`${indent}${prefix}(none)`];
    }
    const maxArrayItems = 25;
    const lines: string[] = [];
    if (label) {
      lines.push(`${indent}${label}:`);
      budget.remaining -= 1;
    }
    for (const item of value.slice(0, maxArrayItems)) {
      if (budget.remaining <= 0) {
        lines.push(`${indent}- ... (output line limit reached)`);
        break;
      }
      if (item !== null && typeof item === 'object') {
        lines.push(`${indent}-`);
        budget.remaining -= 1;
        lines.push(...readableLines(item, depth + 1, undefined, budget));
      } else {
        lines.push(`${indent}- ${String(item)}`);
        budget.remaining -= 1;
      }
    }
    if (value.length > maxArrayItems && budget.remaining > 0) {
      lines.push(`${indent}- ... (${value.length - maxArrayItems} more)`);
      budget.remaining -= 1;
    }
    return lines;
  }
  const lines: string[] = [];
  if (label) {
    lines.push(`${indent}${label}:`);
    budget.remaining -= 1;
  }
  for (const [key, item] of Object.entries(value)) {
    if (budget.remaining <= 0) {
      lines.push(`${indent}... (output line limit reached)`);
      break;
    }
    lines.push(...readableLines(item, label ? depth + 1 : depth, key, budget));
  }
  return lines;
}
