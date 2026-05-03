import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  connectAsync,
  connectQuicAsync,
  createQuicServer,
  createSecureServer,
} from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { generateTestCerts } from '../support/generate-certs.js';

function listenH3(server: ReturnType<typeof createSecureServer>): Promise<number> {
  return new Promise((resolve) => {
    server.once('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
    server.listen(0, '127.0.0.1');
  });
}

function collectStream(stream: ClientHttp3Stream | QuicStream): Promise<number> {
  return new Promise((resolve, reject) => {
    let bytes = 0;
    stream.on('response', () => {
      // H3 only; raw QUIC streams never emit this event.
    });
    stream.on('data', (chunk: Buffer) => {
      bytes += chunk.length;
    });
    stream.once('end', () => {
      resolve(bytes);
    });
    stream.once('error', reject);
  });
}

describe('flow-control window replenishment', () => {
  it('keeps HTTP/3 sessions open after more than the initial connection credit is consumed', async () => {
    const certs = generateTestCerts();
    const payload = Buffer.alloc(64 * 1024, 0x5a);
    const errors: Error[] = [];

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      initialMaxData: 256 * 1024,
      initialMaxStreamsBidi: 1_000,
    }, (stream) => {
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => {
        chunks.push(chunk);
      });
      stream.on('end', () => {
        stream.respond({ ':status': '200' });
        for (let i = 0; i < chunks.length - 1; i += 1) {
          stream.write(chunks[i]);
        }
        stream.end(chunks.at(-1));
      });
    });
    server.on('session', (session) => {
      session.on('error', (error) => {
        errors.push(error);
      });
    });

    const port = await listenH3(server);
    const session = await connectAsync(`https://127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      initialMaxData: 256 * 1024,
      initialMaxStreamsBidi: 1_000,
    });
    session.on('error', (error) => {
      errors.push(error);
    });

    try {
      for (let i = 0; i < 24; i += 1) {
        const stream = await session.requestAsync({
          ':method': 'POST',
          ':path': '/echo',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { timeoutMs: 5_000 });
        stream.end(payload);
        assert.equal(await collectStream(stream), payload.length);
      }
      assert.equal(session.closed, false);
      assert.deepEqual(errors.map((error) => error.message), []);
    } finally {
      await session.close();
      await server.close();
    }
  });

  it('keeps raw QUIC sessions open after more than the initial connection credit is consumed', async () => {
    const certs = generateTestCerts();
    const payload = Buffer.alloc(64 * 1024, 0xa5);
    const errors: Error[] = [];

    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      initialMaxData: 256 * 1024,
      initialMaxStreamsBidi: 1_000,
    });
    server.on('session', (session) => {
      session.on('error', (error) => {
        errors.push(error);
      });
      session.on('stream', (stream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => {
          chunks.push(chunk);
        });
        stream.on('end', () => {
          for (let i = 0; i < chunks.length - 1; i += 1) {
            stream.write(chunks[i]);
          }
          stream.end(chunks.at(-1));
        });
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const session = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      initialMaxData: 256 * 1024,
      initialMaxStreamsBidi: 1_000,
    });
    session.on('error', (error) => {
      errors.push(error);
    });

    try {
      for (let i = 0; i < 24; i += 1) {
        const stream = session.openStream();
        stream.end(payload);
        assert.equal(await collectStream(stream), payload.length);
      }
      assert.equal(session.closed, false);
      assert.deepEqual(errors.map((error) => error.message), []);
    } finally {
      await session.close();
      await server.close();
    }
  });
});
