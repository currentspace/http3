/**
 * Integration test for SSE heartbeat via ServerSentEventStream.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../lib/index.js';
import { createSseStream } from '../lib/sse.js';
import { generateTestCerts } from './generate-certs.js';
import { waitFor } from './helpers.js';

describe('SSE heartbeat', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should receive heartbeat comment frames at configured interval', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('stream', (stream, _headers) => {
      const sse = createSseStream(stream, {
        heartbeatIntervalMs: 100,
        heartbeatComment: 'ping',
      });
      // Send one event then keep alive via heartbeat
      void sse.send('hello');
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

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    const chunks: string[] = [];
    stream.on('data', (chunk: Buffer) => {
      chunks.push(chunk.toString());
    });

    // Wait long enough for at least 2 heartbeats (100ms interval)
    await new Promise<void>((resolve) => { setTimeout(resolve, 350); });

    const allData = chunks.join('');
    // Should contain the event data and at least one heartbeat comment
    assert.ok(allData.includes('data: hello'), 'should contain event data');
    assert.ok(allData.includes(': ping'), 'should contain heartbeat comment');

    await session.close();
    await server.close();
  });

  it('close() should stop heartbeat and end stream', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('stream', (stream, _headers) => {
      const sse = createSseStream(stream, {
        heartbeatIntervalMs: 50,
      });
      void sse.send('initial');
      // Close after a short delay
      setTimeout(() => { sse.close(); }, 100);
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

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let ended = false;
    stream.on('end', () => { ended = true; });
    stream.on('data', () => { /* consume */ });
    await waitFor(() => ended, 3000);

    assert.ok(ended, 'stream should end after sse.close()');

    await session.close();
    await server.close();
  });

  it('send() and comment() should be no-op after close()', async () => {
    let postCloseWritesCompleted = false;
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('stream', (stream, _headers) => {
      void (async () => {
        const sse = createSseStream(stream);
        await sse.send('before-close');
        sse.close();
        await sse.send('after-close');
        await sse.comment('after-close');
        postCloseWritesCompleted = true;
      })().catch((err: unknown) => {
        stream.destroy(err instanceof Error ? err : new Error(String(err)));
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

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let ended = false;
    const chunks: string[] = [];
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk.toString()); });
    stream.on('end', () => { ended = true; });
    await waitFor(() => ended && postCloseWritesCompleted, 3000);

    const body = chunks.join('');
    assert.ok(body.includes('data: before-close'));
    assert.ok(!body.includes('after-close'));

    await session.close();
    await server.close();
  });
});
