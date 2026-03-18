import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

async function establishSession(session: ReturnType<typeof connect>): Promise<void> {
  const stream = session.request({
    ':method': 'GET',
    ':path': '/ready',
    ':authority': 'localhost',
    ':scheme': 'https',
  }, { endStream: true });
  await new Promise<void>((resolve, reject) => {
    stream.on('error', reject);
    stream.on('end', () => resolve());
    stream.resume();
  });
}

describe('Datagram API', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('sends and receives datagrams when enabled on both peers', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      enableDatagrams: true,
    }, (stream) => {
      stream.respond({ ':status': '204' }, { endStream: true });
    });

    server.on('session', (session) => {
      session.on('datagram', (payload: Buffer) => {
        session.sendDatagram(Buffer.from(`echo:${payload.toString()}`));
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

    const client = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      enableDatagrams: true,
    });
    let connected = false;
    client.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);
    await establishSession(client);

    let echoedText = '';
    client.on('datagram', (payload: Buffer) => {
      echoedText = payload.toString();
    });

    const ok = client.sendDatagram(Buffer.from('ping'));
    assert.strictEqual(ok, true);
    await waitFor(() => echoedText.length > 0, 3000);
    assert.strictEqual(echoedText, 'echo:ping');

    await client.close();
    await server.close();
  });

  it('rejects oversized datagrams', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      enableDatagrams: true,
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

    const client = connect(`127.0.0.1:${port}`, {
      rejectUnauthorized: false,
      enableDatagrams: true,
    });
    let connected = false;
    client.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);
    await establishSession(client);

    const ok = client.sendDatagram(Buffer.alloc(64 * 1024, 1));
    assert.strictEqual(ok, false);

    await client.close();
    await server.close();
  });
});
