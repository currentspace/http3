import assert from 'node:assert';
import { describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
  connectQuicAsync,
  createQuicServer,
} from '../../lib/index.js';
import { binding } from '../../lib/event-loop.js';
import type { QuicClientSession, QuicServer, QuicServerSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { generateTestCerts } from '../support/generate-certs.js';
import { echoStream } from '../support/echo-stream.js';
import {
  appendLifecycleArtifacts,
  beginLifecycleCapture,
  captureLifecycleFailureArtifacts,
  endLifecycleCapture,
} from '../support/failure-artifacts.js';

function isFastPathUnavailable(error: unknown): boolean {
  return error instanceof Http3Error && error.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE;
}

function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
    stream.on('error', (error: Error) => { clearTimeout(timer); reject(error); });
  });
}

describe('QUIC client worker topology', () => {
  it('concurrent fast-mode sessions exchange data correctly', { timeout: 20_000 }, async (t) => {
    const certs = generateTestCerts();
    const payload = Buffer.from('quic-worker-test');
    let server: QuicServer | null = null;
    let clients: QuicClientSession[] = [];

    try {
      beginLifecycleCapture();
      server = createQuicServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'fast',
        fallbackPolicy: 'error',
      });

      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });

      const addr = await server.listen(0, '127.0.0.1');
      clients = await Promise.all(Array.from({ length: 4 }, () => connectQuicAsync(
        `127.0.0.1:${addr.port}`,
        { rejectUnauthorized: false, runtimeMode: 'fast', fallbackPolicy: 'error' },
      )));

      for (const client of clients) {
        assert.strictEqual(client.runtimeInfo?.selectedMode, 'fast');
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collectStream(stream, 5_000);
        assert.deepStrictEqual(echoed, payload);
      }

      const telemetry = binding.runtimeTelemetry();
      assert.strictEqual(telemetry.rawQuicClientSessionsOpened, clients.length);

      await Promise.all(clients.map((client) => client.close()));
      clients = [];
      await new Promise<void>((resolve) => { setTimeout(resolve, 50); });
      const closedTelemetry = binding.runtimeTelemetry();
      assert.ok(closedTelemetry.rawQuicClientSessionsClosed >= 1);
      assert.ok(closedTelemetry.workerThreadStopsTotal >= 1);
      assert.ok(
        closedTelemetry.workerLoopExitByCommandTotal + closedTelemetry.workerLoopExitByHandlerDoneTotal >= 1,
      );
      assert.ok(closedTelemetry.shutdownCompleteEmittedTotal >= 1);
      assert.ok(closedTelemetry.eventBatchFlushesTotal >= 1);
      assert.ok(closedTelemetry.eventBatchAttemptedEventsTotal >= 1);
      assert.ok(closedTelemetry.eventBatchDeliveredEventsTotal >= 1);
      assert.strictEqual(closedTelemetry.eventBatchDroppedEventsTotal, 0);
      assert.strictEqual(closedTelemetry.eventBatchSinkErrorsTotal, 0);
      const artifacts = captureLifecycleFailureArtifacts('quic-fast-worker-close');
      assert.ok(artifacts.lifecycleTrace.eventCount >= 1);
      assert.ok(
        artifacts.lifecycleTrace.events.some((event) => event.action === 'worker-loop-start'),
        'lifecycle trace should include worker-loop-start',
      );
      assert.ok(
        artifacts.lifecycleTrace.events.some((event) => event.action === 'shutdown-complete-emitted'),
        'lifecycle trace should include shutdown-complete-emitted',
      );
    } catch (error: unknown) {
      if (isFastPathUnavailable(error)) {
        const message = error instanceof Error ? error.message : String(error);
        t.skip(`fast path unavailable on this host: ${message}`);
        return;
      }
      appendLifecycleArtifacts(error, 'quic-fast-worker-topology');
      throw error;
    } finally {
      endLifecycleCapture();
      await Promise.all(clients.map(async (client) => {
        try { await client.close(); } catch { /* cleanup */ }
      }));
      if (server) { await server.close(); }
    }
  });

  it('concurrent portable-mode sessions exchange data correctly', { timeout: 20_000 }, async () => {
    const certs = generateTestCerts();
    const payload = Buffer.from('portable-quic-test');
    let server: QuicServer | null = null;
    let clients: QuicClientSession[] = [];

    try {
      beginLifecycleCapture();
      server = createQuicServer({
        key: certs.key,
        cert: certs.cert,
        disableRetry: true,
        runtimeMode: 'portable',
      });
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => { echoStream(stream); });
      });

      const addr = await server.listen(0, '127.0.0.1');
      clients = await Promise.all(Array.from({ length: 3 }, () => connectQuicAsync(
        `127.0.0.1:${addr.port}`,
        { rejectUnauthorized: false, runtimeMode: 'portable' },
      )));

      for (const client of clients) {
        const stream = client.openStream();
        stream.end(payload);
        const echoed = await collectStream(stream, 5_000);
        assert.deepStrictEqual(echoed, payload);
      }

      const telemetry = binding.runtimeTelemetry();
      assert.strictEqual(telemetry.rawQuicClientSessionsOpened, clients.length);
      assert.ok(telemetry.eventBatchFlushesTotal >= 1);
      assert.ok(telemetry.eventBatchAttemptedEventsTotal >= 1);
      assert.strictEqual(telemetry.eventBatchDroppedEventsTotal, 0);
      assert.strictEqual(telemetry.eventBatchSinkErrorsTotal, 0);
      const artifacts = captureLifecycleFailureArtifacts('quic-portable-worker-topology');
      assert.ok(artifacts.lifecycleTrace.eventCount >= 1);
    } finally {
      endLifecycleCapture();
      await Promise.all(clients.map(async (client) => {
        try { await client.close(); } catch { /* cleanup */ }
      }));
      if (server) { await server.close(); }
    }
  });
});
