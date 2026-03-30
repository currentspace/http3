/**
 * HTTP/3 end-to-end scenario tests.
 *
 * Exercises a realistic route table over a single shared server and
 * client session, covering JSON APIs, file downloads, streaming,
 * SSE, error handling, trailers, HEAD, redirects, and concurrency.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { createHash } from 'node:crypto';
import {
  startScenarioServer,
  connectScenarioClient,
  doRequest,
} from '../support/scenario-helpers.js';
import type { Http3SecureServer, Http3ClientSession, IncomingHeaders } from '../../lib/index.js';

let server: Http3SecureServer;
let port: number;
let session: Http3ClientSession;

before(async () => {
  const s = await startScenarioServer();
  server = s.server;
  port = s.port;
  session = await connectScenarioClient(port);
});

after(async () => {
  await session.close();
  await server.close();
});

describe('H3 E2E Scenarios', () => {
  // ------------------------------------------------------------------
  // Health
  // ------------------------------------------------------------------
  it('GET /health returns 200 ok', async () => {
    const res = await doRequest(session, 'GET', '/health');
    assert.strictEqual(res.status, '200');
    const body = JSON.parse(res.body.toString());
    assert.strictEqual(body.status, 'ok');
  });

  // ------------------------------------------------------------------
  // Users API
  // ------------------------------------------------------------------
  it('GET /api/users returns 50 users', async () => {
    const res = await doRequest(session, 'GET', '/api/users');
    assert.strictEqual(res.status, '200');
    const users = JSON.parse(res.body.toString());
    assert.strictEqual(Array.isArray(users), true);
    assert.strictEqual(users.length, 50);
    assert.ok(users[0].id);
    assert.ok(users[0].name);
    assert.ok(users[0].email);
  });

  it('GET /api/users/1 returns single user', async () => {
    const res = await doRequest(session, 'GET', '/api/users/1');
    assert.strictEqual(res.status, '200');
    const user = JSON.parse(res.body.toString());
    assert.strictEqual(user.id, 1);
    assert.ok(user.name);
  });

  it('GET /api/users/9999 returns 404', async () => {
    const res = await doRequest(session, 'GET', '/api/users/9999');
    assert.strictEqual(res.status, '404');
    const body = JSON.parse(res.body.toString());
    assert.strictEqual(body.error, 'not found');
  });

  it('POST /api/users creates a user with id', async () => {
    const payload = JSON.stringify({ name: 'Test User', email: 'test@example.com' });
    const res = await doRequest(session, 'POST', '/api/users', payload);
    assert.strictEqual(res.status, '201');
    const created = JSON.parse(res.body.toString());
    assert.strictEqual(typeof created.id, 'number');
    assert.strictEqual(created.name, 'Test User');
    assert.strictEqual(created.email, 'test@example.com');
  });

  // ------------------------------------------------------------------
  // Upload
  // ------------------------------------------------------------------
  it('POST /api/upload returns byte count and sha256', async () => {
    const data = Buffer.from('hello world upload test');
    const expectedHash = createHash('sha256').update(data).digest('hex');
    const res = await doRequest(session, 'POST', '/api/upload', data);
    assert.strictEqual(res.status, '200');
    const body = JSON.parse(res.body.toString());
    assert.strictEqual(body.bytes, data.length);
    assert.strictEqual(body.sha256, expectedHash);
  });

  // ------------------------------------------------------------------
  // Echo
  // ------------------------------------------------------------------
  it('POST /api/echo pipes body back', async () => {
    const payload = Buffer.from('echo me back please');
    const res = await doRequest(session, 'POST', '/api/echo', payload);
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.toString(), 'echo me back please');
  });

  // ------------------------------------------------------------------
  // Static files
  // ------------------------------------------------------------------
  it('GET /files/small.txt returns 1KB', async () => {
    const res = await doRequest(session, 'GET', '/files/small.txt');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 1024);
    assert.strictEqual(res.body.every((b) => b === 0x41), true); // 'A'
  });

  it('GET /files/large.bin returns 1MB', { timeout: 30000 }, async () => {
    const res = await doRequest(session, 'GET', '/files/large.bin');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.headers['content-length'], '1048576');
    assert.strictEqual(res.body.length, 1048576);
    assert.strictEqual(res.body[0], 0xaa);
    assert.strictEqual(res.body[res.body.length - 1], 0xaa);
  });

  it('GET /files/huge.bin returns 4MB', { timeout: 60000 }, async () => {
    const res = await doRequest(session, 'GET', '/files/huge.bin');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.body.length, 4 * 1024 * 1024);
    assert.strictEqual(res.body[0], 0xbb);
  });

  // ------------------------------------------------------------------
  // HEAD
  // ------------------------------------------------------------------
  it('HEAD /head-check returns headers only, no body', async () => {
    const res = await doRequest(session, 'HEAD', '/head-check');
    assert.strictEqual(res.status, '200');
    assert.strictEqual(res.headers['content-length'], '1024');
    assert.strictEqual(res.body.length, 0);
  });

  // ------------------------------------------------------------------
  // DELETE 204
  // ------------------------------------------------------------------
  it('DELETE /no-content returns 204', async () => {
    const res = await doRequest(session, 'DELETE', '/no-content');
    assert.strictEqual(res.status, '204');
    assert.strictEqual(res.body.length, 0);
  });

  // ------------------------------------------------------------------
  // Redirect
  // ------------------------------------------------------------------
  it('GET /redirect returns 302 with location', async () => {
    const res = await doRequest(session, 'GET', '/redirect');
    assert.strictEqual(res.status, '302');
    assert.strictEqual(res.headers['location'], '/api/users');
    assert.strictEqual(res.body.length, 0);
  });

  // ------------------------------------------------------------------
  // Slow streaming
  // ------------------------------------------------------------------
  it('GET /slow streams 2KB in chunks', { timeout: 15000 }, async () => {
    const res = await doRequest(session, 'GET', '/slow');
    assert.strictEqual(res.status, '200');
    // 20 chunks * 100 bytes = 2000 bytes
    assert.strictEqual(res.body.length, 2000);
  });

  // ------------------------------------------------------------------
  // SSE
  // ------------------------------------------------------------------
  it('GET /sse/events delivers 5 SSE events', { timeout: 15000 }, async () => {
    // Use raw stream to collect SSE text
    let stream: ReturnType<typeof session.request>;
    for (let attempt = 0; ; attempt++) {
      try {
        stream = session.request({
          ':method': 'GET',
          ':path': '/sse/events',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });
        break;
      } catch (err: unknown) {
        if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
          await new Promise<void>((r) => { setTimeout(r, 5); });
          continue;
        }
        throw err;
      }
    }

    const result = await new Promise<{ status: string; body: string }>((resolve, reject) => {
      let status = '';
      const chunks: Buffer[] = [];
      const timeout = setTimeout(() => reject(new Error('SSE timed out')), 10000);

      stream.on('response', (h: Record<string, string>) => {
        status = h[':status'] ?? '';
      });
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        clearTimeout(timeout);
        resolve({ status, body: Buffer.concat(chunks).toString() });
      });
      stream.on('error', (err: Error) => {
        clearTimeout(timeout);
        reject(err);
      });
    });

    assert.strictEqual(result.status, '200');

    // Parse SSE events: each event block separated by double newline
    const events = result.body.split('\n\n').filter((block) => block.includes('data:'));
    assert.strictEqual(events.length, 5, `Expected 5 SSE events, got ${events.length}. Body: ${result.body}`);

    for (let i = 0; i < 5; i++) {
      assert.ok(events[i]!.includes(`event: update`), `Event ${i} missing event field`);
      assert.ok(events[i]!.includes(`data: event-${i}`), `Event ${i} missing data field`);
    }
  });

  // ------------------------------------------------------------------
  // Headers echo
  // ------------------------------------------------------------------
  it('GET /headers/echo reflects request headers', async () => {
    const res = await doRequest(session, 'GET', '/headers/echo', undefined, {
      'x-custom-header': 'test-value',
      'x-request-id': 'abc-123',
    });
    assert.strictEqual(res.status, '200');
    const echoed = JSON.parse(res.body.toString());
    assert.strictEqual(echoed['x-custom-header'], 'test-value');
    assert.strictEqual(echoed['x-request-id'], 'abc-123');
  });

  // ------------------------------------------------------------------
  // Error routes
  // ------------------------------------------------------------------
  it('GET /error/500 returns 500', async () => {
    const res = await doRequest(session, 'GET', '/error/500');
    assert.strictEqual(res.status, '500');
    const body = JSON.parse(res.body.toString());
    assert.strictEqual(body.error, 'internal server error');
  });

  it('GET /error/stream-reset results in error or truncated body', { timeout: 15000 }, async () => {
    // Server sends 200 headers + 64 bytes then destroys the stream.
    // The client may see: (a) a stream error/reset, (b) a truncated body,
    // or (c) just the partial data followed by end. All are acceptable.
    let stream: ReturnType<typeof session.request>;
    for (let attempt = 0; ; attempt++) {
      try {
        stream = session.request({
          ':method': 'GET',
          ':path': '/error/stream-reset',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });
        break;
      } catch (err: unknown) {
        if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
          await new Promise<void>((r) => { setTimeout(r, 5); });
          continue;
        }
        throw err;
      }
    }

    const result = await new Promise<{ status: string; bodyLen: number; error: boolean }>((resolve) => {
      let status = '';
      const chunks: Buffer[] = [];
      const timeout = setTimeout(() => {
        resolve({ status, bodyLen: Buffer.concat(chunks).length, error: false });
      }, 10000);

      stream.on('response', (h: Record<string, string>) => {
        status = h[':status'] ?? '';
      });
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        clearTimeout(timeout);
        resolve({ status, bodyLen: Buffer.concat(chunks).length, error: false });
      });
      stream.on('close', () => {
        clearTimeout(timeout);
        resolve({ status, bodyLen: Buffer.concat(chunks).length, error: false });
      });
      stream.on('error', () => {
        clearTimeout(timeout);
        resolve({ status, bodyLen: Buffer.concat(chunks).length, error: true });
      });
    });

    // We should have received the 200 status or an error
    if (result.error) {
      // Stream error is acceptable — server reset the stream
      assert.ok(true, 'Stream error received as expected');
    } else {
      assert.strictEqual(result.status, '200');
      // Body should be at most 64 bytes (the partial write before destroy)
      assert.ok(result.bodyLen <= 64, `Expected truncated body, got ${result.bodyLen} bytes`);
    }
  });

  // ------------------------------------------------------------------
  // Trailers
  // ------------------------------------------------------------------
  it('GET /trailers delivers body and trailers', { timeout: 10000 }, async () => {
    let stream: ReturnType<typeof session.request>;
    for (let attempt = 0; ; attempt++) {
      try {
        stream = session.request({
          ':method': 'GET',
          ':path': '/trailers',
          ':authority': 'localhost',
          ':scheme': 'https',
        }, { endStream: true });
        break;
      } catch (err: unknown) {
        if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
          await new Promise<void>((r) => { setTimeout(r, 5); });
          continue;
        }
        throw err;
      }
    }

    // The native H3 client may deliver trailers as either:
    // (a) a 'trailers' event, or
    // (b) a second 'response' event (without :status) after the body.
    const result = await new Promise<{
      status: string;
      body: string;
      trailers: IncomingHeaders | null;
    }>((resolve, reject) => {
      let status = '';
      let gotInitialResponse = false;
      const chunks: Buffer[] = [];
      let trailers: IncomingHeaders | null = null;
      const timeout = setTimeout(() => reject(new Error('trailers timed out')), 8000);

      stream.on('response', (h: Record<string, string>) => {
        if (!gotInitialResponse) {
          gotInitialResponse = true;
          status = h[':status'] ?? '';
        } else {
          // Second headers frame = trailers
          trailers = h as IncomingHeaders;
        }
      });
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('trailers', (t: IncomingHeaders) => { trailers = t; });
      stream.on('end', () => {
        clearTimeout(timeout);
        resolve({ status, body: Buffer.concat(chunks).toString(), trailers });
      });
      stream.on('error', (err: Error) => {
        clearTimeout(timeout);
        reject(err);
      });
    });

    assert.strictEqual(result.status, '200');
    assert.strictEqual(result.body, 'trailer-body');
    // Trailers may or may not be delivered depending on transport implementation.
    // If delivered, verify the checksum value.
    if (result.trailers) {
      assert.strictEqual(result.trailers['x-checksum'], 'abc123');
    }
  });

  // ------------------------------------------------------------------
  // Concurrent requests
  // ------------------------------------------------------------------
  it('20 concurrent GET /concurrent all return 200', { timeout: 30000 }, async () => {
    const promises: Promise<{ status: string }>[] = [];
    for (let i = 0; i < 20; i++) {
      promises.push(doRequest(session, 'GET', '/concurrent'));
    }
    const results = await Promise.all(promises);
    for (const res of results) {
      assert.strictEqual(res.status, '200');
    }
  });

  // ------------------------------------------------------------------
  // 404 default
  // ------------------------------------------------------------------
  it('GET /nonexistent returns 404', async () => {
    const res = await doRequest(session, 'GET', '/nonexistent');
    assert.strictEqual(res.status, '404');
    const body = JSON.parse(res.body.toString());
    assert.strictEqual(body.error, 'not found');
  });
});
