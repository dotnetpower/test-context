import { basename } from 'node:path';

export type Outcome = 'success' | 'invalid_request' | 'system_error';

export interface UsageEvent {
  operation: string;
  root: string | null;
  error?: string;
  durationMs?: number;
  savedMs?: number;
  cachedFiles?: number;
  executedFiles?: number;
  passed?: number;
  failed?: number;
  skipped?: number;
  mode?: string;
  ruffIssues?: number;
}

export interface OperationMetrics {
  calls: number;
  successes: number;
  errors: number;
  totalMs: number;
  timedCalls: number;
  savedMs: number;
  cachedFiles: number;
  executedFiles: number;
}

export interface DaemonWorkspaceSnapshot {
  root: string;
  loaded: boolean;
  indexedTotalFiles: number;
  indexedPythonFiles: number;
  indexedRustFiles: number;
  indexedFrontendFiles: number;
  totalTestFiles: number;
  totalPythonTestFiles: number;
  totalRustTestFiles: number;
  totalFrontendTestFiles: number;
  cachedPassedFiles: number;
  cachedFailedFiles: number;
  uncachedOrStaleFiles: number;
  ruffCachedFiles: number;
  ruffDiagnosticsCount: number;
  backgroundRunning: boolean;
  backgroundPendingQueue: number;
  backgroundInFlight: number;
  backgroundBatches: number;
  totalBackgroundRuns: number;
  totalCacheHits: number;
  totalDeltaRuns: number;
  totalSavedMs: number;
  watchedDirectories: number;
  watcherOverflowPending: boolean;
  lastBackgroundError?: string;
  lastWatcherError?: string;
  cacheMemoryBytes: number;
  cacheHistoryRecords: number;
  cacheWalBytes: number;
  zygoteReady: boolean;
  zygotePid?: number;
  zygoteRuns: number;
  zygoteRssBytes?: number;
  zygoteLastError?: string;
  zygoteCircuitOpen: boolean;
}

export interface DaemonStatusSnapshot {
  ok: boolean;
  version: string;
  pid?: number;
  uptimeSecs: number;
  registeredRoots: number;
  loadedRoots: string[];
  totalRequests: number;
  errorRequests: number;
  testRuns: number;
  totalSavedMs: number;
  totalCachedFiles: number;
  totalExecutedFiles: number;
  transport?: string;
  updatedAt: number;
  workspace?: DaemonWorkspaceSnapshot;
}

export function classifyOutcome(error?: string): Outcome {
  if (error === undefined) {
    return 'success';
  }
  if (
    /not indexed|requires|invalid|must be|NO_TESTS_MATCHED|SOURCE_CHANGED_DURING_TEST|TEST_CANCELLED|cancelled/iu.test(
      error,
    )
  ) {
    return 'invalid_request';
  }
  return 'system_error';
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function asNumber(value: unknown, fallback = 0): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function asOptionalNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : undefined;
}

function formatCompactDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return '0.0s';
  }
  if (seconds < 10) {
    return `${seconds.toFixed(1)}s`;
  }
  if (seconds < 60) {
    return `${Math.round(seconds)}s`;
  }
  const mins = Math.floor(seconds / 60);
  const remSecs = Math.round(seconds % 60);
  if (mins < 60) {
    return remSecs > 0 ? `${mins}m ${remSecs}s` : `${mins}m`;
  }
  const hrs = Math.floor(mins / 60);
  const remMins = mins % 60;
  return remMins > 0 ? `${hrs}h ${remMins}m` : `${hrs}h`;
}

function formatSmartMs(ms: number | undefined): string {
  if (ms === undefined || !Number.isFinite(ms)) {
    return '-';
  }
  if (ms < 1) {
    return `${ms.toFixed(2)}ms`;
  }
  if (ms < 100) {
    return `${ms.toFixed(1)}ms`;
  }
  if (ms < 1_000) {
    return `${Math.round(ms)}ms`;
  }
  return `${(ms / 1_000).toFixed(2)}s`;
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return '0 B';
  }
  if (bytes < 1024) {
    return `${Math.round(bytes)} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function formatLocalTime(epochMs: number): string {
  const d = new Date(epochMs);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return `${hh}:${mm}:${ss}`;
}

function escapeMdCell(raw: string): string {
  return raw
    .replace(/\\/gu, '\\\\')
    .replace(/\|/gu, '\\|')
    .replace(/\[/gu, '\\[')
    .replace(/\]/gu, '\\]')
    .replace(/`/gu, "'")
    .replace(/\r?\n/gu, ' ');
}

