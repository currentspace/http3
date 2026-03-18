import assert from 'node:assert';
import { describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
  connectQuicAsync,
  createQuicServer,
} from '../lib/index.js';
import type { QuicClientSession, QuicServer, QuicServerSession } from '../lib/index.js';
import type { QuicStream } from '../lib/quic-stream.js';
import { generateTestCerts } from './generate-certs.js';

function isFastPathUnavailable(error: unknown): boolean {
  return error instanceof Http3Error && error.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE;
}

function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream timed out')), timeoutMs);
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

function localPortForSession(session: QuicClientSession): number {
  const loop = (session as unknown as {
    _eventLoop: { worker: { localAddress(): { port: number } } } | null;
  })._eventLoop;
  assert.ok(loop, 'expected client event loop to be available');
  return loop.worker.localAddress().port;
}

describe('raw QUIC fast shared worker', () => {
  it('reuses one local UDP port across concurrent fast sessions', { timeout: 20_000 }, async (t) => {
    const certs = generateTestCerts();
    const payload = Buffer.from('shared-fast-worker');
    let server: QuicServer | null = null;
    let clients: QuicClientSession[] = [];

    try {
      server = createQuicServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'fast',
        fallbackPolicy: 'error',
      });

      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => {
          stream.pipe(stream);
        });
      });

      const addr = await server.listen(0, '127.0.0.1');
      clients = await Promise.all(Array.from({ length: 4 }, () => connectQuicAsync(
        `127.0.0.1:${addr.port}`,
        {
          rejectUnauthorized: false,
          runtimeMode: 'fast',
          fallbackPolicy: 'error',
        },
      )));

      for (const client of clients) {
        assert.strictEqual(client.runtimeInfo?.selectedMode, 'fast');
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collectStream(stream, 5_000);
        assert.deepStrictEqual(echoed, payload);
      }

      const localPorts = clients.map(localPortForSession);
      assert.strictEqual(new Set(localPorts).size, 1, `expected a shared client UDP port, got ${localPorts.join(', ')}`);
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
});
