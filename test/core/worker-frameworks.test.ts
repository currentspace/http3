/**
 * Worker mode tests: framework adapters (Fetch and Express-style).
 * Uses serveFetch() and inline Express-style handlers — no external dependencies.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect } from '../../lib/index.js';
import { serveFetch } from '../../lib/fetch-adapter.js';
import type { Http3SecureServer, Http3ClientSession, ClientHttp3Stream } from '../../lib/index.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

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
      for (const [k, v] of Object.entries(h)) hdrs[k] = v;
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

async function connectClient(port: number): Promise<Http3ClientSession> {
  const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
  let connected = false;
  session.on('connect', () => { connected = true; });
  await waitFor(() => connected, 3000);
  return session;
}

async function startFetchServer(
  certs: { key: Buffer; cert: Buffer },
  handler: (req: Request) => Response | Promise<Response>,
): Promise<{ server: Http3SecureServer; port: number }> {
  const server = serveFetch({
    port: 0,
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    fetch: handler,
  });

  const port = await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
  });

  return { server, port };
}

describe('Worker Frameworks — Fetch Adapter', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('Fetch: GET text', async () => {
    const { server, port } = await startFetchServer(certs, () => {
      return new Response('hello fetch');
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'hello fetch');
    await session.close();
    await server.close();
  });

  it('Fetch: GET JSON', async () => {
    const { server, port } = await startFetchServer(certs, () => {
      return new Response(JSON.stringify({ msg: 'h3' }), {
        headers: { 'content-type': 'application/json' },
      });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/json');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.headers['content-type'], 'application/json');
    const parsed = JSON.parse(res.body.toString()) as { msg: string };
    assert.strictEqual(parsed.msg, 'h3');
    await session.close();
    await server.close();
  });

  it('Fetch: POST JSON echo', async () => {
    const { server, port } = await startFetchServer(certs, async (req) => {
      const text = await req.text();
      return new Response(text, {
        headers: { 'content-type': 'application/json' },
      });
    });
    const session = await connectClient(port);
    const payload = JSON.stringify({ echo: true });
    const res = await doRequest(session, 'POST', '/echo', Buffer.from(payload));
    assert.strictEqual(res.status, '200');
    const parsed = JSON.parse(res.body.toString()) as { echo: boolean };
    assert.strictEqual(parsed.echo, true);
    await session.close();
    await server.close();
  });

  it('Fetch: 404 not found', async () => {
    const { server, port } = await startFetchServer(certs, (req) => {
      const url = new URL(req.url);
      if (url.pathname === '/exists') {
        return new Response('found');
      }
      return new Response('not found', { status: 404 });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/nope');
    assert.strictEqual(res.status, '404');
    assert.strictEqual(res.body.toString(), 'not found');
    await session.close();
    await server.close();
  });

  it('Fetch: custom headers', async () => {
    const { server, port } = await startFetchServer(certs, () => {
      return new Response('ok', {
        headers: {
          'x-powered-by': 'http3-worker',
          'x-request-id': 'fetch-test',
        },
      });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/headers');
    assert.strictEqual(res.headers['x-powered-by'], 'http3-worker');
    assert.strictEqual(res.headers['x-request-id'], 'fetch-test');
    await session.close();
    await server.close();
  });

  it('Fetch: streaming response body', async () => {
    const { server, port } = await startFetchServer(certs, () => {
      const stream = new ReadableStream({
        start(controller) {
          controller.enqueue(new TextEncoder().encode('chunk1'));
          controller.enqueue(new TextEncoder().encode('chunk2'));
          controller.enqueue(new TextEncoder().encode('chunk3'));
          controller.close();
        },
      });
      return new Response(stream);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/stream');
    assert.strictEqual(res.body.toString(), 'chunk1chunk2chunk3');
    await session.close();
    await server.close();
  });

  it('Fetch: 4 concurrent requests', async () => {
    const { server, port } = await startFetchServer(certs, (req) => {
      const url = new URL(req.url);
      return new Response(`path:${url.pathname}`);
    });
    const session = await connectClient(port);

    const results = await Promise.all(
      Array.from({ length: 4 }, (_, i) => doRequest(session, 'GET', `/p${i}`)),
    );

    assert.strictEqual(results.length, 4);
    const bodies = results.map(r => r.body.toString()).sort();
    for (let i = 0; i < 4; i++) {
      assert.ok(bodies.includes(`path:/p${i}`));
    }

    await session.close();
    await server.close();
  });

  it('Fetch: large response (64KB)', { timeout: 15000 }, async () => {
    const payload = Buffer.alloc(64 * 1024, 'F');
    const { server, port } = await startFetchServer(certs, () => {
      return new Response(payload);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/large');
    assert.strictEqual(res.body.length, 64 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);
    await session.close();
    await server.close();
  });
});

describe('Worker Frameworks — Express-style Adapter', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('Express: GET text', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('express style');
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'express style');
    await session.close();
    await server.close();
  });

  it('Express: POST body echo', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, _headers, flags) => {
      if (flags.endStream) {
        stream.respond({ ':status': '200' });
        stream.end('no body');
        return;
      }
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        const body = Buffer.concat(chunks);
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end(body);
      });
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = await connectClient(port);
    const res = await doRequest(session, 'POST', '/echo', Buffer.from('express echo'));
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'express echo');
    await session.close();
    await server.close();
  });

  it('Express: status + headers', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({
        ':status': '201',
        'content-type': 'application/json',
        'x-created': 'true',
      });
      stream.end(JSON.stringify({ created: true }));
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = await connectClient(port);
    const res = await doRequest(session, 'POST', '/create');
    assert.strictEqual(res.status, '201');
    assert.strictEqual(res.headers['x-created'], 'true');
    const parsed = JSON.parse(res.body.toString()) as { created: boolean };
    assert.strictEqual(parsed.created, true);
    await session.close();
    await server.close();
  });

  it('Express: concurrent requests', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      const path = headers[':path'] as string;
      stream.respond({ ':status': '200' });
      stream.end(`express:${path}`);
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = await connectClient(port);
    const results = await Promise.all(
      Array.from({ length: 3 }, (_, i) => doRequest(session, 'GET', `/e${i}`)),
    );

    assert.strictEqual(results.length, 3);
    for (let i = 0; i < 3; i++) {
      assert.ok(results.some(r => r.body.toString() === `express:/e${i}`));
    }

    await session.close();
    await server.close();
  });
});
