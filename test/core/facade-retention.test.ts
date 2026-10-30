import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { connect as connectH2 } from 'node:http2';
import { setImmediate as nextTurn } from 'node:timers/promises';
import express from 'express';
import { Http3NodeServer, Http3IncomingMessage } from '../../lib/node-server.js';
import { createExpressHandler } from '../../lib/express-adapter.js';
import { connectAsync } from '../../lib/client.js';
import { generateTestCerts } from '../support/generate-certs.js';

const variants = process.env.HTTP3_WASM === '1' ? ['h2', 'native-h3', 'wasm-h3'] as const : ['h2', 'native-h3'] as const;
for (const variant of variants) for (const useExpress of [false, true]) {
  test(`${variant} ${useExpress ? 'Express' : 'Node facade'}: completed GETs release streams on a persistent connection`, async t => {
    const body = Buffer.alloc(2 * 1024 * 1024, 0x61);
    let active = 0;
    const handler: Parameters<typeof createExpressHandler>[0] = (req, res) => {
      assert.ok(req instanceof Http3IncomingMessage || useExpress);
      const stream = (req as unknown as Http3IncomingMessage).stream;
      active++;
      stream.once('close', () => { active--; });
      res.end(req.url === '/large' ? body : 'ok');
    };
    const app = express(); app.use(handler);
    const server = new Http3NodeServer({ ...generateTestCerts(), runtimeMode: variant === 'wasm-h3' ? 'wasm' : 'portable' }, useExpress ? createExpressHandler(app) : handler);
    t.after(async () => new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())));
    server.listen(0, '127.0.0.1'); await once(server, 'listening');
    const authority = `https://127.0.0.1:${server.address()!.port}`;
    const session = variant === 'h2' ? connectH2(authority, { rejectUnauthorized: false })
      : await connectAsync(authority, { rejectUnauthorized: false, runtimeMode: 'portable' });
    t.after(async () => { await session.close(); });
    for (let batch = 0; batch < 3; batch++) {
      for (let i = 0; i < 8; i++) {
        const large = i === 0;
        const stream = session.request({ ':method': 'GET', ':path': large ? '/large' : '/' }, { endStream: true });
        const chunks: Buffer[] = [];
        for await (const chunk of stream) chunks.push(chunk as Buffer);
        assert.deepEqual(Buffer.concat(chunks), large ? body : Buffer.from('ok'));
      }
      await nextTurn(); await nextTurn();
      assert.equal(active, 0, 'finished requests must not survive until session shutdown');
    }
  });
}
