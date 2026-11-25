import assert from 'node:assert/strict';
import test from 'node:test';

import {
  formatToolResult,
  formatTraceLog,
  shouldReportTrace,
  validateDaemonStatus,
} from '../src/resultFormatter.js';
import {
  buildAffectedArguments,
  buildGraphArguments,
  buildInvalidateArguments,
  buildRunArguments,
  normalizeTargetForRoot,
  resolveRegisteredRoot,
} from '../src/rootResolver.js';
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
    suspected_culprit_files: ['src/pkg/service.py'],
    untested_modified_files: ['src/pkg/orphan.py'],
    flaky_tests: ['tests/test_flaky.py'],
    stdout_summary: '=== PASSED (12 passed, 0 failed, 1 skipped) in 0.002s ===',
  });
  assert.match(text, /status=passed mode=instant_cache_hit/);
  assert.match(text, /cached_files=3 executed_files=0/);
  assert.match(text, /saved_ms=1450/);
  assert.match(text, /suspected_culprit_files=src\/pkg\/service\.py/);
  assert.match(text, /untested_modified_files=src\/pkg\/orphan\.py/);
  assert.match(text, /flaky_tests=tests\/test_flaky\.py/);

  const compileErrText = formatToolResult('test_run', {
    status: 'failed',
    mode: 'full_run',
    total_test_files: 1,
    cached_files_count: 0,
    executed_files_count: 1,
    passed: 0,
    failed: 0,
    errors: 1,
    skipped: 0,
    duration_ms: 420,
    saved_ms: 0,
    failed_nodeids: ['crates/sub/src/lib.rs'],
    stdout_summary: '=== FAILED (0 passed, 0 failed, 1 errors, 0 skipped) in 0.420s ===',
  });
  assert.match(compileErrText, /passed=0 failed=0 errors=1 skipped=0/);
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
      watched_directories: 3,
      last_watcher_error: 'watch /repo/future: path not found',
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
  assert.match(status, /^watcher directories=3$/mu);
  assert.match(status, /^watcher error=watch \/repo\/future: path not found$/mu);
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
  assert.doesNotMatch(unloaded, /^watcher /mu);
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
  assert.equal(resolveRegisteredRoot(workspaces, 'sub', []), '/home/user/dev/repo/sub');
  assert.equal(
    resolveRegisteredRoot(workspaces, '[service.py](/home/user/dev/repo/src/service.py:42)', []),
    '/home/user/dev/repo',
  );
});

