import { Buffer } from 'node:buffer';
import type { Duplex } from 'node:stream';

import {
  connectNativeTransport,
  nativeEndpoint,
  RequestCancelledError,
  startNativeService,
} from './nativeTransport.js';
export { defaultSocketPath, normalizeSocketPath, RequestCancelledError } from './nativeTransport.js';

import type {
  CancellationLike,
  ClientSettings,
  DaemonRequest,
  DisposableLike,
  JsonValue,
} from './types.js';

const MAX_FRAME_BYTES = 4 * 1024 * 1024;
const FRAMED_JSON_MAGIC = Buffer.from('TCTXJ1\n', 'ascii');

interface PendingResponse {
  resolve(value: JsonValue): void;
  reject(error: Error): void;
  timer: NodeJS.Timeout;
  cancellation: DisposableLike;
  cancelTarget?: {
    settings: ClientSettings;
    root: string;
    cancelId: string;
  };
}

export interface ClientTrace {
  [key: string]: JsonValue;
  trace_id: string;
  operation: string;
  root: string | null;
  queue_ms: number;
  prepare_ms: number;
  ipc_ms: number;
  serialize_ms: number;
  total_ms: number;
  started_unix_ms: number;
  completed_unix_ms: number;
  success: boolean;
  available: boolean;
  outcome?: string;
}

export class DaemonResponseError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'DaemonResponseError';
  }
}

export class ConnectionResetError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'ConnectionResetError';
  }
}

export class TracedRequestError extends Error {
  public constructor(
    public readonly trace: ClientTrace,
    cause: unknown,
  ) {
    super(cause instanceof Error ? cause.message : String(cause), { cause });
    this.name = 'TracedRequestError';
  }
}

const FAST_READ_ONLY_OPS = new Set([
  'ping',
  'test_status',
  'test_graph',
  'test_affected',
  'test_workspaces',
  'test_trace',
  'test_explain',
]);

// Ultra-fast control operations get an isolated lane when the fast read-only lane is also busy
// so a stalled `test_graph`/`test_affected` can never block `test_workspaces` refreshes or `ping`.
const CONTROL_LANE_OPS = new Set(['ping', 'test_workspaces']);

export class DaemonClient {
  private socket: Duplex | undefined;
  private socketPath: string | undefined;
  private receiveChunks: Buffer[] = [];
  private receiveBytes = 0;
  private pending: PendingResponse | undefined;
  private queue: Promise<void> = Promise.resolve();
  private inFlightCount = 0;
  private fastLane: DaemonClient | undefined;
  private fastLaneIdleTimer: NodeJS.Timeout | undefined;
  private controlLane: DaemonClient | undefined;
  private controlLaneIdleTimer: NodeJS.Timeout | undefined;
  private disposed = false;
  private tracingAvailable: boolean | undefined;
  private connectionGeneration = 0;

  public constructor(
    private readonly settings: () => ClientSettings,
    private readonly serviceStarter: (settings: ClientSettings) => Promise<void> = startNativeService,
    private readonly connector = connectNativeTransport,
    private readonly allowFastLane = true,
  ) {}

  public get usesWslBridge(): boolean {
    return process.platform === 'win32';
  }

  public get wslDistribution(): string | undefined {
    return this.settings().wslDistribution;
  }

  public get generation(): number {
    return this.connectionGeneration;
  }

  public get endpoint(): string {
    return nativeEndpoint(this.settings());
  }

  public request<T = JsonValue>(
    request: DaemonRequest,
    cancellation: CancellationLike,
  ): Promise<T> {
    return this.enqueue<T>(request, cancellation);
  }

