import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

describe('Load smoke gate', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('handles bounded concurrent request load', { timeout: 30000 }, async () => {
    const total = Number.parseInt(process.env.HTTP3_LOAD_SMOKE_TOTAL ?? '150', 10);
    const concurrency = Number.parseInt(process.env.HTTP3_LOAD_SMOKE_CONCURRENCY ?? '25', 10);
    const maxDurationMs = Number.parseInt(process.env.HTTP3_LOAD_SMOKE_MAX_MS ?? '9000', 10);

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      const path = String(headers[':path'] ?? '/0');
      stream.respond({ ':status': '200' });
      stream.end(path.slice(1));
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    let next = 0;
    let failures = 0;
    const startedAt = Date.now();

    const runWorker = async (): Promise<void> => {
      while (next < total) {
        const id = next++;
        const body = await new Promise<string>((resolve, reject) => {
          const stream = session.request({
            ':method': 'GET',
            ':path': `/${id}`,
            ':authority': 'localhost',
            ':scheme': 'https',
          }, { endStream: true });
          const chunks: Buffer[] = [];
          stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
          stream.on('error', reject);
          stream.on('end', () => { resolve(Buffer.concat(chunks).toString()); });
        }).catch(() => null);

        if (body === null || body !== String(id)) {
          failures++;
        }
      }
    };

    await Promise.all(Array.from({ length: concurrency }, () => runWorker()));
    const elapsedMs = Date.now() - startedAt;

    await session.close();
    await server.close();

    assert.strictEqual(failures, 0, `load smoke had ${failures} failed responses`);
    assert.ok(
      elapsedMs <= maxDurationMs,
      `load smoke exceeded threshold: ${elapsedMs}ms > ${maxDurationMs}ms`,
    );
  });
});
