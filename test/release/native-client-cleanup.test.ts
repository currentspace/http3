import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';
import { test } from 'node:test';

const root = resolve(__dirname, '../../..');

for (const constructor of ['NativeWorkerClient', 'NativeQuicClient']) {
  for (const cleanup of ['joinWorker', 'shutdown']) {
    test(`${constructor}.${cleanup} releases the callback before connect`, () => {
      const result = spawnSync(process.execPath, ['-e', `
        const assert = require('node:assert/strict');
        const binding = require(${JSON.stringify(resolve(root, 'index.js'))});
        const client = new binding[${JSON.stringify(constructor)}]({ runtimeMode: 'portable' }, () => {});
        assert.equal(client.requestShutdown(), false);
        client[${JSON.stringify(cleanup)}]();
        // Keep the wrapper alive through process exit: cleanup must release
        // the native callback without depending on garbage collection.
        globalThis.client = client;
      `], {
        cwd: root, encoding: 'utf8', timeout: 10000,
        env: { ...process.env, NAPI_RS_NATIVE_LIBRARY_PATH: '', NAPI_RS_FORCE_WASI: '' },
      });
      assert.equal(result.status, 0, `${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
    });
  }
}
