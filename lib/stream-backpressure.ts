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
 */

import type { Duplex } from 'node:stream';

export interface BackpressureState {
  pendingReads: Array<Buffer | null>;
  readBackpressure: boolean;
  drainCallbacks: Array<() => void>;
}

export function createBackpressureState(): BackpressureState {
  return {
    pendingReads: [],
    readBackpressure: false,
    drainCallbacks: [],
  };
}

/**
 * Push data to a Duplex stream, respecting Readable backpressure.
 * When the internal buffer is full (`push` returns false), subsequent
 * chunks are queued and drained when `drainPendingReads` is called.
 */
export function pushData(
  stream: Duplex,
  state: BackpressureState,
  chunk: Buffer | null,
): void {
  if (state.readBackpressure) {
    state.pendingReads.push(chunk);
    return;
  }
  if (!stream.push(chunk)) {
    state.readBackpressure = true;
  }
}

/**
 * Drain buffered reads that were held back when `push()` returned false.
 * Called from `_read()`.
 */
export function drainPendingReads(stream: Duplex, state: BackpressureState): void {
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
export function fireDrainCallbacks(state: BackpressureState): void {
  const cbs = state.drainCallbacks.splice(0);
  for (const cb of cbs) {
    cb();
  }
}

/**
 * Flush all drain callbacks without waiting for a DRAIN event.
 * Used during stream close to unblock pending writes.
 */
export function flushDrainCallbacks(state: BackpressureState): void {
  for (const cb of state.drainCallbacks) {
    cb();
  }
  state.drainCallbacks.length = 0;
}

/**
 * Drop any queued drain callbacks without replaying writes.
 * Used when a stream is being destroyed and retrying writes would race teardown.
 */
export function cancelDrainCallbacks(state: BackpressureState): void {
  state.drainCallbacks.length = 0;
}
