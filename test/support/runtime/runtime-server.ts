import http from 'node:http';
import { createQuicServer } from '../../../lib/index.js';
import { serveFetch } from '../../../lib/fetch-adapter.js';
import type { FetchHandler } from '../../../lib/fetch-adapter.js';
import { generateTestCerts } from '../generate-certs.js';
import type { QuicServerSession } from '../../../lib/index.js';
import type { QuicStream } from '../../../lib/quic-stream.js';
import { echoStream } from '../echo-stream.js';

const SERVER_RUNTIME_MODE = process.env.SERVER_RUNTIME_MODE as 'auto' | 'fast' | 'portable' | undefined;
const SERVER_FALLBACK_POLICY = process.env.SERVER_FALLBACK_POLICY as 'error' | 'warn-and-fallback' | undefined;
const RAW_PORT = Number.parseInt(process.env.RAW_QUIC_PORT ?? '9080', 10);
const H3_PORT = Number.parseInt(process.env.H3_PORT ?? '9443', 10);
const HEALTH_PORT = Number.parseInt(process.env.HEALTH_PORT ?? '8080', 10);

async function waitForAddress(server: { address(): { address: string; family: string; port: number } | null }, timeoutMs = 5000): Promise<void> {
  const startedAt = Date.now();
  while (!server.address()) {
    if (Date.now() - startedAt > timeoutMs) {
      throw new Error('timed out waiting for HTTP/3 server address');
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

async function main(): Promise<void> {
  const certs = generateTestCerts();

  const quicServer = createQuicServer({
    key: certs.key,
    cert: certs.cert,
    disableRetry: true,
    alpn: ['sfu-repl'],
    runtimeMode: SERVER_RUNTIME_MODE,
    fallbackPolicy: SERVER_FALLBACK_POLICY,
  });

  quicServer.on('session', (session: QuicServerSession) => {
    session.on('stream', (stream: QuicStream) => {
      echoStream(stream);
    });
  });

  const fetchHandler: FetchHandler = async (request: Request) => {
    const pathname = new URL(request.url).pathname;
    if (pathname === '/__runtime') {
      return Response.json({
        quic: quicServer.runtimeInfo,
        http3: h3Server.runtimeInfo,
      });
    }
    return new Response('http3-ok', {
      status: 200,
      headers: {
        'content-type': 'text/plain; charset=utf-8',
      },
    });
  };

  const h3Server = serveFetch({
    key: certs.key,
    cert: certs.cert,
    port: H3_PORT,
    host: '0.0.0.0',
    fetch: fetchHandler,
    disableRetry: true,
    allowHTTP1: false,
    runtimeMode: SERVER_RUNTIME_MODE,
    fallbackPolicy: SERVER_FALLBACK_POLICY,
  });

  const healthServer = http.createServer((_req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    res.end('ok');
  });

  await quicServer.listen(RAW_PORT, '0.0.0.0');
  await waitForAddress(h3Server);
  await new Promise<void>((resolve) => {
    healthServer.listen(HEALTH_PORT, '0.0.0.0', () => resolve());
  });

  const shutdown = async (): Promise<void> => {
    await Promise.allSettled([
      quicServer.close(),
      h3Server.close(),
      new Promise<void>((resolve) => {
        healthServer.close(() => resolve());
      }),
    ]);
    process.exit(0);
  };

  process.on('SIGINT', () => {
    void shutdown();
  });
  process.on('SIGTERM', () => {
    void shutdown();
  });
}

void main().catch((err: unknown) => {
  const error = err instanceof Error ? err : new Error(String(err));
  console.error(error.stack ?? error.message);
  process.exit(1);
});