function shortRootLabel(rootPath: string): string {
  const normalized = rootPath.replace(/[\\/]+$/u, '');
  const base = basename(normalized);
  return base || normalized;
}

export class UsageSummary {
  public readonly counts: Record<Outcome, number> = {
    success: 0,
    invalid_request: 0,
    system_error: 0,
  };
  public readonly byOperation: Record<string, OperationMetrics> = {};
  public last: (UsageEvent & { outcome: Outcome; at: number }) | undefined;
  public lastRoot: string | undefined;
  public totalSavedMs = 0;
  public totalCachedFiles = 0;
  public totalExecutedFiles = 0;
  public totalMsSum = 0;
  public timedCalls = 0;
  public daemonStatus: DaemonStatusSnapshot | undefined;

  public get hasRenderableData(): boolean {
    return this.last !== undefined || this.daemonStatus !== undefined;
  }

  public clearLastRoot(): void {
    this.lastRoot = undefined;
  }

  public clearDaemonStatus(): void {
    this.daemonStatus = undefined;
  }

  public ingestDaemonStatus(
    raw: unknown,
    transport?: string,
    updatedAt = Date.now(),
  ): DaemonStatusSnapshot | undefined {
    const rec = asRecord(raw);
    const daemonRec = asRecord(rec?.daemon);
    if (!daemonRec) {
      return undefined;
    }
    const metricsRec = asRecord(daemonRec.metrics);
    const loadedRoots = Array.isArray(daemonRec.loaded_roots)
      ? daemonRec.loaded_roots.filter((item): item is string => typeof item === 'string')
      : [];

    let workspace: DaemonWorkspaceSnapshot | undefined;
    const wsRec = asRecord(rec?.workspace);
    const wsRoot = asString(wsRec?.root);
    if (wsRec && wsRoot) {
      const loaded = wsRec.loaded !== false && wsRec.state !== 'registered_unloaded';
      const storageRec = asRecord(wsRec.cache_storage);
      const zygoteRec = asRecord(wsRec.zygote);
      const lastBgErr = asString(wsRec.last_background_error);
      const lastWatchErr = asString(wsRec.last_watcher_error);
      const zygotePid = asOptionalNumber(zygoteRec?.pid);
      const zygoteRss = asOptionalNumber(zygoteRec?.rss_bytes);
      const zygoteErr = asString(zygoteRec?.last_error);
      workspace = {
        root: wsRoot,
        loaded,
        indexedTotalFiles: asNumber(wsRec.indexed_total_files),
        indexedPythonFiles: asNumber(wsRec.indexed_python_files),
        indexedRustFiles: asNumber(wsRec.indexed_rust_files),
        indexedFrontendFiles: asNumber(wsRec.indexed_frontend_files),
        totalTestFiles: asNumber(wsRec.total_test_files),
        totalPythonTestFiles: asNumber(wsRec.total_python_test_files),
        totalRustTestFiles: asNumber(wsRec.total_rust_test_files),
        totalFrontendTestFiles: asNumber(wsRec.total_frontend_test_files),
        cachedPassedFiles: asNumber(wsRec.cached_passed_files),
        cachedFailedFiles: asNumber(wsRec.cached_failed_files),
        uncachedOrStaleFiles: asNumber(wsRec.uncached_or_stale_files),
        ruffCachedFiles: asNumber(wsRec.ruff_cached_files),
        ruffDiagnosticsCount: asNumber(wsRec.ruff_diagnostics_count),
        backgroundRunning: wsRec.background_running === true,
        backgroundPendingQueue: asNumber(wsRec.background_pending_queue),
        backgroundInFlight: asNumber(wsRec.background_in_flight),
        backgroundBatches: asNumber(wsRec.background_batches),
        totalBackgroundRuns: asNumber(wsRec.total_background_runs),
        totalCacheHits: asNumber(wsRec.total_cache_hits),
        totalDeltaRuns: asNumber(wsRec.total_delta_runs),
        totalSavedMs: asNumber(wsRec.total_saved_ms),
        watchedDirectories: asNumber(wsRec.watched_directories),
        watcherOverflowPending: wsRec.watcher_overflow_pending === true,
        ...(lastBgErr !== undefined ? { lastBackgroundError: lastBgErr } : {}),
        ...(lastWatchErr !== undefined ? { lastWatcherError: lastWatchErr } : {}),
        cacheMemoryBytes: asNumber(storageRec?.estimated_memory_bytes),
        cacheHistoryRecords: asNumber(storageRec?.history_records),
        cacheWalBytes: asNumber(storageRec?.wal_bytes),
        zygoteReady: zygoteRec?.ready === true,
        ...(zygotePid !== undefined ? { zygotePid } : {}),
        zygoteRuns: asNumber(zygoteRec?.runs),
        ...(zygoteRss !== undefined ? { zygoteRssBytes: zygoteRss } : {}),
        ...(zygoteErr !== undefined ? { zygoteLastError: zygoteErr } : {}),
        zygoteCircuitOpen: zygoteRec?.circuit_open === true,
      };
    }

    const daemonPid = asOptionalNumber(daemonRec.pid);
    const snapshot: DaemonStatusSnapshot = {
      ok: daemonRec.ok === true,
      version: asString(daemonRec.version) ?? '0.1.5',
      ...(daemonPid !== undefined ? { pid: daemonPid } : {}),
      uptimeSecs: asNumber(daemonRec.uptime_secs),
      registeredRoots: asNumber(daemonRec.registered_roots),
      loadedRoots,
      totalRequests: asNumber(metricsRec?.total_requests),
      errorRequests: asNumber(metricsRec?.error_requests),
      testRuns: asNumber(metricsRec?.test_runs),
      totalSavedMs: asNumber(metricsRec?.total_saved_ms),
      totalCachedFiles: asNumber(metricsRec?.total_cached_files),
      totalExecutedFiles: asNumber(metricsRec?.total_executed_files),
      ...(transport ? { transport } : {}),
      updatedAt,
      ...(workspace ? { workspace } : {}),
    };
    this.daemonStatus = snapshot;
    if (workspace?.root && !this.lastRoot) {
      this.lastRoot = workspace.root;
    }
    return snapshot;
  }

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
    if (typeof event.savedMs === 'number' && event.savedMs > 0) {
      this.totalSavedMs += event.savedMs;
    }
    if (typeof event.cachedFiles === 'number' && event.cachedFiles > 0) {
      this.totalCachedFiles += event.cachedFiles;
    }
    if (typeof event.executedFiles === 'number' && event.executedFiles > 0) {
      this.totalExecutedFiles += event.executedFiles;
    }
    if (typeof event.durationMs === 'number' && Number.isFinite(event.durationMs) && event.durationMs >= 0) {
      this.totalMsSum += event.durationMs;
      this.timedCalls += 1;
    }
    const op = this.byOperation[event.operation] ?? {
      calls: 0,
      successes: 0,
      errors: 0,
      totalMs: 0,
      timedCalls: 0,
      savedMs: 0,
      cachedFiles: 0,
      executedFiles: 0,
    };
    op.calls += 1;
    if (outcome === 'success') {
      op.successes += 1;
    } else {
      op.errors += 1;
    }
    if (typeof event.durationMs === 'number' && Number.isFinite(event.durationMs) && event.durationMs >= 0) {
      op.totalMs += event.durationMs;
      op.timedCalls += 1;
    }
    if (typeof event.savedMs === 'number' && event.savedMs > 0) {
      op.savedMs += event.savedMs;
    }
    if (typeof event.cachedFiles === 'number' && event.cachedFiles > 0) {
      op.cachedFiles += event.cachedFiles;
    }
    if (typeof event.executedFiles === 'number' && event.executedFiles > 0) {
      op.executedFiles += event.executedFiles;
    }
    this.byOperation[event.operation] = op;
    this.last = { ...event, outcome, at };
    if (event.root) {
      this.lastRoot = event.root;
    }
  }

  private effectiveTotals(): {
    savedMs: number;
    cachedFiles: number;
    executedFiles: number;
    testRuns: number;
  } {
    const ds = this.daemonStatus;
    const ws = ds?.workspace;
    return {
      savedMs: Math.max(this.totalSavedMs, ds?.totalSavedMs ?? 0, ws?.totalSavedMs ?? 0),
      cachedFiles: Math.max(this.totalCachedFiles, ds?.totalCachedFiles ?? 0),
      executedFiles: Math.max(this.totalExecutedFiles, ds?.totalExecutedFiles ?? 0),
      testRuns: Math.max(this.byOperation.test_run?.calls ?? 0, ds?.testRuns ?? 0),
    };
  }

  public text(): string {
    const ds = this.daemonStatus;
    if (ds) {
      const ws = ds.workspace;
      const { savedMs } = this.effectiveTotals();
      const parts: string[] = ['$(beaker) TCTX'];
      if (savedMs >= 100) {
        parts.push(`~${formatCompactDuration(savedMs / 1_000)} saved`);
      }
      if (ws?.loaded && ws.totalTestFiles > 0) {
        parts.push(`${ws.cachedPassedFiles}/${ws.totalTestFiles} cached`);
        if (ws.cachedFailedFiles > 0) {
          parts.push(`$(error) ${ws.cachedFailedFiles} fail`);
        }
        if (ws.ruffDiagnosticsCount > 0) {
          parts.push(`$(warning) ${ws.ruffDiagnosticsCount} lint`);
        }
        if (ws.backgroundRunning || ws.backgroundPendingQueue > 0) {
          const active = ws.backgroundInFlight + ws.backgroundPendingQueue;
          parts.push(`$(sync~spin) BG ${active}`);
        }
      } else if (ds.testRuns > 0) {
        parts.push(`${ds.testRuns} runs`);
      } else if (ds.registeredRoots > 0) {
        parts.push(`Ready · ${ds.loadedRoots.length}/${ds.registeredRoots} roots`);
      } else {
        parts.push('Ready');
      }
      return parts.join(' · ');
    }
    const counts = this.counts;
    const base = `$(beaker) TCTX S:${counts.success} E:${counts.invalid_request + counts.system_error}`;
    if (this.totalSavedMs >= 100) {
      return `${base} · ~${formatCompactDuration(this.totalSavedMs / 1_000)}`;
    }
    return base;
  }

  public tooltip(): string {
    const c = this.counts;
    const ds = this.daemonStatus;
    const ws = ds?.workspace;
    const { savedMs, cachedFiles, executedFiles, testRuns } = this.effectiveTotals();
    const totalFiles = cachedFiles + executedFiles;
    const hitPct = totalFiles > 0 ? `${((100 * cachedFiles) / totalFiles).toFixed(1)}%` : 'n/a';
    const lines: string[] = [
      ds
        ? `Test Context v${ds.version} (${ds.loadedRoots.length}/${ds.registeredRoots} roots loaded, ${testRuns} test runs).`
        : 'Test Context native tool calls in this extension host.',
      `Saved: ~${formatCompactDuration(savedMs / 1_000)} · Cache hit: ${hitPct} (${cachedFiles} cached / ${executedFiles} executed).`,
    ];
    if (ws?.loaded) {
      lines.push(
        `Workspace ${shortRootLabel(ws.root)}: ${ws.indexedTotalFiles} files (${ws.totalTestFiles} tests: ${ws.cachedPassedFiles} passed, ${ws.cachedFailedFiles} failed, ${ws.uncachedOrStaleFiles} uncached), ${ws.ruffDiagnosticsCount} lint issue(s).`,
      );
    }
    lines.push(
      `Host calls: success ${c.success}; invalid ${c.invalid_request}; errors ${c.system_error}.`,
      this.last
        ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} ${this.last.outcome}`
        : 'No host tool calls observed yet.',
      this.lastRoot ? `Active root: ${this.lastRoot}` : '',
      'Click to inspect full Test Context status.',
    );
    return lines.filter((line) => line.length > 0).join('\n');
  }

  public markdownTooltip(): string {
    const c = this.counts;
    const ds = this.daemonStatus;
    const ws = ds?.workspace;
    const { savedMs, cachedFiles, executedFiles, testRuns } = this.effectiveTotals();
    const totalFiles = cachedFiles + executedFiles;
    const hitPct = totalFiles > 0 ? `${((100 * cachedFiles) / totalFiles).toFixed(1)}%` : '-';
    const activeRoot = ws?.root ?? this.lastRoot;
    const titleSuffix = activeRoot
      ? `${escapeMdCell(shortRootLabel(activeRoot))}${ds?.transport ? ` · ${escapeMdCell(ds.transport)}` : ''}`
      : ds
        ? `Daemon v${escapeMdCell(ds.version)}${ds.transport ? ` · ${escapeMdCell(ds.transport)}` : ''}`
        : 'Extension Host';

    const cachedCell = ws?.loaded
      ? ws.cachedFailedFiles > 0
        ? `**${ws.cachedPassedFiles}/${ws.totalTestFiles}** ($(error) ${ws.cachedFailedFiles})`
        : `**${ws.cachedPassedFiles}/${ws.totalTestFiles}**${ws.uncachedOrStaleFiles > 0 ? ` (${ws.uncachedOrStaleFiles} stale)` : ' ✅'}`
      : ws
        ? '_Unloaded_'
        : `${cachedFiles} files`;
    const lintCell = ws?.loaded
      ? ws.ruffDiagnosticsCount > 0
        ? `$(warning) **${ws.ruffDiagnosticsCount}** (${ws.ruffCachedFiles}f)`
        : `✅ **0** (${ws.ruffCachedFiles}f)`
      : '-';

    const lines: string[] = [
      `### $(beaker) Test Context — ${titleSuffix}`,
      '',
      '| ⏱️ Time Saved | 🎯 Cache Hit Rate | 🧪 Cached Tests | 🔍 Lint Issues |',
      '| :---: | :---: | :---: | :---: |',
      `| **~${formatCompactDuration(savedMs / 1_000)}** (${testRuns} runs) | **${hitPct}** (${cachedFiles}/${totalFiles}) | ${cachedCell} | ${lintCell} |`,
      '',
    ];

    if (ws?.loaded) {
      const otherIndexed = Math.max(
        0,
        ws.indexedTotalFiles - ws.indexedPythonFiles - ws.indexedRustFiles - ws.indexedFrontendFiles,
      );
      const otherTests = Math.max(
        0,
        ws.totalTestFiles - ws.totalPythonTestFiles - ws.totalRustTestFiles - ws.totalFrontendTestFiles,
      );
      const langRows: Array<[string, number, number]> = [
        ['Python (`pytest`)', ws.indexedPythonFiles, ws.totalPythonTestFiles],
        ['Rust (`cargo test`)', ws.indexedRustFiles, ws.totalRustTestFiles],
        ['Frontend (`vitest`/`jest`/`node`)', ws.indexedFrontendFiles, ws.totalFrontendTestFiles],
      ];
      if (otherIndexed > 0 || otherTests > 0) {
        langRows.push(['C/C++ · C# · Data/Config', otherIndexed, otherTests]);
      }
      const nonEmptyRows = langRows.filter(([, idxCnt, testCnt]) => idxCnt > 0 || testCnt > 0);
      if (nonEmptyRows.length > 0) {
        lines.push(
          `**$(file-code) AST Graph & Cache** · \`${ws.indexedTotalFiles.toLocaleString('en-US')}\` files · \`${ws.watchedDirectories}\` watched dirs · \`${formatBytes(ws.cacheMemoryBytes)}\` cache (\`${ws.cacheHistoryRecords}\` entries)`,
          '',
          '| Ecosystem | Indexed Files | Test Files |',
          '| :--- | ---: | ---: |',
        );
        for (const [label, idxCnt, testCnt] of nonEmptyRows) {
          lines.push(`| ${label} | ${idxCnt.toLocaleString('en-US')} | ${testCnt.toLocaleString('en-US')} |`);
        }
        lines.push('');
      }

      const bgState = ws.backgroundRunning
        ? `$(sync~spin) Running (\`${ws.backgroundInFlight}\` active, \`${ws.backgroundPendingQueue}\` queued)`
        : ws.backgroundPendingQueue > 0
          ? `⏳ Queued (\`${ws.backgroundPendingQueue}\`)`
          : '🟢 Idle';
      const zygoteState = ws.zygoteCircuitOpen
        ? '$(warning) Circuit open'
        : ws.zygoteReady
          ? `🟢 Ready (PID \`${ws.zygotePid ?? '-'}\`, \`${ws.zygoteRuns}\` runs${ws.zygoteRssBytes !== undefined ? `, \`${formatBytes(ws.zygoteRssBytes)}\`` : ''})`
          : ws.zygoteLastError
            ? `$(warning) ${escapeMdCell(ws.zygoteLastError.slice(0, 72))}`
            : '⚪ Standby';
      lines.push(
        `**$(server-process) Workers** · Background: ${bgState} (\`${ws.totalBackgroundRuns}\` bg runs, \`${ws.totalCacheHits}\` hits) · Pytest Zygote: ${zygoteState}`,
      );
      if (ws.watcherOverflowPending || ws.lastWatcherError || ws.lastBackgroundError) {
        const alerts: string[] = [];
        if (ws.watcherOverflowPending) {
          alerts.push('watcher overflow pending');
        }
        if (ws.lastWatcherError) {
          alerts.push(`watcher: ${escapeMdCell(ws.lastWatcherError.slice(0, 80))}`);
        }
        if (ws.lastBackgroundError) {
          alerts.push(`bg: ${escapeMdCell(ws.lastBackgroundError.slice(0, 80))}`);
        }
        lines.push(`$(warning) _Alerts: ${alerts.join(' · ')}_`);
      }
      lines.push('');
    } else if (ws && !ws.loaded) {
      lines.push(
        `_Workspace \`${escapeMdCell(ws.root)}\` is registered and unloaded (lazy load on first test/sync call)._`,
        '',
      );
    }

    const opEntries = Object.entries(this.byOperation);
    if (opEntries.length > 0) {
      lines.push(
        '**$(zap) Extension Host Native Tool Breakdown**',
        '',
        '| Tool | Calls (OK/Err) | Cached / Exec | Saved | Avg Speed |',
        '| :--- | ---: | ---: | ---: | ---: |',
      );
      for (const [opName, stats] of opEntries) {
        const avg = stats.timedCalls > 0 ? formatSmartMs(stats.totalMs / stats.timedCalls) : '-';
        const savedLabel = stats.savedMs > 0 ? `~${formatCompactDuration(stats.savedMs / 1_000)}` : '-';
        const filesLabel =
          stats.cachedFiles > 0 || stats.executedFiles > 0
            ? `${stats.cachedFiles} / ${stats.executedFiles}`
            : '-';
        lines.push(
          `| \`${escapeMdCell(opName)}\` | ${stats.calls} (${stats.successes}/${stats.errors}) | ${filesLabel} | ${savedLabel} | ${avg} |`,
        );
      }
      lines.push('');
    }

    if (this.last) {
      const modeTag = this.last.mode ? ` · \`${escapeMdCell(this.last.mode)}\`` : '';
      const durTag = this.last.durationMs !== undefined ? ` · ${formatSmartMs(this.last.durationMs)}` : '';
      lines.push(
        `_Last call: \`${escapeMdCell(this.last.operation)}\` (${this.last.outcome}${modeTag}${durTag}) at ${formatLocalTime(this.last.at)}_`,
      );
    }

    const daemonFooter = ds
      ? `${ds.ok ? '🟢' : '🔴'} Daemon \`v${escapeMdCell(ds.version)}\`${ds.pid !== undefined ? ` (PID \`${ds.pid}\`, up \`${formatCompactDuration(ds.uptimeSecs)}\`)` : ''} · \`${ds.loadedRoots.length}/${ds.registeredRoots}\` roots · \`${ds.totalRequests}\` reqs · Updated \`${formatLocalTime(ds.updatedAt)}\``
      : `🟢 Extension Host · \`${c.success}\` OK / \`${c.invalid_request + c.system_error}\` Err`;
    lines.push(
      daemonFooter,
      '',
      '[$(pulse) Show Status](command:testContextNative.showStatus) · [$(sync) Sync Graph](command:testContextNative.syncWorkspace) · [$(beaker) Self-Test](command:testContextNative.selfTest) · [$(graph) Session ROI](command:testContextNative.showSessionRoi)',
    );
    return lines.join('\n');
  }

  public sessionRoiReport(): string {
    const c = this.counts;
    const total = c.success + c.invalid_request + c.system_error;
    const { savedMs, cachedFiles, executedFiles, testRuns } = this.effectiveTotals();
    return [
      `session_roi total_calls=${total} success=${c.success} invalid_request=${c.invalid_request} system_error=${c.system_error} saved_ms=${savedMs} cached_files=${cachedFiles} executed_files=${executedFiles} daemon_test_runs=${testRuns}`,
      `active_root=${this.daemonStatus?.workspace?.root ?? this.lastRoot ?? '-'}`,
    ].join('\n');
  }
}
