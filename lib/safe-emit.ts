/**
 * Safe error emission helpers.
 *
 * Node's `EventEmitter` treats `'error'` specially: if no listener is
 * attached when an `'error'` event is emitted, the process crashes with
 * an unhandled error. Several places in this library re-emit errors
 * sourced from the underlying `node:http2` session/stream, the native
 * worker, or the TLS socket onto adapter objects that the user receives
 * via `'session'` / `'stream'` events. There is an inherent race
 * between handing the adapter to the user and the underlying source
 * emitting an error — for routine peer disconnects (`ECONNRESET`,
 * `EPIPE`, etc.) the user has no way to attach a listener in time, and
 * the process dies.
 *
 * This module provides:
 *
 *  - {@link isBenignDisconnect} — recognises common peer-disconnect codes.
 *  - {@link safeEmitError} — emits `'error'` only when a listener is
 *    attached, otherwise routes the error to `'peerDisconnect'` (benign)
 *    or `'sessionError'` (unknown), so it can never escape to
 *    `uncaughtException`.
 *  - {@link safeDestroyStream} — the equivalent for `Duplex` adapters,
 *    where errors are normally surfaced via `destroy(err)`.
 */

import type { EventEmitter } from 'node:events';
import type { Duplex } from 'node:stream';

/**
 * Error codes that represent routine peer disconnects rather than
 * exceptional failures. These should never crash the process.
 */
export const BENIGN_DISCONNECT_CODES: ReadonlySet<string> = new Set([
  'ECONNRESET',
  'EPIPE',
  'ECANCELED',
  'ENOTCONN',
  'ETIMEDOUT',
  'ERR_HTTP2_STREAM_CANCEL',
  'ERR_HTTP2_INVALID_SESSION',
  'ERR_HTTP2_INVALID_STREAM',
  'ERR_HTTP2_SESSION_ERROR',
  'ERR_STREAM_PREMATURE_CLOSE',
]);

interface ErrorWithCode {
  code?: unknown;
}

/** Returns `true` if `err` looks like a routine client disconnect. */
export function isBenignDisconnect(err: unknown): boolean {
  if (!err || typeof err !== 'object') return false;
  const code = (err as ErrorWithCode).code;
  return typeof code === 'string' && BENIGN_DISCONNECT_CODES.has(code);
}

/**
 * Emit an error on `emitter` without ever letting it escape unhandled.
 *
 * - Benign disconnects are emitted as `'peerDisconnect'`.
 * - Errors with at least one `'error'` listener are emitted as `'error'`
 *   so existing consumers continue to receive them.
 * - Otherwise the error is emitted as `'sessionError'`, which has no
 *   special EventEmitter semantics and will not crash the process.
 */
export function safeEmitError(emitter: EventEmitter, err: Error): void {
  if (isBenignDisconnect(err)) {
    emitter.emit('peerDisconnect', err);
    return;
  }
  if (emitter.listenerCount('error') > 0) {
    emitter.emit('error', err);
    return;
  }
  emitter.emit('sessionError', err);
}

/**
 * Destroy a `Duplex` adapter without crashing when no `'error'` listener
 * is attached. Mirrors {@link safeEmitError} for stream-shaped objects.
 */
export function safeDestroyStream(stream: Duplex, err: Error): void {
  // Defensive: tests and internal callers occasionally pass minimal
  // stream-shaped mocks that only implement `destroy(err)`. Fall back
  // to a plain destroy in that case — there's nothing to crash, since
  // the mock isn't an EventEmitter.
  if (typeof (stream as { listenerCount?: unknown }).listenerCount !== 'function') {
    stream.destroy(err);
    return;
  }
  if (isBenignDisconnect(err)) {
    stream.emit('peerDisconnect', err);
    if (!stream.destroyed) stream.destroy();
    return;
  }
  if (stream.listenerCount('error') > 0) {
    stream.destroy(err);
    return;
  }
  stream.emit('sessionError', err);
  if (!stream.destroyed) stream.destroy();
}
