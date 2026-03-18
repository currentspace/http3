import assert from 'node:assert';
import { before, describe, it } from 'node:test';
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

describe('Spec parity additions', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('emits per-session metrics and supports ping/settings access', async () => {
    let serverSessionSeen = false;
    let serverMetricsSeen = false;

    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      metricsIntervalMs: 100,
    }, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('ok');
    });

    server.on('session', (session) => {
      serverSessionSeen = true;
      session.on('metrics', () => { serverMetricsSeen = true; });
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
      metricsIntervalMs: 100,
    });

    let connected = false;
    let clientMetricsSeen = false;
    client.on('connect', () => { connected = true; });
    client.on('metrics', () => { clientMetricsSeen = true; });
    await waitFor(() => connected, 3000);

    const req = client.request({
      ':method': 'GET',
      ':path': '/',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });
    await new Promise<void>((resolve) => {
      req.on('end', () => { resolve(); });
      req.resume();
    });

    await waitFor(() => serverSessionSeen && serverMetricsSeen && clientMetricsSeen, 3000);

    const metrics = client.getMetrics();
    assert.ok(metrics);
    assert.ok(metrics.cwnd >= 0);
    assert.ok(metrics.rttMs >= 0);

    const pingRtt = client.ping();
    assert.ok(pingRtt >= 0);

    const remoteSettings = client.getRemoteSettings();
    assert.ok(remoteSettings);
    assert.strictEqual(typeof remoteSettings, 'object');

    await client.close();
    await server.close();
  });

  it('supports stream timeout callbacks for HTTP/3 streams', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      setTimeout(() => {
        stream.respond({ ':status': '200' });
        stream.end('late');
      }, 150);
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const client = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    let connected = false;
    client.on('connect', () => { connected = true; });
    await waitFor(() => connected, 3000);

    const req = client.request({
      ':method': 'GET',
      ':path': '/timeout',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let timedOut = false;
    req.setTimeout(25, () => { timedOut = true; });
    await waitFor(() => timedOut, 2000);

    await client.close();
    await server.close();
  });

  it('enforces safe methods when using pre-connect 0-RTT requests', async () => {
    const session = connect('127.0.0.1:65535', {
      rejectUnauthorized: false,
      allow0RTT: true,
    });
    session.on('error', () => {});

    assert.throws(() => {
      session.request({
        ':method': 'POST',
        ':path': '/unsafe',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });
    }, /0-RTT is restricted to safe methods/);

    await session.close();
  });
});
