import assert from 'node:assert/strict';
import test from 'node:test';

import {
  formatToolResult,
  formatTraceLog,
  shouldReportTrace,
  validateDaemonStatus,
} from '../src/resultFormatter.js';
import { resolveRegisteredRoot } from '../src/rootResolver.js';
import { classifyOutcome, UsageSummary } from '../src/usage.js';

void test('formatToolResult formats instant_cache_hit test_run output', () => {
  const text = formatToolResult('test_run', {
    status: 'passed',
    mode: 'instant_cache_hit',
    total_test_files: 3,
    cached_files_count: 3,
    executed_files_count: 0,
    passed: 12,
    failed: 0,
    skipped: 1,
    duration_ms: 2,
    saved_ms: 1450,
    failed_nodeids: [],
    stdout_summary: '=== PASSED (12 passed, 0 failed, 1 skipped) in 0.002s ===',
  });
  assert.match(text, /status=passed mode=instant_cache_hit/);
  assert.match(text, /cached_files=3 executed_files=0/);
  assert.match(text, /saved_ms=1450/);
});

void test('formatToolResult formats test_affected output', () => {
  const text = formatToolResult('test_affected', {
    total_affected_test_files: 1,
    affected_tests: [
      {
        test_file: 'tests/test_service.py',
        cache_state: 'cached_passed',
        reason_chain: ['src/pkg/math.py', 'src/pkg/service.py', 'tests/test_service.py'],
      },
    ],
    ruff_diagnostics: [
      {
        file: 'src/pkg/service.py',
        row: 4,
        column: 1,
        code: 'F401',
        message: '`os` imported but unused',
        fixable: true,
      },
    ],
  });
  assert.match(text, /affected_test_files=1/);
  assert.match(
    text,
    /tests\/test_service\.py state=cached_passed chain=\[src\/pkg\/math\.py -> src\/pkg\/service\.py -> tests\/test_service\.py\]/,
  );
  assert.match(text, /ruff_errors=1/);
  assert.match(text, /ruff src\/pkg\/service\.py:4:1 F401 `os` imported but unused \[fixable\]/);
});

void test('status and workspace logs are readable text rather than JSON', () => {
  const status = formatToolResult('test_status', {
    daemon: {
      ok: true,
      version: '0.1.0',
      pid: 42,
      uptime_secs: 9,
      registered_roots: 2,
      loaded_roots: ['/repo'],
    },
    capabilities: {},
    workspace: {
      root: '/repo',
      indexed_python_files: 20,
      total_test_files: 4,
      cached_passed_files: 3,
      uncached_or_stale_files: 1,
      background_pending_queue: 0,
      background_in_flight: 0,
      cache_storage: {
        history_records: 4,
        latest_records: 4,
        estimated_memory_bytes: 1024,
        wal_bytes: 128,
        invocation_patterns: 1,
      },
      zygote: {
        ready: false,
        pid: null,
        rss_bytes: null,
        runs: 0,
        active_pgid: 0,
      },
    },
  });
  assert.match(status, /^daemon status=ready/mu);
  assert.match(status, /^workspace root=\/repo/mu);
  assert.doesNotMatch(status, /[{}"]/u);

  const workspaces = formatToolResult('test_workspaces', [
    { root: '/repo', scopes: ['.'], pytest_cmd: ['python', '-m', 'pytest'] },
  ]);
  assert.match(workspaces, /^workspaces count=1/mu);
  assert.match(workspaces, /^workspace root=\/repo/mu);
  assert.doesNotMatch(workspaces, /[{}"]/u);

  const unloaded = formatToolResult('test_status', {
    daemon: {
      ok: true,
      version: '0.1.0',
      pid: 42,
      uptime_secs: 9,
      registered_roots: 2,
      loaded_roots: [],
    },
    capabilities: {},
    workspace: {
      root: '/repo',
      loaded: false,
      state: 'registered_unloaded',
      pytest_cmd: ['python', '-m', 'pytest'],
    },
  });
  assert.match(unloaded, /workspace root=\/repo state=registered_unloaded/u);
  assert.doesNotMatch(unloaded, /python_files=/u);
});

void test('trace logs are concise and exclude nested daemon payloads', () => {
  const trace = {
    trace_id: 'trace-1',
    operation: 'test_run',
    root: '/repo',
    queue_ms: 1,
    prepare_ms: 2,
    ipc_ms: 3,
    serialize_ms: 4,
    total_ms: 10,
    started_unix_ms: 1,
    completed_unix_ms: 2,
    success: true,
    available: true,
    outcome: 'success',
    daemon: { secret: 'must-not-be-logged' },
  };
  const line = formatTraceLog(trace);
  assert.match(line, /^\[trace\] operation=test_run outcome=success/u);
  assert.match(line, /duration=10\.000ms/u);
  assert.doesNotMatch(line, /secret|must-not-be-logged|[{}"]/u);
  assert.equal(shouldReportTrace(trace), true);
  assert.equal(
    shouldReportTrace({ ...trace, success: false, outcome: 'system_error' }),
    false,
  );
});

void test('resolveRegisteredRoot selects deepest matching workspace root', () => {
  const workspaces = [
    { root: '/home/user/dev/repo', scopes: ['.'], globs: ['*.py'] },
    { root: '/home/user/dev/repo/sub', scopes: ['.'], globs: ['*.py'] },
  ];
  const resolved = resolveRegisteredRoot(workspaces, '/home/user/dev/repo/sub/tests', []);
  assert.equal(resolved, '/home/user/dev/repo/sub');
});

void test('validateDaemonStatus validates required capabilities', () => {
  validateDaemonStatus({
    capabilities: {
      schema: 1,
      test_run: true,
      test_affected: true,
      test_graph: true,
      request_tracing: true,
    },
  });
});

void test('UsageSummary tracks counts and formats status text', () => {
  const usage = new UsageSummary();
  usage.record({ operation: 'test_run', root: '/tmp/repo' });
  assert.equal(classifyOutcome(undefined), 'success');
  assert.match(usage.text(), /TCTX S:1 E:0/);
});
