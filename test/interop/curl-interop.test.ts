/**
 * Curl interop tests — verify that an external ngtcp2/nghttp3 client (curl)
 * can complete HTTP/3 requests against our Node.js server.
 *
 * These tests require curl built with HTTP/3 support (--http3-only flag).
 * Tests are skipped if curl is not available or lacks HTTP/3 support.
 */

import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { spawn, execSync } from 'node:child_process';
import { generateTestCerts } from '../support/generate-certs.js';
import { createSecureServer, createSseStream } from '../../lib/index.js';
import type { Http3SecureServer } from '../../lib/index.js';

// ----- Helpers -----

interface CurlResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

type CurlProtocol = 'h3' | 'h2';

async function spawnCurl(url: string, args: string[] = [], protocol: CurlProtocol = 'h3'): Promise<CurlResult> {
  const protocolArgs = protocol === 'h3' ? ['--http3-only'] : ['--http2'];
  return new Promise((resolve) => {
    const proc = spawn('curl', [
      ...protocolArgs,
      '-k',
      '-s',
      '-o', '-',
      '-D', '/dev/stderr',
      '--max-time', '5',
      ...args,
      url,
    ]);

    let stdout = '';
    let stderr = '';
    proc.stdout.on('data', (d: Buffer) => { stdout += d.toString(); });
    proc.stderr.on('data', (d: Buffer) => { stderr += d.toString(); });
    proc.on('close', (code) => {
      resolve({ stdout, stderr, exitCode: code ?? 1 });
    });
  });
}

function hasCurlHttp3(): boolean {
  try {
    const version = execSync('curl --version 2>/dev/null', { encoding: 'utf-8' });
    return version.includes('HTTP3') || version.includes('http3');
  } catch {
    return false;
  }
}

function hasCurlHttp2(): boolean {
  try {
    const version = execSync('curl --version 2>/dev/null', { encoding: 'utf-8' });
    return version.includes('HTTP2') || version.includes('http2');
  } catch {
    return false;
  }
}

async function startServer(
  certs: { key: Buffer; cert: Buffer },
  opts: { disableRetry?: boolean },
  handler: (stream: import('../../lib/stream.js').ServerHttp3Stream, headers: Record<string, string | string[]>) => void,
): Promise<{ server: Http3SecureServer; port: number }> {
  const server = createSecureServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: opts.disableRetry,
  }, (stream, headers) => {
    handler(stream, headers);
  });

  const port = await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
    server.listen(0, '127.0.0.1');
  });

  return { server, port };
}

// ----- Tests -----

const SKIP = !hasCurlHttp3();
const SKIP_H2 = !hasCurlHttp2();

describe('Curl Interop', { skip: SKIP && 'curl with HTTP/3 not available' }, () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('GET request, retry disabled', async () => {
    const { server, port } = await startServer(certs, { disableRetry: true }, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('hello from h3');
    });

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout, 'hello from h3');
    } finally {
      await server.close();
    }
  });

  it('GET request, retry enabled', async () => {
    const { server, port } = await startServer(certs, { disableRetry: false }, (stream) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end('hello with retry');
    });

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout, 'hello with retry');
    } finally {
      await server.close();
    }
  });

  it('POST with body', async () => {
    const { server, port } = await startServer(certs, { disableRetry: true }, (stream, _headers) => {
      const chunks: Buffer[] = [];
      stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
      stream.on('end', () => {
        const body = Buffer.concat(chunks).toString();
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end(`echo: ${body}`);
      });
    });

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`, [
        '-X', 'POST',
        '-d', 'test-body-data',
      ]);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout, 'echo: test-body-data');
    } finally {
      await server.close();
    }
  });

  it('large response (100KB)', async () => {
    const largeBody = 'A'.repeat(100 * 1024);

    const { server, port } = await startServer(certs, { disableRetry: true }, (stream) => {
      stream.respond({
        ':status': '200',
        'content-type': 'text/plain',
        'content-length': String(largeBody.length),
      });
      stream.end(largeBody);
    });

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout.length, largeBody.length, 'response length mismatch');
      assert.strictEqual(result.stdout, largeBody);
    } finally {
      await server.close();
    }
  });

  it('multiple sequential requests', async () => {
    let requestCount = 0;

    const { server, port } = await startServer(certs, { disableRetry: true }, (stream) => {
      requestCount++;
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end(`request-${requestCount}`);
    });

    try {
      for (let i = 1; i <= 3; i++) {
        const result = await spawnCurl(`https://127.0.0.1:${port}/`);
        assert.strictEqual(result.exitCode, 0, `curl request ${i} failed: ${result.stderr}`);
        assert.strictEqual(result.stdout, `request-${i}`);
      }
    } finally {
      await server.close();
    }
  });
});

