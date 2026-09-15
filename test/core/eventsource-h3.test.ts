import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
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

  for (const responseDelayMs of [0, 80]) {
    it(`reconnects and sends Last-Event-ID (response delay ${responseDelayMs}ms)`, async (t) => {
      beginLifecycleCapture();
      let server: ReturnType<typeof createSecureServer> | undefined;
      let source: ReturnType<typeof createEventSource> | undefined;
      t.after(async () => {
        try {
          if (source) {
            const closed = once(source, 'close').then(() => undefined);
            source.close();
            await withLifecycleTimeout(closed, 3000, 'eventsource-h3/source-close');
          }
        } finally {
          try {
            if (server) await withLifecycleTimeout(server.close(), 3000, 'eventsource-h3/server-close');
          } finally { endLifecycleCapture(); }
        }
      });
      try {
        let counter = 0;
        const seenLastIds: string[] = [];
        server = createSecureServer({
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
          const id = ++counter;
          const send = async (): Promise<void> => {
            if (responseDelayMs) await delay(responseDelayMs);
            await sse.send({ id: String(id), data: `msg-${id}` });
          };
          send().then(
            () => { sse.close(); },
            (err: unknown) => {
              stream.destroy(err instanceof Error ? err : new Error(String(err)));
            },
          );
        });

        const listening = once(server, 'listening');
        server.listen(0, '127.0.0.1');
        await listening;
        const address = server.address();
        assert.ok(address);
        const port = address.port;

        const events: EventSourceMessage[] = [];
        source = createEventSource(`https://127.0.0.1:${port}/events`, {
          rejectUnauthorized: false,
          initialRetryMs: 30,
          maxRetryMs: 250,
        });
        source.on('message', (event: EventSourceMessage) => {
          events.push(event);
        });
        const errors: Error[] = [];
        source.on('error', (error) => { errors.push(error); });

        await waitFor(() => errors.length > 0 || events.length >= 2, 5000);
        if (errors[0]) throw errors[0];

        assert.strictEqual(events[0]?.data, 'msg-1');
        assert.strictEqual(events[1]?.data, 'msg-2');
        await waitFor(() => seenLastIds.includes('1'), 2000);
      } catch (error: unknown) {
        appendLifecycleArtifacts(error, 'eventsource-h3-last-event-id');
        throw error;
      }
    });
  }
});
