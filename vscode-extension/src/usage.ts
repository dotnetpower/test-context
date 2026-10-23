export type Outcome = 'success' | 'invalid_request' | 'system_error';

export interface UsageEvent {
  operation: string;
  root: string | null;
  error?: string;
}

export function classifyOutcome(error?: string): Outcome {
  if (error === undefined) {
    return 'success';
  }
  if (/not indexed|requires|invalid|must be/u.test(error)) {
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

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
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
      this.last
        ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} ${this.last.outcome}`
        : 'No calls observed yet.',
      this.lastRoot ? `Last requested root: ${this.lastRoot}` : '',
    ]
      .filter((line) => line.length > 0)
      .join('\n');
  }
}
