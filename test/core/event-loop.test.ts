/**
 * Integration tests for worker-only event loop adapters.
 * UDP, polling, and timers run entirely inside Rust worker threads.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { WorkerEventLoop, ClientEventLoop, binding } from '../../lib/event-loop.js';
import type { NativeEvent } from '../../lib/event-loop.js';

const EVENT_NEW_SESSION = 1;
const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_HANDSHAKE_COMPLETE = 11;

describe('Worker Event Loops', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should bind and accept a connection', async () => {
    const serverEvents: NativeEvent[] = [];
    const clientEvents: NativeEvent[] = [];

    const nativeServer = new binding.NativeWorkerServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (_err: Error | null, events: NativeEvent[]) => {
      serverEvents.push(...events);
    });
    const serverLoop = new WorkerEventLoop(nativeServer);
    const addr = nativeServer.listen(0, '127.0.0.1');
    assert.ok(addr.port > 0);

    const nativeClient = new binding.NativeWorkerClient({
      rejectUnauthorized: false,
    }, (_err: Error | null, events: NativeEvent[]) => {
      clientEvents.push(...events);
    });
    const clientLoop = new ClientEventLoop(nativeClient);
    await clientLoop.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await waitFor(() =>
      serverEvents.some(e => e.eventType === EVENT_HANDSHAKE_COMPLETE)
      && clientEvents.some(e => e.eventType === EVENT_HANDSHAKE_COMPLETE), 2000);

    assert.ok(serverEvents.some(e => e.eventType === EVENT_NEW_SESSION), 'server should see new session');
    assert.ok(clientEvents.some(e => e.eventType === EVENT_HANDSHAKE_COMPLETE), 'client handshake should complete');

    await clientLoop.close();
    await serverLoop.close();
  });

  it('should handle a full request/response cycle', async () => {
    const serverEvents: NativeEvent[] = [];
    const clientEvents: NativeEvent[] = [];

    const loopRef: { current: WorkerEventLoop | null } = { current: null };
    const nativeServer = new binding.NativeWorkerServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (_err: Error | null, events: NativeEvent[]) => {
      for (const event of events) {
        serverEvents.push(event);
        if (event.eventType === EVENT_HEADERS && event.headers) {
          const path = event.headers.find(h => h.name === ':path')?.value;
          if (path === '/hello') {
            loopRef.current?.sendResponseHeaders(
              event.connHandle,
              event.streamId,
              [{ name: ':status', value: '200' }, { name: 'content-type', value: 'text/plain' }],
              false,
            );
            loopRef.current?.streamSend(
              event.connHandle,
              event.streamId,
              Buffer.from('Hello, HTTP/3!'),
              true,
            );
          }
        }
      }
    });
    const serverLoop = new WorkerEventLoop(nativeServer);
    loopRef.current = serverLoop;
    const addr = nativeServer.listen(0, '127.0.0.1');

    const nativeClient = new binding.NativeWorkerClient({ rejectUnauthorized: false }, (_err: Error | null, events: NativeEvent[]) => {
      clientEvents.push(...events);
    });
    const clientLoop = new ClientEventLoop(nativeClient);
    await clientLoop.connect(`127.0.0.1:${addr.port}`, 'localhost');

    await waitFor(() => clientEvents.some(e => e.eventType === EVENT_HANDSHAKE_COMPLETE), 2000);

    clientLoop.sendRequest(
      [
        { name: ':method', value: 'GET' },
        { name: ':path', value: '/hello' },
        { name: ':authority', value: 'localhost' },
        { name: ':scheme', value: 'https' },
      ],
      true,
    );

    await waitFor(() =>
      clientEvents.some(e => e.eventType === EVENT_HEADERS && e.headers?.some(h => h.name === ':status')),
    2000);

    const responseHeaders = clientEvents.find(
      e => e.eventType === EVENT_HEADERS && e.headers?.some(h => h.name === ':status'),
    );
    assert.ok(responseHeaders);
    const status = responseHeaders.headers?.find(h => h.name === ':status');
    assert.strictEqual(status?.value, '200');

    await waitFor(() => clientEvents.some(e => e.eventType === EVENT_DATA), 1000);
    const dataEvent = clientEvents.find(e => e.eventType === EVENT_DATA);
    assert.ok(dataEvent?.data);
    assert.strictEqual(Buffer.from(dataEvent.data).toString(), 'Hello, HTTP/3!');

    await clientLoop.close();
    await serverLoop.close();
  });

  it('should handle multiple concurrent requests', async () => {
    const clientEvents: NativeEvent[] = [];

    const loopRef: { current: WorkerEventLoop | null } = { current: null };
    const nativeServer = new binding.NativeWorkerServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (_err: Error | null, events: NativeEvent[]) => {
      for (const event of events) {
        if (event.eventType === EVENT_HEADERS && event.headers) {
          const path = event.headers.find(h => h.name === ':path')?.value ?? '';
          loopRef.current?.sendResponseHeaders(
            event.connHandle,
            event.streamId,
            [{ name: ':status', value: '200' }],
            false,
          );
          loopRef.current?.streamSend(
            event.connHandle,
            event.streamId,
            Buffer.from(`response for ${path}`),
            true,
          );
        }
      }
    });
    const serverLoop = new WorkerEventLoop(nativeServer);
    loopRef.current = serverLoop;
    const addr = nativeServer.listen(0, '127.0.0.1');

    const nativeClient = new binding.NativeWorkerClient({ rejectUnauthorized: false }, (_err: Error | null, events: NativeEvent[]) => {
      clientEvents.push(...events);
    });
    const clientLoop = new ClientEventLoop(nativeClient);
    await clientLoop.connect(`127.0.0.1:${addr.port}`, 'localhost');
    await waitFor(() => clientEvents.some(e => e.eventType === EVENT_HANDSHAKE_COMPLETE), 2000);

    for (let i = 0; i < 3; i++) {
      clientLoop.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: `/path${i}` },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );
    }

    await waitFor(() => {
      const responses = clientEvents.filter(
        e => e.eventType === EVENT_HEADERS && e.headers?.some(h => h.name === ':status'),
      );
      return responses.length >= 3;
    }, 3000);

    const responses = clientEvents.filter(
      e => e.eventType === EVENT_HEADERS && e.headers?.some(h => h.name === ':status'),
    );
    assert.strictEqual(responses.length, 3);

    await clientLoop.close();
    await serverLoop.close();
  });

  it('should allow multiple worker servers on one UDP port with reusePort', async () => {
    const nativeServerA = new binding.NativeWorkerServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      reusePort: true,
    }, () => {});
    const nativeServerB = new binding.NativeWorkerServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      reusePort: true,
    }, () => {});

    const serverLoopA = new WorkerEventLoop(nativeServerA);
    const serverLoopB = new WorkerEventLoop(nativeServerB);

    const addrA = nativeServerA.listen(0, '127.0.0.1');
    assert.ok(addrA.port > 0);
    const addrB = nativeServerB.listen(addrA.port, '127.0.0.1');
    assert.strictEqual(addrB.port, addrA.port);

    await serverLoopA.close();
    await serverLoopB.close();
  });
});

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}
