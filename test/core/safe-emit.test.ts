/**
 * Unit tests for lib/safe-emit.ts and the session/stream adapters that
 * use it. These tests verify that benign peer disconnects (ECONNRESET,
 * EPIPE, etc.) routed through our adapters never crash the process when
 * the user has not (yet) attached an `'error'` listener.
 *
 * Pure unit tests — no network, no native worker.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import {
  isBenignDisconnect,
  safeEmitError,
  safeDestroyStream,
  BENIGN_DISCONNECT_CODES,
} from '../../lib/safe-emit.js';
import { Http2ServerSessionAdapter } from '../../lib/session.js';
import { ServerHttp2StreamAdapter } from '../../lib/stream.js';

function makeError(code: string, message = code): NodeJS.ErrnoException {
  const err = new Error(message) as NodeJS.ErrnoException;
  err.code = code;
  return err;
}

describe('isBenignDisconnect', () => {
  it('recognizes ECONNRESET and other peer-disconnect codes', () => {
    for (const code of BENIGN_DISCONNECT_CODES) {
      assert.equal(isBenignDisconnect(makeError(code)), true, `expected ${code} to be benign`);
    }
  });

  it('returns false for unknown errors', () => {
    assert.equal(isBenignDisconnect(makeError('EACCES')), false);
    assert.equal(isBenignDisconnect(new Error('boom')), false);
    assert.equal(isBenignDisconnect(null), false);
    assert.equal(isBenignDisconnect(undefined), false);
    assert.equal(isBenignDisconnect('ECONNRESET'), false);
  });
});

describe('safeEmitError', () => {
  it('emits peerDisconnect for benign codes regardless of listeners', () => {
    const ee = new EventEmitter();
    const seen: Error[] = [];
    ee.on('peerDisconnect', (err: Error) => seen.push(err));
    // Intentionally NO 'error' listener — this would crash with raw emit.
    assert.doesNotThrow(() => safeEmitError(ee, makeError('ECONNRESET')));
    assert.equal(seen.length, 1);
    assert.equal((seen[0] as NodeJS.ErrnoException).code, 'ECONNRESET');
  });

  it('emits error when a listener is attached', () => {
    const ee = new EventEmitter();
    const seen: Error[] = [];
    ee.on('error', (err: Error) => seen.push(err));
    safeEmitError(ee, new Error('boom'));
    assert.equal(seen.length, 1);
    assert.equal(seen[0]!.message, 'boom');
  });

  it('emits sessionError (not error) when no listener is attached', () => {
    const ee = new EventEmitter();
    const seen: Error[] = [];
    ee.on('sessionError', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => safeEmitError(ee, new Error('mystery')));
    assert.equal(seen.length, 1);
  });
});

describe('safeDestroyStream', () => {
  it('does not crash a Duplex with no error listener on benign code', () => {
    const dup = new PassThrough();
    const seen: Error[] = [];
    dup.on('peerDisconnect', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => safeDestroyStream(dup, makeError('ECONNRESET')));
    assert.equal(seen.length, 1);
    assert.equal(dup.destroyed, true);
  });

  it('forwards real errors to listeners that exist', async () => {
    const dup = new PassThrough();
    const seen: Error[] = [];
    dup.on('error', (err: Error) => seen.push(err));
    safeDestroyStream(dup, new Error('boom'));
    // Duplex.destroy(err) emits 'error' on the next tick.
    await new Promise<void>((r) => setImmediate(r));
    assert.equal(seen.length, 1);
    assert.equal(dup.destroyed, true);
  });

  it('routes unknown errors to sessionError when no error listener', () => {
    const dup = new PassThrough();
    const seen: Error[] = [];
    dup.on('sessionError', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => safeDestroyStream(dup, new Error('mystery')));
    assert.equal(seen.length, 1);
    assert.equal(dup.destroyed, true);
  });
});

/**
 * Build a fake `Http2Session`-like object: an `EventEmitter` with the
 * minimum surface area the adapter touches in its constructor.
 */
function makeFakeH2Session(): EventEmitter & {
  socket: EventEmitter & { remoteAddress: string; remotePort: number; bytesRead: number; bytesWritten: number };
} {
  const socket = Object.assign(new EventEmitter(), {
    remoteAddress: '127.0.0.1',
    remotePort: 12345,
    bytesRead: 0,
    bytesWritten: 0,
  });
  const session = Object.assign(new EventEmitter(), { socket });
  return session as never;
}

describe('Http2ServerSessionAdapter — error race regression', () => {
  it('does not crash when the underlying H2 session emits ECONNRESET with no listener', () => {
    const fake = makeFakeH2Session();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new Http2ServerSessionAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('peerDisconnect', (err: Error) => seen.push(err));
    // No 'error' listener — pre-fix this would crash the process.
    assert.doesNotThrow(() => fake.emit('error', makeError('ECONNRESET', 'read ECONNRESET')));
    assert.equal(seen.length, 1);
    assert.equal((seen[0] as NodeJS.ErrnoException).code, 'ECONNRESET');
  });

  it('does not crash when the TLS socket emits ECONNRESET directly', () => {
    const fake = makeFakeH2Session();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new Http2ServerSessionAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('peerDisconnect', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => fake.socket.emit('error', makeError('ECONNRESET')));
    assert.equal(seen.length, 1);
  });

  it('still delivers unknown errors via the error event when a listener is present', () => {
    const fake = makeFakeH2Session();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new Http2ServerSessionAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('error', (err: Error) => seen.push(err));
    fake.emit('error', new Error('protocol error'));
    assert.equal(seen.length, 1);
    assert.equal(seen[0]!.message, 'protocol error');
  });

  it('routes unknown errors to sessionError when no error listener is attached', () => {
    const fake = makeFakeH2Session();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new Http2ServerSessionAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('sessionError', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => fake.emit('error', new Error('protocol error')));
    assert.equal(seen.length, 1);
  });
});

describe('ServerHttp2StreamAdapter — error race regression', () => {
  /**
   * Build a fake `ServerHttp2Stream`-like object. It is a `Duplex`-ish
   * `EventEmitter` with the methods the adapter calls during teardown.
   */
  function makeFakeH2Stream(): EventEmitter & { close: (code?: number) => void; destroyed: boolean } {
    const stream = Object.assign(new EventEmitter(), {
      destroyed: false,
      close(_code?: number): void {
        this.destroyed = true;
      },
    });
    return stream as never;
  }

  it('does not crash when the underlying H2 stream emits ECONNRESET with no listener', () => {
    const fake = makeFakeH2Stream();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new ServerHttp2StreamAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('peerDisconnect', (err: Error) => seen.push(err));
    assert.doesNotThrow(() => fake.emit('error', makeError('ECONNRESET')));
    assert.equal(seen.length, 1);
    assert.equal(adapter.destroyed, true);
  });

  it('still delivers protocol errors via destroy(err) when a listener exists', async () => {
    const fake = makeFakeH2Stream();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const adapter = new ServerHttp2StreamAdapter(fake as any);
    const seen: Error[] = [];
    adapter.on('error', (err: Error) => seen.push(err));
    fake.emit('error', new Error('protocol violation'));
    await new Promise<void>((r) => setImmediate(r));
    assert.equal(seen.length, 1);
    assert.equal(adapter.destroyed, true);
  });
});

