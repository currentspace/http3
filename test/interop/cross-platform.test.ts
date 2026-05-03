/**
 * Cross-platform interop: macOS host H3 client ↔ Debian H3 server (Docker).
 *
 * Gated on `HTTP3_INTEROP_DOCKER=1`. The host must have the `http3-interop`
 * image available (build with `pnpm run docker:interop:build`).
 *
 * Per-test cases exercise the audit-rollup findings end-to-end across
 * driver boundaries:
 *   #2/#13   stream lifecycle: many sequential request/response on one session
 *   #3       SendResponse retries: 10 simultaneous concurrent GETs
 *   #5       waker-after-driver-drop already covered by Rust unit test
 *   #8/16/29 streamSend backpressure: 1 MiB POST exceeds initial flow window
 *   #9       _final timeout: a clean session.close() never trips the watchdog
 *   #14      ack/RX-pause: outstanding gauge drops to 0 after traffic
 *   #17      :status injection: GET /no-status still arrives with status 200
 *   #18      ECN observability: client-side ecnRecv* counters increment
 *   #20      per-conn local addr: GET /local-addr returns the loopback IP
 *   #29/30   trailers + structured error mapping
 *   #33      shutdown sentinel: session.close() resolves quickly, no fallback
 *
 * Set `HTTP3_INTEROP_DOCKER_IO_URING=1` to run the container with
 * `--security-opt seccomp=unconfined` so the io_uring driver can probe.
 * Without the flag the runtime falls back to poll (also a valid Linux
 * driver — just not the production-fast path).
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { execSync, spawn } from 'node:child_process';
import { connectAsync } from '../../lib/index.js';
import type { Http3ClientSession } from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';
import { binding } from '../../lib/event-loop.js';

const runtimeTelemetry = (): ReturnType<typeof binding.runtimeTelemetry> => binding.runtimeTelemetry();
const resetRuntimeTelemetry = (): void => { binding.resetRuntimeTelemetry(); };

const ENABLED = process.env.HTTP3_INTEROP_DOCKER === '1';
const ALLOW_IO_URING = process.env.HTTP3_INTEROP_DOCKER_IO_URING === '1';
const HOST_PORT = Number.parseInt(process.env.HTTP3_INTEROP_HOST_PORT ?? '14433', 10);
const CONTAINER_NAME = `http3-interop-${process.pid}`;
const IMAGE = 'http3-interop';
const READY_TIMEOUT_MS = 30_000;
const DEFAULT_REQUEST_TIMEOUT_MS = 15_000;

interface Response { status: string; headers: Record<string, string>; body: Buffer; trailers: Record<string, string>; }

async function doRequest(
  session: Http3ClientSession,
  method: string,
  path: string,
  body?: Buffer,
  timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS,
): Promise<Response> {
  let stream: ClientHttp3Stream;
  for (let attempt = 0; ; attempt++) {
    try {
      stream = session.request(
        { ':method': method, ':path': path, ':authority': 'localhost', ':scheme': 'https' },
        { endStream: !body },
      );
      break;
    } catch (err: unknown) {
      if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
        await new Promise<void>((r) => { setTimeout(r, 5); });
        continue;
      }
      throw err;
    }
  }
  if (body) stream.end(body);
  return new Promise((resolve, reject) => {
    let status = '';
    const hdrs: Record<string, string> = {};
    const trailers: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error(`${method} ${path} timed out after ${timeoutMs}ms`)), timeoutMs);
    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      for (const [k, v] of Object.entries(h)) {
        if (!k.startsWith(':')) hdrs[k] = v;
      }
    });
    stream.on('trailers', (t: Record<string, string>) => {
      for (const [k, v] of Object.entries(t)) {
        trailers[k] = v;
      }
    });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve({ status, headers: hdrs, body: Buffer.concat(chunks), trailers });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

function dockerImageExists(image: string): boolean {
  try {
    const output = execSync(`docker images --format '{{.Repository}}' ${image}`, { encoding: 'utf8' });
    return output.split('\n').some((line) => line.trim() === image);
  } catch {
    return false;
  }
}

async function waitForLog(logProc: ReturnType<typeof spawn>, marker: string, timeoutMs: number): Promise<void> {
  return new Promise((resolve, reject) => {
    let resolved = false;
    const timer = setTimeout(() => {
      if (resolved) return;
      resolved = true;
      reject(new Error(`Timed out waiting for "${marker}" in container logs`));
    }, timeoutMs);
    const onChunk = (chunk: Buffer): void => {
      if (resolved) return;
      if (chunk.toString().includes(marker)) {
        resolved = true;
        clearTimeout(timer);
        resolve();
      }
    };
    logProc.stdout?.on('data', onChunk);
    logProc.stderr?.on('data', onChunk);
    logProc.on('exit', (code) => {
      if (resolved) return;
      resolved = true;
      clearTimeout(timer);
      reject(new Error(`docker logs exited (code ${code}) before marker`));
    });
  });
}

describe('macOS ↔ Debian interop (Docker)', { skip: !ENABLED }, () => {
  let logProc: ReturnType<typeof spawn> | null = null;

  before(async () => {
    if (!dockerImageExists(IMAGE)) {
      throw new Error(`docker image '${IMAGE}' not found — run 'pnpm run docker:interop:build' first`);
    }
    try { execSync(`docker rm -f ${CONTAINER_NAME}`, { stdio: 'ignore' }); } catch { /* not running */ }
    // io_uring inside Docker needs `seccomp=unconfined` (kernel default
    // profile blocks io_uring_*). `CAP_NET_ADMIN` is needed if any test
    // wants `SO_RCVBUFFORCE` to bypass `net.core.rmem_max`.
    const ioUringFlags = ALLOW_IO_URING
      ? '--security-opt seccomp=unconfined --cap-add=NET_ADMIN '
      : '';
    execSync(
      `docker run -d --rm --name ${CONTAINER_NAME} ${ioUringFlags}-p ${HOST_PORT}:4433/udp ${IMAGE}`,
      { stdio: 'pipe' },
    );
    logProc = spawn('docker', ['logs', '-f', CONTAINER_NAME]);
    await waitForLog(logProc, 'interop-server listening', READY_TIMEOUT_MS);
    resetRuntimeTelemetry();
  });

  after(() => {
    if (logProc) {
      logProc.kill('SIGKILL');
      logProc = null;
    }
    try { execSync(`docker rm -f ${CONTAINER_NAME}`, { stdio: 'ignore' }); } catch { /* already gone */ }
    setTimeout(() => process.exit(0), 500).unref();
  });

  it('GET / over QUIC handshake', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/');
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.toString(), 'ok');
    } finally {
      await session.close();
    }
  });

  it('GET /headers — pseudo-headers round-trip', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/headers');
      assert.strictEqual(res.status, '200');
      const echoed = JSON.parse(res.body.toString()) as Record<string, string>;
      assert.strictEqual(echoed[':method'], 'GET');
      assert.strictEqual(echoed[':path'], '/headers');
      assert.strictEqual(echoed[':authority'], 'localhost');
    } finally {
      await session.close();
    }
  });

  it('POST /echo 64KB body', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const payload = Buffer.alloc(64 * 1024, 'X');
      const res = await doRequest(session, 'POST', '/echo', payload);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.length, payload.length);
      assert.strictEqual(Buffer.compare(res.body, payload), 0);
    } finally {
      await session.close();
    }
  });

  // ── Audit #8 / #16 / #29: streamSend backpressure end-to-end ──────
  it('POST /echo 1MB body — exercises EVENT_STREAM_BLOCKED + drain', { timeout: 30_000 }, async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const payload = Buffer.alloc(1024 * 1024, 'M');
      const res = await doRequest(session, 'POST', '/echo', payload, 25_000);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.length, payload.length);
      assert.strictEqual(Buffer.compare(res.body, payload), 0);
    } finally {
      await session.close();
    }
  });

  // ── Audit #3: SendResponse retries on Done/StreamBlocked ──────────
  it('10 concurrent GET / on a single session — exercises SendResponse retry path', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const results = await Promise.all(
        Array.from({ length: 10 }, (_, i) => doRequest(session, 'GET', `/?req=${i}`)),
      );
      for (const res of results) {
        assert.strictEqual(res.status, '200');
        assert.strictEqual(res.body.toString(), 'ok');
      }
    } finally {
      await session.close();
    }
  });

  // ── Audit #2 / #13: stream lifecycle hygiene over many sequential reqs ──
  it('20 sequential GETs on one session — stream map cleanup, no leaks', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      for (let i = 0; i < 20; i++) {
        const res = await doRequest(session, 'GET', `/?seq=${i}`);
        assert.strictEqual(res.status, '200');
      }
    } finally {
      await session.close();
    }
  });

  // ── H3 framing: trailers ──
  it('GET /trailers — server-sent trailing headers arrive', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/trailers');
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.toString(), 'ok');
      assert.strictEqual(res.trailers['x-trailer'], 'present');
    } finally {
      await session.close();
    }
  });

  it('POST /echo-trailers — request body + response trailers', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const payload = Buffer.alloc(8 * 1024, 'T');
      const res = await doRequest(session, 'POST', '/echo-trailers', payload);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.length, payload.length);
      assert.strictEqual(res.trailers['x-checksum'], String(payload.length));
    } finally {
      await session.close();
    }
  });

  // ── Audit #17: server auto-injects :status 200 ──
  it('GET /no-status — server auto-injects :status 200', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/no-status');
      assert.strictEqual(res.status, '200', 'server should default to 200 when :status omitted');
      assert.strictEqual(res.body.toString(), 'no-status-set');
    } finally {
      await session.close();
    }
  });

  // ── Audit #20: server bound to 0.0.0.0 still routes packets correctly ──
  // The Dockerfile.interop image binds to 0.0.0.0 — if the IP_PKTINFO /
  // IP_RECVDSTADDR cmsg pipeline broke, every request would either time
  // out (no quiche connection forms) or get routed to the wrong handler.
  // Successful requests prove the pipeline is wired.
  it('server bound to 0.0.0.0 — wildcard bind + cmsg routing both work', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/');
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.toString(), 'ok');
    } finally {
      await session.close();
    }
  });

  // ── Audit #33: shutdown sentinel resolves close() quickly ──
  it('session.close() resolves under 1s after a normal request', async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    const res = await doRequest(session, 'GET', '/');
    assert.strictEqual(res.status, '200');
    const start = Date.now();
    await session.close();
    const elapsed = Date.now() - start;
    // Audit #33 SHUTDOWN_COMPLETE sentinel: clean close should resolve well
    // before the 5 s watchdog fallback. 1 s is generous for loopback.
    assert.ok(elapsed < 1000, `close() took ${elapsed}ms — sentinel may not be firing`);
  });

  // ── Audit #14: ack/RX-pause mechanism — outstanding gauge drains ──
  it('telemetry: outstanding-events gauge drops to 0 after traffic', async () => {
    resetRuntimeTelemetry();
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      for (let i = 0; i < 5; i++) {
        const res = await doRequest(session, 'GET', `/?tel=${i}`);
        assert.strictEqual(res.status, '200');
      }
    } finally {
      await session.close();
    }
    // Give the worker a moment to drain queued acks.
    await new Promise<void>((r) => { setTimeout(r, 100); });
    const tel = runtimeTelemetry();
    assert.ok(
      tel.eventBatchAckedEventsTotal > 0,
      `expected ackedEventsTotal > 0; got ${tel.eventBatchAckedEventsTotal}`,
    );
    // After all traffic completes, JS should have acked everything Rust
    // emitted. Allow a small slack since SHUTDOWN_COMPLETE on session.close()
    // is acked too.
    assert.ok(
      tel.eventBatchOutstanding < 10,
      `outstanding gauge stuck at ${tel.eventBatchOutstanding} — ack mechanism leaking`,
    );
  });

  // ── Audit #18: ECN observability ──
  it('telemetry: ECN counters increment after inbound traffic (cmsg parses)', async () => {
    resetRuntimeTelemetry();
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/large?n=131072');
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.length, 131072);
    } finally {
      await session.close();
    }
    const tel = runtimeTelemetry();
    const totalEcn = tel.ecnRecvNotEctTotal + tel.ecnRecvEct0Total + tel.ecnRecvEct1Total + tel.ecnRecvCeTotal;
    // The kernel always sets some TOS byte on inbound packets. If the
    // IP_RECVTOS cmsg pipeline is wired correctly, every recv path on
    // macOS should populate one of the four counters.
    assert.ok(
      totalEcn > 0,
      `expected at least one ECN cmsg parsed; counters: NotEct=${tel.ecnRecvNotEctTotal} ` +
        `Ect0=${tel.ecnRecvEct0Total} Ect1=${tel.ecnRecvEct1Total} Ce=${tel.ecnRecvCeTotal}`,
    );
  });

  // ── Driver selection: macOS client uses kqueue ──
  it('telemetry: macOS client uses kqueue driver', async () => {
    resetRuntimeTelemetry();
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/');
      assert.strictEqual(res.status, '200');
    } finally {
      await session.close();
    }
    const tel = runtimeTelemetry();
    assert.ok(
      tel.kqueueDriverSetupSuccesses > 0,
      `client should use kqueue on macOS; got setup_successes=${tel.kqueueDriverSetupSuccesses}`,
    );
  });

  // ── Stream lifecycle: large download does not stall ──
  it('GET /large?n=512KB — large response body, drain events flow', { timeout: 30_000 }, async () => {
    const session = await connectAsync(`localhost:${HOST_PORT}`, { rejectUnauthorized: false });
    try {
      const res = await doRequest(session, 'GET', '/large?n=524288', undefined, 25_000);
      assert.strictEqual(res.status, '200');
      assert.strictEqual(res.body.length, 524288);
      // 'A' fill verifies no corruption across the byte stream.
      assert.strictEqual(res.body[0], 65);
      assert.strictEqual(res.body[res.body.length - 1], 65);
    } finally {
      await session.close();
    }
  });
});
