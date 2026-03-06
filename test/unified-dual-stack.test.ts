import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { connect as connectHttp2 } from 'node:http2';
import type { ClientHttp2Session, IncomingHttpHeaders } from 'node:http2';
import { connect, createSecureServer } from '../lib/index.js';
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

async function h3Request(port: number, path: string, body?: Buffer): Promise<{ status: string; body: string }> {
  const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
  let connected = false;
  session.once('connect', () => { connected = true; });
  await waitFor(() => connected, 3000);

  const stream = session.request({
    ':method': body ? 'POST' : 'GET',
    ':path': path,
    ':authority': 'localhost',
    ':scheme': 'https',
  }, { endStream: !body });
  if (body) {
    stream.end(body);
  }

  const result = await new Promise<{ status: string; body: string }>((resolve, reject) => {
    let status = '';
    const chunks: Buffer[] = [];
    stream.on('response', (headers: Record<string, string>) => {
      status = headers[':status'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
    });
    stream.on('end', () => {
      resolve({ status, body: Buffer.concat(chunks).toString() });
    });
    stream.on('error', reject);
  });

  await session.close();
  return result;
}

async function h2Request(port: number, path: string, body?: Buffer): Promise<{ status: string; body: string; trailers: IncomingHttpHeaders | null }> {
  const client: ClientHttp2Session = connectHttp2(`https://127.0.0.1:${port}`, {
    rejectUnauthorized: false,
    ALPNProtocols: ['h2'],
  });

  return new Promise((resolve, reject) => {
    const req = client.request({
      ':method': body ? 'POST' : 'GET',
      ':path': path,
      ':authority': 'localhost',
    });
    let status = '';
    let trailers: IncomingHttpHeaders | null = null;
    const chunks: Buffer[] = [];

    req.on('response', (headers: IncomingHttpHeaders) => {
      const raw = headers[':status'];
      status = typeof raw === 'number' ? String(raw) : String(raw ?? '');
    });
    req.on('trailers', (h: IncomingHttpHeaders) => {
      trailers = h;
    });
    req.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
    });
    req.on('end', () => {
      client.close();
      resolve({ status, body: Buffer.concat(chunks).toString(), trailers });
    });
    req.on('error', (err) => {
      client.destroy();
      reject(err);
    });

    if (body) {
      req.end(body);
      return;
    }
    req.end();
  });
}

describe('Unified dual-stack server', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('serves H3 and H2 on the same port', async () => {
    const protocols = new Set<string>();
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      allowHTTP1: false,
    }, (stream, headers) => {
      const path = String(headers[':path'] ?? '/');
      stream.respond({ ':status': '200', 'x-handler': 'unified' });
      stream.end(`ok:${path}`);
    });
    server.on('session', (session) => {
      protocols.add(session.alpnProtocol);
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    try {
      const h3 = await h3Request(port, '/h3');
      const h2 = await h2Request(port, '/h2');
      assert.strictEqual(h3.status, '200');
      assert.strictEqual(h2.status, '200');
      assert.strictEqual(h3.body, 'ok:/h3');
      assert.strictEqual(h2.body, 'ok:/h2');

      await waitFor(() => protocols.has('h2') && protocols.has('h3'), 3000);
    } finally {
      await server.close();
    }
  });

  it('keeps request body and trailer behavior aligned', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      allowHTTP1: false,
    }, (stream, headers, flags) => {
      const method = String(headers[':method'] ?? 'GET');
      if (method === 'POST' && !flags.endStream) {
        const chunks: Buffer[] = [];
        stream.on('data', (chunk: Buffer) => chunks.push(chunk));
        stream.on('end', () => {
          stream.respond({ ':status': '200' });
          stream.write(Buffer.concat(chunks));
          stream.sendTrailers({ 'x-checksum': 'ok' });
          stream.end();
        });
        return;
      }
      stream.respond({ ':status': '200' });
      stream.end('empty');
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    try {
      const payload = Buffer.from('parity-body');
      const [h3, h2] = await Promise.all([
        h3Request(port, '/echo', payload),
        h2Request(port, '/echo', payload),
      ]);

      assert.strictEqual(h3.status, '200');
      assert.strictEqual(h2.status, '200');
      assert.strictEqual(h3.body, payload.toString());
      assert.strictEqual(h2.body, payload.toString());
      if (h2.trailers) {
        assert.strictEqual(String(h2.trailers['x-checksum'] ?? ''), 'ok');
      }
    } finally {
      await server.close();
    }
  });
});
