import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';
import { test } from 'node:test';

const root = resolve(__dirname, '../../..');

for (const failure of ['invalid-address', 'occupied-port']) {
  test(`native raw QUIC releases startup resources after ${failure}`, () => {
    const result = spawnSync(process.execPath, ['-e', `
      const assert = require('node:assert/strict');
      const { once } = require('node:events');
      const { createSocket } = require('node:dgram');
      const { QuicServer } = require('./dist-test/lib/quic-server.js');
      const { generateTestCerts } = require('./dist-test/test/support/generate-certs.js');
      const { NativeQuicServer } = require('./index.js');
      const nativeListen = NativeQuicServer.prototype.listen;
      const nativeShutdown = NativeQuicServer.prototype.shutdown;
      let originalError, shutdowns = 0, events = 0, warnings = 0;
      NativeQuicServer.prototype.listen = function(...args) {
        try { return nativeListen.apply(this, args); }
        catch (error) { originalError = error; throw error; }
      };
      NativeQuicServer.prototype.shutdown = function() {
        shutdowns++;
        return nativeShutdown.call(this);
      };
      process.on('warning', warning => { if (warning.code === 'WARN_HTTP3_RUNTIME_FALLBACK') warnings++; });
      (async () => {
        const blocker = ${JSON.stringify(failure)} === 'occupied-port' ? createSocket('udp4') : undefined;
        if (blocker) { blocker.bind(0, '127.0.0.1'); await once(blocker, 'listening'); }
        const server = new QuicServer({ ...generateTestCerts(), runtimeMode: 'portable' });
        globalThis.server = server;
        server.on('runtime', () => events++);
        server.on('listening', () => assert.fail('failed startup published a listener'));
        try {
          await assert.rejects(server.listen(blocker?.address().port ?? 0, blocker ? '127.0.0.1' : 'invalid-address'), error => {
            assert.ok(error === originalError || error.cause === originalError, 'retain the native listen error');
            assert.match(error.message, blocker ? /in use/i : /invalid socket address/i);
            return true;
          });
          await server.close();
          await new Promise(setImmediate);
          assert.equal(shutdowns, 1, 'the owning native factory must release its binding');
          assert.equal(events, 0);
          assert.equal(warnings, 0);
          assert.equal(server.address(), null);
          assert.equal(server.runtimeInfo.selectedMode, null);
        } finally {
          await server.close();
          if (blocker) await new Promise(resolve => blocker.close(resolve));
        }
      })().catch(error => { console.error(error); process.exitCode = 1; });
    `], {
      cwd: root, encoding: 'utf8', timeout: 10000,
      env: { ...process.env, NAPI_RS_NATIVE_LIBRARY_PATH: '', NAPI_RS_FORCE_WASI: '' },
    });
    assert.equal(result.status, 0, `${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  });
}
