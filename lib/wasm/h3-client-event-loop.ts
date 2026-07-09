/**
 * `WasmClientEventLoop` (HTTP/3) — implements `ClientEventLoopLike`
 * (`lib/event-loop.ts`) over the `http3-wasm` core (`core-loader.ts`) and a
 * `DatagramTransport` (`datagram-transport.ts`). See
 * docs/WASM_CLIENT_PLAN.md §6.4 for the normative pump/close spec.
 *
 * This file does not implement `ClientEventLoopLike` by importing it —
 * doing so would require importing `lib/event-loop.ts`, which the
 * `lib/wasm/**` ESLint zone forbids (docs/WASM_CLIENT_PLAN.md §6.6).
 * `WasmH3ClientEventLoop`'s method surface is *structurally* identical to
 * `ClientEventLoopLike` (TypeScript checks method parameters bivariantly,
 * so this class's `Uint8Array`-typed parameters still satisfy an interface
 * declaring `Buffer` — Buffer instances are Uint8Array instances), which is
 * all TypeScript's structural typing needs for `lib/client-event-loop-factory.ts`
 * (outside `lib/wasm/`, free to import the real interface) to use an
 * instance of this class wherever a `ClientEventLoopLike` is expected.
 *
 * **Host-agnostic by construction (Phase 5, docs/WASM_CLIENT_PLAN.md §9):**
 * this file takes an already-instantiated {@link Http3WasmCore} and a
 * caller-supplied `transportFactory` — it never touches `node:fs` or
 * `node:dgram`, directly or transitively, so it compiles under
 * `tsconfig.workerd.json` (no `@types/node`) and is exactly what a workerd
 * host constructs against (`lib/wasm/index.workerd.ts`). The Node
 * convenience of "just give me a `.wasm` path and a UDP socket" now lives
 * one layer up, at the caller (`lib/client.ts` uses
 * `lib/wasm/node-core-loader.ts` + `lib/wasm/node-udp-adapter.ts`
 * explicitly) — see those files' doc comments for why this couldn't stay a
 * same-file `if` branch.
 */

import type { Http3WasmCore } from './core-loader.js';
import { decodeEventBatch, drainKeylog } from './events.js';
import type { WasmEvent } from './events.js';
import type { DatagramTransport } from './datagram-transport.js';
import { buildCommonOptionsJson, formatLocalAddr, parseSocketAddress, randomScidHex } from './wasm-options.js';
import type { CommonWasmClientOptions } from './wasm-options.js';

/** Must match `lib/event-loop.ts`'s `EVENT_SHUTDOWN_COMPLETE` sentinel. */
const EVENT_SHUTDOWN_COMPLETE = 15;

/** Bounded wait for `close()`'s "pump until is_done" step — must comfortably beat `lib/event-loop.ts`'s 5 s `SHUTDOWN_TIMEOUT_MS` fallback. */
const CLOSE_DRAIN_DEADLINE_MS = 2000;
const CLOSE_DRAIN_POLL_MS = 5;

/**
 * Feature-detects `unref()` before calling it. `setTimeout`'s return type
 * is host-dependent (`NodeJS.Timeout` under Node's real types, a plain
 * `number` under `@cloudflare/workers-types` — neither declares `unref` on
 * a bare `number`), so the parameter is deliberately typed as whatever
 * `setTimeout` itself returns under whichever tsconfig compiles this file,
 * then narrowed via a runtime-safe cast. Workers has no timer `unref`
 * concept at all (no threads to keep a process alive around); this is a
 * true no-op there, which is correct, not a gap (docs/WASM_CLIENT_PLAN.md
 * §9 C15 is about `nodejs_compat` *polyfill* fidelity — this code path
 * never depends on nodejs_compat, it feature-detects the real platform
 * global directly).
 */
function unrefIfSupported(timer: ReturnType<typeof setTimeout>): void {
  const maybeUnrefable = timer as unknown as { unref?: () => void };
  if (typeof maybeUnrefable.unref === 'function') maybeUnrefable.unref();
}

async function sleep(ms: number): Promise<void> {
  await new Promise<void>((resolve) => {
    const timer = setTimeout(resolve, ms);
    unrefIfSupported(timer);
  });
}

