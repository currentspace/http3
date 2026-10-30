import { test } from 'node:test';
import type { TestContext } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer, get } from 'node:http';
import type { IncomingMessage, RequestListener } from 'node:http';
import { proxy } from '../../lib/proxy.js';

async function listen(t: TestContext, handler: RequestListener): Promise<string> {
  const server = createServer(handler);
  t.after(async () => {
    server.closeAllConnections();
    await new Promise<void>(resolve => server.close(() => resolve()));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return `http://127.0.0.1:${(server.address() as { port: number }).port}`;
}

function request(origin: string): Promise<IncomingMessage> {
  return new Promise((resolve, reject) => {
    get(origin, { agent: false }, resolve).once('error', reject);
  });
}

async function body(response: IncomingMessage): Promise<string> {
  let text = '';
  for await (const chunk of response) text += String(chunk);
  return text;
}

for (const contentType of ['text/event-stream', 'Text/Event-Stream; charset=utf-8']) {
  test(`proxy preserves quiet SSE after ${contentType} headers`, async t => {
    const upstream = await listen(t, (_req, res) => {
      res.writeHead(200, { 'content-type': contentType });
      res.write('data: first\n\n');
      const timer = setTimeout(() => res.end('data: second\n\n'), 500);
      res.once('close', () => clearTimeout(timer));
    });
    const front = await listen(t, proxy(upstream, { timeoutMs: 100 }));
    const response = await request(front);
    assert.equal(response.statusCode, 200);
    assert.equal(await body(response), 'data: first\n\ndata: second\n\n');
  });
}

test('proxy still times out an upstream that never sends response headers', async t => {
  const upstream = await listen(t, () => {});
  const front = await listen(t, proxy(upstream, { timeoutMs: 100 }));
  const response = await request(front);
  assert.equal(response.statusCode, 502);
  assert.equal(await body(response), 'Upstream request failed');
});

for (const contentType of ['application/json', 'text/event-streaming', undefined]) {
  test(`proxy retains body inactivity timeout for ${contentType ?? 'missing content type'}`, async t => {
    const upstream = await listen(t, (_req, res) => {
      if (contentType) res.setHeader('content-type', contentType);
      res.write('first');
    });
    const front = await listen(t, proxy(upstream, { timeoutMs: 100 }));
    const response = await request(front);
    assert.equal(response.statusCode, 200);
    await assert.rejects(body(response), { code: 'ECONNRESET' });
  });
}
