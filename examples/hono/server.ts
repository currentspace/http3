/**
 * Hono HTTP/3 server example using serveFetch().
 *
 * Setup:
 *   cd examples/hono && npm install
 *   cd ../certs && ./generate.sh
 *
 * Usage:
 *   npx tsx examples/hono/server.ts
 *
 * Test:
 *   npx tsx examples/raw-api/client.ts localhost:4433 /
 *   npx tsx examples/raw-api/client.ts localhost:4433 /json
 *   npx tsx examples/raw-api/client.ts localhost:4433 /hello/world
 */

import { Hono } from 'hono';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { serveFetch } from '../../dist/fetch-adapter.js';

const certDir = join(import.meta.dirname ?? __dirname, '..', 'certs');

const app = new Hono();

app.get('/', (c) => c.text('Hello from Hono on HTTP/3!'));

app.get('/json', (c) =>
  c.json({
    framework: 'Hono',
    protocol: 'HTTP/3',
    transport: 'QUIC',
    message: 'No framework changes required!',
  }),
);

app.get('/hello/:name', (c) => {
  const name = c.req.param('name');
  return c.text(`Hello, ${name}! You're on HTTP/3.`);
});

app.post('/echo', async (c) => {
  const body = await c.req.text();
  return c.text(body);
});

const server = serveFetch({
  port: 4433,
  key: readFileSync(join(certDir, 'server.key')),
  cert: readFileSync(join(certDir, 'server.crt')),
  disableRetry: true,
  fetch: app.fetch,
});

server.on('listening', () => {
  const addr = server.address();
  console.log(`Hono HTTP/3 server listening on https://localhost:${addr?.port ?? 4433}`);
  console.log('');
  console.log('Routes:');
  console.log('  GET  /           → text greeting');
  console.log('  GET  /json       → JSON response');
  console.log('  GET  /hello/:name → parameterized greeting');
  console.log('  POST /echo       → echo request body');
});
