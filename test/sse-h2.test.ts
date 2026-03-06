import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { connect as connectHttp2 } from 'node:http2';
import type { ClientHttp2Session, IncomingHttpHeaders } from 'node:http2';
import { createSecureServer, createSseStream } from '../lib/index.js';
import { generateTestCerts } from './generate-certs.js';

function parseSseData(payload: string): string[] {
  const messages: string[] = [];
  const frames = payload.split('\n\n').filter(Boolean);
  for (const frame of frames) {
    let combined = '';
    for (const line of frame.split('\n')) {
      if (line.startsWith('data:')) {
        const value = line.slice(5).trimStart();
        combined = combined.length > 0 ? `${combined}\n${value}` : value;
      }
    }
    if (combined.length > 0) messages.push(combined);
  }
  return messages;
}

describe('SSE over H2', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('streams SSE with the same server entrypoint', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      allowHTTP1: false,
    }, (stream, headers) => {
      if (headers[':path'] !== '/events') {
        stream.respond({ ':status': '404' }, { endStream: true });
        return;
      }
      const sse = createSseStream(stream);
      void (async () => {
        await sse.send({ data: 'alpha' });
        await sse.send({ data: ['beta', 'line'] });
        sse.close();
      })();
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const client: ClientHttp2Session = connectHttp2(`https://127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      ALPNProtocols: ['h2'],
    });
    const req = client.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
    });

    let status = '';
    const chunks: Buffer[] = [];
    await new Promise<void>((resolve, reject) => {
      req.on('response', (headers: IncomingHttpHeaders) => {
        const raw = headers[':status'];
        status = typeof raw === 'number' ? String(raw) : String(raw ?? '');
      });
      req.on('data', (chunk: Buffer) => chunks.push(chunk));
      req.on('end', () => resolve());
      req.on('error', reject);
      req.end();
    });

    const events = parseSseData(Buffer.concat(chunks).toString('utf8'));
    assert.strictEqual(status, '200');
    assert.deepStrictEqual(events, ['alpha', 'beta\nline']);

    client.close();
    await server.close();
  });
});