void test('subagent argument builders normalize aliases, line numbers, URIs, and root-as-file inputs', () => {
  assert.equal(
    normalizeTargetForRoot('/repo', '[test_a.py](file:///repo/tests/test_a.py:18)::test_ok'),
    'tests/test_a.py::test_ok',
  );
  assert.equal(
    normalizeTargetForRoot('/repo', 'file:///repo/tests/test_a.py?version=1#L10'),
    'tests/test_a.py',
  );
  assert.equal(
    normalizeTargetForRoot('/repo', 'file:///repo/tests/test_%3Fa.py?version=2#L5'),
    'tests/test_?a.py',
  );
  assert.deepEqual(
    buildRunArguments(
      {
        root: '/repo/tests/test_service.py::test_Fast',
        filter: 'test_Fast',
        marker: 'unit',
        fix: true,
        args: '-q',
      },
      '/repo',
    ),
    {
      targets: ['tests/test_service.py::test_Fast'],
      rerun_failed: true,
      no_cache: false,
      pytest_args: ['-q'],
      ruff_fix: true,
      k: 'test_Fast',
      mark: 'unit',
    },
  );
  assert.deepEqual(
    buildGraphArguments(
      {
        root: '/repo/src/pkg/service.py:42',
      },
      '/repo',
    ),
    {
      file: 'src/pkg/service.py',
    },
  );
  assert.deepEqual(
    buildAffectedArguments(
      {
        targets: '`/repo/src/a.py:10`, `/repo/src/b.py#L20`',
      },
      '/repo',
    ),
    {
      files: ['src/a.py', 'src/b.py'],
      git_diff: true,
    },
  );
  assert.deepEqual(
    buildInvalidateArguments(
      {
        filePath: '/repo/tests/test_a.py::test_one',
      },
      '/repo',
    ),
    {
      files: ['tests/test_a.py'],
    },
  );
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

void test('UsageSummary ingests daemon status and renders rich Markdown hover tooltip', () => {
  const usage = new UsageSummary();
  usage.record({
    operation: 'test_run',
    root: '/home/dev/demo',
    durationMs: 0.72,
    savedMs: 1_500,
    cachedFiles: 4,
    executedFiles: 0,
    mode: 'instant_cache_hit',
  });
  const snap = usage.ingestDaemonStatus(
    {
      daemon: {
        ok: true,
        version: '0.1.4',
        pid: 4242,
        uptime_secs: 185,
        registered_roots: 3,
        loaded_roots: ['/home/dev/demo'],
        metrics: {
          total_requests: 28,
          error_requests: 0,
          test_runs: 12,
          total_saved_ms: 42_500,
          total_cached_files: 48,
          total_executed_files: 6,
        },
      },
      workspace: {
        root: '/home/dev/demo',
        indexed_total_files: 120,
        indexed_python_files: 50,
        indexed_rust_files: 40,
        indexed_frontend_files: 25,
        total_test_files: 18,
        total_python_test_files: 8,
        total_rust_test_files: 6,
        total_frontend_test_files: 4,
        cached_passed_files: 17,
        cached_failed_files: 0,
        uncached_or_stale_files: 1,
        ruff_cached_files: 115,
        ruff_diagnostics_count: 2,
        background_running: true,
        background_pending_queue: 1,
        background_in_flight: 1,
        background_batches: 5,
        total_background_runs: 9,
        total_cache_hits: 30,
        total_delta_runs: 4,
        total_saved_ms: 42_500,
        watched_directories: 16,
        watcher_overflow_pending: false,
        last_watcher_error: 'bad [link](command:evil) | pipe',
        cache_storage: {
          estimated_memory_bytes: 32_768,
          history_records: 24,
          wal_bytes: 12_288,
        },
        zygote: {
          ready: true,
          pid: 5151,
          runs: 7,
          rss_bytes: 94_371_840,
          circuit_open: false,
        },
      },
    },
    'Unix socket',
    Date.UTC(2026, 10, 25, 7, 30, 0),
  );
  assert.ok(snap);
  assert.equal(usage.hasRenderableData, true);
  assert.match(usage.text(), /~43s saved/);
  assert.match(usage.text(), /17\/18 cached/);
  assert.match(usage.text(), /2 lint/);
  assert.match(usage.text(), /BG 2/);

  const md = usage.markdownTooltip();
  assert.match(md, /### \$\(beaker\) Test Context — demo · Unix socket/);
  assert.match(md, /\| ⏱️ Time Saved \| 🎯 Cache Hit Rate \| 🧪 Cached Tests \| 🔍 Lint Issues \|/);
  assert.match(md, /88\.9%.*48\/54/);
  assert.match(md, /Python \(`pytest`\)/);
  assert.match(md, /Rust \(`cargo test`\)/);
  assert.match(md, /Frontend \(`vitest`\/`jest`\/`node`\)/);
  assert.match(md, /C\/C\+\+ · C# · Data\/Config/);
  assert.match(md, /Pytest Zygote: 🟢 Ready \(PID `5151`, `7` runs, `90\.0 MiB`\)/);
  assert.match(md, /Extension Host Native Tool Breakdown/);
  assert.match(md, /\[.*Show Status\]\(command:testContextNative\.showStatus\)/);
  assert.match(md, /\[.*Sync Graph\]\(command:testContextNative\.syncWorkspace\)/);
  assert.match(md, /\[.*Self-Test\]\(command:testContextNative\.selfTest\)/);
  // Untrusted watcher error strings must be markdown-escaped so they cannot inject command links
  assert.ok(!md.includes('[link](command:evil)'));
  assert.match(md, /bad \\\[link\\\]\(command:evil\) \\\| pipe/);
});
