import assert from 'node:assert/strict';
import { it } from 'node:test';
import type { RuntimeInfo } from '../../lib/runtime.js';
import { assertRuntimeInfo, parseExpectedFallback } from '../support/runtime/runtime-expectations.js';

const fast: RuntimeInfo = {
  requestedMode: 'auto', fallbackPolicy: 'warn-and-fallback', selectedMode: 'fast',
  driver: 'io_uring', fallbackOccurred: false, reasonCode: 'auto-selected-fast', fastAttempt: null,
};
const blocked: RuntimeInfo = {
  ...fast, selectedMode: 'portable', driver: 'poll', fallbackOccurred: true,
  reasonCode: 'fast-path-unavailable', errno: 1, syscall: 'io_uring_setup',
  fastAttempt: { code: 'ERR_HTTP3_FAST_PATH_UNAVAILABLE', driver: 'io_uring', errno: 1, syscall: 'io_uring_setup', message: 'Operation not permitted' },
};

it('Docker auto expectations accept true fast selection and truthful kernel-blocked fallback', () => {
  const expectedFallback = parseExpectedFallback('auto');
  assertRuntimeInfo(fast, 'auto', expectedFallback, 'available');
  assertRuntimeInfo(blocked, 'auto', expectedFallback, 'blocked');
  assert.throws(() => assertRuntimeInfo({ ...blocked, fallbackOccurred: false }, 'auto', expectedFallback, 'hidden fallback'), /auto fallback consistency/);
  assert.throws(() => assertRuntimeInfo({ ...blocked, reasonCode: 'requested-portable' }, 'auto', expectedFallback, 'wrong reason'), /auto reason/);
});

it('Docker portable and strict fast expectations remain exact', () => {
  assertRuntimeInfo({ ...blocked, requestedMode: 'portable', fallbackOccurred: false, reasonCode: 'requested-portable', fastAttempt: null }, 'portable', parseExpectedFallback('false'), 'portable');
  assertRuntimeInfo({ ...fast, requestedMode: 'fast', reasonCode: 'requested-fast' }, 'fast', parseExpectedFallback('false'), 'fast');
  assert.throws(() => assertRuntimeInfo(blocked, 'portable', parseExpectedFallback('false'), 'portable must not fallback'), /fallbackOccurred/);
  assert.throws(() => assertRuntimeInfo(blocked, 'fast', parseExpectedFallback('false'), 'fast must not fallback'), /selectedMode/);
  assert.throws(() => assertRuntimeInfo({ ...fast, driver: 'poll' }, 'fast', false, 'fast must use io_uring'), /driver/);
});

it('Docker fallback expectation parsing rejects misspellings instead of treating them as false', () => {
  assert.equal(parseExpectedFallback('true'), true);
  assert.equal(parseExpectedFallback('false'), false);
  assert.equal(parseExpectedFallback(undefined), undefined);
  assert.throws(() => parseExpectedFallback(''), /invalid fallback expectation/);
  assert.throws(() => parseExpectedFallback('atuo'), /invalid fallback expectation/);
});
