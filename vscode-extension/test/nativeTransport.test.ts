import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { once } from 'node:events';
import test from 'node:test';

import {
  connectNativeTransport,
  nativeEndpoint,
  RequestCancelledError,
  wslLaunch,
} from '../src/nativeTransport.js';
import { resolveWslRoot } from '../src/rootResolver.js';
import type { CancellationLike, ClientSettings, DisposableLike } from '../src/types.js';

const settings: ClientSettings = {
  socketPath: '',
  autoStartService: false,
  requestTimeoutMs: 1_000,
  wslDistribution: 'Ubuntu-22.04',
  wslExecutablePath: '/home/test/.local/bin/tctx',
};

function cancellation(): CancellationLike & { cancel(): void } {
  const listeners = new Set<() => void>();
  let cancelled = false;
  return {
    get isCancellationRequested(): boolean {
      return cancelled;
    },
    onCancellationRequested(listener: () => void): DisposableLike {
      listeners.add(listener);
      return {
        dispose: (): void => {
          listeners.delete(listener);
        },
      };
    },
    cancel(): void {
      cancelled = true;
      for (const listener of listeners) {
        listener();
      }
    },
  };
}

function launcher(script: string): () => ChildProcessWithoutNullStreams {
  return () => spawn(process.execPath, ['-e', script], { stdio: 'pipe' });
}

void test('WSL launch uses tctx native IPC without MCP or a network port', () => {
  const launch = wslLaunch(settings);
  assert.deepEqual(launch.args, [
    '--distribution',
    'Ubuntu-22.04',
    '--exec',
    '/home/test/.local/bin/tctx',
    'native-bridge',
  ]);
  assert.ok(launch.command.endsWith('\\System32\\wsl.exe'));
  assert.ok(nativeEndpoint(settings, 'win32').includes('Ubuntu-22.04'));
  assert.throws(() => wslLaunch({ ...settings, wslDistribution: '--shutdown' }));
  assert.throws(() => wslLaunch({ ...settings, wslDistribution: 'bad\nname' }));
  assert.throws(() => wslLaunch({ ...settings, wslExecutablePath: 'relative/tctx' }));
  assert.throws(() => wslLaunch({ ...settings, socketPath: '/other/daemon.sock' }));
});

void test('WSL readiness may be fragmented and the bridge is bidirectional', async () => {
  const stream = await connectNativeTransport(
    settings,
    cancellation(),
    'win32',
    launcher(`
      process.stdout.write('TCT');
      setTimeout(() => process.stdout.write('XN1\\n'), 10);
      process.stdin.on('data', data => process.stdout.write(data));
      process.stdin.on('end', () => process.exit(0));
    `),
  );
  try {
    const received = new Promise<Buffer>((resolve, reject) => {
      stream.once('data', (data: Buffer) => {
        resolve(data);
      });
      stream.once('error', reject);
    });
    stream.write(Buffer.from('native request'));
    assert.equal((await received).toString(), 'native request');
  } finally {
    stream.destroy();
  }
});

void test('invalid bridge readiness is rejected', async () => {
  await assert.rejects(
    connectNativeTransport(
      settings,
      cancellation(),
      'win32',
      launcher(`
        process.stdout.write('MCP READY\\n');
        process.stdin.resume();
      `),
    ),
    /invalid readiness frame/u,
  );
});

void test('cancelling bridge startup terminates the owned WSL process', async () => {
  const token = cancellation();
  let child: ChildProcessWithoutNullStreams | undefined;
  const pending = connectNativeTransport(settings, token, 'win32', () => {
    child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'pipe',
    });
    return child;
  });
  assert.ok(child);
  const closed = once(child, 'close');
  token.cancel();
  await assert.rejects(pending, RequestCancelledError);
  await closed;
});

void test('Windows Agent roots preserve absolute Linux paths', () => {
  const roots = [
    { root: '/home/test/repo', scopes: ['.'], globs: ['*.py'] },
    { root: '/home/test/other', scopes: ['.'], globs: ['*.py'] },
  ];
  assert.equal(resolveWslRoot(roots, '/home/test/repo/src/..'), '/home/test/repo');
  assert.equal(resolveWslRoot(roots, '/home/test/repo/src/sub'), '/home/test/repo');
  assert.equal(
    resolveWslRoot(roots, undefined, ['/home/test/other/src/main.rs']),
    '/home/test/other',
  );
  assert.equal(
    resolveWslRoot(roots, undefined, ['\\\\wsl.localhost\\Ubuntu-22.04\\home\\test\\repo\\src']),
    '/home/test/repo',
  );
  assert.throws(() => resolveWslRoot(roots, undefined), /explicit absolute Linux root/u);
  assert.throws(() => resolveWslRoot(roots, 'C:\\Users\\test'), /absolute Linux path/u);
});
