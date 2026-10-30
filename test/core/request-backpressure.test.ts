import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { Readable, Writable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { connect as connectH2 } from 'node:http2';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createDevTls } from '../../lib/dev-cert.js';
import { Http3NodeServer, Http3IncomingMessage } from '../../lib/node-server.js';
import { connectAsync } from '../../lib/client.js';
import type { IncomingMessage } from 'node:http';

let directory: string;
let tls: ReturnType<typeof createDevTls>;
before(() => { directory = mkdtempSync(join(tmpdir(), 'http3-backpressure-')); tls = createDevTls({ directory }); });
after(() => rmSync(directory, { recursive: true, force: true }));
const modes = process.env.HTTP3_WASM === '1' ? ['portable', 'wasm'] as const : ['portable'] as const;
for (const runtimeMode of modes) {
  for (const protocol of ['h2', 'h3'] as const) {
    test(`${runtimeMode} ${protocol}: paused 20 MiB upload stays bounded and does not block another stream`, async t => {
      let startReading: () => void = () => assert.fail('upload not received');
      let received!: (request: IncomingMessage) => void;
      const incoming = new Promise<IncomingMessage>(resolve => { received = resolve; });
      let completed!: () => void;
      let failed!: (error: Error) => void;
      const handled = new Promise<void>((resolve, reject) => { completed = resolve; failed = reject; });
      const server = new Http3NodeServer({ key: tls.key, cert: tls.cert, runtimeMode }, (req, res) => {
        if (req.url !== '/upload') { res.end('concurrent'); return; }
        received(req);
        startReading = () => {
          let bytes = 0;
          const hash = createHash('sha256');
          const slow = new Writable({ highWaterMark: 16384, write(chunk: Buffer, _encoding, callback) {
            bytes += chunk.length; hash.update(chunk); setTimeout(callback, 1);
          } });
          pipeline(req, slow).then(() => {
            res.setHeader('content-type', 'application/json');
            res.end(JSON.stringify({ bytes, hash: hash.digest('hex') })); completed();
          }, failed);
        };
      });
      server.listen(0); await once(server, 'listening');
      t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
      const origin = `https://localhost:${server.address()!.port}`;
      const session = protocol === 'h2' ? connectH2(origin, { ca: tls.ca }) : await connectAsync(origin, { ca: tls.ca, runtimeMode: 'portable' });
      t.after(async () => { await session.close(); });
      const upload = session.request({ ':method': 'PATCH', ':path': '/upload', 'content-type': 'multipart/form-data; boundary=test' });
      const response = (async () => { const chunks: Buffer[] = []; for await (const chunk of upload) chunks.push(chunk as Buffer); return Buffer.concat(chunks).toString(); })();
      const chunk = Buffer.alloc(64 * 1024, 0x61);
      const expected = createHash('sha256');
      const source = Readable.from((function* () { for (let i = 0; i < 320; i++) { expected.update(chunk); yield chunk; } })());
      const sending = pipeline(source, upload);
      const req = await incoming;
      // Hold the upload without consuming any body, then use the same session.
      await new Promise(resolve => setTimeout(resolve, 150));
      const other = session.request({ ':path': '/ready' }, { endStream: true });
      let text = ''; for await (const value of other) text += String(value);
      assert.equal(text, 'concurrent');
      assert.equal(req.destroyed, false);
      assert.ok(req instanceof Http3IncomingMessage);
      const stream = req.stream;
      assert.ok(stream.readableLength + (stream._bp?.pendingReadBytes ?? 0) <= 256 * 1024, 'paused reader must not accumulate the entire upload');
      startReading();
      await Promise.all([sending, handled]);
      assert.deepEqual(JSON.parse(await response), { bytes: 20 * 1024 * 1024, hash: expected.digest('hex') });
    });
  }
}
