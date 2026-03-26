/**
 * Native binding isolation tests.
 * Exercises native class construction and basic operations directly,
 * bypassing TypeScript wrappers, to verify the N-API boundary works
 * correctly for both H3 (Worker) and raw QUIC classes.
 */

import { describe, it, after, before } from 'node:test';
import assert from 'node:assert/strict';
import { generateTestCerts } from '../support/generate-certs.js';

// Load the native binding directly (bypassing TS wrappers).
// eslint-disable-next-line @typescript-eslint/no-require-imports, @typescript-eslint/no-unsafe-assignment
const binding = require('../../../index.js');

// Event type constants (mirrored from h3_event.rs).
const EVENT_NEW_SESSION = 1;
const EVENT_NEW_STREAM = 2;
const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_SHUTDOWN_COMPLETE = 15;

function waitForEvent(
  events: any[],
  eventType: number,
  timeoutMs = 10000,
): Promise<any> {
  return new Promise((resolve, reject) => {
    const start = Date.now();
    const check = (): void => {
      const found = events.find((e: any) => e.eventType === eventType);
      if (found) {
        resolve(found);
        return;
      }
      if (Date.now() - start > timeoutMs) {
        reject(
          new Error(
            `Timed out waiting for eventType=${eventType} after ${timeoutMs}ms. ` +
            `Received events: [${events.map((e: any) => e.eventType).join(', ')}]`,
          ),
        );
        return;
      }
      setTimeout(check, 10);
    };
    check();
  });
}

function waitForShutdown(events: any[], timeoutMs = 5000): Promise<void> {
  return waitForEvent(events, EVENT_SHUTDOWN_COMPLETE, timeoutMs).then(() => {});
}

