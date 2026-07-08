/**
 * `WasmClientEventLoop` (raw QUIC) — implements `QuicClientEventLoopLike`
 * (`lib/quic-stream.ts`) over the `http3-wasm` core (`core-loader.ts`) and
 * a `DatagramTransport` (`datagram-transport.ts`). Mirrors
 * `h3-client-event-loop.ts` exactly (same pump discipline, same reasoning)
 * with the `qc_*` ABI prefix instead of `h3c_*`, minus `sendRequest`/
 * `getRemoteSettings` (raw QUIC has no HTTP/3 SETTINGS), plus `openStream`.
 * See docs/WASM_CLIENT_PLAN.md §6.4.
 *
 * As with the H3 variant, this does not literally `implements
 * QuicClientEventLoopLike` — that would require importing
 * `lib/quic-stream.ts`'s interface only, which is fine, but the concrete
 * class stays decoupled from `lib/event-loop.ts` (forbidden in
 * `lib/wasm/**` by ESLint) via the same structural-typing argument used
 * there: this class's method surface matches the interface shape exactly
 * (TypeScript checks method parameters bivariantly, so `Uint8Array`
 * parameters here still satisfy an interface declaring `Buffer`).
 *
 * **Host-agnostic by construction (Phase 5, docs/WASM_CLIENT_PLAN.md §9):**
 * see `h3-client-event-loop.ts`'s identical note — this file takes an
 * already-instantiated {@link Http3WasmCore} and a caller-supplied
 * `transportFactory`, never touching `node:fs`/`node:dgram` itself.
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
 * Feature-detects `unref()` before calling it. See
 * `h3-client-event-loop.ts`'s identical function for why the parameter is
 * typed as `ReturnType<typeof setTimeout>` (host-dependent: `NodeJS.Timeout`
 * vs. a plain `number` under `@cloudflare/workers-types`) rather than
 * `NodeJS.Timeout` directly.
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

export interface WasmQuicClientEventLoopOptions extends CommonWasmClientOptions {
  /**
   * An already-instantiated wasm core — see
   * `WasmH3ClientEventLoopOptions.core`'s identical doc comment
   * (`h3-client-event-loop.ts`).
   */
  core: Http3WasmCore;
  /** PEM-encoded client certificate chain for mutual TLS. */
  cert?: Uint8Array;
  /** PEM-encoded client private key for mutual TLS. */
  key?: Uint8Array;
  /** ALPN protocol strings. Default (Rust-side): `["quic"]`. */
  alpn?: string[];
  /**
   * The datagram transport factory — required, not defaulted. See
   * `WasmH3ClientEventLoopOptions.transportFactory`'s identical doc
   * comment (`h3-client-event-loop.ts`) for why.
   */
  transportFactory: (host: string, port: number) => Promise<DatagramTransport>;
}

/**
 * Implements the raw-QUIC `QuicClientEventLoopLike` contract over the wasm
 * core. See the module doc comment for why this doesn't literally
 * `implements` the imported interface.
 */
export class WasmQuicClientEventLoop {
  private readonly core: Http3WasmCore;
  private readonly opts: WasmQuicClientEventLoopOptions;
  private readonly dispatch: (events: WasmEvent[]) => void;
  private readonly onKeylog: ((line: Uint8Array) => void) | undefined;

  private handle = 0;
  private transport: DatagramTransport | null = null;
  private outPtrCell = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private armedAbsoluteDeadlineMs: number | null = null;
  private closeRequested = false;
  private closePromise: Promise<void> | null = null;

  constructor(
    opts: WasmQuicClientEventLoopOptions,
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
      await transport.close();
      return;
    }

    this.transport = transport;
    const local = transport.localAddress();

    const optsJson = {
      ...buildCommonOptionsJson(this.opts),
      ...(this.opts.cert && { cert: new TextDecoder('utf-8').decode(this.opts.cert) }),
      ...(this.opts.key && { key: new TextDecoder('utf-8').decode(this.opts.key) }),
      ...(this.opts.alpn && { alpn: this.opts.alpn }),
      serverAddr,
      serverName,
      localAddr: formatLocalAddr(local.address, local.family, local.port),
      scidHex: randomScidHex(),
    };

    const { ptr, len } = this.core.writeUtf8(JSON.stringify(optsJson));
    const handle = this.core.exports.qc_new(ptr, len);
    this.core.free(ptr, len);

    if (handle === 0) {
      const message = this.core.readLastError(this.core.exports.qc_last_error, 0);
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

  openStream(): number {
    const result = this.core.exports.qc_open_stream(this.handle);
    this.pump();
    const streamId = Number(result);
    if (streamId < 0) {
      const message = this.core.readLastError(this.core.exports.qc_last_error, this.handle);
      throw new Error(message);
    }
    return streamId;
  }

  streamSend(streamId: number, data: Uint8Array, fin: boolean): number {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.qc_stream_send(this.handle, BigInt(streamId), ptr, len, fin ? 1 : 0));
    this.core.free(ptr, len);
    this.pump();
    // See h3-client-event-loop.ts's identical comment: negative results
    // map to 0 (streamSendOutcomeBytes convention); real errors flow via
    // the EVENT_ERROR already pushed into this pump's event batch.
    return result < 0 ? 0 : result;
  }

