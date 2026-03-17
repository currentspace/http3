/**
 * Integration tests for the Express-like adapter.
 * Validates the req/res shim over a real H3 server + client.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../lib/index.js';
import { createExpressAdapter } from '../lib/express-adapter.js';
import type { ExpressLikeRequest, ExpressLikeResponse } from '../lib/express-adapter.js';
import { generateTestCerts } from './generate-certs.js';
import { waitFor } from './helpers.js';

describe('Express Adapter', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should deliver correct req.method and req.url for GET', async () => {
    let capturedMethod = '';
    let capturedUrl = '';

    const handler = createExpressAdapter((req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      capturedMethod = req.method;
      capturedUrl = req.url;
      res.writeHead(200);
      res.end('hello from adapter');
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/test',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let done = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(capturedMethod, 'GET');
    assert.strictEqual(capturedUrl, '/test');
    assert.strictEqual(body, 'hello from adapter');

    await session.close();
    await server.close();
  });

  it('should read POST body and echo it back', async () => {
    const handler = createExpressAdapter((req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      const chunks: Buffer[] = [];
      req.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      req.on('end', () => {
        const reqBody = Buffer.concat(chunks).toString();
        res.writeHead(200);
        res.end(reqBody);
      });
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'POST',
      ':path': '/echo',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: false });

    stream.end(Buffer.from('ping payload'));

    let responseBody = '';
    let done = false;
    stream.on('data', (chunk: Buffer) => { responseBody += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 5000);

    assert.strictEqual(responseBody, 'ping payload');

    await session.close();
    await server.close();
  });

  it('should deliver custom status and headers via res.writeHead()', async () => {
    const handler = createExpressAdapter((_req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      res.writeHead(201, { 'x-custom': 'yes' });
      res.end('created');
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/create',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    let customHeader = '';
    let body = '';
    let done = false;

    stream.on('response', (headers: Record<string, string>) => {
      status = headers[':status'] ?? '';
      customHeader = headers['x-custom'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(status, '201');
    assert.strictEqual(customHeader, 'yes');
    assert.strictEqual(body, 'created');

    await session.close();
    await server.close();
  });

  it('should auto-flush headers set via res.setHeader() on first write', async () => {
    const handler = createExpressAdapter((_req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      res.setHeader('x-before-write', 'auto');
      res.write('chunk1');
      res.end('chunk2');
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/auto-flush',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let headerValue = '';
    let body = '';
    let done = false;

    stream.on('response', (headers: Record<string, string>) => {
      headerValue = headers['x-before-write'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(headerValue, 'auto');
    assert.strictEqual(body, 'chunk1chunk2');

    await session.close();
    await server.close();
  });

  it('should default to status 200 with bare res.end()', async () => {
    const handler = createExpressAdapter((_req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      res.end('ok');
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/default-status',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    let body = '';
    let done = false;

    stream.on('response', (headers: Record<string, string>) => {
      status = headers[':status'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(status, '200');
    assert.strictEqual(body, 'ok');

    await session.close();
    await server.close();
  });

  it('should return 500 when the handler throws synchronously', async () => {
    const handler = createExpressAdapter(() => {
      throw new Error('adapter boom');
    });

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });
    server.on('error', () => { /* expected during stream teardown */ });
    server.on('stream', (stream, headers, flags) => {
      stream.on('error', () => { /* expected during adapter teardown */ });
      handler(stream, headers, flags);
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    try {
      const stream = session.request({
        ':method': 'GET',
        ':path': '/boom',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });

      let status = '';
      let endedOrErrored = false;
      stream.on('response', (headers: Record<string, string>) => {
        status = headers[':status'] ?? '';
      });
      stream.on('data', () => { /* consume */ });
      stream.on('end', () => { endedOrErrored = true; });
      stream.on('error', () => { endedOrErrored = true; });
      await waitFor(() => status !== '' && endedOrErrored, 3000);

      assert.strictEqual(status, '500');
    } finally {
      await session.close();
      await server.close();
    }
  });

  it('should lowercase header names set via res.setHeader()', async () => {
    const handler = createExpressAdapter((_req: ExpressLikeRequest, res: ExpressLikeResponse) => {
      res.setHeader('Content-Type', 'text/plain');
      res.end('typed');
    });

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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/content-type',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let contentType = '';
    let done = false;

    stream.on('response', (headers: Record<string, string>) => {
      contentType = headers['content-type'] ?? '';
    });
    stream.on('data', () => { /* consume */ });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(contentType, 'text/plain');

    await session.close();
    await server.close();
  });
});
