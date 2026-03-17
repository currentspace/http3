/**
 * Integration tests for HTTP/1.1 fallback via serveFetch().
 * Uses node:https client to hit the H2 fallback path.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import https from 'node:https';
import { serveFetch } from '../lib/fetch-adapter.js';
import type { Http3SecureServer } from '../lib/server.js';
import { generateTestCerts } from './generate-certs.js';

function httpsRequest(
  url: string,
  options: https.RequestOptions,
): Promise<{ status: number; body: string }> {
  return new Promise((resolve, reject) => {
    const req = https.request(url, options, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      res.on('end', () => {
        resolve({
          status: res.statusCode ?? 0,
          body: Buffer.concat(chunks).toString(),
        });
      });
    });
    req.on('error', reject);
    req.end();
  });
}

function httpsPost(
  url: string,
  options: https.RequestOptions,
  body: string,
): Promise<{ status: number; body: string }> {
  return new Promise((resolve, reject) => {
    const req = https.request(url, { ...options, method: 'POST' }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      res.on('end', () => {
        resolve({
          status: res.statusCode ?? 0,
          body: Buffer.concat(chunks).toString(),
        });
      });
    });
    req.on('error', reject);
    req.write(body);
    req.end();
  });
}

describe('fetch HTTP/1 fallback', () => {
  let certs: { key: Buffer; cert: Buffer };
  let server: Http3SecureServer;
  let port: number;

  before(async () => {
    certs = generateTestCerts();

    server = serveFetch({
      key: certs.key,
      cert: certs.cert,
      port: 0,
      host: '127.0.0.1',
      allowHTTP1: true,
      disableRetry: true,
      fetch: async (req: Request) => {
        const url = new URL(req.url);

        if (url.pathname === '/echo' && req.method === 'POST') {
          const body = await req.text();
          return new Response(body, { status: 200 });
        }

        if (url.pathname === '/error') {
          throw new Error('intentional error');
        }

        return new Response('hello from fetch', { status: 200 });
      },
    });

    port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
    });
  });

  after(async () => {
    await server.close();
  });

  it('should serve GET request via HTTP/1.1', async () => {
    const result = await httpsRequest(`https://127.0.0.1:${port}/`, {
      rejectUnauthorized: false,
    });

    assert.strictEqual(result.status, 200);
    assert.strictEqual(result.body, 'hello from fetch');
  });

  it('should serve POST with body via HTTP/1.1', async () => {
    const result = await httpsPost(
      `https://127.0.0.1:${port}/echo`,
      { rejectUnauthorized: false },
      'ping from http1',
    );

    assert.strictEqual(result.status, 200);
    assert.strictEqual(result.body, 'ping from http1');
  });

  it('should return 500 when handler throws', async () => {
    const result = await httpsRequest(`https://127.0.0.1:${port}/error`, {
      rejectUnauthorized: false,
    });

    assert.strictEqual(result.status, 500);
    assert.ok(result.body.includes('intentional error'));
  });
});
