import { EventEmitter } from 'node:events';
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  anyEventWithTimeout,
  onceEventWithTimeout,
  withTimeoutError,
  withTimeoutValue,
} from '../support/async-race.js';

describe('async race helpers', () => {
  it('clears timeout branches when the primary promise wins', async () => {
    assert.equal(await withTimeoutValue(Promise.resolve('value'), 1000, 'timeout'), 'value');
    await assert.doesNotReject(
      withTimeoutError(Promise.resolve('value'), 1000, new Error('timeout')),
    );
  });

  it('resolves timeout branches when the primary promise does not settle', async () => {
    const value = await withTimeoutValue(new Promise<never>(() => {}), 1, 'timeout');
    assert.equal(value, 'timeout');
    await assert.rejects(
      withTimeoutError(new Promise<never>(() => {}), 1, new Error('timeout')),
      /timeout/,
    );
  });

  it('removes a one-shot event listener after the event branch wins', async () => {
    const emitter = new EventEmitter();
    const result = onceEventWithTimeout<string | null>(emitter, 'done', 1000, null);
    assert.equal(emitter.listenerCount('done'), 1);

    emitter.emit('done', 'ok');

    assert.equal(await result, 'ok');
    assert.equal(emitter.listenerCount('done'), 0);
  });

  it('removes a one-shot event listener after the timeout branch wins', async () => {
    const emitter = new EventEmitter();
    const result = onceEventWithTimeout<string>(emitter, 'done', 1, 'timeout');
    assert.equal(emitter.listenerCount('done'), 1);

    assert.equal(await result, 'timeout');
    assert.equal(emitter.listenerCount('done'), 0);
  });

  it('removes all candidate event listeners after any event wins', async () => {
    const emitter = new EventEmitter();
    const result = anyEventWithTimeout(
      emitter,
      [{ name: 'close', value: 'close' }, { name: 'error', value: 'error' }],
      1000,
      'timeout',
    );
    assert.equal(emitter.listenerCount('close'), 1);
    assert.equal(emitter.listenerCount('error'), 1);

    emitter.emit('error');

    assert.equal(await result, 'error');
    assert.equal(emitter.listenerCount('close'), 0);
    assert.equal(emitter.listenerCount('error'), 0);
  });
});
