import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createServer, type Server, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  DaemonClient,
  DaemonResponseError,
  RequestCancelledError,
} from '../src/daemonClient.js';
import type {
  CancellationLike,
  ClientSettings,
  DaemonRequest,
  DisposableLike,
} from '../src/types.js';

const FRAMED_JSON_MAGIC = Buffer.from('TCTXJ1\n', 'ascii');

class TestCancellation implements CancellationLike {
  private readonly listeners = new Set<() => void>();
  public isCancellationRequested = false;

  public onCancellationRequested(listener: () => void): DisposableLike {
    this.listeners.add(listener);
    return { dispose: () => this.listeners.delete(listener) };
  }

  public cancel(): void {
    this.isCancellationRequested = true;
    for (const listener of [...this.listeners]) {
      listener();
    }
  }
}

function writeResponse(socket: Socket, value: unknown): void {
  const payload = Buffer.from(JSON.stringify(value), 'utf8');
  const frame = Buffer.allocUnsafe(payload.byteLength + 4);
  frame.writeUInt32BE(payload.byteLength, 0);
  payload.copy(frame, 4);
  socket.write(frame);
}

function responseFrame(value: unknown): Buffer {
  const payload = Buffer.from(JSON.stringify(value), 'utf8');
  const frame = Buffer.allocUnsafe(payload.byteLength + 4);
  frame.writeUInt32BE(payload.byteLength, 0);
  payload.copy(frame, 4);
  return frame;
}

function daemonRequest(name: string): DaemonRequest {
  return { root: null, name, arguments: {}, compact: true };
}

function attachFramedHandler(
  socket: Socket,
  handler: (requestValue: unknown, socket: Socket) => void,
): void {
  let seenMagic = false;
  let buffer = Buffer.alloc(0);
  socket.on('data', (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk]);
    if (!seenMagic) {
      if (buffer.byteLength < FRAMED_JSON_MAGIC.byteLength) {
        return;
      }
      const header = buffer.subarray(0, FRAMED_JSON_MAGIC.byteLength);
      assert.deepEqual(header, FRAMED_JSON_MAGIC);
      buffer = buffer.subarray(FRAMED_JSON_MAGIC.byteLength);
      seenMagic = true;
    }
    while (buffer.byteLength >= 4) {
      const length = buffer.readUInt32BE(0);
      if (buffer.byteLength < length + 4) {
        return;
      }
      const body = buffer.subarray(4, length + 4);
      buffer = buffer.subarray(length + 4);
      handler(JSON.parse(body.toString('utf8')), socket);
    }
  });
}

async function createMockDaemon(
  handler: (requestValue: unknown, socket: Socket) => void,
): Promise<{
  path: string;
  server: Server;
  connections: () => number;
  cleanup(): Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), 'tctx-native-'));
  const path = join(dir, 'daemon.sock');
  let count = 0;
  const server = createServer((socket) => {
    count += 1;
    attachFramedHandler(socket, handler);
  });
  await new Promise<void>((resolveListen, rejectListen) => {
    server.once('error', rejectListen);
    server.listen(path, resolveListen);
  });
  return {
    path,
    server,
    connections: () => count,
    cleanup: async (): Promise<void> => {
      await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
      await rm(dir, { recursive: true, force: true });
    },
  };
}

function settings(socketPath: string, timeout = 2_000): ClientSettings {
  return {
    socketPath,
    autoStartService: false,
    requestTimeoutMs: timeout,
  };
}

void test('DaemonClient reuses persistent framed connection and negotiates trace IDs', async () => {
  let pings = 0;
  const mock = await createMockDaemon((raw, socket) => {
    const req = raw as DaemonRequest;
    if (req.name === 'ping') {
      pings += 1;
      writeResponse(socket, {
        result: {
          capabilities: { request_tracing: true, trace_clock: 'unix_ms' },
        },
      });
    } else {
      writeResponse(socket, {
        result: {
          _tctx_trace: { trace_id: req.arguments._tctx_trace, execute_ms: 0.4 },
          value: { status: 'passed', mode: 'instant_cache_hit' },
        },
      });
    }
  });

  const client = new DaemonClient(() => settings(mock.path));
  try {
    const res1 = await client.requestTraced(
      { root: '/tmp/repo', name: 'test_run', arguments: {}, compact: false },
      new TestCancellation(),
      'trace-1',
    );
    const res2 = await client.requestTraced(
      { root: '/tmp/repo', name: 'test_run', arguments: {}, compact: false },
      new TestCancellation(),
      'trace-2',
    );
    assert.deepEqual(res1.value, { status: 'passed', mode: 'instant_cache_hit' });
    assert.deepEqual(res2.value, { status: 'passed', mode: 'instant_cache_hit' });
    assert.equal(pings, 1);
    assert.equal(mock.connections(), 1);
  } finally {
    client.dispose();
    await mock.cleanup();
  }
});

