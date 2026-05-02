/**
 * Shared readable/writable backpressure primitives for QUIC and H3 streams.
 *
 * Both QuicStream and the H3 stream classes (ServerHttp3Stream,
 * ClientHttp3Stream) need identical logic for:
 *
 * 1. **Readable backpressure** — when `push()` returns false, buffer
 *    subsequent data and drain it when `_read()` fires.
 * 2. **Write-side drain** — queue callbacks when a write is flow-controlled
 *    and replay them when the native DRAIN event fires.
 *
 * This module provides standalone functions that operate on a simple
 * `BackpressureState` object.  Each stream class holds one instance and
 * delegates to these functions from its `_read`, `_pushData`, and
 * `_onNativeDrain` implementations.
 *
 * **Lazy initialization:** `createBackpressureState()` returns `null`.
 * The state is only allocated when backpressure actually occurs, saving
 * 3 JS object allocations per stream in the common no-backpressure case.
 */

import type { Duplex } from 'node:stream';

/**
 * Drain callback. Invoked with no argument on a real drain (resume the
 * pending write) or with an Error on stream close (reject the pending
 * write callback so callers don't hang). See {@link rejectDrainCallbacks}.
 */
export type DrainCallback = (err?: Error) => void;

export interface BackpressureState {
  pendingReads: Array<Buffer | null>;
  readBackpressure: boolean;
  drainCallbacks: Array<DrainCallback>;
}

export type NativeWriteCallback = (error?: Error | null) => void;

export interface NativeWriteWindow {
  highWaterMark: number;
  admittedBytes: number;
  releaseScheduled: boolean;
  delayedCallbacks: Array<NativeWriteCallback>;
}

export function createBackpressureState(): BackpressureState | null {
  return null;
}

/**
 * Local Node -> native admission window for write callbacks.
 *
 * This is intentionally not tied to peer ACKs or QUIC delivery. It keeps
 * Writable's normal highWaterMark meaningful by allowing at most one local
 * window of bytes to complete synchronously per stream per event-loop turn.
 */
export function createNativeWriteWindow(highWaterMark: number): NativeWriteWindow {
  const hwm = Number.isFinite(highWaterMark) && highWaterMark > 0
    ? Math.floor(highWaterMark)
    : 64 * 1024;
  return {
    highWaterMark: hwm,
    admittedBytes: 0,
    releaseScheduled: false,
    delayedCallbacks: [],
  };
}

function scheduleNativeWindowRelease(state: NativeWriteWindow): void {
  if (state.releaseScheduled) return;
  state.releaseScheduled = true;
  setImmediate(() => {
    state.releaseScheduled = false;
    state.admittedBytes = 0;
    const callbacks = state.delayedCallbacks.splice(0);
    for (const cb of callbacks) {
      cb();
    }
  });
}

/**
 * Complete a write after native admission while preserving a bounded local
 * write window. Small bursts up to highWaterMark complete synchronously;
 * anything beyond that waits for the next event-loop turn, so Node's own
 * Writable buffering can apply backpressure.
 */
export function completeNativeWrite(
  state: NativeWriteWindow,
  admittedBytes: number,
  callback: NativeWriteCallback,
): void {
  const size = Math.max(1, admittedBytes);
  state.admittedBytes += size;
  scheduleNativeWindowRelease(state);

  if (state.admittedBytes <= state.highWaterMark) {
    callback();
    return;
  }

  state.delayedCallbacks.push(callback);
}

/** Reject callbacks delayed by the local native-write window. */
export function rejectNativeWriteWindow(state: NativeWriteWindow, err: Error): void {
  state.admittedBytes = 0;
  const callbacks = state.delayedCallbacks.splice(0);
  for (const cb of callbacks) {
    cb(err);
  }
}

/** Materialize state on first backpressure. */
function materialize(state: BackpressureState | null): BackpressureState {
  return state ?? {
    pendingReads: [],
    readBackpressure: false,
    drainCallbacks: [],
  };
}

/**
 * Ensure a writable-side BackpressureState exists (for drain callback
 * registration).  Call sites assign the return value back to `_bp`.
 */
export function ensureBackpressureState(state: BackpressureState | null): BackpressureState {
  return materialize(state);
}

/**
 * Push data to a Duplex stream, respecting Readable backpressure.
 * Returns the (possibly newly allocated) state so the caller can
 * update its `_bp` field.
 */
export function pushData(
  stream: Duplex,
  state: BackpressureState | null,
  chunk: Buffer | null,
): BackpressureState | null {
  if (state !== null && state.readBackpressure) {
    state.pendingReads.push(chunk);
    return state;
  }
  if (!stream.push(chunk)) {
    const s = materialize(state);
    s.readBackpressure = true;
    return s;
  }
  return state;
}

/**
 * Drain buffered reads that were held back when `push()` returned false.
 * Called from `_read()`.
 */
export function drainPendingReads(stream: Duplex, state: BackpressureState | null): void {
  if (state === null) return;
  state.readBackpressure = false;
  while (state.pendingReads.length > 0) {
    // shift() within the length check always yields an element; cast to the
    // declared element type rather than a non-null assertion.
    const chunk = state.pendingReads.shift() as Buffer | null;
    if (!stream.push(chunk)) {
      // push(null) returns false too, so the EOF case naturally exits here.
      state.readBackpressure = true;
      break;
    }
  }
}

/**
 * Execute and clear all pending drain callbacks.
 * Called when the native DRAIN event fires (flow control window opens).
 */
export function fireDrainCallbacks(state: BackpressureState | null): void {
  if (state === null) return;
  const cbs = state.drainCallbacks.splice(0);
  for (const cb of cbs) {
    cb();
  }
}

/**
 * Flush all drain callbacks without waiting for a DRAIN event.
 * Used during stream close to unblock pending writes.
 */
export function flushDrainCallbacks(state: BackpressureState | null): void {
  if (state === null) return;
  for (const cb of state.drainCallbacks) {
    cb();
  }
  state.drainCallbacks.length = 0;
}

/**
 * Drop any queued drain callbacks without replaying writes.
 * Used when a stream is being destroyed and retrying writes would race teardown.
 */
export function cancelDrainCallbacks(state: BackpressureState | null): void {
  if (state === null) return;
  state.drainCallbacks.length = 0;
}

/**
 * Invoke every queued drain callback with an error so the caller's pending
 * `write(chunk, cb)` callback fires (with that error) instead of hanging.
 * Used by `stream.close()`. Audit finding #13.
 */
export function rejectDrainCallbacks(state: BackpressureState | null, err: Error): void {
  if (state === null) return;
  const cbs = state.drainCallbacks.splice(0);
  for (const cb of cbs) {
    cb(err);
  }
}
