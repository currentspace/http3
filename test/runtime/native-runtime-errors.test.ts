import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { afterEach, beforeEach, describe, it } from 'node:test';
import { ERR_HTTP3_FAST_PATH_UNAVAILABLE, ERR_HTTP3_RUNTIME_UNSUPPORTED, Http3Error } from '../../lib/errors.js';
import { parseNativeRuntimeError, resetRuntimeSelectionCacheForTests, runWithRuntimeSelection, runWithRuntimeSelectionSync } from '../../lib/runtime.js';
import type { RuntimeInfo, RuntimeOptions, SelectedRuntimeMode } from '../../lib/runtime.js';

class RuntimeTarget extends EventEmitter {
  _runtimeInfo: RuntimeInfo | null = null;
}

// Exact Http3NativeError::tagged_message() wire format, delivered as a NAPI
// GenericFailure by NativeWorkerServer.listen before fromNapiError is involved.
const fastText = 'ERR_HTTP3_FAST_PATH_UNAVAILABLE driver=io_uring syscall=io_uring_register_buf_ring errno=22 reason_code=fast-path-unavailable: Invalid argument (os error 22)';
const taggedFastText = `[h3:fast-path|driver=io_uring|syscall=io_uring_register_buf_ring|errno=22] ${fastText}`;
const runtimeText = 'ERR_HTTP3_RUNTIME_UNSUPPORTED driver=io_uring syscall=io_uring_setup errno=22 reason_code=driver-setup-failed: Invalid argument (os error 22)';
const taggedRuntimeText = `[h3:runtime-io|driver=io_uring|syscall=io_uring_setup|reason=driver-setup-failed|errno=22] ${runtimeText}`;
function assertOriginalCause(error: Error, original: Error): void {
  let cause: unknown = error;
  while (cause instanceof Error && cause.cause !== undefined) cause = cause.cause;
  assert.equal(cause, original);
}

function nativeError(message = taggedFastText): Error {
  return Object.assign(new Error(message), { code: 'GenericFailure' });
}

describe('tagged native runtime errors', () => {
  beforeEach(() => resetRuntimeSelectionCacheForTests());
  afterEach(() => resetRuntimeSelectionCacheForTests());

  it('parses the native tagged fast-path and runtime-IO encodings and legacy text', () => {
    assert.deepEqual(parseNativeRuntimeError(nativeError()), {
      code: ERR_HTTP3_FAST_PATH_UNAVAILABLE, driver: 'io_uring',
      syscall: 'io_uring_register_buf_ring', errno: 22,
      reasonCode: 'fast-path-unavailable', message: 'Invalid argument (os error 22)',
    });
    assert.deepEqual(parseNativeRuntimeError(nativeError(fastText)), parseNativeRuntimeError(nativeError()));
    assert.deepEqual(parseNativeRuntimeError(nativeError(taggedRuntimeText)), {
      code: ERR_HTTP3_RUNTIME_UNSUPPORTED, driver: 'io_uring',
      syscall: 'io_uring_setup', errno: 22,
      reasonCode: 'driver-setup-failed', message: 'Invalid argument (os error 22)',
    });
  });

  it('does not strip unrelated, malformed, or mismatched native tags', () => {
    for (const message of [
      `[h3:io|errno=22] ${fastText}`, `[h3:config] ${fastText}`,
      `[h3:tls] ${fastText}`, `[h3:unknown] ${fastText}`,
      `[h3:runtime-io|errno=22] ${fastText}`, `[h3:fast-path] ${runtimeText}`,
      `[h3:fast-path|malformed] ${fastText}`, `prefix ${taggedFastText}`,
      `[h3:fast-path ${fastText}`, '[h3:io|errno=22] IO error: Invalid argument (os error 22)',
    ]) assert.equal(parseNativeRuntimeError(nativeError(message)), null, message);
  });

  for (const selection of ['sync', 'async'] as const) {
    const select = async (target: RuntimeTarget, options: RuntimeOptions, attempt: (mode: SelectedRuntimeMode) => string): Promise<string> => {
      if (selection === 'sync') return runWithRuntimeSelectionSync(target, options, attempt, { wasm: false });
      return runWithRuntimeSelection(target, options, async mode => attempt(mode), { wasm: false });
    };

    it(`${selection}: auto falls back to portable with tagged failure details and one warning`, async t => {
      const target = new RuntimeTarget();
      const attempts: SelectedRuntimeMode[] = [];
      let events = 0;
      let warnings = 0;
      target.on('runtime', () => { events++; });
      t.mock.method(process, 'emitWarning', () => { warnings++; });
      const result = await select(target, { runtimeMode: 'auto' }, mode => {
        attempts.push(mode);
        if (mode === 'fast') throw nativeError();
        return 'portable';
      });
      assert.equal(result, 'portable');
      assert.deepEqual(attempts, ['fast', 'portable']);
      assert.equal(target._runtimeInfo?.selectedMode, 'portable');
      assert.equal(target._runtimeInfo?.fallbackOccurred, true);
      assert.equal(target._runtimeInfo?.fastAttempt?.errno, 22);
      assert.equal(target._runtimeInfo?.fastAttempt?.syscall, 'io_uring_register_buf_ring');
      assert.equal(target._runtimeInfo?.fastAttempt?.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
      assert.equal(events, 1);
      assert.equal(warnings, 1);
    });

    for (const options of [
      { runtimeMode: 'fast', fallbackPolicy: 'warn-and-fallback' },
      { runtimeMode: 'fast', fallbackPolicy: 'error' },
      { runtimeMode: 'auto', fallbackPolicy: 'error' },
    ] as const) {
      it(`${selection}: ${options.runtimeMode}/${options.fallbackPolicy} preserves strict failure and original cause`, async t => {
        const target = new RuntimeTarget();
        const original = nativeError();
        const attempts: SelectedRuntimeMode[] = [];
        t.mock.method(process, 'emitWarning', () => assert.fail('strict failure must not warn'));
        target.on('runtime', () => assert.fail('failed selection must not emit success'));
        await assert.rejects(select(target, options, mode => {
          attempts.push(mode);
          throw original;
        }), error => {
          assert.ok(error instanceof Http3Error);
          assert.equal(error.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
          assert.equal(error.errno, 22);
          assert.equal(error.syscall, 'io_uring_register_buf_ring');
          assertOriginalCause(error, original);
          return true;
        });
        assert.deepEqual(attempts, ['fast']);
        assert.equal(target._runtimeInfo?.selectedMode, null);
        assert.equal(target._runtimeInfo?.fallbackOccurred, false);
      });
    }

    it(`${selection}: raw IO, configuration, and runtime-IO failures do not fall back`, async t => {
      t.mock.method(process, 'emitWarning', () => assert.fail('non-capability failure must not warn'));
      for (const message of ['[h3:io|errno=22] IO error: Invalid argument (os error 22)', '[h3:config] config error: bad TLS certificate', taggedRuntimeText]) {
        const original = nativeError(message);
        const attempts: SelectedRuntimeMode[] = [];
        const target = new RuntimeTarget();
        await assert.rejects(select(target, { runtimeMode: 'auto' }, mode => {
          attempts.push(mode);
          throw original;
        }), error => {
          if (message === taggedRuntimeText) {
            assert.ok(error instanceof Http3Error);
            assert.equal(error.code, ERR_HTTP3_RUNTIME_UNSUPPORTED);
            assertOriginalCause(error, original);
          } else assert.equal(error, original);
          return true;
        });
        assert.deepEqual(attempts, ['fast']);
        assert.equal(target._runtimeInfo?.selectedMode, null);
      }
    });
  }
});
