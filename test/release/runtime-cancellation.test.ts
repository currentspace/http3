import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { test } from 'node:test';

const root = resolve(__dirname, '../../..');
const lib = resolve(__dirname, '../../lib');
const hasWasm = existsSync(join(root, 'dist/wasm/http3_client.wasm'));

for (const protocol of ['h3', 'quic']) {
  for (const mode of ['auto', 'wasm', 'portable']) {
    for (const stage of ['construction', 'connect']) {
      test(`${protocol} ${mode}: cancellation during ${stage} does not publish successful selection`, {
        skip: mode !== 'portable' && !hasWasm ? 'requires built WASM artifact' : false,
      }, () => {
        const result = spawnSync(process.execPath, ['-e', `
          const assert = require('node:assert/strict');
          const { join } = require('node:path');
          const lib = ${JSON.stringify(lib)};
          const controller = new AbortController();
          const factory = require(join(lib, 'client-event-loop-factory.js'));
          if (${JSON.stringify(stage)} === 'connect') {
            const create = factory.createClientEventLoop;
            factory.createClientEventLoop = async (...args) => {
              const loop = await create(...args);
              loop.connect = async () => { controller.abort(); };
              return loop;
            };
          }
          const connect = ${JSON.stringify(protocol)} === 'h3'
            ? require(join(lib, 'client.js')).connect : require(join(lib, 'quic-client.js')).connectQuic;
          let events = 0, warnings = 0, errors = 0;
          process.on('warning', warning => { if (warning.code === 'WARN_HTTP3_RUNTIME_FALLBACK') warnings++; });
          const session = connect('https://127.0.0.1:9', { runtimeMode: ${JSON.stringify(mode)}, signal: controller.signal });
          session.on('runtime', () => events++);
          session.on('error', () => errors++);
          const cancelled = assert.rejects(session.ready(), error => error.name === 'AbortError');
          if (${JSON.stringify(stage)} === 'construction') queueMicrotask(() => controller.abort());
          (async () => {
            await cancelled;
            await session.close();
            await new Promise(setImmediate);
            assert.equal(errors, 1, 'abort must be reported once');
            assert.equal(events, 0, 'cancelled selection must not emit a runtime success');
            assert.equal(warnings, 0, 'cancelled selection must not consume the fallback warning');
            assert.equal(session.runtimeInfo.selectedMode, null);
            assert.equal(session.runtimeInfo.fallbackOccurred, false);
            if (${JSON.stringify(mode)} === 'auto') assert.equal(session.runtimeInfo.nativeBinding.category, 'override');
          })().catch(error => { console.error(error); process.exitCode = 1; });
        `], {
          cwd: root, encoding: 'utf8', timeout: 10000,
          env: { ...process.env, NODE_PATH: '', NAPI_RS_NATIVE_LIBRARY_PATH: mode === 'auto' ? join(root, 'missing-cancellation-test.node') : '', NAPI_RS_FORCE_WASI: '' },
        });
        assert.equal(result.status, 0, `${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
      });
    }
  }
}
