/**
 * Shared scenario helpers for E2E tests and longhaul mixes.
 *
 * Provides a pre-configured HTTP/3 server with a realistic route table,
 * client connection helpers, request utilities, and statistical tools.
 */

import { createHash } from 'node:crypto';
import { createSecureServer, connectAsync } from '../../lib/index.js';
import type {
  Http3SecureServer,
  Http3ClientSession,
  ServerHttp3Stream,
  IncomingHeaders,
  StreamFlags,
} from '../../lib/index.js';
import type { ClientHttp3Stream } from '../../lib/stream.js';
import { createSseStream } from '../../lib/sse.js';
import { generateTestCerts } from './generate-certs.js';

// ---------------------------------------------------------------------------
// Data generators
// ---------------------------------------------------------------------------

export interface FakeUser {
  id: number;
  name: string;
  email: string;
}

const FIRST_NAMES = [
  'Alice', 'Bob', 'Carol', 'Dave', 'Eve', 'Frank', 'Grace', 'Hank',
  'Ivy', 'Jack', 'Karen', 'Leo', 'Mona', 'Nick', 'Olivia', 'Pat',
  'Quinn', 'Rose', 'Sam', 'Tina',
];

const LAST_NAMES = [
  'Smith', 'Jones', 'Brown', 'Davis', 'Miller', 'Wilson', 'Moore',
  'Taylor', 'Anderson', 'Thomas', 'Jackson', 'White', 'Harris',
  'Martin', 'Garcia', 'Clark', 'Lewis', 'Lee', 'Walker', 'Hall',
];

export function generateUsers(n: number): FakeUser[] {
  const users: FakeUser[] = [];
  for (let i = 0; i < n; i++) {
    const first = FIRST_NAMES[i % FIRST_NAMES.length]!;
    const last = LAST_NAMES[i % LAST_NAMES.length]!;
    users.push({
      id: i + 1,
      name: `${first} ${last}`,
      email: `${first.toLowerCase()}.${last.toLowerCase()}${i}@example.com`,
    });
  }
  return users;
}

// ---------------------------------------------------------------------------
// Pre-computed route data
// ---------------------------------------------------------------------------

const USERS = generateUsers(50);

const SMALL_TXT = Buffer.alloc(1024, 'A');
const LARGE_BIN = Buffer.alloc(1048576, 0xaa);
const HUGE_BIN = Buffer.alloc(4 * 1024 * 1024, 0xbb);

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

export interface ScenarioServerHandle {
  server: Http3SecureServer;
  port: number;
  certs: { key: Buffer; cert: Buffer };
}

function collectBody(stream: ServerHttp3Stream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => { resolve(Buffer.concat(chunks)); });
    stream.on('error', reject);
  });
}

function sendJson(stream: ServerHttp3Stream, status: string, data: unknown): void {
  const body = JSON.stringify(data);
  stream.respond({
    ':status': status,
    'content-type': 'application/json',
    'content-length': String(Buffer.byteLength(body)),
  });
  stream.end(body);
}

