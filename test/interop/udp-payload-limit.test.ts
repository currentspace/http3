import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createSocket } from 'node:dgram';
import type { Readable } from 'node:stream';
import { createSecureServer, connectAsync, createQuicServer, connectQuicAsync } from '../../lib/index.js';
import { generateMutualTlsTestCerts } from '../support/generate-certs.js';

// Observe real native datagrams without decrypting, filtering, or resizing them.
// A loopback interface permits jumbo packets, so it must not replace a user cap.
for (const protocol of ['h3', 'quic']) {
  for (const cappedSide of ['client', 'server']) {
    test(`${protocol}: explicit ${cappedSide} UDP limit survives native worker startup`, async t => {
      const certs = generateMutualTlsTestCerts();
      const options = { ...certs.server, runtimeMode: 'portable' as const, disableRetry: true,
        ...(cappedSide === 'server' ? { maxUdpPayloadSize: 1200 } : {}) };
      const server = protocol === 'h3'
        ? createSecureServer(options, stream => { stream.respond({ ':status': '200' }); stream.pipe(stream); })
        : createQuicServer(options);
      if (protocol === 'quic') {
        (server as ReturnType<typeof createQuicServer>).on('session', session => {
          session.on('stream', stream => stream.pipe(stream));
        });
      }
      const relay = createSocket('udp4');
      let closeClient: (() => Promise<void>) | undefined;
      let relayBound = false;
      t.after(async () => {
        try { await closeClient?.(); }
        finally { await server.close(); if (relayBound) relay.close(); }
      });
      const listening = once(server, 'listening');
      await server.listen(0, '127.0.0.1');
      await listening;
      const serverPort = server.address()!.port;
      let clientPort: number | undefined;
      const clientSizes: number[] = [], serverSizes: number[] = [];
      relay.on('message', (packet, remote) => {
        if (remote.port === serverPort) {
          serverSizes.push(packet.length);
          if (clientPort) relay.send(packet, clientPort, '127.0.0.1');
        } else {
          clientPort = remote.port;
          clientSizes.push(packet.length);
          relay.send(packet, serverPort, '127.0.0.1');
        }
      });
      relay.bind(0, '127.0.0.1');
      await once(relay, 'listening');
      relayBound = true;
      const clientOptions = { ca: certs.ca.cert, servername: 'localhost', runtimeMode: 'portable' as const,
        ...(cappedSide === 'client' ? { maxUdpPayloadSize: 1200 } : {}) };
      const address = `https://127.0.0.1:${relay.address().port}`;
      const client = protocol === 'h3' ? await connectAsync(address, clientOptions)
        : await connectQuicAsync(address, clientOptions);
      closeClient = async () => { await client.close(); };
      const stream = 'request' in client ? client.request({ ':method': 'POST', ':path': '/', ':authority': 'localhost', ':scheme': 'https' })
        : client.openStream();
      const payload = Buffer.alloc(256 * 1024, 'q');
      const chunks: Buffer[] = [];
      const received = (async () => { for await (const chunk of stream as Readable) chunks.push(chunk as Buffer); })();
      stream.end(payload);
      await received;
      assert.deepEqual(Buffer.concat(chunks), payload);
      assert.ok(clientSizes.length > 10 && serverSizes.length > 10);
      assert.ok(clientSizes.every(size => size <= 1200), `client exceeded cap: ${Math.max(...clientSizes)}`);
      assert.ok(serverSizes.every(size => size <= 1200), `server exceeded cap: ${Math.max(...serverSizes)}`);
    });
  }
}
