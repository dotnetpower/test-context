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

export class TracedRequestError extends Error {
  public constructor(
    public readonly trace: ClientTrace,
    cause: unknown,
  ) {
    super(cause instanceof Error ? cause.message : String(cause), { cause });
    this.name = 'TracedRequestError';
  }
}

export class DaemonClient {
  private socket: Duplex | undefined;
  private socketPath: string | undefined;
  private receiveBuffer = Buffer.alloc(0);
  private pending: PendingResponse | undefined;
  private queue: Promise<void> = Promise.resolve();
  private disposed = false;
  private tracingAvailable: boolean | undefined;

  public constructor(
    private readonly settings: () => ClientSettings,
    private readonly serviceStarter: (settings: ClientSettings) => Promise<void> = startNativeService,
    private readonly connector = connectNativeTransport,
  ) {}

  public get usesWslBridge(): boolean {
    return process.platform === 'win32';
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
      const value = await this.enqueue<T>(request, cancellation, trace);
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
  ): Promise<T> {
    const queuedAt = performance.now();
    const execute = (): Promise<T> => {
      if (trace) {
        trace.queue_ms = performance.now() - queuedAt;
      }
      return this.requestWithRetry<T>(request, cancellation, trace);
    };
    const run = this.queue.then(execute, execute);
    this.queue = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  public dispose(): void {
    this.disposed = true;
    this.disconnect(new Error('Test Context native client was disposed.'));
  }

  public reset(): void {
    if (!this.disposed) {
      this.disconnect(new Error('Test Context native connection was reset.'));
    }
  }

  private async requestWithRetry<T>(
    request: DaemonRequest,
    cancellation: CancellationLike,
    trace?: ClientTrace,
  ): Promise<T> {
    if (this.disposed) {
      throw new Error('Test Context native client is disposed.');
    }
    if (cancellation.isCancellationRequested) {
      throw new RequestCancelledError();
    }
    let firstError: Error | undefined;
    for (let attempt = 0; attempt < 2; attempt += 1) {
      try {
        const preparing = performance.now();
        await this.ensureConnected(cancellation);
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
        const outgoing =
          trace && this.tracingAvailable
            ? { ...request, arguments: { ...request.arguments, _tctx_trace: trace.trace_id } }
            : request;
        const ipcStarted = performance.now();
        let value: JsonValue;
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
        if (failure instanceof RequestCancelledError || failure instanceof DaemonResponseError) {
          throw failure;
        }
        firstError ??= failure;
        this.disconnect(failure);
        if (attempt === 0 && this.settings().autoStartService) {
          await this.serviceStarter(this.settings());
          await delay(100, cancellation);
        }
      }
    }
    throw new Error(
      `Test Context daemon request failed after reconnect: ${firstError?.message ?? 'unknown error'}`,
    );
  }

  private async ensureConnected(cancellation: CancellationLike): Promise<void> {
    const settings = this.settings();
    const expectedPath = nativeEndpoint(settings);
    if (this.socket && !this.socket.destroyed && this.socketPath === expectedPath) {
      return;
    }
    this.disconnect(new Error('Replacing stale Test Context connection.'));
    const socket = await this.connector(settings, cancellation);
    if (this.disposed || cancellation.isCancellationRequested) {
      socket.destroy();
      if (cancellation.isCancellationRequested) {
        throw new RequestCancelledError();
      }
      throw new Error('Test Context native client was disposed while connecting.');
    }
    this.socket = socket;
    this.socketPath = expectedPath;
    this.receiveBuffer = Buffer.alloc(0);
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
    const payload = Buffer.from(JSON.stringify(request), 'utf8');
    if (payload.byteLength > MAX_FRAME_BYTES) {
      throw new Error('Test Context native request exceeds 4 MiB.');
    }
    const frame = Buffer.allocUnsafe(payload.byteLength + 4);
    frame.writeUInt32BE(payload.byteLength, 0);
    payload.copy(frame, 4);
    const timeoutMs = this.settings().requestTimeoutMs;
    return new Promise<JsonValue>((resolveResponse, rejectResponse) => {
      const timer = setTimeout(() => {
        this.disconnect(new Error(`Test Context daemon request timed out after ${timeoutMs} ms.`));
      }, timeoutMs);
      const cancellationListener = cancellation.onCancellationRequested(() => {
        if (request.root) {
          void this.fireRemoteCancellation(request.root);
        }
        this.disconnect(new RequestCancelledError());
      });
      this.pending = {
        resolve: resolveResponse,
        reject: rejectResponse,
        timer,
        cancellation: cancellationListener,
      };
      socket.write(frame, (error?: Error | null) => {
        if (error) {
          this.disconnect(error);
        }
      });
    });
  }

  private async fireRemoteCancellation(root: string): Promise<void> {
    const uncancelled: CancellationLike = {
      isCancellationRequested: false,
      onCancellationRequested: () => ({ dispose: () => undefined }),
    };
    try {
      const transport = await this.connector(this.settings(), uncancelled);
      const request: DaemonRequest = {
        root,
        name: 'test_cancel',
        arguments: {},
        compact: true,
      };
      const payload = Buffer.from(JSON.stringify(request), 'utf8');
      const frame = Buffer.allocUnsafe(FRAMED_JSON_MAGIC.length + 4 + payload.length);
      FRAMED_JSON_MAGIC.copy(frame, 0);
      frame.writeUInt32BE(payload.length, FRAMED_JSON_MAGIC.length);
      payload.copy(frame, FRAMED_JSON_MAGIC.length + 4);
      await new Promise<void>((resolveWrite) => {
        transport.write(frame, () => {
          transport.destroy();
          resolveWrite();
        });
      });
    } catch {
      // Best-effort cancellation; the server-side timeout remains authoritative.
    }
  }

  private onData(socket: Duplex, chunk: Buffer): void {
    if (this.socket !== socket) {
      return;
    }
    this.receiveBuffer = Buffer.concat([this.receiveBuffer, chunk]);
    if (this.receiveBuffer.byteLength > MAX_FRAME_BYTES + 4) {
      this.disconnect(new Error('Test Context daemon response exceeds 4 MiB.'));
      return;
    }
    if (this.receiveBuffer.byteLength < 4) {
      return;
    }
    const length = this.receiveBuffer.readUInt32BE(0);
    if (length > MAX_FRAME_BYTES) {
      this.disconnect(new Error('Test Context daemon response exceeds 4 MiB.'));
      return;
    }
    if (this.receiveBuffer.byteLength < length + 4) {
      return;
    }
    const frame = this.receiveBuffer.subarray(4, length + 4);
    this.receiveBuffer = this.receiveBuffer.subarray(length + 4);
    const pending = this.takePending();
    if (!pending) {
      this.disconnect(new Error('Test Context daemon sent an unsolicited response.'));
      return;
    }
    if (this.receiveBuffer.byteLength > 0) {
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
    this.receiveBuffer = Buffer.alloc(0);
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
