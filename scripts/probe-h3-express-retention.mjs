#!/usr/bin/env node
// Diagnostic reproducer: no per-request stream Set or profiler is installed.
import { createRequire } from 'node:module';
import { once } from 'node:events';
import { setTimeout as sleep } from 'node:timers/promises';
import { writeFileSync } from 'node:fs';
const require = createRequire(import.meta.url);
const { createSecureServer } = require('../dist-test/lib/server.js');
const { createExpressAdapter } = require('../dist-test/lib/express-adapter.js');
const { Http3IncomingMessage, Http3ServerResponse } = require('../dist-test/lib/node-server.js');
const { connectAsync } = require('../dist-test/lib/client.js');
const { generateTestCerts } = require('../dist-test/test/support/generate-certs.js');
if (!global.gc) throw new Error('Run with node --expose-gc');
const app = require('express')();
app.get('/express', (_req, res) => res.json({ ok: true }));
const adapter = createExpressAdapter(app);
const server = createSecureServer({ ...generateTestCerts(), runtimeMode: 'portable' });
server.on('stream', (stream, headers, flags) => {
  stream.on('error', () => {});
  if (headers[':path'] === '/express') adapter(stream, headers, flags);
  else if (headers[':path'] === '/facade') {
    const req = new Http3IncomingMessage(stream, headers, flags);
    new Http3ServerResponse(req).end('ok');
  }
  else { stream.respond({ ':status': '200' }); stream.end('ok'); }
});
const listening = once(server, 'listening'); server.listen(0, '127.0.0.1'); await listening;
let client;
const results = [];
async function snapshot(phase) {
  await sleep(100); global.gc();
  const first = server._streams.values().next().value;
  results.push({ phase, memory: process.memoryUsage(), trackedStreams: server._streams.size,
    firstStream: first && { readableEnded: first.readableEnded, readableFlowing: first.readableFlowing,
      readableLength: first.readableLength, writableFinished: first.writableFinished, destroyed: first.destroyed } });
}
try {
  client = await connectAsync(`https://127.0.0.1:${server.address().port}`, { rejectUnauthorized: false, runtimeMode: 'portable' });
  await snapshot('connected');
  for (const path of ['/raw', '/facade', '/express', '/express']) {
    for (let i = 0; i < 1000; i++) {
      const stream = client.request({ ':method': 'GET', ':path': path }, { endStream: true });
      const ended = once(stream, 'end'); stream.resume(); await ended;
    }
    await snapshot(`${path}: 1000 completed requests`);
  }
  const output = JSON.stringify(results, null, 2) + '\n';
  if (process.argv[2]) writeFileSync(process.argv[2], output);
  console.log(output);
} finally {
  await client?.close(); await server.close();
}
