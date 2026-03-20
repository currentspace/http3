/**
 * High-connection QUIC diagnostic tests.
 * Targets the failure modes seen at 50+ connections with concurrent streams.
 * Reports per-connection diagnostics instead of hiding errors.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { generateTestCerts } from '../support/generate-certs.js';
import { createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type { QuicServer, QuicServerSession, QuicClientSession } from '../../lib/index.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { echoStream } from '../support/echo-stream.js';

let certs: { key: Buffer; cert: Buffer };

function collectStream(stream: QuicStream, timeoutMs: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('stream timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
    stream.on('error', (err: Error) => { clearTimeout(timer); reject(err); });
  });
}

interface ConnResult {
  connIndex: number;
  connected: boolean;
  connectError?: string;
  streams: Array<{ streamIndex: number; ok: boolean; error?: string }>;
}

async function runHighConnTest(
  serverPort: number,
  connections: number,
  streamsPerConn: number,
  messageSize: number,
  streamTimeoutMs: number,
): Promise<ConnResult[]> {
  const payload = Buffer.alloc(messageSize, 0xcc);
  const results: ConnResult[] = [];

  // Phase 1: Connect all clients
  const clients: Array<{ client: QuicClientSession; index: number }> = [];
  for (let c = 0; c < connections; c++) {
    const result: ConnResult = { connIndex: c, connected: false, streams: [] };
    try {
      const client = await connectQuicAsync(`127.0.0.1:${serverPort}`, {
        rejectUnauthorized: false,
        initialMaxStreamsBidi: 50_000,
      });
      result.connected = true;
      clients.push({ client, index: c });
    } catch (err) {
      result.connectError = err instanceof Error ? err.message : String(err);
    }
    results.push(result);
  }

  // Phase 2: Run streams
  const streamPromises: Promise<void>[] = [];
  for (const { client, index } of clients) {
    const result = results[index];
    for (let s = 0; s < streamsPerConn; s++) {
      const streamIndex = s;
      streamPromises.push(
        (async () => {
          const streamResult = { streamIndex, ok: false, error: undefined as string | undefined };
          try {
            const stream = client.openStream();
            stream.end(payload);
            const echoed = await collectStream(stream, streamTimeoutMs);
            if (echoed.length === payload.length) {
              streamResult.ok = true;
            } else {
              streamResult.error = `length mismatch: expected ${payload.length}, got ${echoed.length}`;
            }
          } catch (err) {
            streamResult.error = err instanceof Error ? err.message : String(err);
          }
          result.streams.push(streamResult);
        })(),
      );
    }
  }

  await Promise.all(streamPromises);

  // Phase 3: Close
  await Promise.all(clients.map(({ client }) => client.close()));

  return results;
}

function printDiagnostics(label: string, results: ConnResult[]): void {
  const totalConns = results.length;
  const connectedCount = results.filter((r) => r.connected).length;
  const totalStreams = results.reduce((sum, r) => sum + r.streams.length, 0);
  const okStreams = results.reduce((sum, r) => sum + r.streams.filter((s) => s.ok).length, 0);
  const failedStreams = totalStreams - okStreams;

  console.log(`\n  -- ${label} --`);
  console.log(`  Connections: ${connectedCount}/${totalConns} established`);
  console.log(`  Streams:     ${okStreams}/${totalStreams} succeeded, ${failedStreams} failed`);

  // Categorize errors
  const errorCategories: Record<string, number> = {};
  for (const r of results) {
    if (r.connectError) {
      const cat = categorizeError(r.connectError);
      errorCategories[cat] = (errorCategories[cat] ?? 0) + 1;
    }
    for (const s of r.streams) {
      if (s.error) {
        const cat = categorizeError(s.error);
        errorCategories[cat] = (errorCategories[cat] ?? 0) + 1;
      }
    }
  }

  if (Object.keys(errorCategories).length > 0) {
    console.log('  Error breakdown:');
    for (const [cat, count] of Object.entries(errorCategories)) {
      console.log(`    ${cat}: ${count}`);
    }
  }

  // Show which connections failed
  const failedConns = results.filter(
    (r) => !r.connected || r.streams.some((s) => !s.ok),
  );
  if (failedConns.length > 0 && failedConns.length <= 10) {
    console.log('  Failed connections:');
    for (const r of failedConns) {
      if (!r.connected) {
        console.log(`    conn[${r.connIndex}]: connect failed — ${r.connectError}`);
      } else {
        const failed = r.streams.filter((s) => !s.ok);
        console.log(`    conn[${r.connIndex}]: ${failed.length} stream(s) failed — ${failed.map((s) => s.error).join(', ')}`);
      }
    }
  } else if (failedConns.length > 10) {
    console.log(`  (${failedConns.length} connections had errors — showing first 5)`);
    for (const r of failedConns.slice(0, 5)) {
      if (!r.connected) {
        console.log(`    conn[${r.connIndex}]: connect failed — ${r.connectError}`);
      } else {
        const failed = r.streams.filter((s) => !s.ok);
        console.log(`    conn[${r.connIndex}]: ${failed.length} stream(s) failed`);
      }
    }
  }
}

function categorizeError(msg: string): string {
  const lower = msg.toLowerCase();
  if (lower.includes('timed out') || lower.includes('timeout')) return 'timeout';
  if (lower.includes('reset')) return 'reset';
  if (lower.includes('refused') || lower.includes('econnrefused')) return 'refused';
  if (lower.includes('length mismatch')) return 'length-mismatch';
  return 'other';
}

describe('QUIC high-connection tests', () => {
  let server: QuicServer;
  let serverPort: number;

  before(async () => {
    certs = generateTestCerts();

    server = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      maxConnections: 1000,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        echoStream(stream);
      });
    });

    const addr = await server.listen(0, '127.0.0.1');
    serverPort = addr.port;
  });

  it('50 connections x 5 streams x 4KB', async () => {
    const results = await runHighConnTest(serverPort, 50, 5, 4096, 30_000);
    printDiagnostics('50x5x4KB', results);

    const totalExpected = 50 * 5;
    const okStreams = results.reduce((sum, r) => sum + r.streams.filter((s) => s.ok).length, 0);
    const errors = totalExpected - okStreams;
    const errorRate = errors / totalExpected;

    assert.ok(
      errorRate <= 0.05,
      `error rate ${(errorRate * 100).toFixed(1)}% exceeds 5% (${errors}/${totalExpected} failed)`,
    );
  });

  it('100 connections x 1 stream x 1KB (connection establishment)', async () => {
    const results = await runHighConnTest(serverPort, 100, 1, 1024, 30_000);
    printDiagnostics('100x1x1KB', results);

    const connectedCount = results.filter((r) => r.connected).length;
    const okStreams = results.reduce((sum, r) => sum + r.streams.filter((s) => s.ok).length, 0);
    const totalExpected = 100;
    const errors = totalExpected - okStreams;
    const errorRate = errors / totalExpected;

    console.log(`  Connection success: ${connectedCount}/100`);
    assert.ok(
      errorRate <= 0.05,
      `error rate ${(errorRate * 100).toFixed(1)}% exceeds 5% (${errors}/${totalExpected} failed)`,
    );
  });

  it('cleanup', async () => {
    await server.close();
  });
});
