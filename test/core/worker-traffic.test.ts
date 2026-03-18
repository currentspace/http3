/**
 * Worker mode tests: core HTTP/3 request/response traffic patterns.
 * Server and client both use Rust-owned worker-thread transport paths.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect } from '../../lib/index.js';
import type { Http3SecureServer, Http3ClientSession, ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';

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

async function startServer(
  certs: { key: Buffer; cert: Buffer },
  handler: (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void,
): Promise<{ server: Http3SecureServer; port: number }> {
  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
  }, handler);

  const port = await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
    server.listen(0, '127.0.0.1');
  });

  return { server, port };
}

async function connectClient(port: number): Promise<Http3ClientSession> {
  const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
  let connected = false;
  session.on('connect', () => { connected = true; });
  await waitFor(() => connected, 3000);
  return session;
}

describe('Worker Traffic', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('GET small text body', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('hello worker');
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'hello worker');
    await session.close();
    await server.close();
  });

  it('GET 64KB body', async () => {
    const payload = Buffer.alloc(64 * 1024, 'A');
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end(payload);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/64k');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 64 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);
    await session.close();
    await server.close();
  });

  it('GET 256KB body', async () => {
    const payload = Buffer.alloc(256 * 1024, 'B');
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end(payload);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/256k');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 256 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);
    await session.close();
    await server.close();
  });

  it('GET 1MB body', { timeout: 15000 }, async () => {
    const payload = Buffer.alloc(1024 * 1024, 'C');
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end(payload);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/1m');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 1024 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);
    await session.close();
    await server.close();
  });

  it('POST echo small body', async () => {
    const { server, port } = await startServer(certs, (stream, _headers, flags) => {
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
    });
    const session = await connectClient(port);
    const body = Buffer.from('echo this back');
    const res = await doRequest(session, 'POST', '/echo', body);
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'echo this back');
    await session.close();
    await server.close();
  });

  it('POST echo 256KB body', { timeout: 15000 }, async () => {
    const payload = Buffer.alloc(256 * 1024, 'D');
    const { server, port } = await startServer(certs, (stream, _headers, flags) => {
      if (flags.endStream) {
        stream.respond({ ':status': '200' });
        stream.end();
        return;
      }
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        stream.respond({ ':status': '200' });
        stream.end(Buffer.concat(chunks));
      });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'POST', '/echo256k', payload);
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 256 * 1024);
    assert.strictEqual(Buffer.compare(res.body, payload), 0);
    await session.close();
    await server.close();
  });

  it('HEAD request — no body sent', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200', 'content-length': '42' }, { endStream: true });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'HEAD', '/head');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.headers['content-length'], '42');
    assert.strictEqual(res.body.length, 0);
    await session.close();
    await server.close();
  });

  it('204 No Content', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '204' }, { endStream: true });
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/nocontent');
    assert.strictEqual(res.status, '204');
    assert.strictEqual(res.body.length, 0);
    await session.close();
    await server.close();
  });

  it('200 with empty body', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200', 'content-length': '0' });
      stream.end();
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/empty');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 0);
    await session.close();
    await server.close();
  });

  it('streaming response (5 writes)', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      let i = 0;
      const interval = setInterval(() => {
        stream.write(`chunk${i}`);
        i++;
        if (i >= 5) {
          clearInterval(interval);
          stream.end();
        }
      }, 10);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/streaming');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'chunk0chunk1chunk2chunk3chunk4');
    await session.close();
    await server.close();
  });

  it('custom headers round-trip', async () => {
    const { server, port } = await startServer(certs, (stream, headers) => {
      const custom = headers['x-request-id'] as string | undefined;
      stream.respond({
        ':status': '200',
        'x-response-id': custom ?? 'missing',
        'x-custom': 'server-value',
      });
      stream.end('ok');
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET',
      ':path': '/custom',
      ':authority': 'localhost',
      ':scheme': 'https',
      'x-request-id': 'test-123',
    }, { endStream: true });

    const res = await new Promise<DoRequestResult>((resolve, reject) => {
      let status = '';
      const hdrs: Record<string, string> = {};
      const chunks: Buffer[] = [];
      const timeout = setTimeout(() => reject(new Error('timeout')), 5000);
      reqStream.on('response', (h: Record<string, string>) => {
        status = h[':status'] ?? '';
        for (const [k, v] of Object.entries(h)) hdrs[k] = v;
      });
      reqStream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      reqStream.on('end', () => {
        clearTimeout(timeout);
        resolve({ status, headers: hdrs, body: Buffer.concat(chunks) });
      });
    });

    assert.strictEqual(res.headers['x-response-id'], 'test-123');
    assert.strictEqual(res.headers['x-custom'], 'server-value');
    await session.close();
    await server.close();
  });

  it('different paths same connection', async () => {
    const { server, port } = await startServer(certs, (stream, headers) => {
      const path = headers[':path'] as string;
      stream.respond({ ':status': '200' });
      stream.end(path);
    });
    const session = await connectClient(port);

    const r1 = await doRequest(session, 'GET', '/a');
    const r2 = await doRequest(session, 'GET', '/b');
    const r3 = await doRequest(session, 'GET', '/c');

    assert.strictEqual(r1.body.toString(), '/a');
    assert.strictEqual(r2.body.toString(), '/b');
    assert.strictEqual(r3.body.toString(), '/c');

    await session.close();
    await server.close();
  });

  it('binary response body', async () => {
    const binary = Buffer.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0xfd, 0x80, 0x7f]);
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
      stream.end(binary);
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/binary');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(Buffer.compare(res.body, binary), 0);
    await session.close();
    await server.close();
  });

  it('trailers on response', async () => {
    const { server, port } = await startServer(certs, (stream) => {
      stream.respond({ ':status': '200' });
      stream.write('body data');
      stream.sendTrailers({ 'x-checksum': 'abc123' });
      stream.end();
    });
    const session = await connectClient(port);

    const reqStream = session.request({
      ':method': 'GET',
      ':path': '/trailers',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    const chunks: Buffer[] = [];
    let complete = false;

    reqStream.on('response', (h: Record<string, string>) => { status = h[':status'] ?? ''; });
    reqStream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    reqStream.on('end', () => { complete = true; });

    await waitFor(() => complete, 5000);

    assert.strictEqual(status, '200');
    assert.strictEqual(Buffer.concat(chunks).toString(), 'body data');
    await session.close();
    await server.close();
  });

  it('endStream on headers (GET with endStream:true)', async () => {
    let receivedEndStream = false;
    const { server, port } = await startServer(certs, (stream, _headers, flags) => {
      receivedEndStream = flags.endStream;
      stream.respond({ ':status': '200' });
      stream.end('got it');
    });
    const session = await connectClient(port);
    const res = await doRequest(session, 'GET', '/endstream');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'got it');
    assert.ok(receivedEndStream, 'server should see endStream flag');
    await session.close();
    await server.close();
  });
});
