import type { JsonValue } from './types.js';

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
      lines.push(summary);
      return lines.join('\n');
    }
    case 'test_affected': {
      const result = recordValue(value, 'test_affected result');
      const total = integerValue(result.total_affected_test_files, 'total_affected_test_files');
      const items = arrayValue(result.affected_tests, 'affected_tests');
      const lines = [`affected_test_files=${total}`];
      for (const [idx, raw] of items.entries()) {
        const item = recordValue(raw, `affected test ${idx}`);
        const file = stringValue(item.test_file, 'test_file');
        const state = stringValue(item.cache_state, 'cache_state');
        const chain = arrayValue(item.reason_chain, 'reason_chain')
          .map((c) => stringValue(c, 'chain step'))
          .join(' -> ');
        lines.push(`${file} state=${state} chain=[${chain}]`);
      }
      return lines.join('\n');
    }
    default:
      return JSON.stringify(value, null, 2);
  }
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
