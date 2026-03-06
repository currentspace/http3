/**
 * Express HTTP/3 server example using the Express adapter.
 *
 * Setup:
 *   cd examples/express-adapter && npm install
 *   cd ../certs && ./generate.sh
 *
 * Usage:
 *   npx tsx examples/express-adapter/server.ts
 *
 * Test:
 *   npx tsx examples/raw-api/client.ts localhost:4433 /
 *   npx tsx examples/raw-api/client.ts localhost:4433 /json
 */

import express from 'express';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createSecureServer } from '../../dist/index.js';
import { createExpressAdapter } from './adapter.js';

const certDir = join(import.meta.dirname ?? __dirname, '..', 'certs');

const app = express();

app.get('/', (_req, res) => {
  res.setHeader('content-type', 'text/plain');
  res.end('Hello from Express on HTTP/3!\n');
});

app.get('/json', (_req, res) => {
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(JSON.stringify({
    framework: 'Express',
    protocol: 'HTTP/3',
    transport: 'QUIC',
  }));
});

app.get('/hello/:name', (req, res) => {
  res.setHeader('content-type', 'text/plain');
  res.end(`Hello, ${req.params.name}! You're on HTTP/3 via Express.\n`);
});

const adapter = createExpressAdapter(app as unknown as (req: unknown, res: unknown) => void);

const server = createSecureServer(
  {
    key: readFileSync(join(certDir, 'server.key')),
    cert: readFileSync(join(certDir, 'server.crt')),
    disableRetry: true,
  },
  adapter,
);

server.on('listening', () => {
  const addr = server.address();
  console.log(`Express HTTP/3 server listening on https://localhost:${addr?.port ?? 4433}`);
  console.log('');
  console.log('Routes:');
  console.log('  GET /          → text greeting');
  console.log('  GET /json      → JSON response');
  console.log('  GET /hello/:name → parameterized greeting');
});

server.listen(4433, '0.0.0.0');
