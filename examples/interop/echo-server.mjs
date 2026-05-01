// Minimal HTTP/3 echo server for cross-platform interop tests
// (test/interop/cross-platform.test.ts). Plain JS so the Dockerfile.interop
// image can run it without tsx / tsc.
//
// Listens on $HTTP3_INTEROP_PORT (default 4433), responds to:
//   GET /         -> 200 "ok"
//   GET /headers  -> 200 echoing the request headers back as JSON
//   POST /echo    -> 200 echoing the request body
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

  stream.respond({ ':status': '200', 'content-type': 'text/plain' });
  stream.end('ok');
});

server.on('listening', () => {
  const addr = server.address();
  // eslint-disable-next-line no-console
  console.log(`interop-server listening on ${addr?.address}:${addr?.port}`);
});

server.listen(port, host);
