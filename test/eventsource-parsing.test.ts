/**
 * Integration tests for Http3EventSource field parsing.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { createSecureServer } from '../lib/index.js';
import { Http3EventSource } from '../lib/eventsource.js';
import type { EventSourceMessage } from '../lib/eventsource.js';
import { generateTestCerts } from './generate-certs.js';
import { waitFor } from './helpers.js';

describe('EventSource parsing', () => {
  let certs: { key: Buffer; cert: Buffer };
  let port: number;
  let server: ReturnType<typeof createSecureServer>;

  before(async () => {
    certs = generateTestCerts();
    server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('stream', (stream, headers) => {
      const path = headers[':path'] as string;

      if (path === '/comments') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        stream.end(': this is a comment\ndata: visible\n\n');
        return;
      }

      if (path === '/multiline') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        stream.end('data: line1\ndata: line2\ndata: line3\n\n');
        return;
      }

      if (path === '/named') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        stream.end('event: custom\ndata: payload\n\n');
        return;
      }

      if (path === '/bare-field') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        // Bare field name without colon
        stream.end('data\n\n');
        return;
      }

      if (path === '/retry') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        stream.end('retry: 5000\ndata: visible\n\n');
        return;
      }

      if (path === '/invalid-id') {
        stream.respond({
          ':status': '200',
          'content-type': 'text/event-stream',
        });
        stream.end(Buffer.from('id: first\ndata: one\n\nid: bad\0id\ndata: two\n\n', 'utf8'));
        return;
      }

      stream.respond({ ':status': '404' });
      stream.end();
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

  it('should ignore comment lines', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/comments`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const messages: EventSourceMessage[] = [];
    es.on('message', (msg: EventSourceMessage) => { messages.push(msg); });

    await waitFor(() => messages.length >= 1, 5000);
    es.close();

    assert.strictEqual(messages.length, 1);
    assert.strictEqual(messages[0].data, 'visible');
  });

  it('should concatenate multi-line data fields with newline', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/multiline`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const messages: EventSourceMessage[] = [];
    es.on('message', (msg: EventSourceMessage) => { messages.push(msg); });

    await waitFor(() => messages.length >= 1, 5000);
    es.close();

    assert.strictEqual(messages[0].data, 'line1\nline2\nline3');
  });

  it('should dispatch named events', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/named`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const customs: EventSourceMessage[] = [];
    es.on('custom', (msg: EventSourceMessage) => { customs.push(msg); });

    await waitFor(() => customs.length >= 1, 5000);
    es.close();

    assert.strictEqual(customs[0].type, 'custom');
    assert.strictEqual(customs[0].data, 'payload');
  });

  it('should handle bare field name without colon', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/bare-field`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const messages: EventSourceMessage[] = [];
    es.on('message', (msg: EventSourceMessage) => { messages.push(msg); });

    await waitFor(() => messages.length >= 1, 5000);
    es.close();

    // Bare "data" without colon should result in empty-string data
    assert.strictEqual(messages[0].data, '');
  });

  it('should update and clamp retry delay from retry field', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/retry`, {
      rejectUnauthorized: false,
      reconnect: false,
      maxRetryMs: 250,
    });

    const messages: EventSourceMessage[] = [];
    es.on('message', (msg: EventSourceMessage) => { messages.push(msg); });

    await waitFor(() => messages.length >= 1, 5000);
    const retryMs = Reflect.get(es, '_retryMs') as number;
    es.close();

    assert.strictEqual(messages[0].data, 'visible');
    assert.strictEqual(retryMs, 250);
  });

  it('should ignore id field containing null bytes', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/invalid-id`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const messages: EventSourceMessage[] = [];
    es.on('message', (msg: EventSourceMessage) => { messages.push(msg); });

    await waitFor(() => messages.length >= 2, 5000);
    es.close();

    assert.strictEqual(messages[0].lastEventId, 'first');
    assert.strictEqual(messages[1].lastEventId, 'first');
    assert.strictEqual(messages[1].data, 'two');
  });

  it('readyState should transition CONNECTING → OPEN → CLOSED', async () => {
    const es = new Http3EventSource(`https://127.0.0.1:${port}/comments`, {
      rejectUnauthorized: false,
      reconnect: false,
    });

    const states: number[] = [es.readyState];
    es.on('open', () => {
      states.push(es.readyState);
    });
    es.on('close', () => {
      states.push(es.readyState);
    });

    await waitFor(() => states.includes(Http3EventSource.OPEN) && states.includes(Http3EventSource.CLOSED), 5000);

    assert.strictEqual(states[0], Http3EventSource.CONNECTING);
    assert.ok(states.includes(Http3EventSource.OPEN));
    assert.strictEqual(es.readyState, Http3EventSource.CLOSED);
  });

  // Clean up after all tests
  it('cleanup', async () => {
    await server.close();
  });
});
