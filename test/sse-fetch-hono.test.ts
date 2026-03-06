import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { connect } from '../lib/index.js';
import { createSseFetchResponse, serveFetch } from '../lib/fetch-adapter.js';
import { generateTestCerts } from './generate-certs.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const started = Date.now();
  while (!condition()) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

function parseSse(payload: string): string[] {
  return payload
    .split('\n\n')
    .filter(Boolean)
    .map((frame) => frame
      .split('\n')
      .filter(line => line.startsWith('data:'))
      .map(line => line.slice(5).trimStart())
      .join('\n'))
    .filter(Boolean);
}

describe('Fetch adapter SSE helper', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('creates streaming SSE responses for fetch handlers', async () => {
    const server = serveFetch({
      port: 0,
      host: '127.0.0.1',
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      fetch: () => {
        async function* events(): AsyncGenerator<{ data: string }> {
          yield { data: 'first' };
          yield { data: 'second' };
        }
        return createSseFetchResponse(events());
      },
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let contentType = '';
    const chunks: Buffer[] = [];
    await new Promise<void>((resolve, reject) => {
      stream.on('response', (headers: Record<string, string>) => {
        contentType = headers['content-type'] ?? '';
      });
      stream.on('data', (chunk: Buffer) => chunks.push(chunk));
      stream.on('end', () => resolve());
      stream.on('error', reject);
    });

    const events = parseSse(Buffer.concat(chunks).toString('utf8'));
    assert.ok(contentType.includes('text/event-stream'));
    assert.deepStrictEqual(events, ['first', 'second']);

    await session.close();
    await server.close();
  });

  it('cancels SSE iterators when clients disconnect', async () => {
    let activeClients = 0;
    let cleanupCount = 0;

    const server = serveFetch({
      port: 0,
      host: '127.0.0.1',
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      fetch: () => {
        async function* events(): AsyncGenerator<{ data: string }> {
          activeClients += 1;
          try {
            let seq = 0;
            for (;;) {
              yield { data: `tick-${String(seq++)}` };
              await new Promise<void>((resolve) => { setTimeout(resolve, 25); });
            }
          } finally {
            activeClients = Math.max(0, activeClients - 1);
            cleanupCount += 1;
          }
        }
        return createSseFetchResponse(events());
      },
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    await new Promise<void>((resolve, reject) => {
      stream.on('data', () => resolve());
      stream.on('error', reject);
    });

    stream.close();
    await session.close();
    await waitFor(() => cleanupCount >= 1 && activeClients === 0, 3000);

    await server.close();
  });
});
