/**
 * H3 loopback tests: HTTP/3 request/response over localhost.
 * Exercises the full createSecureServer + connect/connectAsync path.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect, connectAsync } from '../../lib/index.js';
import type { Http3SecureServer, Http3ClientSession, ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';

interface DoRequestResult {
  status: string;
  headers: Record<string, string>;
  body: Buffer;
}

async function doRequest(
  session: Http3ClientSession,
  method: string,
  path: string,
  body?: Buffer,
): Promise<DoRequestResult> {
  // Retry on StreamBlocked — H3 may not have peer stream credits yet
  let stream: ClientHttp3Stream;
  for (let attempt = 0; ; attempt++) {
    try {
      stream = session.request({
        ':method': method,
        ':path': path,
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: !body });
      break;
    } catch (err: unknown) {
      if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
        await new Promise<void>((r) => { setTimeout(r, 5); });
        continue;
      }
      throw err;
    }
  }

  if (body) {
    stream.end(body);
  }

  return new Promise((resolve, reject) => {
    let status = '';
    const hdrs: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error(`doRequest ${method} ${path} timed out`)), 15000);

    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      for (const [k, v] of Object.entries(h)) {
        hdrs[k] = v;
      }
    });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve({ status, headers: hdrs, body: Buffer.concat(chunks) });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

function collect(stream: ClientHttp3Stream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error('collect timed out')), 10000);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

describe('H3 loopback', () => {
  let certs: { key: Buffer; cert: Buffer };
  let server: Http3SecureServer;
  let port: number;

  before(async () => {
    certs = generateTestCerts();

    // Echo handler: echoes request body on POST, serves static responses based on path
    server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
      const method = headers[':method'] as string;
      const path = headers[':path'] as string;

      if (method === 'HEAD') {
        stream.respond({ ':status': '200', 'content-length': '0' }, { endStream: true });
        return;
      }

      if (path === '/hello') {
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end('Hello H3');
        return;
      }

      if (path === '/no-content') {
        stream.respond({ ':status': '204' }, { endStream: true });
        return;
      }

      if (path === '/large-stream') {
        stream.respond({ ':status': '200' });
        const payload = Buffer.alloc(256 * 1024, 'L');
        stream.end(payload);
        return;
      }

      if (path === '/custom-headers') {
        stream.respond({
          ':status': '200',
          'x-custom': 'test-value',
          'x-server': 'h3-loopback',
        });
        stream.end('ok');
        return;
      }

      // Default: echo handler for POST bodies, path echo for GET
      if (method === 'POST') {
        if (flags.endStream) {
          stream.respond({ ':status': '200' });
          stream.end('no body');
          return;
        }
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
        stream.on('end', () => {
          stream.respond({ ':status': '200' });
          stream.end(Buffer.concat(chunks));
        });
        return;
      }

      // Default GET: echo the path
      stream.respond({ ':status': '200' });
      stream.end(path);
    });

    port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });
  });

  after(async () => {
    await server.close();
  });

  it('basic GET request/response', async () => {
    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const res = await doRequest(session, 'GET', '/hello');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'Hello H3');

    await session.close();
  });

  it('POST with echo body', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const payload = Buffer.alloc(4 * 1024, 'E');
    const res = await doRequest(session, 'POST', '/echo', payload);
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 4 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);

    await session.close();
  });

  it('POST with 1MB body', { timeout: 30000 }, async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const payload = Buffer.alloc(1024 * 1024, 'M');
    const res = await doRequest(session, 'POST', '/echo-1m', payload);
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 1024 * 1024);

    await session.close();
  });

  it('multiple concurrent sessions to same server', async () => {
    const sessions = await Promise.all(
      Array.from({ length: 3 }, () =>
        connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false }),
      ),
    );

    const results = await Promise.all(
      sessions.map(async (session, i) => {
        const res = await doRequest(session, 'GET', `/concurrent-${i}`);
        return { status: res.status, body: res.body.toString() };
      }),
    );

    for (let i = 0; i < 3; i++) {
      assert.strictEqual(results[i].status, '200');
      assert.strictEqual(results[i].body, `/concurrent-${i}`);
    }

    await Promise.all(sessions.map((s) => s.close()));
  });

  it('5 sequential request/response cycles on same session', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    for (let i = 0; i < 5; i++) {
      const res = await doRequest(session, 'GET', `/seq-${i}`);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.toString(), `/seq-${i}`);
    }

    await session.close();
  });

  it('large streaming response: 256KB chunked', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const res = await doRequest(session, 'GET', '/large-stream');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 256 * 1024);

    await session.close();
  });

  it('server responds with 204 No Content', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const res = await doRequest(session, 'GET', '/no-content');
    assert.strictEqual(res.status, '204');
    assert.strictEqual(res.body.length, 0);

    await session.close();
  });

  it('session metrics available', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    // Do a request so metrics accumulate
    await doRequest(session, 'GET', '/hello');

    // Wait briefly for metrics to settle
    await new Promise<void>((r) => { setTimeout(r, 50); });

    const metrics = session.getMetrics();
    assert.ok(metrics, 'metrics should be available');
    assert.ok(metrics!.packetsIn > 0, 'should have received packets');

    await session.close();
  });

  it('HEAD request returns no body', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const res = await doRequest(session, 'HEAD', '/hello');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 0);

    await session.close();
  });

  it('custom response headers preserved', async () => {
    const session = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });

    const res = await doRequest(session, 'GET', '/custom-headers');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.headers['x-custom'], 'test-value');
    assert.strictEqual(res.headers['x-server'], 'h3-loopback');

    await session.close();
  });
});
