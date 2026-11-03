import type { JsonValue } from './types.js';
import type { ClientTrace } from './daemonClient.js';

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
      const durationMs = integerValue(result.duration_ms, 'duration_ms');
      const savedMs = integerValue(result.saved_ms, 'saved_ms');
      const summary = stringValue(result.stdout_summary, 'stdout_summary');
      const failedNodeids = arrayValue(result.failed_nodeids, 'failed_nodeids').map((x) =>
        stringValue(x, 'failed nodeid'),
      );
      const lines = [
        `status=${status} mode=${mode} total_files=${totalFiles} cached_files=${cachedFiles} executed_files=${executedFiles} passed=${passed} failed=${failed} skipped=${skipped} duration_ms=${durationMs} saved_ms=${savedMs}`,
      ];
      if (failedNodeids.length > 0) {
        lines.push(`failed_nodeids=${failedNodeids.join(', ')}`);
      }
      if (Array.isArray(result.suspected_culprit_files) && result.suspected_culprit_files.length > 0) {
        const culprits = result.suspected_culprit_files.map((c) => stringValue(c, 'culprit file'));
        lines.push(`suspected_culprit_files=${culprits.join(', ')}`);
      }
      if (Array.isArray(result.structured_failures) && result.structured_failures.length > 0) {
        for (const [idx, rawFail] of result.structured_failures.slice(0, 5).entries()) {
          const fail = recordValue(rawFail, `structured failure ${idx}`);
          lines.push(
            `failure ${stringValue(fail.file, 'file')}:${integerValue(fail.line, 'line')} ${stringValue(fail.nodeid, 'nodeid')} ${stringValue(fail.message, 'message')}`,
          );
        }
      }
      if (Array.isArray(result.untested_modified_files) && result.untested_modified_files.length > 0) {
        const untested = result.untested_modified_files.map((u) => stringValue(u, 'untested file'));
        lines.push(`untested_modified_files=${untested.join(', ')}`);
      }
      if (Array.isArray(result.flaky_tests) && result.flaky_tests.length > 0) {
        const flaky = result.flaky_tests.map((f) => stringValue(f, 'flaky test'));
        lines.push(`flaky_tests=${flaky.join(', ')}`);
      }
      lines.push(summary);
      return lines.join('\n');
    }
    case 'test_affected': {
      const result = recordValue(value, 'test_affected result');
      const total = integerValue(result.total_affected_test_files, 'total_affected_test_files');
      const items = arrayValue(result.affected_tests, 'affected_tests');
      const lines = [`affected_test_files=${total}`];
      const maxInlineAffected = 30;
      for (const [idx, raw] of items.slice(0, maxInlineAffected).entries()) {
        const item = recordValue(raw, `affected test ${idx}`);
        const file = stringValue(item.test_file, 'test_file');
        const state = stringValue(item.cache_state, 'cache_state');
        const chain = arrayValue(item.reason_chain, 'reason_chain')
          .map((c) => stringValue(c, 'chain step'))
          .join(' -> ');
        lines.push(`${file} state=${state} chain=[${chain}]`);
      }
      if (items.length > maxInlineAffected) {
        lines.push(`... and ${items.length - maxInlineAffected} more affected test files`);
      }
      if (Array.isArray(result.untested_modified_files) && result.untested_modified_files.length > 0) {
        const untested = result.untested_modified_files.map((u) => stringValue(u, 'untested file'));
        lines.push(`untested_modified_files=${untested.join(', ')}`);
      }
      if (Array.isArray(result.suggested_tests) && result.suggested_tests.length > 0) {
        const suggested = result.suggested_tests.map((s) => stringValue(s, 'suggested test'));
        lines.push(`suggested_tests=${suggested.join(', ')}`);
      }
      if (Array.isArray(result.ruff_diagnostics) && result.ruff_diagnostics.length > 0) {
        lines.push(`ruff_errors=${result.ruff_diagnostics.length}`);
        for (const [idx, rawDiag] of result.ruff_diagnostics.slice(0, 10).entries()) {
          const diag = recordValue(rawDiag, `ruff diagnostic ${idx}`);
          const fixTag = diag.fixable === true ? ' [fixable]' : '';
          lines.push(
            `ruff ${stringValue(diag.file, 'file')}:${integerValue(diag.row, 'row')}:${integerValue(diag.column, 'column')} ${stringValue(diag.code, 'code')} ${stringValue(diag.message, 'message')}${fixTag}`,
          );
        }
      }
      return lines.join('\n');
    }
    case 'test_status':
      return formatStatus(value);
    case 'test_workspaces':
      return formatWorkspaces(value);
    case 'test_graph':
    case 'test_sync':
    case 'test_invalidate':
      return formatReadableValue(value);
    default:
      return formatReadableValue(value);
  }
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

function readableLines(value: JsonValue, depth: number, label?: string): string[] {
  const indent = '  '.repeat(depth);
  const prefix = label ? `${label}: ` : '';
  if (value === null || typeof value !== 'object') {
    return [`${indent}${prefix}${String(value)}`];
  }
  if (Array.isArray(value)) {
    if (value.length === 0) {
      return [`${indent}${prefix}(none)`];
    }
    const maxArrayItems = 40;
    const lines = label ? [`${indent}${label}:`] : [];
    for (const item of value.slice(0, maxArrayItems)) {
      if (item !== null && typeof item === 'object') {
        lines.push(`${indent}-`);
        lines.push(...readableLines(item, depth + 1));
      } else {
        lines.push(`${indent}- ${String(item)}`);
      }
    }
    if (value.length > maxArrayItems) {
      lines.push(`${indent}- ... (${value.length - maxArrayItems} more)`);
    }
    return lines;
  }
  const lines = label ? [`${indent}${label}:`] : [];
  for (const [key, item] of Object.entries(value)) {
    lines.push(...readableLines(item, label ? depth + 1 : depth, key));
  }
  return lines;
}
