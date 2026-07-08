/**
 * Decodes the wire format `h3c_drain_events`/`qc_drain_events` return: one
 * JSON array using the same field names as the native NAPI event contract
 * (`eventType`, `streamId`, `headers`, `fin`, `meta`, `metrics` —
 * `crates/http3-wasm/src/events.rs`), except binary payloads, which arrive
 * as `dataOff`/`dataLen` — offsets into the wasm module's own linear
 * memory, valid only until the next ABI call on the handle
 * (`crates/http3-wasm/src/lib.rs`'s buffer-lifetime rule).
 *
 * This module deliberately does NOT import `NativeEvent` from
 * `lib/event-loop.ts` — the `lib/wasm/**` ESLint zone forbids importing
 * that file (docs/WASM_CLIENT_PLAN.md §6.6). {@link WasmEvent} below is a
 * structurally identical shape, kept in sync by hand: structural typing
 * means a `(events: WasmEvent[]) => void` callback and a real
 * `(events: NativeEvent[]) => void` callback (e.g. `session._dispatchEvents`)
 * are mutually assignable, so `lib/client-event-loop-factory.ts` (outside
 * `lib/wasm/`, free to import the real type) can pass one straight through
 * with no adapter shim.
 */

import type { Http3WasmCore } from './core-loader.js';

/** Mirrors `NativeEvent` in `lib/event-loop.ts` field-for-field — keep in sync by hand. */
export interface WasmEvent {
  eventType: number;
  connHandle: number;
  streamId: number;
  headers?: Array<{ name: string; value: string }>;
  data?: Buffer;
  fin?: boolean;
  meta?: {
    errorCode?: number;
    errorReason?: string;
    errorCategory?: string;
    remoteAddr?: string;
    remotePort?: number;
    serverName?: string;
    reasonCode?: string;
    runtimeDriver?: string;
    runtimeMode?: string;
    requestedRuntimeMode?: string;
    fallbackOccurred?: boolean;
    errno?: number;
    syscall?: string;
    peerCertificatePresented?: boolean;
    peerCertificateChain?: Buffer[];
    durationMs?: number;
  };
  metrics?: {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
}

/** Raw per-event JSON shape `serde_json` produces (`crates/http3-wasm/src/events.rs`). */
interface RawWasmEventJson {
  eventType: number;
  streamId: number;
  headers?: Array<{ name: string; value: string }>;
  fin?: boolean;
  dataOff?: number;
  dataLen?: number;
  meta?: WasmEvent['meta'];
  metrics?: WasmEvent['metrics'];
}

/**
 * Decode a `drain_events` JSON array and copy every `dataOff`/`dataLen`
 * payload out of linear memory into a fresh `Buffer` **synchronously**,
 * right now — before the caller makes any further ABI call on this
 * handle. `connHandle` is stamped onto every event because the wire JSON
 * omits it: the native NAPI event batch carries a `connHandle` per event
 * for the multi-connection worker-thread case, but a wasm session is
 * always exactly one connection, so the caller (which already owns the
 * handle number) supplies it once here instead of threading it through
 * the Rust->JSON->JS round trip.
 */
export function decodeEventBatch(core: Http3WasmCore, json: string, connHandle: number): WasmEvent[] {
  const raw = JSON.parse(json) as RawWasmEventJson[];
  const events: WasmEvent[] = [];
  for (const e of raw) {
    const event: WasmEvent = {
      eventType: e.eventType,
      connHandle,
      streamId: e.streamId,
    };
    if (e.headers) event.headers = e.headers;
    if (e.fin !== undefined) event.fin = e.fin;
    if (e.meta) event.meta = e.meta;
    if (e.metrics) event.metrics = e.metrics;
    if (typeof e.dataOff === 'number' && typeof e.dataLen === 'number' && e.dataLen > 0) {
      event.data = core.copyOut(e.dataOff, e.dataLen);
    }
    events.push(event);
  }
  return events;
}

/**
 * Drain a `*_take_keylog(handle, outPtrPtr)` export (`h3c_take_keylog` or
 * `qc_take_keylog` — pass the bound export function directly; both share
 * this exact shape) and copy out any accumulated NSS-format keylog lines.
 * Returns `null` when nothing has accumulated since the last call.
 */
export function drainKeylog(
  core: Http3WasmCore,
  takeKeylogFn: (handle: number, outPtrPtr: number) => bigint,
  handle: number,
  outPtrPtr: number,
): Buffer | null {
  const len = Number(takeKeylogFn(handle, outPtrPtr));
  if (len <= 0) return null;
  return core.readOutPtrResult(outPtrPtr, len);
}
