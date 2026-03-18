import assert from 'node:assert';
import { EventEmitter } from 'node:events';
import { describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
  WARN_HTTP3_RUNTIME_FALLBACK,
} from '../lib/index.js';
import type { RuntimeInfo } from '../lib/runtime.js';
import { runWithRuntimeSelectionSync, setPendingRuntimeInfo } from '../lib/runtime.js';

class RuntimeTarget extends EventEmitter {
  _runtimeInfo: RuntimeInfo | null = null;
}

function fastPathUnavailableError(): Error {
  return new Error(
    'ERR_HTTP3_FAST_PATH_UNAVAILABLE driver=io_uring syscall=io_uring_setup errno=1 reason_code=fast-path-unavailable: Operation not permitted (os error 1)',
  );
}

describe('runtime selection', () => {
  it('falls back from auto to portable with an observable runtime event', () => {
    const target = new RuntimeTarget();
    setPendingRuntimeInfo(target, {
      runtimeMode: 'auto',
      fallbackPolicy: 'warn-and-fallback',
    });

    let runtimeEvent: RuntimeInfo | null = null;
    target.on('runtime', (info: RuntimeInfo) => {
      runtimeEvent = info;
    });

    const warnings: Array<{ warning: string | Error; options?: unknown }> = [];
    const originalEmitWarning = process.emitWarning.bind(process) as typeof process.emitWarning;
    (process.emitWarning as unknown as (warning: string | Error, options?: unknown) => void) =
      (warning: string | Error, options?: unknown): void => {
        warnings.push({ warning, options });
      };

    try {
      const result = runWithRuntimeSelectionSync(
        target,
        {
          runtimeMode: 'auto',
          fallbackPolicy: 'warn-and-fallback',
        },
        (mode) => {
          if (mode === 'fast') {
            throw fastPathUnavailableError();
          }
          return 'portable-ok';
        },
      );

      assert.strictEqual(result, 'portable-ok');
      assert.ok(runtimeEvent !== null);
      const info = runtimeEvent as RuntimeInfo;
      assert.strictEqual(info.requestedMode, 'auto');
      assert.strictEqual(info.selectedMode, 'portable');
      assert.strictEqual(info.fallbackOccurred, true);
      assert.strictEqual(info.driver, process.platform === 'linux' ? 'poll' : 'kqueue');
      assert.strictEqual(info.warningCode, WARN_HTTP3_RUNTIME_FALLBACK);
      assert.strictEqual(info.fastAttempt?.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
      assert.strictEqual(info.fastAttempt?.driver, 'io_uring');
      assert.strictEqual(warnings.length, 1);
    } finally {
      process.emitWarning = originalEmitWarning;
    }
  });

  it('returns a typed fast-path error when fallback is forbidden', () => {
    const target = new RuntimeTarget();
    setPendingRuntimeInfo(target, {
      runtimeMode: 'fast',
      fallbackPolicy: 'error',
    });

    assert.throws(
      () => runWithRuntimeSelectionSync(
        target,
        {
          runtimeMode: 'fast',
          fallbackPolicy: 'error',
        },
        () => {
          throw fastPathUnavailableError();
        },
      ),
      (err: unknown) => {
        assert.ok(err instanceof Http3Error);
        assert.strictEqual(err.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
        assert.strictEqual(err.requestedMode, 'fast');
        assert.strictEqual(err.driver, 'io_uring');
        assert.strictEqual(err.errno, 1);
        assert.ok(err.runtimeInfo);
        return true;
      },
    );
  });
});
