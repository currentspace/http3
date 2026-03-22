/**
 * Protocol-level misuse tests.
 * Validates that invalid usage patterns are handled gracefully
 * without crashes or resource leaks.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';
import { waitFor } from '../support/helpers.js';

describe('Negative Protocol', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('should treat double respond() as no-op on second call', async () => {
    let respondCallCount = 0;

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200', 'x-first': 'yes' });
      respondCallCount++;
      stream.respond({ ':status': '404', 'x-second': 'yes' });
      respondCallCount++;
      stream.end('once');
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
      ':path': '/double-respond',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    let firstHeader = '';
    let secondHeader = '';
    let body = '';
    let done = false;

    stream.on('response', (headers: Record<string, string>) => {
      status = headers[':status'] ?? '';
      firstHeader = headers['x-first'] ?? '';
      secondHeader = headers['x-second'] ?? '';
    });
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    // Both respond() calls completed without error
    assert.strictEqual(respondCallCount, 2);
    // Client should see only the first set of headers
    assert.strictEqual(status, '200');
    assert.strictEqual(firstHeader, 'yes');
    assert.strictEqual(secondHeader, '', 'second respond headers should not reach client');
    assert.strictEqual(body, 'once');

    await session.close();
    await server.close();
  });

  it('should handle double end() without crashing', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      // Suppress the expected ERR_STREAM_WRITE_AFTER_END from double end()
      stream.on('error', () => { /* expected */ });
      stream.respond({ ':status': '200' });
      stream.end('done');
      // Second end is harmless — Duplex emits error but no crash
      stream.end('ignored');
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
      ':path': '/double-end',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let body = '';
    let done = false;
    stream.on('data', (chunk: Buffer) => { body += chunk.toString(); });
    stream.on('end', () => { done = true; });
    await waitFor(() => done, 3000);

    assert.strictEqual(body, 'done');

    await session.close();
    await server.close();
  });

  it('should emit error on write after end()', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.on('error', () => { /* teardown noise is irrelevant to this case */ });
      stream.respond({ ':status': '200' });
      stream.end('finished');
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
      ':path': '/write-after-end',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: false });

    // Consume response data to prevent backpressure
    stream.on('data', () => { /* consume */ });

    // End the client stream first
    stream.end(Buffer.from('request body'));

    // Wait for the writable side to finish
    await new Promise<void>((resolve) => {
      stream.on('finish', () => { resolve(); });
    });

    // Now try to write after end — should emit 'error'
    const writeError = await new Promise<Error>((resolve) => {
      stream.on('error', (err: Error) => { resolve(err); });
      stream.write(Buffer.from('too late'));
    });

    assert.ok(writeError, 'should have received an error');
    const errCode = (writeError as NodeJS.ErrnoException).code;
    assert.ok(
      errCode === 'ERR_STREAM_WRITE_AFTER_END' || writeError.message.includes('write after end'),
      `expected write-after-end error, got: ${writeError.message}`,
    );

    await session.close();
    await server.close();
    await new Promise<void>((resolve) => { setTimeout(resolve, 50); });
  });

  it('should error cleanly when requesting on a closed session', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('ok');
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

    // Close the session
    await session.close();

    // Attempting to request on a closed session should throw or error
    assert.throws(() => {
      session.request({
        ':method': 'GET',
        ':path': '/after-close',
        ':authority': 'localhost',
        ':scheme': 'https',
      });
    });

    await server.close();
  });
});
