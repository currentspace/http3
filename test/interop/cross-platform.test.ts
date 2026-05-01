/**
 * Cross-platform interop: macOS host H3 client ↔ Debian H3 server (Docker).
 *
 * Gated on `HTTP3_INTEROP_DOCKER=1`. The host must have the `http3-interop`
 * image available (build with `pnpm run docker:interop:build`).
 *
 * The test stands up the interop server in a detached container with the
 * QUIC port published on the host, polls the published UDP port until the
 * server logs `interop-server listening`, then runs HTTP/3 requests against
 * `localhost:<published-port>` from the host process. macOS uses kqueue,
 * the container uses io_uring (or poll fallback) — so the QUIC handshake
 * truly crosses driver boundaries.
 */

import { describe, it, before, after } from 'node:test';
import assert from 'node:assert';
import { execSync, spawn } from 'node:child_process';
import { connectAsync } from '../../lib/index.js';
import type { Http3ClientSession } from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';

const ENABLED = process.env.HTTP3_INTEROP_DOCKER === '1';
const HOST_PORT = Number.parseInt(process.env.HTTP3_INTEROP_HOST_PORT ?? '14433', 10);
const CONTAINER_NAME = `http3-interop-${process.pid}`;
const IMAGE = 'http3-interop';
const READY_TIMEOUT_MS = 30_000;

interface Response { status: string; body: Buffer; }

async function doRequest(
  session: Http3ClientSession,
  method: string,
  path: string,
  body?: Buffer,
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
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error(`${method} ${path} timed out`)), 15_000);
    stream.on('response', (h: Record<string, string>) => { status = h[':status'] ?? ''; });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve({ status, body: Buffer.concat(chunks) });
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
    // Remove any leftover container from a prior run.
    try { execSync(`docker rm -f ${CONTAINER_NAME}`, { stdio: 'ignore' }); } catch { /* not running */ }
    execSync(
      `docker run -d --rm --name ${CONTAINER_NAME} -p ${HOST_PORT}:4433/udp ${IMAGE}`,
      { stdio: 'pipe' },
    );
    logProc = spawn('docker', ['logs', '-f', CONTAINER_NAME]);
    await waitForLog(logProc, 'interop-server listening', READY_TIMEOUT_MS);
  });

  after(() => {
    if (logProc) {
      logProc.kill('SIGKILL');
      logProc = null;
    }
    try { execSync(`docker rm -f ${CONTAINER_NAME}`, { stdio: 'ignore' }); } catch { /* already gone */ }
    setTimeout(() => process.exit(0), 500).unref();
  });

  it('GET / over QUIC handshake (macOS client → Debian server)', async () => {
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
});
