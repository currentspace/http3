#!/usr/bin/env node
/**
 * Reproducer for the 1-in-200 lost stream bug.
 * Runs the 200-stream mixed-size echo test in a loop until it fails,
 * then prints diagnostic info about which stream was lost.
 */
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { createQuicServer, connectQuicAsync } = require('../../dist/index.js');
const { generateTestCerts } = require('../../dist-test/test/support/generate-certs.js');

const STREAMS = 200;
const TIMEOUT_MS = 10_000;
const MAX_RUNS = 100;
const sizes = [0, 1, 100, 1024, 4096, 16384, 65536];

async function runOnce(runIndex) {
  const certs = generateTestCerts();
  const server = createQuicServer({
    key: certs.key, cert: certs.cert, disableRetry: true,
    initialMaxStreamsBidi: 100_000,
  });

  // Track which streams the server saw
  const serverStreams = new Map(); // streamId -> { bytesIn, echoed }
  server.on('session', (session) => {
    session.on('stream', (stream) => {
      const sid = stream.id;
      const entry = { bytesIn: 0, echoed: false, finSeen: false };
      serverStreams.set(sid, entry);
      const chunks = [];
      stream.on('data', (c) => { chunks.push(c); entry.bytesIn += c.length; });
      stream.on('end', () => {
        entry.finSeen = true;
        const buf = Buffer.concat(chunks);
        stream.end(buf);
        entry.echoed = true;
      });
      stream.on('error', (e) => { entry.error = e.message; });
    });
  });

  const addr = await server.listen(0, '127.0.0.1');
  const client = await connectQuicAsync(`127.0.0.1:${addr.port}`, { rejectUnauthorized: false });

  const clientStreams = new Map(); // streamId -> { size, bytesOut, bytesIn, finished, timedOut }

  const results = await Promise.all(
    Array.from({ length: STREAMS }, async (_, i) => {
      const size = sizes[i % sizes.length];
      const payload = Buffer.alloc(size, i & 0xff);
      const stream = client.openStream();
      const sid = stream.id;
      const entry = { size, bytesOut: size, bytesIn: 0, finished: false, timedOut: false };
      clientStreams.set(sid, entry);

      stream.end(payload);

      return new Promise((resolve) => {
        const chunks = [];
        const timer = setTimeout(() => {
          entry.timedOut = true;
          resolve(null);
        }, TIMEOUT_MS);
        stream.on('data', (c) => { chunks.push(c); entry.bytesIn += c.length; });
        stream.on('end', () => {
          clearTimeout(timer);
          entry.finished = true;
          resolve(Buffer.concat(chunks));
        });
        stream.on('error', (e) => {
          clearTimeout(timer);
          entry.error = e.message;
          resolve(null);
        });
      });
    })
  );

  const ok = results.filter(r => r !== null).length;
  const lost = STREAMS - ok;

  if (lost > 0) {
    console.log(`\n=== FAILURE on run ${runIndex}: ${lost} stream(s) lost ===`);
    for (const [sid, entry] of clientStreams) {
      if (entry.timedOut || !entry.finished) {
        const serverEntry = serverStreams.get(sid);
        console.log(`  stream ${sid}:`);
        console.log(`    client: size=${entry.size} bytesOut=${entry.bytesOut} bytesIn=${entry.bytesIn} finished=${entry.finished} timedOut=${entry.timedOut} error=${entry.error || 'none'}`);
        if (serverEntry) {
          console.log(`    server: bytesIn=${serverEntry.bytesIn} finSeen=${serverEntry.finSeen} echoed=${serverEntry.echoed} error=${serverEntry.error || 'none'}`);
        } else {
          console.log(`    server: NEVER SEEN`);
        }
      }
    }
  }

  await client.close();
  await server.close();
  return { ok, lost };
}

async function main() {
  for (let i = 1; i <= MAX_RUNS; i++) {
    const { ok, lost } = await runOnce(i);
    if (lost > 0) {
      process.exit(1);
    }
    if (i % 10 === 0) process.stderr.write(`${i} runs ok\n`);
  }
  console.log(`${MAX_RUNS} runs, no failures`);
}

main().catch((e) => { console.error(e); process.exit(1); });
