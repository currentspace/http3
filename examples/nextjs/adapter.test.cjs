const { test } = require('node:test');
const assert = require('node:assert/strict');
const { once } = require('node:events');
const { Duplex } = require('node:stream');
const { gunzipSync } = require('node:zlib');
const { readFileSync } = require('node:fs');
const { Http3IncomingMessage, Http3ServerResponse } = require('../../dist/node-server.js');
const compression = require('next/dist/compiled/compression');
const send = require('next/dist/compiled/send');
const { checkIsOnDemandRevalidate } = require('next/dist/server/api-utils');

class Wire extends Duplex {
  constructor() { super({ autoDestroy: false, highWaterMark: 8 }); this.bytes = []; this.headers = []; }
  _read() {}
  // Delay writes to exercise async completion and backpressure.
  _write(chunk, encoding, callback) { this.bytes.push(Buffer.from(chunk)); setImmediate(callback); }
  respond(headers) { this.headers.push(headers); }
  close() { this.destroy(); }
}
function pair(t, method = 'GET') {
  const wire = new Wire();
  const req = new Http3IncomingMessage(wire, {
    ':method': method, ':path': '/', 'accept-encoding': 'gzip',
  }, { endStream: true });
  const res = new Http3ServerResponse(req);
  t.after(() => { res.destroy(); req.destroy(); wire.destroy(); });
  return { wire, req, res };
}
test('Next revalidation accepts the request headers', t => {
  const { req } = pair(t);
  assert.equal(checkIsOnDemandRevalidate(req.headers, { previewModeId: 'fixture' }).isOnDemandRevalidate, false);
});
test('Next static send completes and delivers the file bytes', async t => {
  const { wire, req, res } = pair(t);
  const finished = once(res, 'finish');
  send(req, '/app/counter.jsx', { root: __dirname }).pipe(res);
  await finished;
  assert.deepEqual(Buffer.concat(wire.bytes), readFileSync(`${__dirname}/app/counter.jsx`));
  assert.equal(res.socket.writableEnded, true);
});
for (const flush of [false, true]) {
  for (const method of ['GET', 'HEAD']) {
    test(`Next compression: ${method}, flushHeaders=${flush}`, async t => {
      const { wire, req, res } = pair(t, method);
      compression({ threshold: 0 })(req, res, () => {});
      res.setHeader('content-type', 'text/plain');
      const finished = once(res, 'finish');
      if (flush) res.flushHeaders();
      res.write('a'.repeat(3000));
      res.end('b'.repeat(3000));
      await finished;
      assert.equal(wire.headers.length, 1);
      const body = Buffer.concat(wire.bytes);
      if (method === 'HEAD') assert.equal(body.length, 0);
      else {
        assert.equal(wire.headers[0]['content-encoding'], 'gzip');
        assert.match(wire.headers[0].vary, /Accept-Encoding/i);
        assert.equal(gunzipSync(body).toString(), 'a'.repeat(3000) + 'b'.repeat(3000));
      }
    });
  }
}