export interface WasmH3ClientEventLoopOptions extends CommonWasmClientOptions {
  /**
   * An already-instantiated wasm core. Node callers build this via
   * `lib/wasm/node-core-loader.ts`'s `loadHttp3WasmCoreFromFile`; a
   * workerd host builds it via `core-loader.ts`'s host-agnostic
   * `loadHttp3WasmCore({ module })` from its own precompiled `.wasm`
   * module import. This class never loads or compiles the module itself
   * (Phase 5, docs/WASM_CLIENT_PLAN.md §9).
   */
  core: Http3WasmCore;
  /**
   * The datagram transport factory — required, not defaulted: this class
   * has no built-in notion of "the Node way" to open a socket (that would
   * mean statically depending on `node:dgram`, which would break
   * compilation under `tsconfig.workerd.json`). Node callers pass
   * `lib/wasm/node-udp-adapter.ts`'s `connectNodeUdp` explicitly
   * (`lib/client.ts` does); a workerd host passes its own implementation
   * of `DatagramTransport` once one exists (docs/WASM_CLIENT_PLAN.md §9
   * C1); tests substitute a mock transport.
   */
  transportFactory: (host: string, port: number) => Promise<DatagramTransport>;
}

/**
 * Implements the H3 `ClientEventLoopLike` contract over the wasm core.
 * See the module doc comment for why this doesn't literally `implements`
 * the imported interface.
 */
export class WasmH3ClientEventLoop {
  private readonly core: Http3WasmCore;
  private readonly opts: WasmH3ClientEventLoopOptions;
  private readonly dispatch: (events: WasmEvent[]) => void;
  private readonly onKeylog: ((line: Uint8Array) => void) | undefined;

  private handle = 0;
  private transport: DatagramTransport | null = null;
  private outPtrCell = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private armedAbsoluteDeadlineMs: number | null = null;
  private closeRequested = false;
  private closePromise: Promise<void> | null = null;

  /**
   * @param dispatch Receives already-decoded event batches (never dispatch
   *   synchronously from inside a command call — this class always defers
   *   via `queueMicrotask`, per §6.4).
   * @param onKeylog Mirrors native's `'keylog'` session event delivery
   *   (`lib/client.ts` + `lib/keylog.ts`'s file-tailing, replaced here by
   *   draining `take_keylog` each pump) — wired by
   *   `lib/client-event-loop-factory.ts`/`lib/client.ts`, not part of the
   *   shared `ClientEventLoopLike` contract. Receives a `Uint8Array`, not a
   *   `Buffer` — `lib/client.ts`/`lib/quic-client.ts` wrap it in a real
   *   `Buffer.from(...)` before emitting the public `'keylog'` event
   *   (Phase 5, docs/WASM_CLIENT_PLAN.md §9).
   */
  constructor(
    opts: WasmH3ClientEventLoopOptions,
    dispatch: (events: WasmEvent[]) => void,
    onKeylog?: (line: Uint8Array) => void,
  ) {
    this.opts = opts;
    this.dispatch = dispatch;
    this.onKeylog = onKeylog;
    this.core = opts.core;
  }

  async connect(serverAddr: string, serverName: string): Promise<void> {
    const { host, port } = parseSocketAddress(serverAddr);
    const transport = await this.opts.transportFactory(host, port);

    if (this.closeRequested) {
      // A close() raced connect()'s async transport bind — tear down what
      // we just created instead of leaving a zombie session.
      await transport.close();
      return;
    }

    this.transport = transport;
    const local = transport.localAddress();

    const optsJson = {
      ...buildCommonOptionsJson(this.opts),
      serverAddr,
      serverName,
      localAddr: formatLocalAddr(local.address, local.family, local.port),
      scidHex: randomScidHex(),
    };

    const { ptr, len } = this.core.writeUtf8(JSON.stringify(optsJson));
    const handle = this.core.exports.h3c_new(ptr, len);
    this.core.free(ptr, len);

    if (handle === 0) {
      const message = this.core.readLastError(this.core.exports.h3c_last_error, 0);
      throw new Error(message);
    }

    this.handle = handle;
    this.outPtrCell = this.core.allocOutPtrCell();
    transport.onMessage((datagram) => {
      this.onDatagram(datagram);
    });

    // Initial pump — flushes the Initial ClientHello.
    this.pump();
  }

  sendRequest(headers: Array<{ name: string; value: string }>, fin: boolean): number {
    const { ptr, len } = this.core.writeUtf8(JSON.stringify(headers));
    const result = this.core.exports.h3c_send_request(this.handle, ptr, len, fin ? 1 : 0);
    this.core.free(ptr, len);
    this.pump();

    const streamId = Number(result);
    if (streamId < 0) {
      const message = this.core.readLastError(this.core.exports.h3c_last_error, this.handle);
      throw new Error(message);
    }
    return streamId;
  }