  streamClose(streamId: number, errorCode: number): boolean {
    const result = Number(this.core.exports.qc_stream_close(this.handle, BigInt(streamId), errorCode));
    this.pump();
    return result >= 0;
  }

  sendDatagram(data: Uint8Array): boolean {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.qc_send_datagram(this.handle, ptr, len));
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
    const len = Number(this.core.exports.qc_session_metrics(this.handle, this.outPtrCell));
    if (len <= 0) {
      throw new Error('failed to read wasm QUIC session metrics');
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

  ping(): boolean {
    const result = Number(this.core.exports.qc_ping(this.handle));
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
      this.core.exports.qc_close(this.handle, errorCode, ptr, len);
      this.core.free(ptr, len);
      this.pump();

      // See h3-client-event-loop.ts's identical, more-commented version:
      // actively force an on_timeout check each poll tick rather than
      // passively waiting on whatever timer was armed *before* close()
      // started — qc_on_timeout is a safe no-op when not yet due.
      const deadline = Date.now() + CLOSE_DRAIN_DEADLINE_MS;
      while (this.core.exports.qc_is_done(this.handle) === 0 && Date.now() < deadline) {
        await sleep(CLOSE_DRAIN_POLL_MS);
        this.core.exports.qc_on_timeout(this.handle);
        this.pump();
      }

      this.dispatch([{ eventType: EVENT_SHUTDOWN_COMPLETE, connHandle: this.handle, streamId: -1 }]);

      if (this.timer) {
        clearTimeout(this.timer);
        this.timer = null;
      }
      this.core.free(this.outPtrCell, 4);
      this.core.exports.qc_free(this.handle);
      this.handle = 0;
    }

    if (this.transport) {
      await this.transport.close();
      this.transport = null;
    }
  }

  // ---- Binding-compat surface (not part of QuicClientEventLoopLike, kept
  // for parity with NativeQuicClientBinding — docs/WASM_CLIENT_PLAN.md §6.4). ----

  /**
   * Test-only hook (C5 deterministic-timer tests) — see the identical,
   * more-commented version on WasmH3ClientEventLoop for the full
   * rationale (why this must not send fresh data first, unlike e.g. `ping()`).
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

  // ---- Pump discipline (§6.4) — see h3-client-event-loop.ts's identical, more-commented version. ----

  private onDatagram(datagram: Uint8Array): void {
    if (this.handle === 0) return;
    const rxPtr = this.core.exports.qc_rx_buffer(this.handle);
    this.core.writeAt(rxPtr, datagram);
    this.core.exports.qc_recv(this.handle, datagram.length);
    this.pump();
  }

  private onTimerFire(): void {
    this.timer = null;
    // Must also forget the deadline the just-fired timer was armed for, not
    // only the timer handle itself — otherwise rearmTimer()'s dedup check
    // below can compare the *new* deadline `pump()` computes against this
    // *stale* (already-consumed) one. Both `process_timers_for_handle` and
    // `qc_timeout_ms` frequently recompute a fresh deadline whose absolute
    // value coincides (often to the millisecond, e.g. immediately following
    // a draining-state transition) with the one that just fired, which used
    // to make rearmTimer() believe a timer was still armed for it and skip
    // arming a real one — silently orphaning the connection with no timer
    // left to ever recheck `is_done()`/`is_closed()` again (found via C4's
    // `test/interop/quic-loopback.test.ts` parameterization,
    // docs/WASM_CLIENT_PLAN.md §7: a mismatched/missing client certificate
    // puts the connection into "draining" before the peer's
    // CONNECTION_CLOSE is fully recognized as closed, needing exactly one
    // more timer tick to finish — see the QuicClientEventLoopLike
    // counterpart in h3-client-event-loop.ts for the identical fix).
    this.armedAbsoluteDeadlineMs = null;
    if (this.handle === 0) return;
    this.core.exports.qc_on_timeout(this.handle);
    this.pump();
  }

  private flushSends(): void {
    if (!this.transport || this.handle === 0) return;
    for (;;) {
      const len = Number(this.core.exports.qc_next_send(this.handle));
      if (len <= 0) break;
      const txPtr = this.core.exports.qc_tx_buffer(this.handle);
      const payload = this.core.copyOut(txPtr, len);
      this.transport.send(payload);
    }
  }

  private pump(): void {
    if (this.handle === 0) return;

    this.flushSends();

    const len = Number(this.core.exports.qc_drain_events(this.handle, this.outPtrCell));
    const json = len > 0 ? this.core.readOutPtrResultUtf8(this.outPtrCell, len) : '[]';
    const events = decodeEventBatch(this.core, json, this.handle);

    this.flushSends();

    if (this.opts.keylog && this.onKeylog) {
      const lines = drainKeylog(this.core, this.core.exports.qc_take_keylog, this.handle, this.outPtrCell);
      if (lines) this.onKeylog(lines);
    }

    if (events.length > 0) {
      queueMicrotask(() => {
        this.dispatch(events);
      });
    }

    this.rearmTimer();
  }

  private rearmTimer(): void {
    if (this.handle === 0) return;
    const relativeMs = Number(this.core.exports.qc_timeout_ms(this.handle));

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