function routeHandler(stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags): void {
  const method = (headers[':method'] as string) ?? 'GET';
  const path = (headers[':path'] as string) ?? '/';

  // ---- Health ---------------------------------------------------------
  if (method === 'GET' && path === '/health') {
    sendJson(stream, '200', { status: 'ok' });
    return;
  }

  // ---- Users API ------------------------------------------------------
  if (method === 'GET' && path === '/api/users') {
    sendJson(stream, '200', USERS);
    return;
  }

  const userMatch = path.match(/^\/api\/users\/(\d+)$/);
  if (method === 'GET' && userMatch) {
    const id = parseInt(userMatch[1]!, 10);
    const user = USERS.find((u) => u.id === id);
    if (user) {
      sendJson(stream, '200', user);
    } else {
      sendJson(stream, '404', { error: 'not found' });
    }
    return;
  }

  if (method === 'POST' && path === '/api/users') {
    if (flags.endStream) {
      sendJson(stream, '400', { error: 'empty body' });
      return;
    }
    collectBody(stream).then((buf) => {
      try {
        const parsed = JSON.parse(buf.toString()) as Record<string, unknown>;
        parsed.id = Date.now();
        sendJson(stream, '201', parsed);
      } catch {
        sendJson(stream, '400', { error: 'invalid json' });
      }
    }).catch(() => {
      sendJson(stream, '500', { error: 'read error' });
    });
    return;
  }

  // ---- Upload ---------------------------------------------------------
  if (method === 'POST' && path === '/api/upload') {
    if (flags.endStream) {
      sendJson(stream, '200', { bytes: 0, sha256: createHash('sha256').update('').digest('hex') });
      return;
    }
    collectBody(stream).then((buf) => {
      const sha256 = createHash('sha256').update(buf).digest('hex');
      sendJson(stream, '200', { bytes: buf.length, sha256 });
    }).catch(() => {
      sendJson(stream, '500', { error: 'read error' });
    });
    return;
  }

  // ---- Echo -----------------------------------------------------------
  if (method === 'POST' && path === '/api/echo') {
    if (flags.endStream) {
      stream.respond({ ':status': '200' });
      stream.end();
      return;
    }
    stream.respond({ ':status': '200' });
    stream.on('data', (chunk: Buffer) => { stream.write(chunk); });
    stream.on('end', () => { stream.end(); });
    return;
  }

  // ---- Static files ---------------------------------------------------
  if (method === 'GET' && path === '/files/small.txt') {
    stream.respond({
      ':status': '200',
      'content-type': 'text/plain',
      'content-length': String(SMALL_TXT.length),
    });
    stream.end(SMALL_TXT);
    return;
  }

  if (method === 'GET' && path === '/files/large.bin') {
    stream.respond({
      ':status': '200',
      'content-type': 'application/octet-stream',
      'content-length': '1048576',
    });
    stream.end(LARGE_BIN);
    return;
  }

  if (method === 'GET' && path === '/files/huge.bin') {
    stream.respond({
      ':status': '200',
      'content-type': 'application/octet-stream',
      'content-length': String(HUGE_BIN.length),
    });
    stream.end(HUGE_BIN);
    return;
  }

  // ---- HEAD -----------------------------------------------------------
  if (method === 'HEAD' && path === '/head-check') {
    stream.respond({
      ':status': '200',
      'content-length': '1024',
    }, { endStream: true });
    return;
  }

  // ---- DELETE no-content ----------------------------------------------
  if (method === 'DELETE' && path === '/no-content') {
    stream.respond({ ':status': '204' }, { endStream: true });
    return;
  }

  // ---- Redirect -------------------------------------------------------
  if (method === 'GET' && path === '/redirect') {
    stream.respond({
      ':status': '302',
      'location': '/api/users',
    }, { endStream: true });
    return;
  }

  // ---- Slow streaming -------------------------------------------------
  if (method === 'GET' && path === '/slow') {
    stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
    const chunk = Buffer.alloc(100, 0x42);
    let count = 0;
    const interval = setInterval(() => {
      stream.write(chunk);
      count++;
      if (count >= 20) {
        clearInterval(interval);
        stream.end();
      }
    }, 50);
    return;
  }

  // ---- SSE ------------------------------------------------------------
  if (method === 'GET' && path === '/sse/events') {
    const sse = createSseStream(stream);
    let i = 0;
    const interval = setInterval(() => {
      void sse.send({ event: 'update', data: `event-${i}` });
      i++;
      if (i >= 5) {
        clearInterval(interval);
        sse.close();
      }
    }, 200);
    return;
  }

  // ---- Headers echo ---------------------------------------------------
  if (method === 'GET' && path === '/headers/echo') {
    const echoHeaders: Record<string, string> = {};
    for (const [k, v] of Object.entries(headers)) {
      if (!k.startsWith(':')) {
        echoHeaders[k] = v as string;
      }
    }
    sendJson(stream, '200', echoHeaders);
    return;
  }

  // ---- Error routes ---------------------------------------------------
  if (method === 'GET' && path === '/error/500') {
    sendJson(stream, '500', { error: 'internal server error' });
    return;
  }

  if (method === 'GET' && path === '/error/stream-reset') {
    stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
    stream.write(Buffer.alloc(64, 0xcc));
    // Destroy after a short delay to ensure partial write is sent
    setTimeout(() => { stream.destroy(); }, 50);
    return;
  }

  // ---- Trailers -------------------------------------------------------
  if (method === 'GET' && path === '/trailers') {
    stream.respond({ ':status': '200' });
    stream.write('trailer-body');
    stream.sendTrailers({ 'x-checksum': 'abc123' });
    stream.end();
    return;
  }

  // ---- Concurrent delay -----------------------------------------------
  if (method === 'GET' && path === '/concurrent') {
    setTimeout(() => {
      sendJson(stream, '200', { ok: true });
    }, 100);
    return;
  }

  // ---- Default 404 ----------------------------------------------------
  sendJson(stream, '404', { error: 'not found' });
}

