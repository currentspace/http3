/**
 * Raw HTTP/3 server example using createSecureServer.
 *
 * Usage:
 *   npx tsx examples/raw-api/server.ts
 *
 * Test with the client example or curl:
 *   curl --http3-only -k https://localhost:4433/
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createSecureServer } from '../../dist/index.js';

const certDir = join(import.meta.dirname ?? __dirname, '..', 'certs');
const key = readFileSync(join(certDir, 'server.key'));
const cert = readFileSync(join(certDir, 'server.crt'));

const server = createSecureServer({ key, cert, disableRetry: true });

server.on('session', (session) => {
  console.log(`New session from ${session.remoteAddress}:${session.remotePort}`);
});

server.on('stream', (stream, headers) => {
  const method = headers[':method'] as string | undefined;
  const path = headers[':path'] as string | undefined;
  console.log(`${method ?? 'GET'} ${path ?? '/'}`);

  if (path === '/json') {
    stream.respond({
      ':status': '200',
      'content-type': 'application/json',
    });
    stream.end(JSON.stringify({ hello: 'world', protocol: 'HTTP/3' }));
  } else {
    stream.respond({
      ':status': '200',
      'content-type': 'text/plain',
    });
    stream.end(`Hello from HTTP/3!\nPath: ${path ?? '/'}\n`);
  }
});

server.on('listening', () => {
  const addr = server.address();
  console.log(`HTTP/3 server listening on https://localhost:${addr?.port ?? 4433}`);
});

server.listen(4433, '0.0.0.0');
