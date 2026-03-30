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

export interface BackpressureState {
  pendingReads: Array<Buffer | null>;
  readBackpressure: boolean;
  drainCallbacks: Array<() => void>;
}

export function createBackpressureState(): BackpressureState | null {
  return null;
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
    const chunk = state.pendingReads.shift()!;
    if (!stream.push(chunk)) {
      state.readBackpressure = true;
      break;
    }
    if (chunk === null) break; // EOF
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