void test('DaemonClient surfaces application errors without reconnecting and supports cancellation', async () => {
  const mock = await createMockDaemon((raw, socket) => {
    const req = raw as DaemonRequest;
    if (req.name === 'bad_req') {
      writeResponse(socket, { error: 'file is not indexed in the Python AST graph' });
    }
  });

  const client = new DaemonClient(() => settings(mock.path));
  try {
    await assert.rejects(
      () =>
        client.request(
          { root: '/tmp/repo', name: 'bad_req', arguments: {}, compact: false },
          new TestCancellation(),
        ),
      DaemonResponseError,
    );
    assert.equal(mock.connections(), 1);

    const cancelled = new TestCancellation();
    cancelled.cancel();
    await assert.rejects(
      () =>
        client.request(
          { root: '/tmp/repo', name: 'test_status', arguments: {}, compact: false },
          cancelled,
        ),
      RequestCancelledError,
    );
  } finally {
    client.dispose();
    await mock.cleanup();
  }
});

void test('active request cancellation sends test_cancel over a second connection', async () => {
  let cancelSeen = false;
  let resolveCancel!: () => void;
  const cancelArrived = new Promise<void>((resolve) => {
    resolveCancel = resolve;
  });
  const mock = await createMockDaemon((raw, socket) => {
    const req = raw as DaemonRequest;
    if (req.name === 'test_cancel') {
      cancelSeen = true;
      // The production cancellation client is fire-and-forget; no response is required.
      resolveCancel();
      socket.end();
    }
    // Deliberately leave test_run pending until the client cancels.
  });
  const client = new DaemonClient(() => settings(mock.path, 2_000));
  const cancellation = new TestCancellation();
  try {
    const pending = client.request(
      { root: '/tmp/repo', name: 'test_run', arguments: {}, compact: false },
      cancellation,
    );
    await new Promise((resolve) => setTimeout(resolve, 25));
    cancellation.cancel();
    await assert.rejects(() => pending, RequestCancelledError);
    await Promise.race([
      cancelArrived,
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error('test_cancel was not observed')), 1_000),
      ),
    ]);
    assert.equal(cancelSeen, true);
    assert.equal(mock.connections(), 2);
  } finally {
    client.dispose();
    await mock.cleanup();
  }
});

void test('multiple response frames are rejected as protocol desynchronization', async () => {
  const mock = await createMockDaemon((_raw, socket) => {
    socket.write(
      Buffer.concat([
        responseFrame({ result: { ok: 1 } }),
        responseFrame({ result: { unsolicited: true } }),
      ]),
    );
  });
  const client = new DaemonClient(() => settings(mock.path));
  try {
    await assert.rejects(
      () => client.request(daemonRequest('ping'), new TestCancellation()),
      /multiple responses|desynchronized/u,
    );
  } finally {
    client.dispose();
    await mock.cleanup();
  }
});

void test('timeout reconnects and succeeds on the second connection', async () => {
  let calls = 0;
  const mock = await createMockDaemon((_raw, socket) => {
    calls += 1;
    if (calls >= 2) {
      writeResponse(socket, { result: { recovered: true } });
    }
  });
  const client = new DaemonClient(() => settings(mock.path, 30));
  try {
    const result = await client.request<{ recovered: boolean }>(
      daemonRequest('ping'),
      new TestCancellation(),
    );
    assert.deepEqual(result, { recovered: true });
    assert.equal(mock.connections(), 2);
  } finally {
    client.dispose();
    await mock.cleanup();
  }
});
