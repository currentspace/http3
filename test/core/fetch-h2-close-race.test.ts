import { describe, it } from 'node:test';
import assert from 'node:assert';
import { setTimeout as delay } from 'node:timers/promises';
import { connect as connectHttp2, constants as http2Constants } from 'node:http2';
import type { ClientHttp2Session, IncomingHttpHeaders } from 'node:http2';
import { serveFetch } from '../../lib/fetch-adapter.js';
import { generateTestCerts } from '../support/generate-certs.js';

function streamBody(onCancel: () => void): ReadableStream<Uint8Array> {
  let interval: NodeJS.Timeout | null = null;
  let sent = 0;

  return new ReadableStream<Uint8Array>({
    start(controller) {
      interval = setInterval(() => {
        sent += 1;
        controller.enqueue(new Uint8Array(32 * 1024).fill(sent % 256));
        if (sent >= 200) {
          if (interval) clearInterval(interval);
          interval = null;
          controller.close();
        }
      }, 5);
      interval.unref();
    },
    cancel() {
      if (interval) clearInterval(interval);
      interval = null;
      onCancel();
    },
  });
}

async function waitForServer(server: ReturnType<typeof serveFetch>): Promise<number> {
  return await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
  });
}

async function openH2Session(port: number): Promise<ClientHttp2Session> {
  const client = connectHttp2(`https://127.0.0.1:${port}`, {
    rejectUnauthorized: false,
    ALPNProtocols: ['h2'],
  });
  await new Promise<void>((resolve, reject) => {
    client.once('connect', resolve);
    client.once('error', reject);
  });
  return client;
}

async function h2Get(port: number, path: string): Promise<{ status: string; body: string }> {
  const client = await openH2Session(port);
  try {
    const req = client.request({
      ':method': 'GET',
      ':path': path,
      ':authority': 'localhost',
    });
    const chunks: Buffer[] = [];
    let status = '';

    await new Promise<void>((resolve, reject) => {
      req.on('response', (headers: IncomingHttpHeaders) => {
        const raw = headers[':status'];
        status = typeof raw === 'number' ? String(raw) : String(raw ?? '');
      });
      req.on('data', (chunk: Buffer) => {
        chunks.push(chunk);
      });
      req.on('end', resolve);
      req.on('error', reject);
      req.end();
    });

    return { status, body: Buffer.concat(chunks).toString('utf8') };
  } finally {
    client.close();
  }
}

describe('fetch adapter H2 close races', () => {
  it('handles browser-like response stream cancellation without write-after-end crashes', async () => {
    const certs = generateTestCerts();
    let cancelCount = 0;
    const server = serveFetch({
      port: 0,
      host: '127.0.0.1',
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      allowHTTP1: false,
      fetch: (request: Request) => {
        const url = new URL(request.url);
        if (url.pathname === '/stream') {
          return new Response(streamBody(() => { cancelCount += 1; }), {
            headers: { 'content-type': 'application/octet-stream' },
          });
        }
        return new Response('ok', {
          headers: { 'content-type': 'text/plain; charset=utf-8' },
        });
      },
    });

    const port = await waitForServer(server);
    const client = await openH2Session(port);

    try {
      const req = client.request({
        ':method': 'GET',
        ':path': '/stream',
        ':authority': 'localhost',
      });

      await new Promise<void>((resolve, reject) => {
        let cancelled = false;
        req.once('data', () => {
          cancelled = true;
          req.close(http2Constants.NGHTTP2_CANCEL);
          resolve();
        });
        req.once('error', (err: Error) => {
          if (cancelled) return;
          reject(err);
        });
        req.end();
      });

      await delay(150);

      const followUp = await h2Get(port, '/ok');
      assert.strictEqual(followUp.status, '200');
      assert.strictEqual(followUp.body, 'ok');
      assert.ok(cancelCount >= 1, 'response body reader should be cancelled after the H2 stream closes');
    } finally {
      client.close();
      await server.close();
    }
  });
});
