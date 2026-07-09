/**
 * Node-facing integration glue between the host-agnostic `lib/wasm/**`
 * event shape and `lib/event-loop.ts`'s `NativeEvent` contract (Phase 5,
 * docs/WASM_CLIENT_PLAN.md §9).
 *
 * `lib/wasm/**` deliberately never references Node's `Buffer` global (so
 * it compiles under `tsconfig.workerd.json`, which has no `@types/node`)
 * — its event payloads (`WasmEvent.data`, keylog lines) are plain
 * `Uint8Array`s. Every existing, documented, public API in this package
 * (`'sessionTicket'`/`'datagram'` session events, stream `'data'` chunks,
 * the `'keylog'` event) is typed and has always behaved as real `Buffer`
 * instances, though — this file is the one, explicit, Node-only place
 * that bridges the two, so `lib/client.ts`/`lib/quic-client.ts` can keep
 * feeding wasm event batches into the exact same `session._dispatchEvents`
 * used for native events, with zero behavior change for any consumer.
 *
 * This file lives outside `lib/wasm/**` on purpose: it imports
 * `NativeEvent` from `lib/event-loop.ts`, which the `lib/wasm/**` ESLint
 * zone forbids (docs/WASM_CLIENT_PLAN.md §6.6) — the whole point of that
 * rule is to keep the wasm-facing surface from depending on native-only
 * types, and this bridge is exactly the native-only integration code that
 * rule is designed to push out of that directory.
 */

import type { NativeEvent } from './event-loop.js';
import type { WasmEvent } from './wasm/events.js';

/**
 * Convert a decoded wasm event batch into `NativeEvent[]`, wrapping every
 * `Uint8Array` payload in a real `Buffer.from(...)` copy.
 *
 * `meta.peerCertificateChain` is cast, not converted element-by-element:
 * neither the native nor the wasm client ever actually populates it today
 * (docs/WASM_CLIENT_PLAN.md §12 decision log — "peer_certificate_chain on
 * client HANDSHAKE_COMPLETE" is deferred on both runtimes), so there is no
 * real `Uint8Array[]` value in practice for this to silently mis-convert;
 * the cast only papers over the unused field's type until that lands.
 */
export function toNativeEvents(events: WasmEvent[]): NativeEvent[] {
  return events.map((e) => ({
    ...e,
    data: e.data ? Buffer.from(e.data) : undefined,
  })) as unknown as NativeEvent[];
}

/** Wrap a wasm keylog line (`Uint8Array`) in a real `Buffer` for the public `'keylog'` session event. */
export function toNativeKeylogLine(line: Uint8Array): Buffer {
  return Buffer.from(line);
}
