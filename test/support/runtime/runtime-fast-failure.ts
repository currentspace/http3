import assert from 'node:assert';
import { connectAsync, connectQuicAsync, createQuicServer } from '../../../lib/index.js';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
} from '../../../lib/errors.js';
import { serveFetch } from '../../../lib/fetch-adapter.js';
import type { FetchHandler } from '../../../lib/fetch-adapter.js';
import { generateTestCerts } from '../generate-certs.js';

async function waitForAddress(server: { address(): { address: string; family: string; port: number } | null }, timeoutMs = 5000): Promise<void> {
  const startedAt = Date.now();
  while (!server.address()) {
    if (Date.now() - startedAt > timeoutMs) {
      throw new Error('timed out waiting for fast runtime probe');
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

function assertFastPathError(err: unknown): true {
  assert.ok(err instanceof Http3Error, 'expected Http3Error');
  assert.strictEqual(err.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
  assert.strictEqual(err.driver, 'io_uring');
  assert.ok(typeof err.errno === 'number');
  return true;
}

async function main(): Promise<void> {
  const certs = generateTestCerts();
  const fetchHandler: FetchHandler = () => new Response('ok');

  try {
    const probeServer = serveFetch({
      key: certs.key,
      cert: certs.cert,
      port: 0,
      host: '127.0.0.1',
      fetch: fetchHandler,
      disableRetry: true,
      allowHTTP1: false,
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    });
    await waitForAddress(probeServer);
    await probeServer.close();
    return;
  } catch (err) {
    assertFastPathError(err);
  }

  assert.throws(
    () => serveFetch({
      key: certs.key,
      cert: certs.cert,
      port: 9443,
      host: '0.0.0.0',
      fetch: fetchHandler,
      disableRetry: true,
      allowHTTP1: false,
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    }),
    assertFastPathError,
  );

  await assert.rejects(
    async () => createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      alpn: ['sfu-repl'],
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    }).listen(9080, '0.0.0.0'),
    assertFastPathError,
  );

  await assert.rejects(
    async () => connectQuicAsync('https://127.0.0.1:9080', {
      rejectUnauthorized: false,
      alpn: ['sfu-repl'],
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    }),
    assertFastPathError,
  );

  await assert.rejects(
    async () => connectAsync('https://127.0.0.1:9443', {
      rejectUnauthorized: false,
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    }),
    assertFastPathError,
  );
}

void main().catch((err: unknown) => {
  const error = err instanceof Error ? err : new Error(String(err));
  console.error(error.stack ?? error.message);
  process.exit(1);
});
