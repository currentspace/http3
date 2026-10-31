import assert from 'node:assert';
import type { RuntimeInfo } from '../../../lib/runtime.js';

export type ExpectedRuntimeMode = 'auto' | 'fast' | 'portable';

export function parseExpectedFallback(value: string | undefined): boolean | undefined {
  if (value === undefined || value === 'auto') return undefined;
  assert.ok(value === 'true' || value === 'false', `invalid fallback expectation: ${value}`);
  return value === 'true';
}

// These fixtures run in Linux containers. A claimed fast pass must actually
// exercise io_uring; poll cannot silently satisfy the dedicated fast lane.
export function assertRuntimeInfo(
  actual: RuntimeInfo | null,
  expectedMode: ExpectedRuntimeMode | undefined,
  expectedFallback: boolean | undefined,
  label: string,
): void {
  assert.ok(actual, `${label} should expose runtimeInfo`);
  if (expectedMode === 'auto') {
    assert.strictEqual(actual.requestedMode, 'auto', `${label} requestedMode`);
    assert.match(actual.selectedMode ?? '', /^(fast|portable)$/, `${label} selectedMode`);
    const expectedAutoFallback = actual.selectedMode === 'portable';
    assert.strictEqual(actual.fallbackOccurred, expectedAutoFallback, `${label} auto fallback consistency`);
    assert.strictEqual(actual.reasonCode, expectedAutoFallback ? 'fast-path-unavailable' : 'auto-selected-fast', `${label} auto reason`);
  } else if (expectedMode) {
    assert.strictEqual(actual.selectedMode, expectedMode, `${label} selectedMode`);
  }
  if (expectedFallback !== undefined) {
    assert.strictEqual(actual.fallbackOccurred, expectedFallback, `${label} fallbackOccurred`);
  }
  assert.strictEqual(actual.driver, actual.selectedMode === 'fast' ? 'io_uring' : 'poll', `${label} driver`);
}