describe('Curl Interop (worker-only regression)', { skip: SKIP && 'curl with HTTP/3 not available' }, () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('GET request, worker thread + retry disabled', async () => {
    const { server, port } = await startServer(
      certs,
      { disableRetry: true },
      (stream) => {
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end('hello from worker');
      },
    );

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout, 'hello from worker');
    } finally {
      await server.close();
    }
  });

  it('GET request, worker thread + retry enabled', async () => {
    const { server, port } = await startServer(
      certs,
      { disableRetry: false },
      (stream) => {
        stream.respond({ ':status': '200', 'content-type': 'text/plain' });
        stream.end('worker retry works');
      },
    );

    try {
      const result = await spawnCurl(`https://127.0.0.1:${port}/`);
      assert.strictEqual(result.exitCode, 0, `curl failed: ${result.stderr}`);
      assert.strictEqual(result.stdout, 'worker retry works');
    } finally {
      await server.close();
    }
  });
});

describe('Curl Interop (dual H3+H2)', {
  skip: (SKIP || SKIP_H2) && 'curl with HTTP/3 and HTTP/2 not available',
}, () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('same port handles --http3-only and --http2', async () => {
    const { server, port } = await startServer(certs, { disableRetry: true }, (stream, headers) => {
      stream.respond({ ':status': '200', 'content-type': 'text/plain' });
      stream.end(`dual:${String(headers[':path'] ?? '/')}`);
    });

    try {
      const h3 = await spawnCurl(`https://127.0.0.1:${port}/h3`, [], 'h3');
      const h2 = await spawnCurl(`https://127.0.0.1:${port}/h2`, [], 'h2');
      assert.strictEqual(h3.exitCode, 0, `h3 curl failed: ${h3.stderr}`);
      assert.strictEqual(h2.exitCode, 0, `h2 curl failed: ${h2.stderr}`);
      assert.strictEqual(h3.stdout, 'dual:/h3');
      assert.strictEqual(h2.stdout, 'dual:/h2');
    } finally {
      await server.close();
    }
  });

  it('SSE smoke test with --no-buffer', async () => {
    const { server, port } = await startServer(certs, { disableRetry: true }, (stream, headers) => {
      if (headers[':path'] !== '/events') {
        stream.respond({ ':status': '404' }, { endStream: true });
        return;
      }
      const sse = createSseStream(stream);
      void (async () => {
        await sse.send({ data: 'hello' });
        await sse.send({ data: 'world' });
        sse.close();
      })();
    });

    try {
      const h3 = await spawnCurl(`https://127.0.0.1:${port}/events`, ['--no-buffer'], 'h3');
      const h2 = await spawnCurl(`https://127.0.0.1:${port}/events`, ['--no-buffer'], 'h2');
      assert.strictEqual(h3.exitCode, 0, `h3 curl failed: ${h3.stderr}`);
      assert.strictEqual(h2.exitCode, 0, `h2 curl failed: ${h2.stderr}`);
      assert.ok(h3.stdout.includes('data: hello'));
      assert.ok(h3.stdout.includes('data: world'));
      assert.ok(h2.stdout.includes('data: hello'));
      assert.ok(h2.stdout.includes('data: world'));
    } finally {
      await server.close();
    }
  });
});
