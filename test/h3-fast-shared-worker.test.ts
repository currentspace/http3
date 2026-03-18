import assert from 'node:assert';
import { describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
  connectAsync,
  createSecureServer,
} from '../lib/index.js';
import { binding } from '../lib/event-loop.js';
import type { Http3ClientSession, Http3SecureServer } from '../lib/index.js';
import { generateTestCerts } from './generate-certs.js';

function isFastPathUnavailable(error: unknown): boolean {
  return error instanceof Http3Error && error.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE;
}

function localPortForSession(session: Http3ClientSession): number {
  const loop = (session as unknown as {
    _eventLoop: { worker: { localAddress(): { port: number } } } | null;
  })._eventLoop;
  assert.ok(loop, 'expected client event loop to be available');
  return loop.worker.localAddress().port;
}

async function doRequest(session: Http3ClientSession, body: Buffer): Promise<Buffer> {
  const stream = session.request({
    ':method': 'POST',
    ':path': '/echo',
    ':authority': 'localhost',
    ':scheme': 'https',
  }, { endStream: false });
  stream.end(body);

  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('request timed out')), 5_000);
    stream.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
    });
    stream.on('end', () => {
      clearTimeout(timer);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (error: Error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

describe('H3 shared worker topology', () => {
  it('reuses one local UDP port across concurrent fast sessions', { timeout: 20_000 }, async (t) => {
    const certs = generateTestCerts();
    const payload = Buffer.from('shared-h3-worker');
    let server: Http3SecureServer | null = null;
    let clients: Http3ClientSession[] = [];

    try {
      binding.resetRuntimeTelemetry();
      server = createSecureServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'fast',
        fallbackPolicy: 'error',
      }, (stream, _headers, flags) => {
        if (flags.endStream) {
          stream.respond({ ':status': '200' }, { endStream: true });
          return;
        }
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => {
          chunks.push(chunk);
        });
        stream.on('end', () => {
          stream.respond({ ':status': '200' });
          stream.end(Buffer.concat(chunks));
        });
      });

      const addr = await new Promise<{ address: string; port: number }>((resolve) => {
        server!.on('listening', () => {
          resolve(server!.address()!);
        });
        server!.listen(0, '127.0.0.1');
      });

      clients = await Promise.all(Array.from({ length: 4 }, () => connectAsync(
        `127.0.0.1:${addr.port}`,
        {
          rejectUnauthorized: false,
          runtimeMode: 'fast',
          fallbackPolicy: 'error',
        },
      )));

      for (const client of clients) {
        assert.strictEqual(client.runtimeInfo?.selectedMode, 'fast');
        const echoed = await doRequest(client, payload);
        assert.deepStrictEqual(echoed, payload);
      }

      const localPorts = clients.map(localPortForSession);
      assert.strictEqual(new Set(localPorts).size, 1, `expected a shared client UDP port, got ${localPorts.join(', ')}`);

      const telemetry = binding.runtimeTelemetry();
      assert.strictEqual(telemetry.h3ClientSharedWorkersCreated, 1);
      assert.strictEqual(telemetry.h3ClientDedicatedWorkerSpawns, 0);
      assert.strictEqual(telemetry.h3ClientSessionsOpened, clients.length);
      assert.ok(telemetry.h3ClientSharedWorkerReuses >= clients.length - 1);
      assert.ok(telemetry.clientLocalPortReuseHits >= clients.length - 1);

      await Promise.all(clients.map((client) => client.close()));
      clients = [];
      await new Promise<void>((resolve) => { setTimeout(resolve, 50); });
      const closedTelemetry = binding.runtimeTelemetry();
      assert.ok(closedTelemetry.h3ClientSessionsClosed >= 1);
    } catch (error: unknown) {
      if (isFastPathUnavailable(error)) {
        const message = error instanceof Error ? error.message : String(error);
        t.skip(`fast path unavailable on this host: ${message}`);
        return;
      }
      throw error;
    } finally {
      await Promise.all(clients.map(async (client) => {
        try {
          await client.close();
        } catch {
          // Best-effort cleanup.
        }
      }));
      if (server) {
        await server.close();
      }
    }
  });

  it('reuses one local UDP port across concurrent portable sessions on macOS', { timeout: 20_000 }, async (t) => {
    if (process.platform !== 'darwin') {
      t.skip('kqueue topology alignment is only asserted in this test on macOS');
      return;
    }

    const certs = generateTestCerts();
    let server: Http3SecureServer | null = null;
    let clients: Http3ClientSession[] = [];

    try {
      binding.resetRuntimeTelemetry();
      server = createSecureServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      }, (stream, _headers, flags) => {
        if (flags.endStream) {
          stream.respond({ ':status': '204' }, { endStream: true });
        }
      });

      const addr = await new Promise<{ address: string; port: number }>((resolve) => {
        server!.on('listening', () => {
          resolve(server!.address()!);
        });
        server!.listen(0, '127.0.0.1');
      });

      clients = await Promise.all(Array.from({ length: 3 }, () => connectAsync(
        `127.0.0.1:${addr.port}`,
        {
          rejectUnauthorized: false,
          runtimeMode: 'portable',
        },
      )));

      const localPorts = clients.map(localPortForSession);
      assert.strictEqual(new Set(localPorts).size, 1, `expected a shared portable client UDP port, got ${localPorts.join(', ')}`);

      const telemetry = binding.runtimeTelemetry();
      assert.strictEqual(telemetry.h3ClientSharedWorkersCreated, 1);
      assert.strictEqual(telemetry.h3ClientDedicatedWorkerSpawns, 0);
    } finally {
      await Promise.all(clients.map(async (client) => {
        try {
          await client.close();
        } catch {
          // Best-effort cleanup.
        }
      }));
      if (server) {
        await server.close();
      }
    }
  });
});
