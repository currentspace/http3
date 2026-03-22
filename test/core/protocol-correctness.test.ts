import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connect } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';
import {
  appendLifecycleArtifacts,
  beginLifecycleCapture,
  endLifecycleCapture,
  withLifecycleTimeout,
} from '../support/failure-artifacts.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

async function readBody(stream: NodeJS.EventEmitter): Promise<string> {
  return await new Promise<string>((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('error', reject);
    stream.on('end', () => { resolve(Buffer.concat(chunks).toString()); });
  });
}

describe('Protocol correctness regressions', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('client receives GOAWAY before close on graceful server session close', async () => {
    beginLifecycleCapture();
    try {
      let serverSession: unknown = null;
      const server = createSecureServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
      }, (stream) => {
        stream.respond({ ':status': '200' });
        stream.end('ok');
      });
      server.on('session', (session) => {
        serverSession = session;
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

      const stream = client.request({
        ':method': 'GET',
        ':path': '/goaway',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });
      await withLifecycleTimeout(readBody(stream), 3000, 'protocol-correctness/read-body');

      await waitFor(() => serverSession !== null, 2000);
      const events: string[] = [];
      client.on('goaway', () => { events.push('goaway'); });
      client.on('close', () => { events.push('close'); });

      const activeSession = serverSession as { close: (code?: number) => Promise<void> };
      await withLifecycleTimeout(
        activeSession.close(0),
        5000,
        'protocol-correctness/server-session-close',
      );
      await waitFor(() => events.includes('goaway'), 3000);

      const goawayIdx = events.indexOf('goaway');
      const closeIdx = events.indexOf('close');
      if (closeIdx >= 0) {
        assert.ok(goawayIdx >= 0 && goawayIdx < closeIdx, 'GOAWAY should arrive before close');
      }

      await withLifecycleTimeout(client.close(), 3000, 'protocol-correctness/client-close');
      await withLifecycleTimeout(server.close(), 3000, 'protocol-correctness/server-close');
    } catch (error: unknown) {
      appendLifecycleArtifacts(error, 'protocol-correctness-goaway-before-close');
      throw error;
    } finally {
      endLifecycleCapture();
    }
  });

});
