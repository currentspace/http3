import assert from 'node:assert';
import { EventEmitter } from 'node:events';
import { afterEach, beforeEach, describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  Http3Error,
  WARN_HTTP3_RUNTIME_FALLBACK,
} from '../../lib/index.js';
import type { RuntimeInfo } from '../../lib/runtime.js';
import {
  resetRuntimeSelectionCacheForTests,
  runWithRuntimeSelection,
  runWithRuntimeSelectionSync,
  setPendingRuntimeInfo,
} from '../../lib/runtime.js';

class RuntimeTarget extends EventEmitter {
  _runtimeInfo: RuntimeInfo | null = null;
}

function fastPathUnavailableError(): Error {
  return new Error(
    'ERR_HTTP3_FAST_PATH_UNAVAILABLE driver=io_uring syscall=io_uring_setup errno=1 reason_code=fast-path-unavailable: Operation not permitted (os error 1)',
  );
}

describe('runtime selection', () => {
  beforeEach(() => {
    resetRuntimeSelectionCacheForTests();
  });

  afterEach(() => {
    resetRuntimeSelectionCacheForTests();
  });

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

  it('caches fast-path unavailability across repeated async auto calls', async () => {
    const firstTarget = new RuntimeTarget();
    const secondTarget = new RuntimeTarget();
    const attempts: string[] = [];
    const warnings: Array<{ warning: string | Error; options?: unknown }> = [];
    const originalEmitWarning = process.emitWarning.bind(process) as typeof process.emitWarning;
    (process.emitWarning as unknown as (warning: string | Error, options?: unknown) => void) =
      (warning: string | Error, options?: unknown): void => {
        warnings.push({ warning, options });
      };

    try {
      const first = await runWithRuntimeSelection(
        firstTarget,
        {
          runtimeMode: 'auto',
          fallbackPolicy: 'warn-and-fallback',
        },
        async (mode) => {
          attempts.push(`first:${mode}`);
          if (mode === 'fast') {
            throw fastPathUnavailableError();
          }
          return 'portable-first';
        },
      );

      const second = await runWithRuntimeSelection(
        secondTarget,
        {
          runtimeMode: 'auto',
          fallbackPolicy: 'warn-and-fallback',
        },
        async (mode) => {
          attempts.push(`second:${mode}`);
          return 'portable-second';
        },
      );

      assert.strictEqual(first, 'portable-first');
      assert.strictEqual(second, 'portable-second');
      assert.deepStrictEqual(attempts, [
        'first:fast',
        'first:portable',
        'second:portable',
      ]);
      assert.strictEqual(firstTarget._runtimeInfo?.selectedMode, 'portable');
      assert.strictEqual(secondTarget._runtimeInfo?.selectedMode, 'portable');
      assert.strictEqual(secondTarget._runtimeInfo?.fastAttempt?.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
      assert.strictEqual(warnings.length, 1);
    } finally {
      process.emitWarning = originalEmitWarning;
    }
  });

  it('uses the cached fast-path failure for auto+error without reattempting fast', async () => {
    const warmupTarget = new RuntimeTarget();
    await assert.rejects(
      () => runWithRuntimeSelection(
        warmupTarget,
        {
          runtimeMode: 'auto',
          fallbackPolicy: 'error',
        },
        async (mode) => {
          assert.strictEqual(mode, 'fast');
          throw fastPathUnavailableError();
        },
      ),
      (err: unknown) => err instanceof Http3Error && err.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE,
    );

    const target = new RuntimeTarget();
    let invoked = false;

    await assert.rejects(
      () => runWithRuntimeSelection(
        target,
        {
          runtimeMode: 'auto',
          fallbackPolicy: 'error',
        },
        async () => {
          invoked = true;
          return 'unexpected';
        },
      ),
      (err: unknown) => {
        assert.strictEqual(invoked, false);
        assert.ok(err instanceof Http3Error);
        assert.strictEqual(err.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
        assert.strictEqual(err.requestedMode, 'auto');
        assert.strictEqual(err.runtimeInfo?.fastAttempt?.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
        return true;
      },
    );
  });
});
