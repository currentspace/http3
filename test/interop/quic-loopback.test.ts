/**
 * QUIC loopback tests: raw QUIC (no HTTP/3) Node.js ↔ Node.js over localhost.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { X509Certificate } from 'node:crypto';
import { generateMutualTlsTestCerts, generateTestCerts } from '../support/generate-certs.js';
import { createQuicServer, connectQuic, connectQuicAsync } from '../../lib/index.js';
import type { QuicServerSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';

let certs: { key: Buffer; cert: Buffer };
let mtlsCerts: ReturnType<typeof generateMutualTlsTestCerts>;

function collect(stream: QuicStream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error('collect timed out')), 10000);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve(Buffer.concat(chunks));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

function waitForClose(session: { once(event: 'close', listener: () => void): unknown }, timeoutMs = 2000): Promise<void> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error('close timed out')), timeoutMs);
    session.once('close', () => {
      clearTimeout(timeout);
      resolve();
    });
  });
}

describe('QUIC loopback', () => {
  before(() => {
    certs = generateTestCerts();
    mtlsCerts = generateMutualTlsTestCerts();
  });

  it('echo server — client sends, server echoes back', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');

    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    const payload = Buffer.from('hello QUIC world');
    stream.end(payload);
    const echoed = await collect(stream);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('client-authenticated QUIC works with connectQuicAsync', async () => {
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      disableRetry: true,
      runtimeMode: 'portable',
    });

    const expectedFingerprint = new X509Certificate(mtlsCerts.client.cert).fingerprint256;
    const peerCertificate = new Promise<{ presented: boolean; fingerprint: string | null; chainLength: number }>((resolve) => {
      server.once('session', (session: QuicServerSession) => {
        resolve({
          presented: session.peerCertificatePresented,
          fingerprint: session.getPeerCertificate()?.fingerprint256 ?? null,
          chainLength: session.getPeerCertificateChain().length,
        });
      });
    });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      cert: mtlsCerts.client.cert,
      key: mtlsCerts.client.key,
      servername: 'localhost',
      runtimeMode: 'portable',
      fallbackPolicy: 'error',
    });

    assert.strictEqual(client.runtimeInfo?.requestedMode, 'portable');
    assert.strictEqual(client.runtimeInfo?.selectedMode, 'portable');
    assert.ok(client.runtimeInfo?.driver);
    assert.deepStrictEqual(await peerCertificate, {
      presented: true,
      fingerprint: expectedFingerprint,
      chainLength: 1,
    });

    const stream = client.openStream();
    const payload = Buffer.from('mutual-quic-async');
    stream.end(payload);
    const echoed = await collect(stream);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('client-authenticated QUIC works with connectQuic + ready()', async () => {
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = connectQuic(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      cert: mtlsCerts.client.cert,
      key: mtlsCerts.client.key,
      servername: 'localhost',
    });

    await client.ready();

    const stream = client.openStream();
    const payload = Buffer.from('mutual-quic-evented');
    stream.end(payload);
    const echoed = await collect(stream);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('defaults to requiring client certificates when ca is configured', async () => {
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      disableRetry: true,
    });
    let sessionCount = 0;
    server.on('session', () => {
      sessionCount += 1;
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = connectQuic(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      servername: 'localhost',
    });
    client.on('error', () => {});

    await waitForClose(client);
    assert.strictEqual(sessionCount, 0);

    await server.close();
  });

  it('clientAuth request allows anonymous QUIC clients', async () => {
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      clientAuth: 'request',
      disableRetry: true,
    });

    const peerCertificate = new Promise<{ presented: boolean; certificate: X509Certificate | null; chainLength: number }>((resolve) => {
      server.once('session', (session: QuicServerSession) => {
        resolve({
          presented: session.peerCertificatePresented,
          certificate: session.getPeerCertificate(),
          chainLength: session.getPeerCertificateChain().length,
        });
      });
    });
    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      servername: 'localhost',
    });

    assert.deepStrictEqual(await peerCertificate, {
      presented: false,
      certificate: null,
      chainLength: 0,
    });

    const stream = client.openStream();
    stream.end(Buffer.from('anonymous-ok'));
    const echoed = await collect(stream);
    assert.strictEqual(echoed.toString(), 'anonymous-ok');

    await client.close();
    await server.close();
  });

  it('rejects mismatched client certificate chains', async () => {
    const otherMtlsCerts = generateMutualTlsTestCerts();
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      disableRetry: true,
    });
    let sessionCount = 0;
    server.on('session', () => {
      sessionCount += 1;
    });
    const addr = await server.listen(0, '127.0.0.1');
    const client = connectQuic(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      cert: otherMtlsCerts.client.cert,
      key: otherMtlsCerts.client.key,
      servername: 'localhost',
    });
    client.on('error', () => {});

    await waitForClose(client);
    assert.strictEqual(sessionCount, 0);

    await server.close();
  });

  it('applications can pin an exact client certificate after CA verification', async () => {
    const expectedFingerprint = new X509Certificate(mtlsCerts.client.cert).fingerprint256;
    const alternateFingerprint = new X509Certificate(mtlsCerts.alternateClient.cert).fingerprint256;
    const server = createQuicServer({
      key: mtlsCerts.server.key,
      cert: mtlsCerts.server.cert,
      ca: mtlsCerts.ca.cert,
      clientAuth: 'require',
      disableRetry: true,
    });
    const rejectedFingerprint = new Promise<string | null>((resolve) => {
      server.once('session', (session: QuicServerSession) => {
        const peerCertificate = session.getPeerCertificate();
        if (!peerCertificate || peerCertificate.fingerprint256 !== expectedFingerprint) {
          resolve(peerCertificate?.fingerprint256 ?? null);
          session.close(0x101, 'unexpected client certificate');
          return;
        }
        resolve(peerCertificate.fingerprint256);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = connectQuic(`https://localhost:${addr.port}`, {
      ca: mtlsCerts.ca.cert,
      cert: mtlsCerts.alternateClient.cert,
      key: mtlsCerts.alternateClient.key,
      servername: 'localhost',
    });
    client.on('error', () => {});

    await waitForClose(client);
    assert.strictEqual(await rejectedFingerprint, alternateFingerprint);

    await server.close();
  });

  it('multiple streams on one connection', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const results = await Promise.all(
      [0, 1, 2, 3, 4].map(async (i) => {
        const s = client.openStream();
        const msg = Buffer.from(`stream-${i}`);
        s.end(msg);
        const data = await collect(s);
        return data.toString();
      }),
    );

    assert.deepStrictEqual(results.sort(), ['stream-0', 'stream-1', 'stream-2', 'stream-3', 'stream-4']);

    await client.close();
    await server.close();
  });

  it('large payload transfer', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const payload = Buffer.alloc(256 * 1024, 0xab); // 256KB
    const stream = client.openStream();
    stream.end(payload);
    const echoed = await collect(stream);
    assert.strictEqual(echoed.length, payload.length);
    assert.deepStrictEqual(echoed, payload);

    await client.close();
    await server.close();
  });

  it('server-initiated streams', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    const serverPushPromise = new Promise<Buffer>((resolve) => {
      server.on('session', (session: QuicServerSession) => {
        // When client connects, server opens a stream to push data
        const pushStream = session.openStream();
        pushStream.end(Buffer.from('server push'));

        // Also listen for client streams
        session.on('stream', (stream: QuicStream) => {
          const chunks: Buffer[] = [];
          stream.on('data', (c: Buffer) => chunks.push(c));
          stream.on('end', () => {
            resolve(Buffer.concat(chunks));
            stream.end(Buffer.from('ack'));
          });
        });
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    // Receive server-initiated stream
    const serverData = await new Promise<Buffer>((resolve) => {
      client.on('stream', (stream: QuicStream) => {
        const chunks: Buffer[] = [];
        stream.on('data', (c: Buffer) => chunks.push(c));
        stream.on('end', () => resolve(Buffer.concat(chunks)));
      });
    });

    // Also send from client
    const clientStream = client.openStream();
    clientStream.end(Buffer.from('client data'));
    const ack = await collect(clientStream);

    assert.strictEqual(serverData.toString(), 'server push');
    assert.strictEqual(ack.toString(), 'ack');

    const clientSentData = await serverPushPromise;
    assert.strictEqual(clientSentData.toString(), 'client data');

    await client.close();
    await server.close();
  });

  it('multiple concurrent connections', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');

    const clients = await Promise.all(
      Array.from({ length: 5 }, () =>
        connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false }),
      ),
    );

    const results = await Promise.all(
      clients.map(async (client, i) => {
        const stream = client.openStream();
        const msg = Buffer.from(`conn-${i}`);
        stream.end(msg);
        return (await collect(stream)).toString();
      }),
    );

    assert.deepStrictEqual(results.sort(), ['conn-0', 'conn-1', 'conn-2', 'conn-3', 'conn-4']);

    await Promise.all(clients.map((c) => c.close()));
    await server.close();
  });

  it('session metrics are available', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
    });

    const stream = client.openStream();
    stream.end(Buffer.from('metrics test'));
    await collect(stream);

    // Wait briefly for metrics to settle
    await new Promise<void>((r) => { setTimeout(r, 50); });

    const metrics = client.getMetrics();
    assert.ok(metrics, 'metrics should be available');
    assert.ok(metrics!.packetsIn > 0, 'should have received packets');
    assert.ok(metrics!.packetsOut > 0, 'should have sent packets');
    assert.ok(metrics!.bytesIn > 0, 'should have received bytes');
    assert.ok(metrics!.bytesOut > 0, 'should have sent bytes');

    await client.close();
    await server.close();
  });

  it('custom ALPN negotiation', async () => {
    const server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      alpn: ['my-custom-proto'],
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.pipe(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
      rejectUnauthorized: false,
      alpn: ['my-custom-proto'],
    });

    const stream = client.openStream();
    stream.end(Buffer.from('custom alpn'));
    const echoed = await collect(stream);
    assert.strictEqual(echoed.toString(), 'custom alpn');

    await client.close();
    await server.close();
  });
});
