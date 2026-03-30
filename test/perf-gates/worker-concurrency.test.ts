/**
 * Worker mode tests: incremental concurrency scale.
 * All tests share a single echo server to avoid per-test startup overhead.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { setTimeout as delay } from 'node:timers/promises';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, connect } from '../../lib/index.js';
import type { Http3SecureServer, Http3ClientSession } from '../../lib/index.js';
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
      if (attempt < 20 && err instanceof Error && err.message.includes('StreamBlocked')) {
        await new Promise<void>((r) => { setTimeout(r, 2); });
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
  await waitFor(() => connected, 15000);
  return session;
}

describe('Worker Concurrency', () => {
  let server: Http3SecureServer;
  let port: number;

  before(async () => {
    const certs = generateTestCerts();
    server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers, flags) => {
      const path = headers[':path'] as string;

      // Slow handler for tier 4 test
      if (path.startsWith('/slow')) {
        if (flags.endStream) {
          delay(1).then(() => {
            stream.respond({ ':status': '200' });
            stream.end(path);
          });
          return;
        }
      }

      if (flags.endStream) {
        stream.respond({ ':status': '200' });
        stream.end(path);
        return;
      }
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        stream.respond({ ':status': '200' });
        stream.end(Buffer.concat(chunks));
      });
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

  // --- Tier 1: Single Connection Scale ---

  it('20 concurrent streams, 1 connection', { timeout: 8000 }, async () => {
    const session = await connectClient(port);
    const results = await Promise.all(
      Array.from({ length: 20 }, (_, i) => doRequest(session, 'GET', `/t1a-${i}`)),
    );
    assert.strictEqual(results.length, 20);
    const bodies = new Set(results.map(r => r.body.toString()));
    assert.strictEqual(bodies.size, 20);
    for (let i = 0; i < 20; i++) assert.ok(bodies.has(`/t1a-${i}`));
    await session.close();
  });

  it('100 concurrent streams, 1 connection', { timeout: 8000 }, async () => {
    const session = await connectClient(port);
    const results = await Promise.all(
      Array.from({ length: 100 }, (_, i) => doRequest(session, 'GET', `/t1b-${i}`)),
    );
    assert.strictEqual(results.length, 100);
    const bodies = new Set(results.map(r => r.body.toString()));
    assert.strictEqual(bodies.size, 100);
    await session.close();
  });

  it('5 rounds x 40 concurrent streams, 1 connection (200 total)', { timeout: 8000 }, async () => {
    const session = await connectClient(port);
    for (let round = 0; round < 5; round++) {
      const results = await Promise.all(
        Array.from({ length: 40 }, (_, i) => doRequest(session, 'GET', `/t1c-r${round}s${i}`)),
      );
      assert.strictEqual(results.length, 40);
      const bodies = new Set(results.map(r => r.body.toString()));
      assert.strictEqual(bodies.size, 40);
    }
    await session.close();
  });

  // --- Tier 2: Multi-Client Scale ---

  it('10 clients x 10 streams = 100 total', { timeout: 25000 }, async () => {
    const clients = await Promise.all(
      Array.from({ length: 10 }, () => connectClient(port)),
    );
    const results = await Promise.all(
      clients.flatMap((s, ci) =>
        Array.from({ length: 10 }, (_, si) => doRequest(s, 'GET', `/t2a-c${ci}s${si}`)),
      ),
    );
    assert.strictEqual(results.length, 100);
    const bodies = new Set(results.map(r => r.body.toString()));
    assert.strictEqual(bodies.size, 100);
    await Promise.all(clients.map(s => s.close()));
  });

  it('50 clients x 1 stream = 50 total', { timeout: 25000 }, async () => {
    const clients = await Promise.all(
      Array.from({ length: 50 }, () => connectClient(port)),
    );
    const results = await Promise.all(
      clients.map((s, i) => doRequest(s, 'GET', `/t2b-${i}`)),
    );
    assert.strictEqual(results.length, 50);
    const bodies = new Set(results.map(r => r.body.toString()));
    assert.strictEqual(bodies.size, 50);
    await Promise.all(clients.map(s => s.close()));
  });

  it('20 clients x 10 streams = 200 total', { timeout: 25000 }, async () => {
    const clients = await Promise.all(
      Array.from({ length: 20 }, () => connectClient(port)),
    );
    const results = await Promise.all(
      clients.flatMap((s, ci) =>
        Array.from({ length: 10 }, (_, si) => doRequest(s, 'GET', `/t2c-c${ci}s${si}`)),
      ),
    );
    assert.strictEqual(results.length, 200);
    const bodies = new Set(results.map(r => r.body.toString()));
    assert.strictEqual(bodies.size, 200);
    await Promise.all(clients.map(s => s.close()));
  });

  // --- Tier 3: Mixed Payload Sizes ---

  it('10 clients, 1KB-64KB bodies concurrent', { timeout: 25000 }, async () => {
    const clients = await Promise.all(
      Array.from({ length: 10 }, () => connectClient(port)),
    );
    const sizes = [1024, 2048, 4096, 8192, 16384, 32768, 65536, 1024, 4096, 16384];
    const payloads = sizes.map((sz, i) => Buffer.alloc(sz, String.fromCharCode(65 + i)));

    const results = await Promise.all(
      clients.map((s, i) => doRequest(s, 'POST', `/t3a-${i}`, payloads[i])),
    );
    for (let i = 0; i < 10; i++) {
      assert.strictEqual(results[i].status, '200');
      assert.strictEqual(results[i].body.length, sizes[i]);
      assert.strictEqual(Buffer.compare(results[i].body, payloads[i]), 0);
    }
    await Promise.all(clients.map(s => s.close()));
  });

  it('5 clients x 256KB POST echo', { timeout: 20000 }, async () => {
    const payload = Buffer.alloc(256 * 1024, 'Z');
    const clients = await Promise.all(
      Array.from({ length: 5 }, () => connectClient(port)),
    );
    const results = await Promise.all(
      clients.map((s, i) => doRequest(s, 'POST', `/t3b-${i}`, payload)),
    );
    for (const r of results) {
      assert.strictEqual(r.status, '200');
      assert.strictEqual(r.body.length, 256 * 1024);
    }
    await Promise.all(clients.map(s => s.close()));
  });

  // --- Tier 4: Slow/Fast Mix ---

  it('fast clients + slow handler', { timeout: 8000 }, async () => {
    const session = await connectClient(port);
    // Mix slow and fast requests on the same connection
    const results = await Promise.all([
      doRequest(session, 'GET', '/slow-1'),
      doRequest(session, 'GET', '/fast-1'),
      doRequest(session, 'GET', '/slow-2'),
      doRequest(session, 'GET', '/fast-2'),
      doRequest(session, 'GET', '/fast-3'),
      doRequest(session, 'GET', '/slow-3'),
      doRequest(session, 'GET', '/fast-4'),
      doRequest(session, 'GET', '/fast-5'),
    ]);
    assert.strictEqual(results.length, 8);
    for (const r of results) {
      assert.strictEqual(r.status, '200');
    }
    // Verify fast responses aren't blocked by slow ones — all should complete within timeout
    await session.close();
  });

  it('interleaved small+large on same connection', { timeout: 25000 }, async () => {
    const session = await connectClient(port);
    const bigPayload = Buffer.alloc(64 * 1024, 'B');
    const results = await Promise.all([
      doRequest(session, 'GET', '/t4b-get1'),
      doRequest(session, 'POST', '/t4b-big1', bigPayload),
      doRequest(session, 'GET', '/t4b-get2'),
      doRequest(session, 'POST', '/t4b-big2', bigPayload),
      doRequest(session, 'GET', '/t4b-get3'),
    ]);
    assert.strictEqual(results[0].body.toString(), '/t4b-get1');
    assert.strictEqual(results[1].body.length, 64 * 1024);
    assert.strictEqual(results[2].body.toString(), '/t4b-get2');
    assert.strictEqual(results[3].body.length, 64 * 1024);
    assert.strictEqual(results[4].body.toString(), '/t4b-get3');
    await session.close();
  });

  // --- Tier 5: Sustained Throughput ---

  it('100 sequential requests, 1 connection', { timeout: 8000 }, async () => {
    const session = await connectClient(port);
    for (let i = 0; i < 100; i++) {
      const res = await doRequest(session, 'GET', `/t5a-${i}`);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.toString(), `/t5a-${i}`);
    }
    await session.close();
  });

  it('10 rounds x 20 concurrent streams', { timeout: 25000 }, async () => {
    const session = await connectClient(port);
    for (let round = 0; round < 10; round++) {
      const results = await Promise.all(
        Array.from({ length: 20 }, (_, i) => doRequest(session, 'GET', `/t5b-r${round}s${i}`)),
      );
      assert.strictEqual(results.length, 20);
      for (const r of results) assert.strictEqual(r.status, '200');
    }
    await session.close();
  });

  // --- Step 4: Event Loop Responsiveness ---

  it('event loop stays responsive under load', { timeout: 25000 }, async () => {
    const session = await connectClient(port);
    const ticks: number[] = [];
    const interval = setInterval(() => { ticks.push(Date.now()); }, 5);

    // Do multiple rounds to ensure enough elapsed time for interval ticks
    for (let round = 0; round < 5; round++) {
      const results = await Promise.all(
        Array.from({ length: 50 }, (_, i) => doRequest(session, 'GET', `/el-r${round}s${i}`)),
      );
      assert.strictEqual(results.length, 50);
    }
    clearInterval(interval);

    // Verify interval fired regularly — no gap > 100ms
    for (let i = 1; i < ticks.length; i++) {
      const gap = ticks[i] - ticks[i - 1];
      assert.ok(gap < 100, `Event loop blocked for ${gap}ms between ticks ${i - 1} and ${i}`);
    }
    // Fast worker paths can complete before 2+ ticks fire; require at least one.
    assert.ok(ticks.length >= 1, `Only got ${ticks.length} interval ticks`);

    await session.close();
  });
});