  streamSend(streamId: number, data: Uint8Array, fin: boolean): number {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.h3c_stream_send(this.handle, BigInt(streamId), ptr, len, fin ? 1 : 0));
    this.core.free(ptr, len);
    this.pump();
    // Negative results (backpressure or an already-reported protocol error
    // — see crates/http3-wasm/src/h3.rs's classify_send_outcome) map to 0,
    // exactly like the native streamSendOutcomeBytes convention
    // (lib/event-loop.ts): the real error detail (if any) flows through
    // the EVENT_ERROR the ABI already pushed into this pump's event batch.
    return result < 0 ? 0 : result;
  }

  streamClose(streamId: number, errorCode: number): boolean {
    const result = Number(this.core.exports.h3c_stream_close(this.handle, BigInt(streamId), errorCode));
    this.pump();
    return result >= 0;
  }

  sendDatagram(data: Uint8Array): boolean {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.h3c_send_datagram(this.handle, ptr, len));
    this.core.free(ptr, len);
    this.pump();
    return result >= 0;
  }

  getSessionMetrics(): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  } {
    const len = Number(this.core.exports.h3c_session_metrics(this.handle, this.outPtrCell));
    if (len <= 0) {
      throw new Error('failed to read wasm H3 session metrics');
    }
    const json = this.core.readOutPtrResultUtf8(this.outPtrCell, len);
    return JSON.parse(json) as {
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

  getRemoteSettings(): Array<{ id: number; value: number }> {
    const len = Number(this.core.exports.h3c_remote_settings(this.handle, this.outPtrCell));
    if (len <= 0) return [];
    const json = this.core.readOutPtrResultUtf8(this.outPtrCell, len);
    return JSON.parse(json) as Array<{ id: number; value: number }>;
  }

  ping(): boolean {
    const result = Number(this.core.exports.h3c_ping(this.handle));
    this.pump();
    return result >= 0;
  }

  // N5: qlog is excluded from the wasm build.
  getQlogPath(): string | null {
    return null;
  }

  async close(errorCode = 0, reason = 'client close'): Promise<void> {
    this.closeRequested = true;
    if (this.closePromise) return this.closePromise;
    this.closePromise = this.doClose(errorCode, reason);
    return this.closePromise;
  }

  private async doClose(errorCode: number, reason: string): Promise<void> {
    if (this.handle !== 0) {
      const { ptr, len } = this.core.writeUtf8(reason);
      this.core.exports.h3c_close(this.handle, errorCode, ptr, len);
      this.core.free(ptr, len);
      this.pump();

      // Actively drive the close forward each poll tick rather than
      // passively waiting on whatever timer this.timer happened to have
      // armed *before* close() started (which could be scheduled far in
      // the future, e.g. the connection's idle timer) — quiche only
      // transitions is_closed()/is_reapable() to true from inside
      // process_timers_for_handle/process_packet_for_handle, so without
      // forcing an on_timeout check every tick, this loop would just sleep
      // until CLOSE_DRAIN_DEADLINE_MS and fall through instead of detecting
      // the real (typically sub-100ms on loopback) closure promptly.
      // h3c_on_timeout is a safe no-op when quiche's own deadline isn't
      // due yet.
      const deadline = Date.now() + CLOSE_DRAIN_DEADLINE_MS;
      while (this.core.exports.h3c_is_done(this.handle) === 0 && Date.now() < deadline) {
        await sleep(CLOSE_DRAIN_POLL_MS);
        this.core.exports.h3c_on_timeout(this.handle);
        this.pump();
      }

      this.dispatch([{ eventType: EVENT_SHUTDOWN_COMPLETE, connHandle: this.handle, streamId: -1 }]);

      if (this.timer) {
        clearTimeout(this.timer);
        this.timer = null;
      }
      this.core.free(this.outPtrCell, 4);
      this.core.exports.h3c_free(this.handle);
      this.handle = 0;
    }

    if (this.transport) {
      await this.transport.close();
      this.transport = null;
    }
  }

  // ---- Binding-compat surface (not part of ClientEventLoopLike, kept for
  // parity with NativeWorkerClientBinding — docs/WASM_CLIENT_PLAN.md §6.4). ----

  /**
   * Test-only hook (C5 deterministic-timer tests, docs/WASM_CLIENT_PLAN.md
   * §7): force an `on_timeout` check right now instead of waiting for the
   * armed `setTimeout` to fire on its own. Deliberately does **not** call
   * any command that sends fresh data first (e.g. `ping()`): with a mocked
   * clock jumped far into the future, sending a new ack-eliciting packet
   * would let quiche observe fresh (post-jump) send activity and
   * effectively "revive" the connection's own idle-time bookkeeping,
   * defeating the point of the test. `h3c_on_timeout` is a safe no-op if
   * quiche's own deadline isn't actually due yet.
   */
  _forceTimeoutCheck(): void {
    this.onTimerFire();
  }

  /** No-op: the wasm core has no cross-thread admission queue (A2 task 2). */
  ackEventBatch(_count: number): void {
    /* intentionally empty */
  }

  requestShutdown(): boolean {
    return true;
  }

  /** No-op: there is no separate worker thread to join. */
  joinWorker(): void {
    /* intentionally empty */
  }

  localAddress(): { address: string; family: string; port: number } {
    return this.transport?.localAddress() ?? { address: '0.0.0.0', family: 'IPv4', port: 0 };
  }

  // ---- Pump discipline (§6.4 — the core correctness contract of this class) ----

  private onDatagram(datagram: Uint8Array): void {
    if (this.handle === 0) return;
    const rxPtr = this.core.exports.h3c_rx_buffer(this.handle);
    this.core.writeAt(rxPtr, datagram);
    this.core.exports.h3c_recv(this.handle, datagram.length);
    this.pump();
  }

  private onTimerFire(): void {
    this.timer = null;
    // Must also forget the deadline the just-fired timer was armed for —
    // otherwise rearmTimer()'s dedup check can compare the *new* deadline
    // pump() computes against this *stale* (already-consumed) one and
    // wrongly skip arming a real timer, silently orphaning the connection
    // with nothing left to ever recheck is_done()/is_closed() again. See
    // WasmQuicClientEventLoop's identical fix (quic-client-event-loop.ts)
    // for the full incident writeup — found via C4's
    // test/interop/quic-loopback.test.ts parameterization
    // (docs/WASM_CLIENT_PLAN.md §7).
    this.armedAbsoluteDeadlineMs = null;
    if (this.handle === 0) return;
    this.core.exports.h3c_on_timeout(this.handle);
    this.pump();
  }

  private flushSends(): void {
    if (!this.transport || this.handle === 0) return;
    for (;;) {
      const len = Number(this.core.exports.h3c_next_send(this.handle));
      if (len <= 0) break;
      const txPtr = this.core.exports.h3c_tx_buffer(this.handle);
      const payload = this.core.copyOut(txPtr, len);
      this.transport.send(payload);
    }
  }

  private pump(): void {
    if (this.handle === 0) return;

    // 1. Drain outbound datagrams already ready (from the input that
    //    triggered this pump) and hand each to transport.send().
    this.flushSends();

    // 2. drain_events: settles app/drain events + retried pending writes
    //    (crates/http3-wasm/src/h3.rs's h3c_drain_events doc comment) and
    //    returns the serialized batch. Synchronously decode the JSON and
    //    copy every dataOff/dataLen payload into fresh Uint8Arrays right
    //    now — core.copyOut inside decodeEventBatch does this before any
    //    further ABI call below.
    const len = Number(this.core.exports.h3c_drain_events(this.handle, this.outPtrCell));
    const json = len > 0 ? this.core.readOutPtrResultUtf8(this.outPtrCell, len) : '[]';
    const events = decodeEventBatch(this.core, json, this.handle);

    // 3. Flush again: step 2's internal flush_pending_writes may have just
    //    released previously-blocked stream data into quiche's send
    //    buffer — that data only becomes an actual wire packet on a
    //    subsequent next_send call, so a single pre-drain_events flush
    //    alone would strand it until some unrelated future pump.
    this.flushSends();

    // 4. Keylog (mechanism, not part of the NativeEvent contract — see the
    //    constructor doc comment).
    if (this.opts.keylog && this.onKeylog) {
      const lines = drainKeylog(this.core, this.core.exports.h3c_take_keylog, this.handle, this.outPtrCell);
      if (lines) this.onKeylog(lines);
    }

    // 5. Only after all decoding/copying above is complete may dispatch of
    //    the already-decoded batch be deferred. Dispatching synchronously
    //    from inside a command call is wrong: client.ts registers a new
    //    stream in its map only after sendRequest() returns, and an event
    //    for that stream dispatched synchronously from within
    //    sendRequest's own pump() would be dropped.
    if (events.length > 0) {
      queueMicrotask(() => {
        this.dispatch(events);
      });
    }

    // 6. Re-arm the single timer from timeout_ms (a *relative* ms count —
    //    computed to an absolute deadline here — re-armed only if that
    //    deadline actually moved by more than ~1 ms).
    this.rearmTimer();
  }

  private rearmTimer(): void {
    if (this.handle === 0) return;
    const relativeMs = Number(this.core.exports.h3c_timeout_ms(this.handle));

    if (relativeMs < 0) {
      if (this.timer) {
        clearTimeout(this.timer);
        this.timer = null;
      }
      this.armedAbsoluteDeadlineMs = null;
      return;
    }

    const absoluteDeadlineMs = Date.now() + relativeMs;
    if (this.armedAbsoluteDeadlineMs !== null && Math.abs(absoluteDeadlineMs - this.armedAbsoluteDeadlineMs) <= 1) {
      return;
    }

    if (this.timer) clearTimeout(this.timer);
    this.armedAbsoluteDeadlineMs = absoluteDeadlineMs;
    const timer = setTimeout(() => {
      this.onTimerFire();
    }, relativeMs);
    unrefIfSupported(timer);
    this.timer = timer;
  }
}
