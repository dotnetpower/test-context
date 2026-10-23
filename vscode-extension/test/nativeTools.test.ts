import assert from 'node:assert/strict';
import test from 'node:test';

import { formatToolResult, validateDaemonStatus } from '../src/resultFormatter.js';
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
  });
  assert.match(text, /affected_test_files=1/);
  assert.match(
    text,
    /tests\/test_service\.py state=cached_passed chain=\[src\/pkg\/math\.py -> src\/pkg\/service\.py -> tests\/test_service\.py\]/,
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
