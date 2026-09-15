import { DeadlineTimer, drainUntilDone } from './lifecycle.js';
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
  transportFactory: (host: string, port: number, options?: { signal?: AbortSignal }) => Promise<DatagramTransport>;
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
  private readonly timer = new DeadlineTimer();
  private closeRequested = false;
  private readonly startupAbort = new AbortController();
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
    const transport = await this.opts.transportFactory(host, port, { signal: this.startupAbort.signal });

    if (this.closeRequested) {
      await transport.close();
      return;
    }

    try {
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
      let handle: number;
      try { handle = this.core.exports.qc_new(ptr, len); }
      finally { this.core.free(ptr, len); }

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
    } catch (err) {
      await this.close();
      throw err;
    }
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
    this.startupAbort.abort();
    if (this.closePromise) return this.closePromise;
    this.closePromise = this.doClose(errorCode, reason);
    return this.closePromise;
  }

  private async doClose(errorCode: number, reason: string): Promise<void> {
    try {
      if (this.handle !== 0) {
        const { ptr, len } = this.core.writeUtf8(reason);
        try { this.core.exports.qc_close(this.handle, errorCode, ptr, len); }
        finally { this.core.free(ptr, len); }
        this.pump();
        await drainUntilDone(
          () => this.core.exports.qc_is_done(this.handle) !== 0,
          () => { this.core.exports.qc_on_timeout(this.handle); this.pump(); },
        );
        this.dispatch([{ eventType: EVENT_SHUTDOWN_COMPLETE, connHandle: this.handle, streamId: -1 }]);
      }
    } finally {
      this.timer.cancel();
      try {
        if (this.handle !== 0) {
          this.core.free(this.outPtrCell, 4);
          this.core.exports.qc_free(this.handle);
          this.handle = 0;
          this.outPtrCell = 0;
        }
      } finally {
        const transport = this.transport;
        this.transport = null;
        await transport?.close();
      }
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
    this.timer.cancel();
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
    this.timer.arm(Number(this.core.exports.qc_timeout_ms(this.handle)), () => this.onTimerFire());
  }
}
