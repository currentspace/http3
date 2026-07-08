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
 * there: this class's method surface matches the interface shape exactly.
 */

import { loadHttp3WasmCoreFromFile } from './core-loader.js';
import type { Http3WasmCore } from './core-loader.js';
import { decodeEventBatch, drainKeylog } from './events.js';
import type { WasmEvent } from './events.js';
import { connectNodeUdp } from './node-udp-adapter.js';
import type { DatagramTransport } from './datagram-transport.js';
import { buildCommonOptionsJson, formatLocalAddr, parseSocketAddress, randomScidHex } from './wasm-options.js';
import type { CommonWasmClientOptions } from './wasm-options.js';
import type { ShimOptions } from './wasi-shim.js';

/** Must match `lib/event-loop.ts`'s `EVENT_SHUTDOWN_COMPLETE` sentinel. */
const EVENT_SHUTDOWN_COMPLETE = 15;

/** Bounded wait for `close()`'s "pump until is_done" step — must comfortably beat `lib/event-loop.ts`'s 5 s `SHUTDOWN_TIMEOUT_MS` fallback. */
const CLOSE_DRAIN_DEADLINE_MS = 2000;
const CLOSE_DRAIN_POLL_MS = 5;

/** Feature-detects `unref()` before calling it — workerd's `nodejs_compat` coverage for timer `unref` is unverified (docs/WASM_CLIENT_PLAN.md §9 C15). */
function unrefIfSupported(timer: NodeJS.Timeout): void {
  if (typeof timer.unref === 'function') timer.unref();
}

async function sleep(ms: number): Promise<void> {
  await new Promise<void>((resolve) => {
    const timer = setTimeout(resolve, ms);
    unrefIfSupported(timer);
  });
}

export interface WasmQuicClientEventLoopOptions extends CommonWasmClientOptions {
  /** Absolute path to the compiled `http3_client.wasm` artifact. */
  wasmPath: string;
  /** PEM-encoded client certificate chain for mutual TLS. */
  cert?: Buffer;
  /** PEM-encoded client private key for mutual TLS. */
  key?: Buffer;
  /** ALPN protocol strings. Default (Rust-side): `["quic"]`. */
  alpn?: string[];
  /** WASI shim options — the injectable clock/random hooks C5's deterministic timer tests use. */
  shim?: ShimOptions;
  /** Override the datagram transport factory (tests may substitute a mock transport). Defaults to {@link connectNodeUdp}. */
  transportFactory?: (host: string, port: number) => Promise<DatagramTransport>;
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
  private readonly onKeylog: ((line: Buffer) => void) | undefined;

  private handle = 0;
  private transport: DatagramTransport | null = null;
  private outPtrCell = 0;
  private timer: NodeJS.Timeout | null = null;
  private armedAbsoluteDeadlineMs: number | null = null;
  private closeRequested = false;
  private closePromise: Promise<void> | null = null;

  constructor(
    opts: WasmQuicClientEventLoopOptions,
    dispatch: (events: WasmEvent[]) => void,
    onKeylog?: (line: Buffer) => void,
  ) {
    this.opts = opts;
    this.dispatch = dispatch;
    this.onKeylog = onKeylog;
    this.core = loadHttp3WasmCoreFromFile(opts.wasmPath, opts.shim);
  }

  async connect(serverAddr: string, serverName: string): Promise<void> {
    const { host, port } = parseSocketAddress(serverAddr);
    const transportFactory = this.opts.transportFactory ?? connectNodeUdp;
    const transport = await transportFactory(host, port);

    if (this.closeRequested) {
      await transport.close();
      return;
    }

    this.transport = transport;
    const local = transport.localAddress();

    const optsJson = {
      ...buildCommonOptionsJson(this.opts),
      ...(this.opts.cert && { cert: this.opts.cert.toString('utf8') }),
      ...(this.opts.key && { key: this.opts.key.toString('utf8') }),
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

  streamSend(streamId: number, data: Buffer, fin: boolean): number {
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

  sendDatagram(data: Buffer): boolean {
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
