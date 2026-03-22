import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { once } from 'node:events';
import { createEventSource, createSecureServer, createSseStream } from '../../lib/index.js';
import type { EventSourceMessage } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';
import {
  appendLifecycleArtifacts,
  beginLifecycleCapture,
  endLifecycleCapture,
  withLifecycleTimeout,
} from '../support/failure-artifacts.js';

async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const started = Date.now();
  while (!condition()) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

describe('EventSource over H3', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('reconnects and sends Last-Event-ID', async () => {
    beginLifecycleCapture();
    try {
      let counter = 0;
      const seenLastIds: string[] = [];
      const server = createSecureServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
      }, (stream, headers) => {
        if (headers[':path'] !== '/events') {
          stream.respond({ ':status': '404' }, { endStream: true });
          return;
        }
        const last = headers['last-event-id'];
        if (typeof last === 'string' && last.length > 0) {
          seenLastIds.push(last);
        }

        const sse = createSseStream(stream);
        counter += 1;
        void sse.send({ id: String(counter), data: `msg-${counter}` }).then(() => {
          sse.close();
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

      const events: EventSourceMessage[] = [];
      const source = createEventSource(`https://127.0.0.1:${port}/events`, {
        rejectUnauthorized: false,
        initialRetryMs: 30,
        maxRetryMs: 250,
      });
      source.on('message', (event: EventSourceMessage) => {
        events.push(event);
      });

      await waitFor(() => events.length >= 2, 5000);
      const sourceClosed = once(source, 'close').then(() => undefined);
      source.close();

      assert.strictEqual(events[0]?.data, 'msg-1');
      assert.strictEqual(events[1]?.data, 'msg-2');
      await waitFor(() => seenLastIds.includes('1'), 2000);

      await withLifecycleTimeout(sourceClosed, 3000, 'eventsource-h3/source-close');
      await withLifecycleTimeout(server.close(), 3000, 'eventsource-h3/server-close');
    } catch (error: unknown) {
      appendLifecycleArtifacts(error, 'eventsource-h3-last-event-id');
      throw error;
    } finally {
      endLifecycleCapture();
    }
  });
});