describe('Native binding isolation', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  // Native objects that are constructed but never fully started may
  // retain a ThreadsafeFunction reference. Force clean exit.
  after(() => {
    setTimeout(() => process.exit(0), 100).unref();
  });

  // ── H3 Server isolation ────────────────────────────────────────

  describe('H3 Server (NativeWorkerServer) isolation', () => {
    it('constructor accepts valid options and callback', () => {
      const events: any[] = [];
      const server = new binding.NativeWorkerServer(
        {
          key: certs.key,
          cert: certs.cert,
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      assert.ok(server, 'NativeWorkerServer constructor should return an object');
      // Clean up without listen -- shutdown is safe.
      server.shutdown();
    });

    it('listen() returns address info object', async () => {
      const events: any[] = [];
      const server = new binding.NativeWorkerServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      const addr = server.listen(0, '127.0.0.1');
      assert.ok(typeof addr === 'object', 'listen() should return an object');
      assert.ok(typeof addr.port === 'number', 'addr.port should be a number');
      assert.ok(addr.port > 0, `addr.port should be > 0, got ${addr.port}`);
      assert.ok(typeof addr.address === 'string', 'addr.address should be a string');
      assert.ok(typeof addr.family === 'string', 'addr.family should be a string');

      server.shutdown();
      await waitForShutdown(events);
    });

    it('shutdown() delivers SHUTDOWN_COMPLETE event', async () => {
      const events: any[] = [];
      const server = new binding.NativeWorkerServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();
      await waitForShutdown(events);

      const shutdownEvent = events.find(
        (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
      );
      assert.ok(shutdownEvent, 'should have received SHUTDOWN_COMPLETE event');
    });
  });

  // ── QUIC Server isolation ──────────────────────────────────────

  describe('QUIC Server (NativeQuicServer) isolation', () => {
    it('constructor accepts valid options and callback', () => {
      const events: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      assert.ok(server, 'NativeQuicServer constructor should return an object');
      server.shutdown();
    });

    it('listen() returns address info object', async () => {
      const events: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      const addr = server.listen(0, '127.0.0.1');
      assert.ok(typeof addr === 'object', 'listen() should return an object');
      assert.ok(typeof addr.port === 'number', 'addr.port should be a number');
      assert.ok(addr.port > 0, `addr.port should be > 0, got ${addr.port}`);
      assert.ok(typeof addr.address === 'string', 'addr.address should be a string');
      assert.ok(typeof addr.family === 'string', 'addr.family should be a string');

      server.shutdown();
      await waitForShutdown(events);
    });

    it('shutdown() delivers SHUTDOWN_COMPLETE event', async () => {
      const events: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { events.push(...batch); },
      );
      server.listen(0, '127.0.0.1');
      server.shutdown();
      await waitForShutdown(events);

      const shutdownEvent = events.find(
        (e: any) => e.eventType === EVENT_SHUTDOWN_COMPLETE,
      );
      assert.ok(shutdownEvent, 'should have received SHUTDOWN_COMPLETE event');
    });
  });

  // ── H3 round-trip ──────────────────────────────────────────────

  describe('H3 round-trip via raw binding', () => {
    it('server+client complete request/response via raw binding', async () => {
      const serverEvents: any[] = [];
      const server = new binding.NativeWorkerServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { serverEvents.push(...batch); },
      );
      const sAddr = server.listen(0, '127.0.0.1');

      const clientEvents: any[] = [];
      const client = new binding.NativeWorkerClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        (_err: Error | null, batch: any[]) => { clientEvents.push(...batch); },
      );
      client.connect(`127.0.0.1:${sAddr.port}`, 'localhost');

      // Wait for client HANDSHAKE_COMPLETE.
      await waitForEvent(clientEvents, EVENT_HANDSHAKE_COMPLETE);

      // Client sends request headers.
      const streamId = client.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/test' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true, // fin -- no body
      );
      assert.ok(typeof streamId === 'number', 'sendRequest should return a stream ID');

      // Wait for server to see NEW_SESSION.
      await waitForEvent(serverEvents, EVENT_NEW_SESSION);

      // Wait for server to see HEADERS from the client request.
      await waitForEvent(serverEvents, EVENT_HEADERS);

      // Find the server-side request event to get connHandle and streamId.
      const headersEvent = serverEvents.find(
        (e: any) => e.eventType === EVENT_HEADERS,
      );
      assert.ok(headersEvent, 'server should receive HEADERS event');
      const connHandle = headersEvent.connHandle;
      const serverStreamId = headersEvent.streamId;

      // Server sends response headers.
      server.sendResponseHeaders(
        connHandle,
        serverStreamId,
        [
          { name: ':status', value: '200' },
          { name: 'content-type', value: 'text/plain' },
        ],
        false,
      );

      // Server sends response body and closes stream.
      const body = Buffer.from('hello from raw binding');
      server.streamSend(connHandle, serverStreamId, body, true);

      // Wait for client to see HEADERS from the server response.
      await waitForEvent(clientEvents, EVENT_HEADERS);

      // Wait for client to see DATA.
      await waitForEvent(clientEvents, EVENT_DATA);

      // Verify the client received response data.
      const dataEvents = clientEvents.filter(
        (e: any) => e.eventType === EVENT_DATA && e.data,
      );
      assert.ok(dataEvents.length > 0, 'client should receive DATA events');
      const receivedBody = Buffer.concat(
        dataEvents.map((e: any) => Buffer.from(e.data)),
      );
      assert.strictEqual(
        receivedBody.toString(),
        'hello from raw binding',
        'client should receive the response body',
      );

      // Shutdown both.
      client.shutdown();
      await waitForShutdown(clientEvents);
      server.shutdown();
      await waitForShutdown(serverEvents);
    });
  });

  // ── QUIC round-trip ────────────────────────────────────────────

  describe('QUIC round-trip via raw binding', () => {
    it('server+client complete stream via raw binding', async () => {
      const serverEvents: any[] = [];
      const server = new binding.NativeQuicServer(
        {
          key: certs.key,
          cert: certs.cert,
          disableRetry: true,
          runtimeMode: 'portable',
        },
        (_err: Error | null, batch: any[]) => { serverEvents.push(...batch); },
      );
      const sAddr = server.listen(0, '127.0.0.1');

      const clientEvents: any[] = [];
      const client = new binding.NativeQuicClient(
        { rejectUnauthorized: false, runtimeMode: 'portable' },
        (_err: Error | null, batch: any[]) => { clientEvents.push(...batch); },
      );
      client.connect(`127.0.0.1:${sAddr.port}`, 'localhost');

      // Wait for client HANDSHAKE_COMPLETE.
      await waitForEvent(clientEvents, EVENT_HANDSHAKE_COMPLETE);

      // Client opens a bidirectional stream by sending data on stream 0
      // (QUIC bidi stream IDs for client are 0, 4, 8, ...).
      const payload = Buffer.from('quic raw binding test');
      client.streamSend(0, payload, true);

      // Wait for server to see NEW_SESSION.
      await waitForEvent(serverEvents, EVENT_NEW_SESSION);

      // The raw QUIC layer coalesces the first recv into the NEW_STREAM
      // event (type 2) with data attached, rather than a separate DATA
      // event. Wait for NEW_STREAM which carries the initial data.
      await waitForEvent(serverEvents, EVENT_NEW_STREAM);

      // The data arrives in either a NEW_STREAM or DATA event.
      const serverDataEvent = serverEvents.find(
        (e: any) =>
          (e.eventType === EVENT_NEW_STREAM || e.eventType === EVENT_DATA) && e.data,
      );
      assert.ok(serverDataEvent, 'server should receive stream data');
      const connHandle = serverDataEvent.connHandle;
      const serverStreamId = serverDataEvent.streamId;

      // Verify server received the payload.
      const receivedPayload = Buffer.from(serverDataEvent.data);
      assert.strictEqual(
        receivedPayload.toString(),
        'quic raw binding test',
        'server should receive the client payload',
      );

      // Server echoes back on the same stream.
      const echo = Buffer.from('echo: quic raw binding test');
      server.streamSend(connHandle, serverStreamId, echo, true);

      // Wait for client to see DATA (response on existing bidi stream).
      await waitForEvent(clientEvents, EVENT_DATA);

      // Verify client received echo.
      const clientDataEvents = clientEvents.filter(
        (e: any) => e.eventType === EVENT_DATA && e.data,
      );
      assert.ok(clientDataEvents.length > 0, 'client should receive DATA events');
      const echoedData = Buffer.concat(
        clientDataEvents.map((e: any) => Buffer.from(e.data)),
      );
      assert.strictEqual(
        echoedData.toString(),
        'echo: quic raw binding test',
        'client should receive the echoed data',
      );

      // Shutdown both.
      client.shutdown();
      await waitForShutdown(clientEvents);
      server.shutdown();
      await waitForShutdown(serverEvents);
    });
  });
});
