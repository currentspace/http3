import assert from 'node:assert';
import { EventEmitter } from 'node:events';
import { afterEach, beforeEach, describe, it } from 'node:test';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  Http3Error,
  WARN_HTTP3_RUNTIME_FALLBACK,
} from '../../lib/index.js';
import type { RuntimeInfo } from '../../lib/runtime.js';
import {
  resetRuntimeSelectionCacheForTests,
  resolveRuntimeSelection,
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

function unavailable(): Http3Error {
  return new Http3Error('native binding unavailable for linux x64 (musl)', ERR_HTTP3_RUNTIME_UNSUPPORTED, {
    reasonCode: 'native-binding-unavailable',
    nativeBinding: { platform: 'linux', arch: 'x64', libc: 'musl', packageName: '@currentspace/http3-linux-x64-musl', category: 'missing', message: 'missing binary' },
    cause: new Error('original loader failure'),
  });
}

describe('native availability selection', () => {
  beforeEach(() => resetRuntimeSelectionCacheForTests());
  afterEach(() => resetRuntimeSelectionCacheForTests());

  it('skips the native probe for explicit WASM under either policy', () => {
    for (const fallbackPolicy of ['error', 'warn-and-fallback'] as const) {
      const decision = resolveRuntimeSelection(new RuntimeTarget(), { runtimeMode: 'wasm', fallbackPolicy }, () => assert.fail('native probe'));
      assert.deepEqual(decision, { wasm: true });
    }
  });

  for (const runtimeMode of ['auto', 'fast', 'portable'] as const) {
    it(`preserves typed loader details and causes for strict ${runtimeMode}`, () => {
      const target = new RuntimeTarget();
      const error = unavailable();
      assert.throws(() => resolveRuntimeSelection(target, { runtimeMode, fallbackPolicy: 'error' }, () => { throw error; }), (actual: unknown) => {
        assert.ok(actual instanceof Http3Error);
        assert.equal(actual.code, ERR_HTTP3_RUNTIME_UNSUPPORTED);
        assert.equal(actual.cause, error);
        assert.equal(actual.nativeBinding?.libc, 'musl');
        assert.equal(actual.runtimeInfo?.selectedMode, null);
        assert.equal(actual.runtimeInfo?.reasonCode, 'native-binding-unavailable');
        return true;
      });
    });
  }

  it('does not convert unrelated unsupported errors into fallback', () => {
    const error = new Http3Error('invalid configuration', ERR_HTTP3_RUNTIME_UNSUPPORTED);
    assert.throws(() => resolveRuntimeSelection(new RuntimeTarget(), {}, () => { throw error; }), actual => actual === error);
  });

  it('selects WASM once per target, warns once per cause, and keeps fastAttempt empty', async () => {
    const original = process.emitWarning.bind(process) as typeof process.emitWarning;
    const warnings: unknown[] = [];
    process.emitWarning = ((...args: unknown[]): void => { warnings.push(args); }) as typeof process.emitWarning;
    try {
      for (let i = 0; i < 2; i++) {
        const target = new RuntimeTarget();
        const decision = resolveRuntimeSelection(target, {}, () => { throw unavailable(); });
        const attempts: string[] = [];
        let events = 0;
        target.on('runtime', () => { events++; });
        const result = await runWithRuntimeSelection(target, {}, async mode => { attempts.push(mode); return 42; }, decision);
        assert.equal(result, 42);
        assert.deepEqual(attempts, ['wasm']);
        assert.equal(events, 1);
        assert.equal(target._runtimeInfo?.requestedMode, 'auto');
        assert.equal(target._runtimeInfo?.selectedMode, 'wasm');
        assert.equal(target._runtimeInfo?.reasonCode, 'native-binding-unavailable');
        assert.equal(target._runtimeInfo?.fallbackOccurred, true);
        assert.equal(target._runtimeInfo?.fastAttempt, null);
        assert.equal(target._runtimeInfo?.nativeBinding?.libc, 'musl');
      }
      assert.equal(warnings.length, 1);
    } finally { process.emitWarning = original; }
  });

  it('preserves WASM bootstrap failure and loader context without a success warning', async () => {
    const target = new RuntimeTarget();
    const decision = resolveRuntimeSelection(target, {}, () => { throw unavailable(); });
    const error = Object.assign(new Error('UDP bind denied'), { code: 'EACCES' });
    const original = process.emitWarning.bind(process) as typeof process.emitWarning;
    process.emitWarning = (() => assert.fail('failed bootstrap must not warn success')) as typeof process.emitWarning;
    try {
      await assert.rejects(runWithRuntimeSelection(target, {}, async () => { throw error; }, decision), (actual: unknown) => {
        assert.ok(actual instanceof Http3Error);
        assert.equal(actual.cause, error);
        assert.equal(actual.code, 'EACCES');
        assert.equal(actual.nativeBindingError, decision.nativeBindingError);
        assert.equal((actual.nativeBindingError?.cause as Error)?.message, 'original loader failure');
        assert.equal(actual.nativeBinding?.category, 'missing');
        return true;
      });
      assert.equal(target._runtimeInfo?.selectedMode, null);
      assert.equal(target._runtimeInfo?.fallbackOccurred, false);
    } finally { process.emitWarning = original; }
  });

  it('does not retry native constructor failures through WASM', async () => {
    const target = new RuntimeTarget();
    const decision = resolveRuntimeSelection(target, {}, () => ({}));
    const attempts: string[] = [];
    const error = new Error('TLS configuration failed');
    await assert.rejects(runWithRuntimeSelection(target, {}, async mode => { attempts.push(mode); throw error; }, decision), actual => actual === error);
    assert.deepEqual(attempts, ['fast']);
  });
});