export async function startScenarioServer(): Promise<ScenarioServerHandle> {
  const certs = generateTestCerts();

  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
  }, routeHandler);

  const port = await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      resolve(addr!.port);
    });
    server.listen(0, '127.0.0.1');
  });

  return { server, port, certs };
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export async function connectScenarioClient(port: number): Promise<Http3ClientSession> {
  return connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false });
}

// ---------------------------------------------------------------------------
// Request helper
// ---------------------------------------------------------------------------

export interface ScenarioResponse {
  status: string;
  headers: Record<string, string>;
  body: Buffer;
  latencyMs: number;
}

export async function doRequest(
  session: Http3ClientSession,
  method: string,
  path: string,
  body?: Buffer | string,
  extraHeaders?: Record<string, string>,
): Promise<ScenarioResponse> {
  const hasBody = body != null && (typeof body === 'string' ? body.length > 0 : body.length > 0);
  const t0 = performance.now();

  // Retry on StreamBlocked
  let stream: ClientHttp3Stream;
  for (let attempt = 0; ; attempt++) {
    try {
      stream = session.request({
        ':method': method,
        ':path': path,
        ':authority': 'localhost',
        ':scheme': 'https',
        ...extraHeaders,
      }, { endStream: !hasBody });
      break;
    } catch (err: unknown) {
      if (attempt < 50 && err instanceof Error && err.message.includes('StreamBlocked')) {
        await new Promise<void>((r) => { setTimeout(r, 5); });
        continue;
      }
      throw err;
    }
  }

  if (hasBody) {
    const buf = typeof body === 'string' ? Buffer.from(body) : body!;
    stream.end(buf);
  }

  return new Promise((resolve, reject) => {
    let status = '';
    const hdrs: Record<string, string> = {};
    const chunks: Buffer[] = [];
    const timeout = setTimeout(() => reject(new Error(`doRequest ${method} ${path} timed out`)), 30000);

    stream.on('response', (h: Record<string, string>) => {
      status = h[':status'] ?? '';
      for (const [k, v] of Object.entries(h)) {
        hdrs[k] = v;
      }
    });
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('end', () => {
      clearTimeout(timeout);
      resolve({
        status,
        headers: hdrs,
        body: Buffer.concat(chunks),
        latencyMs: performance.now() - t0,
      });
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timeout);
      reject(err);
    });
  });
}

// ---------------------------------------------------------------------------
// LatencyTracker
// ---------------------------------------------------------------------------

export class LatencyTracker {
  private values: number[] = [];

  record(ms: number): void {
    this.values.push(ms);
  }

  get count(): number {
    return this.values.length;
  }

  percentile(p: number): number {
    if (this.values.length === 0) return 0;
    const sorted = [...this.values].sort((a, b) => a - b);
    const idx = Math.ceil((p / 100) * sorted.length) - 1;
    return sorted[Math.max(0, idx)]!;
  }

  get p50(): number { return this.percentile(50); }
  get p95(): number { return this.percentile(95); }
  get p99(): number { return this.percentile(99); }

  get mean(): number {
    if (this.values.length === 0) return 0;
    return this.values.reduce((a, b) => a + b, 0) / this.values.length;
  }

  summary(): { count: number; mean: number; p50: number; p95: number; p99: number } {
    return {
      count: this.count,
      mean: Math.round(this.mean * 100) / 100,
      p50: Math.round(this.p50 * 100) / 100,
      p95: Math.round(this.p95 * 100) / 100,
      p99: Math.round(this.p99 * 100) / 100,
    };
  }

  /** Alias for {@link summary} (used by longhaul tests). */
  stats(): { count: number; mean: number; p50: number; p95: number; p99: number } {
    return this.summary();
  }
}

// ---------------------------------------------------------------------------
// Weighted random picker
// ---------------------------------------------------------------------------

export function pickWeighted(distribution: [string, number][]): string {
  const total = distribution.reduce((sum, [, w]) => sum + w, 0);
  let r = Math.random() * total;
  for (const [name, weight] of distribution) {
    r -= weight;
    if (r <= 0) return name;
  }
  return distribution[distribution.length - 1]![0];
}
