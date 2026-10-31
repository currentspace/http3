import assert from 'node:assert';
import { connectAsync, connectQuicAsync } from '../../../lib/index.js';
import type { RuntimeInfo } from '../../../lib/runtime.js';
import type { QuicStream } from '../../../lib/quic-stream.js';
import { assertRuntimeInfo, parseExpectedFallback, type ExpectedRuntimeMode } from './runtime-expectations.js';

const SERVER_HOST = process.env.SERVER_HOST ?? 'sfu';
const RAW_PORT = Number.parseInt(process.env.RAW_QUIC_PORT ?? '9080', 10);
const H3_PORT = Number.parseInt(process.env.H3_PORT ?? '9443', 10);
const CLIENT_RUNTIME_MODE = process.env.CLIENT_RUNTIME_MODE as 'auto' | 'fast' | 'portable' | undefined;
const CLIENT_FALLBACK_POLICY = process.env.CLIENT_FALLBACK_POLICY as 'error' | 'warn-and-fallback' | undefined;

const EXPECT_CLIENT_SELECTED_MODE = process.env.EXPECT_CLIENT_SELECTED_MODE as ExpectedRuntimeMode | undefined;
const EXPECT_CLIENT_FALLBACK = parseExpectedFallback(process.env.EXPECT_CLIENT_FALLBACK);
const EXPECT_SERVER_SELECTED_MODE = process.env.EXPECT_SERVER_SELECTED_MODE as ExpectedRuntimeMode | undefined;
const EXPECT_SERVER_FALLBACK = parseExpectedFallback(process.env.EXPECT_SERVER_FALLBACK);

function collectQuic(stream: QuicStream, timeoutMs = 5000): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => reject(new Error('collect timed out')), timeoutMs);
    stream.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
    });
    stream.on('end', () => {
      clearTimeout(timer);
      resolve(Buffer.concat(chunks).toString('utf8'));
    });
    stream.on('error', (err: Error) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

async function requestText(
  session: Awaited<ReturnType<typeof connectAsync>>,
  path: string,
): Promise<string> {
  const stream = session.request({
    ':method': 'GET',
    ':path': path,
    ':authority': `${SERVER_HOST}:${String(H3_PORT)}`,
    ':scheme': 'https',
  }, { endStream: true });

  return await new Promise<string>((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
    });
    stream.on('end', () => {
      resolve(Buffer.concat(chunks).toString('utf8'));
    });
    stream.on('error', reject);
  });
}

async function main(): Promise<void> {
  const rawSession = await connectQuicAsync(`https://${SERVER_HOST}:${RAW_PORT}`, {
    rejectUnauthorized: false,
    alpn: ['sfu-repl'],
    runtimeMode: CLIENT_RUNTIME_MODE,
    fallbackPolicy: CLIENT_FALLBACK_POLICY,
  });
  assertRuntimeInfo(rawSession.runtimeInfo, EXPECT_CLIENT_SELECTED_MODE, EXPECT_CLIENT_FALLBACK, 'raw client');

  const rawStream = rawSession.openStream();
  rawStream.end(Buffer.from('runtime-dns-ok'));
  const echoed = await collectQuic(rawStream);
  assert.strictEqual(echoed, 'runtime-dns-ok');

  const h3Session = await connectAsync(`https://${SERVER_HOST}:${H3_PORT}`, {
    rejectUnauthorized: false,
    runtimeMode: CLIENT_RUNTIME_MODE,
    fallbackPolicy: CLIENT_FALLBACK_POLICY,
  });
  assertRuntimeInfo(h3Session.runtimeInfo, EXPECT_CLIENT_SELECTED_MODE, EXPECT_CLIENT_FALLBACK, 'http3 client');

  const body = await requestText(h3Session, '/');
  assert.strictEqual(body, 'http3-ok');

  const runtimeBody = await requestText(h3Session, '/__runtime');
  const runtime = JSON.parse(runtimeBody) as {
    quic: RuntimeInfo | null;
    http3: RuntimeInfo | null;
  };
  assertRuntimeInfo(runtime.quic, EXPECT_SERVER_SELECTED_MODE, EXPECT_SERVER_FALLBACK, 'raw server');
  assertRuntimeInfo(runtime.http3, EXPECT_SERVER_SELECTED_MODE, EXPECT_SERVER_FALLBACK, 'http3 server');
  console.log(JSON.stringify({ passed: true, client: { quic: rawSession.runtimeInfo, http3: h3Session.runtimeInfo }, server: runtime }));

  await rawSession.close();
  await h3Session.close();
}

main().catch((err: unknown) => {
  const error = err instanceof Error ? err : new Error(String(err));
  console.error(error.stack ?? error.message);
  process.exit(1);
});
