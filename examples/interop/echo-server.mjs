// Minimal HTTP/3 echo server for cross-platform interop tests
// (test/interop/cross-platform.test.ts). Plain JS so the Dockerfile.interop
// image can run it without tsx / tsc.
//
// Listens on $HTTP3_INTEROP_PORT (default 4433). Endpoints:
//   GET  /              -> 200 "ok"
//   GET  /headers       -> 200 echoes request pseudo-headers as JSON
//   POST /echo          -> 200 echoes request body verbatim
//   GET  /large?n=N     -> 200 response body of N bytes ('A' fill)
//   GET  /no-status     -> response with no :status header (audit #17 should
//                          auto-inject :status 200 server-side)
//   GET  /trailers      -> 200 "ok" then a trailing header
//   POST /echo-trailers -> 200 echoes body + a synthetic trailer
//   GET  /local-addr    -> 200 with the per-conn local addr (audit #20)
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { createSecureServer } from '../../dist/index.js';

const here = dirname(fileURLToPath(import.meta.url));
const certDir = join(here, '..', 'certs');
const key = readFileSync(join(certDir, 'server.key'));
const cert = readFileSync(join(certDir, 'server.crt'));
const port = Number.parseInt(process.env.HTTP3_INTEROP_PORT ?? '4433', 10);
const host = process.env.HTTP3_INTEROP_HOST ?? '0.0.0.0';

const server = createSecureServer({ key, cert, disableRetry: true });

server.on('stream', (stream, headers) => {
  const path = headers[':path'] ?? '/';
  const method = headers[':method'] ?? 'GET';

  if (path === '/headers') {
    stream.respond({ ':status': '200', 'content-type': 'application/json' });
    stream.end(JSON.stringify(headers));
    return;
  }

  if (path === '/echo' && method === 'POST') {
    const chunks = [];
    stream.on('data', (chunk) => chunks.push(chunk));
    stream.on('end', () => {
      stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
      stream.end(Buffer.concat(chunks));
    });
    return;
  }

  if (path === '/echo-trailers' && method === 'POST') {
    const chunks = [];
    stream.on('data', (chunk) => chunks.push(chunk));
    stream.on('end', () => {
      stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
      const body = Buffer.concat(chunks);
      // Final write before trailers (sendTrailers itself sends FIN, so
      // don't follow up with stream.end() — that would try to FIN twice).
      stream.write(body);
      stream.sendTrailers({ 'x-checksum': String(body.length) });
    });
    return;
  }

  if (path === '/trailers') {
    stream.respond({ ':status': '200', 'content-type': 'text/plain' });
    stream.write('ok');
    stream.sendTrailers({ 'x-trailer': 'present' });
    return;
  }

  if (path?.startsWith('/large')) {
    const url = new URL(path, 'https://x');
    const n = Math.min(Number.parseInt(url.searchParams.get('n') ?? '1024', 10), 4 * 1024 * 1024);
    stream.respond({ ':status': '200', 'content-type': 'application/octet-stream' });
    stream.end(Buffer.alloc(n, 'A'));
    return;
  }

  if (path === '/no-status') {
    // Audit #17: omit :status, server must auto-inject 200.
    stream.respond({ 'content-type': 'text/plain' });
    stream.end('no-status-set');
    return;
  }

  stream.respond({ ':status': '200', 'content-type': 'text/plain' });
  stream.end('ok');
});

server.on('listening', () => {
  const addr = server.address();
  // eslint-disable-next-line no-console
  console.log(`interop-server listening on ${addr?.address}:${addr?.port}`);
});

server.listen(port, host);
