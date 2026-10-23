import { spawn, execFile, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createConnection } from 'node:net';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve, posix, win32 } from 'node:path';
import { Duplex } from 'node:stream';
import { promisify } from 'node:util';

import type { CancellationLike, ClientSettings, DisposableLike } from './types.js';

const CONNECT_TIMEOUT_MS = 5_000;
const BRIDGE_READY = Buffer.from('TCTXN1\n', 'ascii');
const execFileAsync = promisify(execFile);

export class RequestCancelledError extends Error {
  public constructor() {
    super('Test Context native tool request was cancelled.');
    this.name = 'RequestCancelledError';
  }
}

export function defaultSocketPath(): string {
  return process.env.TCTX_HOME
    ? join(process.env.TCTX_HOME, 'daemon.sock')
    : join(homedir(), '.local/state/test-context/daemon.sock');
}

export function normalizeSocketPath(configured: string): string {
  const value = configured.trim();
  if (value.length === 0) {
    return defaultSocketPath();
  }
  if (value.startsWith('~/')) {
    return join(homedir(), value.slice(2));
  }
  if (!isAbsolute(value)) {
    throw new Error('testContextNative.socketPath must be absolute.');
  }
  return resolve(value);
}

export function wslLaunch(
  settings: ClientSettings,
  operation: 'bridge' | 'service' = 'bridge',
): { command: string; args: string[] } {
  const distribution = settings.wslDistribution ?? '';
  const executable = settings.wslExecutablePath ?? '';
  if (
    !distribution ||
    distribution !== distribution.trim() ||
    distribution.startsWith('-') ||
    /[\0\r\n]/u.test(distribution)
  ) {
    throw new Error('Configure testContextNative.wslDistribution for this Windows window.');
  }
  if (!posix.isAbsolute(executable) || /[\0\r\n]/u.test(executable)) {
    throw new Error('testContextNative.wslExecutablePath must be an absolute Linux tctx path.');
  }
  if (settings.socketPath.trim()) {
    throw new Error('Windows WSL native IPC uses the WSL daemon runtime; leave socketPath empty.');
  }
  const command = win32.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'wsl.exe');
  return {
    command,
    args: [
      '--distribution',
      distribution,
      '--exec',
      ...(operation === 'bridge'
        ? [executable, 'native-bridge']
        : ['systemctl', '--user', 'start', 'test-context.service']),
    ],
  };
}

export function nativeEndpoint(settings: ClientSettings, platform = process.platform): string {
  if (platform === 'win32') {
    return `wsl:${settings.wslDistribution ?? ''}:${settings.wslExecutablePath ?? ''}:${settings.socketPath}`;
  }
  return normalizeSocketPath(settings.socketPath);
}

type BridgeLauncher = (command: string, args: string[]) => ChildProcessWithoutNullStreams;

export async function connectNativeTransport(
  settings: ClientSettings,
  cancellation: CancellationLike,
  platform = process.platform,
  launcher: BridgeLauncher = (command, args) =>
    spawn(command, args, { stdio: 'pipe', windowsHide: true }),
): Promise<Duplex> {
  if (cancellation.isCancellationRequested) {
    throw new RequestCancelledError();
  }
  if (platform === 'win32') {
    const launch = wslLaunch(settings);
    return new Promise<Duplex>((resolveConnect, rejectConnect) => {
      const child = launcher(launch.command, launch.args);
      let transport: Duplex | undefined;
      let ready = Buffer.alloc(0);
      let stderr = '';
      let settled = false;
      const resources: { timer?: NodeJS.Timeout; listener?: DisposableLike } = {};
      const cleanup = (): void => {
        clearTimeout(resources.timer);
        resources.listener?.dispose();
        child.stdout.off('data', onReady);
      };
      const fail = (error: Error): void => {
        if (transport) {
          transport.destroy(error);
        } else if (!settled) {
          settled = true;
          cleanup();
          child.kill();
          rejectConnect(error);
        }
      };
      const onReady = (chunk: Buffer): void => {
        ready = Buffer.concat([ready, chunk]);
        if (
          ready.length > BRIDGE_READY.length ||
          !BRIDGE_READY.subarray(0, ready.length).equals(ready)
        ) {
          fail(new Error('WSL native IPC bridge returned an invalid readiness frame.'));
          return;
        }
        if (ready.length === BRIDGE_READY.length) {
          settled = true;
          cleanup();
          child.stdout.pause();
          transport = Duplex.from({ readable: child.stdout, writable: child.stdin });
          transport.once('close', () => {
            if (child.exitCode === null) {
              child.kill();
            }
          });
          resolveConnect(transport);
        }
      };
      child.stderr.on('data', (chunk: Buffer) => {
        stderr = (stderr + chunk.toString('utf8')).slice(-8192);
      });
      child.once('error', fail);
      child.once('close', (code, signal) => {
        fail(new Error(`WSL native IPC bridge closed (${code ?? signal}): ${stderr.trim()}`));
      });
      child.stdout.on('data', onReady);
      resources.timer = setTimeout(() => {
        fail(new Error('Timed out waiting for the WSL native IPC bridge.'));
      }, CONNECT_TIMEOUT_MS);
      resources.listener = cancellation.onCancellationRequested(() => {
        fail(new RequestCancelledError());
      });
    });
  }
  if (platform !== 'linux') {
    throw new Error('Native Test Context requires Linux or the configured Windows WSL bridge.');
  }
  return new Promise<Duplex>((resolveConnect, rejectConnect) => {
    const socket = createConnection(normalizeSocketPath(settings.socketPath));
    const resources: { timer?: NodeJS.Timeout; listener?: DisposableLike } = {};
    const cleanup = (): void => {
      clearTimeout(resources.timer);
      resources.listener?.dispose();
    };
    const fail = (error: Error): void => {
      cleanup();
      socket.destroy();
      rejectConnect(error);
    };
    resources.timer = setTimeout(() => {
      fail(new Error('Timed out connecting to the Test Context daemon socket.'));
    }, CONNECT_TIMEOUT_MS);
    resources.listener = cancellation.onCancellationRequested(() => {
      fail(new RequestCancelledError());
    });
    socket.once('error', fail);
    socket.once('connect', () => {
      cleanup();
      socket.off('error', fail);
      resolveConnect(socket);
    });
  });
}

export async function startNativeService(
  settings: ClientSettings,
  platform = process.platform,
): Promise<void> {
  try {
    if (platform === 'win32') {
      const launch = wslLaunch(settings, 'service');
      await execFileAsync(launch.command, launch.args, { timeout: CONNECT_TIMEOUT_MS });
      return;
    }
    if (platform === 'linux') {
      await execFileAsync('systemctl', ['--user', 'start', 'test-context.service'], {
        timeout: CONNECT_TIMEOUT_MS,
      });
    }
  } catch {
    // Best-effort service start
  }
}
