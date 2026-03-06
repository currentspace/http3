import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { execSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { createSecureServer, connect } from '../lib/index.js';
import { Http3ClientSession } from '../lib/client.js';
import { Http3SecureServer } from '../lib/server.js';
import { Http3ServerSession } from '../lib/session.js';
import type { NativeEvent } from '../lib/event-loop.js';
import { Http3Error, ERR_HTTP3_SESSION_ERROR, ERR_HTTP3_STREAM_ERROR } from '../lib/errors.js';
import { generateTestCerts } from './generate-certs.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

function makePkcs12(keyPem: Buffer, certPem: Buffer, passphrase: string): Buffer {
  const dir = mkdtempSync(join(tmpdir(), 'h3-pfx-'));
  const keyPath = join(dir, 'key.pem');
  const certPath = join(dir, 'cert.pem');
  const pfxPath = join(dir, 'bundle.p12');
  writeFileSync(keyPath, keyPem);
  writeFileSync(certPath, certPem);
  execSync(`openssl pkcs12 -export -inkey "${keyPath}" -in "${certPath}" -out "${pfxPath}" -passout pass:${passphrase}`);
  return readFileSync(pfxPath);
}

describe('Spec gap regressions', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('supports H3 startup with pfx/passphrase (no key/cert)', async () => {
    const passphrase = 'test-passphrase';
    const pfx = makePkcs12(certs.key, certs.cert, passphrase);
    const server = createSecureServer({
      pfx,
      passphrase,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.end('pfx-ok');
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
      ':path': '/pfx',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    const chunks: Buffer[] = [];
    await new Promise<void>((resolve, reject) => {
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('error', reject);
      stream.on('end', () => resolve());
    });
    assert.strictEqual(Buffer.concat(chunks).toString(), 'pfx-ok');

    await session.close();
    await server.close();
  });

  it('emits keylog events on sessions when keylog is enabled', async () => {
    const keylogPath = join(tmpdir(), `http3-keylog-${randomUUID()}.log`);
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      keylog: keylogPath,
    }, (stream) => {
      stream.respond({ ':status': '204' }, { endStream: true });
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      keylog: keylogPath,
    });

    const keylogLines: Buffer[] = [];
    session.on('keylog', (line: Buffer) => { keylogLines.push(line); });

    let connected = false;
    session.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const stream = session.request({
      ':method': 'GET',
      ':path': '/keylog',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });
    await new Promise<void>((resolve, reject) => {
      stream.on('error', reject);
      stream.on('end', () => resolve());
      stream.resume();
    });

    await waitFor(() => keylogLines.length > 0, 5000);
    const hasClientRandom = keylogLines.some(line => line.toString('utf8').includes('CLIENT_RANDOM'));
    const hasFallback = keylogLines.some(line => line.toString('utf8').includes('# keylog enabled'));
    assert.ok(hasClientRandom || hasFallback, 'expected keylog output line');

    await session.close();
    await server.close();
  });

  it('maps native events to Http3Error in client and server paths', () => {
    const client = new Http3ClientSession('127.0.0.1:443');
    const clientDispatch = Reflect.get(client, '_onError') as (event: NativeEvent) => void;

    let clientSessionError: Error | undefined;
    client.on('error', (err: Error) => { clientSessionError = err; });
    clientDispatch.call(client, {
      eventType: 10,
      connHandle: 0,
      streamId: -1,
      meta: { errorCode: 17, errorReason: 'client session boom' },
    });
    assert.ok(clientSessionError instanceof Http3Error);
    assert.strictEqual(clientSessionError.code, ERR_HTTP3_SESSION_ERROR);
    assert.strictEqual(clientSessionError.quicCode, 17);

    const server = new Http3SecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    const serverDispatch = Reflect.get(server, '_onError') as (event: NativeEvent) => void;
    const streams = Reflect.get(server, '_streams') as Map<string, { destroy: (err?: Error) => void }>;
    let streamError: Error | undefined;
    streams.set('7:3', {
      destroy(err?: Error): void {
        streamError = err;
      },
    });

    serverDispatch.call(server, {
      eventType: 10,
      connHandle: 7,
      streamId: 3,
      meta: { errorCode: 42, errorReason: 'server stream boom' },
    });

    assert.ok(streamError instanceof Http3Error);
    assert.strictEqual(streamError.code, ERR_HTTP3_STREAM_ERROR);
    assert.strictEqual(streamError.h3Code, 42);

    const sessions = Reflect.get(server, '_sessions') as Map<number, Http3ServerSession>;
    const session = new Http3ServerSession();
    session._connHandle = 8;
    sessions.set(8, session);
    let serverSessionError: Error | undefined;
    session.on('error', (err: Error) => { serverSessionError = err; });

    serverDispatch.call(server, {
      eventType: 10,
      connHandle: 8,
      streamId: -1,
      meta: { errorCode: 33, errorReason: 'server session boom' },
    });
    assert.ok(serverSessionError instanceof Http3Error);
    assert.strictEqual(serverSessionError.code, ERR_HTTP3_SESSION_ERROR);
    assert.strictEqual(serverSessionError.quicCode, 33);
  });
});
