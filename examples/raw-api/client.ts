/**
 * Raw HTTP/3 client example using connect() and request().
 *
 * Usage:
 *   # Start the server first:
 *   npx tsx examples/raw-api/server.ts
 *
 *   # Then in another terminal:
 *   npx tsx examples/raw-api/client.ts
 */

import { connect } from '../../dist/index.js';

const authority = process.argv[2] ?? 'localhost:4433';
const path = process.argv[3] ?? '/';

console.log(`Connecting to ${authority}...`);

const session = connect(authority, {
  rejectUnauthorized: false,
});

session.on('error', (err) => {
  console.error('Session error:', err);
});

session.on('connect', () => {
  console.log('Connected! Sending request...');

  const stream = session.request(
    {
      ':method': 'GET',
      ':path': path,
      ':authority': authority.split(':')[0],
      ':scheme': 'https',
    },
    { endStream: true },
  );

  let responseHeaders: Record<string, string> = {};
  const chunks: Buffer[] = [];

  stream.on('response', (headers: Record<string, string>) => {
    responseHeaders = headers;
  });

  stream.on('data', (chunk: Buffer) => {
    chunks.push(Buffer.from(chunk));
  });

  stream.on('end', async () => {
    console.log(`\nStatus: ${responseHeaders[':status'] ?? 'unknown'}`);

    for (const [key, value] of Object.entries(responseHeaders)) {
      if (!key.startsWith(':')) {
        console.log(`${key}: ${value}`);
      }
    }

    console.log('');
    console.log(Buffer.concat(chunks).toString());

    await session.close();
  });
});