  public async requestTraced<T = JsonValue>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    traceId: string,
    prepareMs = 0,
    expectedGen?: number,
    beforeExecute?: () => void,
  ): Promise<{ value: T; trace: ClientTrace }> {
    const started = performance.now();
    const trace: ClientTrace = {
      trace_id: traceId,
      origin: 'native',
      operation: request.name,
      root: request.root,
      queue_ms: 0,
      prepare_ms: prepareMs,
      ipc_ms: 0,
      serialize_ms: 0,
      total_ms: 0,
      success: false,
      available: false,
      daemon: null,
      started_unix_ms: Date.now(),
      completed_unix_ms: 0,
    };
    try {
      const value = await this.enqueue<T>(
        request,
        cancellation,
        trace,
        expectedGen,
        beforeExecute,
      );
      trace.total_ms = performance.now() - started + prepareMs;
      trace.success = true;
      trace.completed_unix_ms = Date.now();
      return { value, trace };
    } catch (error: unknown) {
      trace.total_ms = performance.now() - started + prepareMs;
      trace.completed_unix_ms = Date.now();
      throw new TracedRequestError(trace, error);
    }
  }

  private enqueue<T>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    trace?: ClientTrace,
    expectedGen?: number,
    beforeExecute?: () => void,
  ): Promise<T> {
    if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
      return Promise.reject(
        new ConnectionResetError('Test Context native connection was reset.'),
      );
    }
    if (
      this.allowFastLane &&
      CONTROL_LANE_OPS.has(request.name) &&
      (this.inFlightCount > 0 || (this.fastLane && this.fastLane.inFlightCount > 0))
    ) {
      if (this.controlLaneIdleTimer) {
        clearTimeout(this.controlLaneIdleTimer);
        this.controlLaneIdleTimer = undefined;
      }
      this.controlLane ??= new DaemonClient(
        this.settings,
        this.serviceStarter,
        this.connector,
        false,
      );
      if (this.tracingAvailable !== undefined && this.controlLane.tracingAvailable === undefined) {
        this.controlLane.tracingAvailable = this.tracingAvailable;
      }
      return this.controlLane
        .enqueue<T>(request, cancellation, trace, undefined, beforeExecute)
        .finally(() => {
        if (!this.disposed && this.controlLane && this.controlLane.inFlightCount === 0) {
          if (this.controlLaneIdleTimer) {
            clearTimeout(this.controlLaneIdleTimer);
          }
          const idleMs = this.usesWslBridge ? 60_000 : 15_000;
          this.controlLaneIdleTimer = setTimeout(() => {
            this.controlLaneIdleTimer = undefined;
            if (!this.disposed && this.controlLane && this.controlLane.inFlightCount === 0) {
              this.controlLane.dispose();
              this.controlLane = undefined;
            }
          }, idleMs);
          this.controlLaneIdleTimer.unref();
        }
      });
    }
    if (
      this.allowFastLane &&
      this.inFlightCount > 0 &&
      FAST_READ_ONLY_OPS.has(request.name)
    ) {
      if (this.fastLaneIdleTimer) {
        clearTimeout(this.fastLaneIdleTimer);
        this.fastLaneIdleTimer = undefined;
      }
      this.fastLane ??= new DaemonClient(
        this.settings,
        this.serviceStarter,
        this.connector,
        false,
      );
      if (this.tracingAvailable !== undefined && this.fastLane.tracingAvailable === undefined) {
        this.fastLane.tracingAvailable = this.tracingAvailable;
      }
      return this.fastLane
        .enqueue<T>(request, cancellation, trace, undefined, beforeExecute)
        .finally(() => {
        if (!this.disposed && this.fastLane && this.fastLane.inFlightCount === 0) {
          if (this.fastLaneIdleTimer) {
            clearTimeout(this.fastLaneIdleTimer);
          }
          const idleMs = this.usesWslBridge ? 60_000 : 15_000;
          this.fastLaneIdleTimer = setTimeout(() => {
            this.fastLaneIdleTimer = undefined;
            if (this.fastLane && this.fastLane.inFlightCount === 0) {
              this.fastLane.dispose();
              this.fastLane = undefined;
            }
          }, idleMs);
          this.fastLaneIdleTimer.unref?.();
        }
      });
    }
    if (cancellation.isCancellationRequested) {
      return Promise.reject(new RequestCancelledError());
    }
    const queuedAt = performance.now();
    const enqueueGen = expectedGen ?? this.connectionGeneration;
    this.inFlightCount += 1;
    let settledEarly = false;
    let cancelSub: DisposableLike | undefined;
    const cancelRace = new Promise<never>((_resolve, reject) => {
      cancelSub = cancellation.onCancellationRequested(() => {
        settledEarly = true;
        reject(new RequestCancelledError());
      });
    });
    const execute = (): Promise<T> => {
      cancelSub?.dispose();
      cancelSub = undefined;
      if (settledEarly || cancellation.isCancellationRequested) {
        return Promise.reject(new RequestCancelledError());
      }
      if (this.connectionGeneration !== enqueueGen) {
        return Promise.reject(
          new ConnectionResetError('Test Context native connection was reset while queued.'),
        );
      }
      if (beforeExecute) {
        try {
          beforeExecute();
        } catch (err: unknown) {
          return Promise.reject(err instanceof Error ? err : new Error(String(err)));
        }
      }
      if (trace) {
        trace.queue_ms = performance.now() - queuedAt;
      }
      return this.requestWithRetry<T>(request, cancellation, trace, enqueueGen, beforeExecute);
    };
    const run = this.queue.then(execute, execute).finally(() => {
      cancelSub?.dispose();
      cancelSub = undefined;
      this.inFlightCount = Math.max(0, this.inFlightCount - 1);
    });
    // Prevent unhandled rejection when `cancelRace` rejects while `run` finishes later
    run.catch(() => undefined);
    this.queue = run.then(
      () => undefined,
      () => undefined,
    );
    return Promise.race([run, cancelRace]);
  }

  public dispose(): void {
    const activeCancel = this.pending?.cancelTarget;
    if (activeCancel && !this.disposed) {
      // Send request-scoped cancellation before disconnecting so extension deactivation or
      // client disposal does not leave an active test_run or ruff_fix orphaned on the daemon.
      void this.fireRemoteCancellation(
        activeCancel.root,
        activeCancel.cancelId,
        activeCancel.settings,
      );
    }
    this.disposed = true;
    this.connectionGeneration += 1;
    if (this.fastLaneIdleTimer) {
      clearTimeout(this.fastLaneIdleTimer);
      this.fastLaneIdleTimer = undefined;
    }
    this.fastLane?.dispose();
    this.fastLane = undefined;
    if (this.controlLaneIdleTimer) {
      clearTimeout(this.controlLaneIdleTimer);
      this.controlLaneIdleTimer = undefined;
    }
    this.controlLane?.dispose();
    this.controlLane = undefined;
    this.disconnect(new Error('Test Context native client was disposed.'));
  }

  public reset(): void {
    if (!this.disposed) {
      // Cancel any in-flight `test_run` on the ORIGINAL endpoint/settings before resetting so
      // switching settings mid-run does not leave tests or `ruff_fix` running orphaned.
      const activeCancel = this.pending?.cancelTarget;
      if (activeCancel) {
        void this.fireRemoteCancellation(
          activeCancel.root,
          activeCancel.cancelId,
          activeCancel.settings,
        );
      }
      this.connectionGeneration += 1;
      this.tracingAvailable = undefined;
      this.fastLane?.reset();
      this.controlLane?.reset();
      this.disconnect(new ConnectionResetError('Test Context native connection was reset.'));
    }
  }

  private async requestWithRetry<T>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    trace?: ClientTrace,
    expectedGen?: number,
    beforeExecute?: () => void,
  ): Promise<T> {
    if (this.disposed) {
      throw new Error('Test Context native client is disposed.');
    }
    if (cancellation.isCancellationRequested) {
      throw new RequestCancelledError();
    }
    if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
      throw new ConnectionResetError('Test Context native connection was reset.');
    }
    beforeExecute?.();
    let firstError: Error | undefined;
    const hadConnectedSocket = Boolean(this.socket && !this.socket.destroyed);
    const isIdempotent =
      request.name === 'ping' ||
      request.name === 'test_status' ||
      request.name === 'test_workspaces' ||
      request.name === 'test_graph' ||
      request.name === 'test_affected' ||
      request.name === 'test_cancel';
    const maxAttempts = hadConnectedSocket ? 3 : 2;
    let serviceStarted = false;
    for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
      if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
        throw new ConnectionResetError('Test Context native connection was reset.');
      }
      beforeExecute?.();
      let requestSent = false;
      try {
        const preparing = performance.now();
        await this.ensureConnected(cancellation, expectedGen);
        if (trace && this.tracingAvailable === undefined) {
          const ping = await this.send(
            { root: null, name: 'ping', arguments: {}, compact: false },
            cancellation,
          );
          this.tracingAvailable =
            isRecord(ping) &&
            isRecord(ping.capabilities) &&
            ping.capabilities.request_tracing === true &&
            ping.capabilities.trace_clock === 'unix_ms';
        }
        if (trace) {
          trace.prepare_ms += performance.now() - preparing;
          trace.available = this.tracingAvailable === true;
        }
        if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
          throw new ConnectionResetError('Test Context native connection was reset.');
        }
        // Recheck workspace/root generation immediately before writing the request frame
        // so folder changes during `ensureConnected` or `ping` never dispatch to a stale root.
        beforeExecute?.();
        const outgoing =
          trace && this.tracingAvailable
            ? { ...request, arguments: { ...request.arguments, _tctx_trace: trace.trace_id } }
            : request;
        const ipcStarted = performance.now();
        let value: JsonValue;
        requestSent = true;
        try {
          value = await this.send(outgoing, cancellation);
        } finally {
          if (trace) {
            trace.ipc_ms += performance.now() - ipcStarted;
          }
        }
        if (trace && this.tracingAvailable) {
          if (
            !isRecord(value) ||
            !isRecord(value._tctx_trace) ||
            value._tctx_trace.trace_id !== trace.trace_id ||
            value.value === undefined
          ) {
            throw new DaemonResponseError('Daemon trace is missing or does not match the request.');
          }
          trace.daemon = value._tctx_trace;
          return value.value as T;
        }
        return value as T;
      } catch (error: unknown) {
        const failure = asError(error);
        if (
          failure instanceof RequestCancelledError ||
          failure instanceof DaemonResponseError ||
          failure instanceof ConnectionResetError
        ) {
          throw failure;
        }
        this.disconnect(failure);
        // Never retry a mutating request (test_run, test_sync, test_invalidate) after it was sent,
        // because the daemon may still be executing the original request (e.g. on timeout).
        if (requestSent && !isIdempotent) {
          throw failure;
        }
        firstError ??= failure;
        const shouldStartService =
          !serviceStarted &&
          this.settings().autoStartService &&
          (!hadConnectedSocket || attempt >= 1);
        if (shouldStartService && attempt + 1 < maxAttempts) {
          serviceStarted = true;
          await this.serviceStarter(this.settings());
          if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
            throw new ConnectionResetError('Test Context native connection was reset.');
          }
          await delay(100, cancellation);
          if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
            throw new ConnectionResetError('Test Context native connection was reset.');
          }
        }
      }
    }
    throw new Error(
      `Test Context daemon request failed after reconnect: ${firstError?.message ?? 'unknown error'}`,
    );
  }

  private async ensureConnected(
    cancellation: CancellationLike,
    expectedGen?: number,
  ): Promise<void> {
    if (expectedGen !== undefined && this.connectionGeneration !== expectedGen) {
      throw new ConnectionResetError('Test Context native connection was reset.');
    }
    const settings = this.settings();
    const expectedPath = nativeEndpoint(settings);
    if (this.socket && !this.socket.destroyed && this.socketPath === expectedPath) {
      return;
    }
    const gen = expectedGen ?? this.connectionGeneration;
    const prevPath = this.socketPath;
    this.disconnect(new Error('Replacing stale Test Context connection.'));
    if (prevPath !== expectedPath) {
      this.tracingAvailable = undefined;
    }
    const socket = await this.connector(settings, cancellation);
    if (this.disposed || this.connectionGeneration !== gen || cancellation.isCancellationRequested) {
      socket.destroy();
      if (cancellation.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      if (this.connectionGeneration !== gen && !this.disposed) {
        // Treat settings/endpoint reset during connect as terminal for this request so the caller
        // re-resolves root/distribution against the new settings instead of retrying on another distro.
        throw new ConnectionResetError(
          'Test Context native connection was reset while connecting.',
        );
      }
      throw new Error('Test Context native client was disposed while connecting.');
    }
    this.socket = socket;
    this.socketPath = expectedPath;
    this.receiveChunks = [];
    this.receiveBytes = 0;
    socket.on('data', (chunk: Buffer) => this.onData(socket, chunk));
    socket.on('error', (error: Error) => {
      if (this.socket === socket) {
        this.disconnect(error);
      }
    });
    socket.on('close', () => {
      if (this.socket === socket) {
        this.disconnect(new Error('Test Context daemon connection closed.'));
      }
    });
    await new Promise<void>((resolveConnect, rejectConnect) => {
      socket.write(FRAMED_JSON_MAGIC, (error?: Error | null) => {
        if (error) {
          if (this.socket === socket) {
            this.disconnect(error);
          }
          rejectConnect(error);
        } else {
          resolveConnect();
        }
      });
    });
  }

  private async send(
    request: DaemonRequest,
    cancellation: CancellationLike,
  ): Promise<JsonValue> {
    const socket = this.socket;
    if (!socket || socket.destroyed) {
      throw new Error('Test Context daemon is not connected.');
    }
    if (this.pending) {
      throw new Error('Test Context protocol allows only one in-flight request.');
    }
    const cancelId =
      request.name === 'test_run'
        ? (typeof request.arguments._tctx_cancel_id === 'string'
            ? request.arguments._tctx_cancel_id
            : `run-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`)
        : undefined;
    const wireRequest =
      cancelId !== undefined
        ? { ...request, arguments: { ...request.arguments, _tctx_cancel_id: cancelId } }
        : request;
    const payload = Buffer.from(JSON.stringify(wireRequest), 'utf8');
    if (payload.byteLength > MAX_FRAME_BYTES) {
      throw new Error('Test Context native request exceeds 4 MiB.');
    }
    const frame = Buffer.allocUnsafe(payload.byteLength + 4);
    frame.writeUInt32BE(payload.byteLength, 0);
    payload.copy(frame, 4);
    const activeSettings = this.settings();
    const timeoutMs = activeSettings.requestTimeoutMs;
    const cancelTarget =
      request.root && cancelId !== undefined
        ? { settings: activeSettings, root: request.root, cancelId }
        : undefined;
    return new Promise<JsonValue>((resolveResponse, rejectResponse) => {
      const timer = setTimeout(() => {
        // Send request-scoped test_cancel on timeout so timed-out test_runs do not keep
        // executing server-side after the client reports timeout failure.
        if (cancelTarget) {
          void this.fireRemoteCancellation(
            cancelTarget.root,
            cancelTarget.cancelId,
            cancelTarget.settings,
          );
        }
        this.disconnect(new Error(`Test Context daemon request timed out after ${timeoutMs} ms.`));
      }, timeoutMs);
      const cancellationListener = cancellation.onCancellationRequested(() => {
        // Send request-scoped test_cancel for this test_run only so unrelated concurrent
        // foreground or background runs are never killed.
        if (cancelTarget) {
          void this.fireRemoteCancellation(
            cancelTarget.root,
            cancelTarget.cancelId,
            cancelTarget.settings,
          );
        }
        this.disconnect(new RequestCancelledError());
      });
      this.pending = {
        resolve: resolveResponse,
        reject: rejectResponse,
        timer,
        cancellation: cancellationListener,
        ...(cancelTarget ? { cancelTarget } : {}),
      };
      socket.write(frame, (error?: Error | null) => {
        if (error && this.socket === socket) {
          this.disconnect(error);
        }
      });
    });
  }

  private async fireRemoteCancellation(
    root: string,
    cancelId?: string,
    overrideSettings?: ClientSettings,
  ): Promise<void> {
    const cancelTimeoutMs = 2_000;
    const listeners = new Set<() => void>();
    let cancelled = false;
    const boundedCancel: CancellationLike = {
      get isCancellationRequested(): boolean {
        return cancelled;
      },
      onCancellationRequested: (listener) => {
        listeners.add(listener);
        return { dispose: () => listeners.delete(listener) };
      },
    };
    let transport: Duplex | undefined;
    const deadlineTimer = setTimeout(() => {
      cancelled = true;
      for (const listener of [...listeners]) {
        listener();
      }
      transport?.destroy();
    }, cancelTimeoutMs);
    deadlineTimer.unref?.();
    try {
      transport = await this.connector(overrideSettings ?? this.settings(), boundedCancel);
      transport.on('error', () => {
        // Swallow broken-pipe / disconnect errors on best-effort cancel socket so extension host never crashes.
      });
      const request: DaemonRequest = {
        root,
        name: 'test_cancel',
        arguments: cancelId ? { cancel_id: cancelId } : {},
        compact: true,
      };
      const payload = Buffer.from(JSON.stringify(request), 'utf8');
      const frame = Buffer.allocUnsafe(FRAMED_JSON_MAGIC.length + 4 + payload.length);
      FRAMED_JSON_MAGIC.copy(frame, 0);
      frame.writeUInt32BE(payload.length, FRAMED_JSON_MAGIC.length);
      payload.copy(frame, FRAMED_JSON_MAGIC.length + 4);
      await new Promise<void>((resolveWrite) => {
        if (!transport || transport.destroyed || cancelled) {
          resolveWrite();
          return;
        }
        let settled = false;
        const finish = (): void => {
          if (settled) {
            return;
          }
          settled = true;
          cancelSub.dispose();
          resolveWrite();
        };
        const cancelSub = boundedCancel.onCancellationRequested(finish);
        // Wait for daemon reply frame, clean EOF/close, error, or the 2s deadline before destroying
        // the transport so bridged transports (e.g. WSL) actually forward `test_cancel` to the daemon.
        transport.once('data', finish);
        transport.once('end', finish);
        transport.once('close', finish);
        transport.once('error', finish);
        transport.write(frame, (err?: Error | null) => {
          if (err) {
            finish();
          }
        });
      });
    } catch {
      // Best-effort cancellation; the server-side timeout remains authoritative.
    } finally {
      clearTimeout(deadlineTimer);
      transport?.destroy();
    }
  }

  private peekHeaderLength(): number {
    const first = this.receiveChunks[0];
    if (this.receiveChunks.length === 1 && first && first.byteLength >= 4) {
      return first.readUInt32BE(0);
    }
    const header = Buffer.allocUnsafe(4);
    let offset = 0;
    for (const chunk of this.receiveChunks) {
      const take = Math.min(4 - offset, chunk.byteLength);
      chunk.copy(header, offset, 0, take);
      offset += take;
      if (offset === 4) {
        break;
      }
    }
    return header.readUInt32BE(0);
  }

  private onData(socket: Duplex, chunk: Buffer): void {
    if (this.socket !== socket) {
      return;
    }
    if (chunk.byteLength === 0) {
      return;
    }
    this.receiveChunks.push(chunk);
    this.receiveBytes += chunk.byteLength;
    if (this.receiveBytes > MAX_FRAME_BYTES + 4) {
      this.disconnect(new Error('Test Context daemon response exceeds 4 MiB.'));
      return;
    }
    if (this.receiveBytes < 4) {
      return;
    }
    const length = this.peekHeaderLength();
    if (length > MAX_FRAME_BYTES) {
      this.disconnect(new Error('Test Context daemon response exceeds 4 MiB.'));
      return;
    }
    if (this.receiveBytes < length + 4) {
      return;
    }
    const first = this.receiveChunks[0];
    const combined =
      this.receiveChunks.length === 1 && first
        ? first
        : Buffer.concat(this.receiveChunks, this.receiveBytes);
    const frame = combined.subarray(4, length + 4);
    const remaining = combined.byteLength - (length + 4);
    this.receiveChunks = remaining === 0 ? [] : [Buffer.from(combined.subarray(length + 4))];
    this.receiveBytes = remaining;
    const pending = this.takePending();
    if (!pending) {
      this.disconnect(new Error('Test Context daemon sent an unsolicited response.'));
      return;
    }
    if (this.receiveBytes > 0) {
      pending.reject(new Error('Test Context daemon sent multiple responses.'));
      this.disconnect(new Error('Test Context protocol desynchronized.'));
      return;
    }
    try {
      const response = JSON.parse(frame.toString('utf8')) as {
        result?: JsonValue;
        error?: string;
      };
      if (typeof response.error === 'string') {
        pending.reject(new DaemonResponseError(response.error));
      } else if (response.result !== undefined) {
        pending.resolve(response.result);
      } else {
        pending.reject(new Error('Test Context daemon response has no result.'));
      }
    } catch (error: unknown) {
      pending.reject(new Error(`Invalid Test Context daemon response: ${asError(error).message}`));
      this.disconnect(asError(error));
    }
  }

  private takePending(): PendingResponse | undefined {
    const pending = this.pending;
    this.pending = undefined;
    if (pending) {
      clearTimeout(pending.timer);
      pending.cancellation.dispose();
    }
    return pending;
  }

  private disconnect(reason: Error): void {
    const pending = this.takePending();
    if (this.socket) {
      this.socket.destroy();
      this.socket = undefined;
    }
    this.socketPath = undefined;
    this.receiveChunks = [];
    this.receiveBytes = 0;
    if (pending) {
      pending.reject(reason);
    }
  }
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function isRecord(value: unknown): value is { [key: string]: JsonValue } {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function delay(ms: number, cancellation: CancellationLike): Promise<void> {
  return new Promise<void>((resolveDelay, rejectDelay) => {
    const timer = setTimeout(() => {
      listener.dispose();
      resolveDelay();
    }, ms);
    const listener = cancellation.onCancellationRequested(() => {
      clearTimeout(timer);
      listener.dispose();
      rejectDelay(new RequestCancelledError());
    });
  });
}
