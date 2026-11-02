export type Outcome = 'success' | 'invalid_request' | 'system_error';

export interface UsageEvent {
  operation: string;
  root: string | null;
  error?: string;
  savedMs?: number;
  cachedFiles?: number;
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

export class UsageSummary {
  public readonly counts: Record<Outcome, number> = {
    success: 0,
    invalid_request: 0,
    system_error: 0,
  };
  public last: (UsageEvent & { outcome: Outcome; at: number }) | undefined;
  public lastRoot: string | undefined;
  public totalSavedMs = 0;
  public totalCachedFiles = 0;

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
    if (typeof event.savedMs === 'number' && event.savedMs > 0) {
      this.totalSavedMs += event.savedMs;
    }
    if (typeof event.cachedFiles === 'number' && event.cachedFiles > 0) {
      this.totalCachedFiles += event.cachedFiles;
    }
    this.last = { ...event, outcome, at };
    if (event.root) {
      this.lastRoot = event.root;
    }
  }

  public text(): string {
    const counts = this.counts;
    return `$(beaker) TCTX S:${counts.success} E:${counts.invalid_request + counts.system_error}`;
  }

  public tooltip(): string {
    const c = this.counts;
    return [
      'Test Context native tool calls in this extension host.',
      `Success ${c.success}; invalid input ${c.invalid_request}; system errors ${c.system_error}.`,
      `Cumulative saved: ${(this.totalSavedMs / 1000).toFixed(2)}s (${this.totalCachedFiles} cached files).`,
      this.last
        ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} ${this.last.outcome}`
        : 'No calls observed yet.',
      this.lastRoot ? `Last requested root: ${this.lastRoot}` : '',
    ]
      .filter((line) => line.length > 0)
      .join('\n');
  }

  public sessionRoiReport(): string {
    const c = this.counts;
    const total = c.success + c.invalid_request + c.system_error;
    return [
      `session_roi total_calls=${total} success=${c.success} invalid_request=${c.invalid_request} system_error=${c.system_error} saved_ms=${this.totalSavedMs} cached_files=${this.totalCachedFiles}`,
      `active_root=${this.lastRoot ?? '-'}`,
    ].join('\n');
  }
}
