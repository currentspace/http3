/**
 * `WasmQuicServerEventLoop` — implements `QuicServerEventLoopLike`
 * (`lib/quic-stream.ts`) over the `http3-wasm` core (`core-loader.ts`) and a
 * `DatagramServerTransport` (`datagram-server-transport.ts`).
 *
 * The raw-QUIC server sibling of `h3-server-event-loop.ts`
 * (`WasmH3ServerEventLoop`) — mirrors it exactly (same pump/close/
 * buffer-lifetime discipline, same bound/multi-peer-socket adaptation) with
 * the `qs_*` ABI prefix instead of `hs_*`, minus `sendResponseHeaders`/
 * `sendResponse`/`sendTrailers`/`getRemoteSettings` (no H3 framing/QPACK in
 * raw QUIC — `QuicServerEventLoopLike` itself only declares `streamSend`/
 * `streamClose`, a much narrower surface than `ServerEventLoopLike`).
 *
 * Like its H3 sibling, this file does not literally `implements
 * QuicServerEventLoopLike` — see that file's module doc comment for why
 * (the `lib/wasm/**` ESLint zone forbids importing `lib/quic-stream.ts`'s
 * home file `lib/event-loop.ts`-adjacent types; structural typing via
 * bivariant method-parameter checking is what makes this work anyway).
 *
 * Host-agnostic by construction — see `h3-server-event-loop.ts`'s identical
 * note. `lib/quic-server.ts` builds the `core`/`transportFactory` via
 * `lib/wasm/node-core-loader.ts` + `lib/wasm/node-udp-server-adapter.ts`.
 */

import type { Http3WasmCore } from './core-loader.js';
import { decodeServerEventBatch } from './events.js';
import type { WasmEvent } from './events.js';
import type { DatagramServerTransport, DatagramServerTransportAddress } from './datagram-server-transport.js';
import { buildCommonServerOptionsJson, formatLocalAddr, randomRetryTokenKeyHex } from './wasm-options.js';
import type { CommonWasmServerOptions } from './wasm-options.js';

/** See `h3-server-event-loop.ts`'s identical constant. */
const CLOSE_DRAIN_DEADLINE_MS = 2000;
const CLOSE_DRAIN_POLL_MS = 5;

/** See `h3-client-event-loop.ts`'s identical function. */
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

export interface WasmQuicServerEventLoopOptions extends CommonWasmServerOptions {
  /** An already-instantiated wasm core — see `WasmH3ServerEventLoopOptions.core`'s identical doc comment. */
  core: Http3WasmCore;
  /** ALPN protocol strings. Default (Rust-side): `["quic"]`. */
  alpn?: string[];
  /**
   * The bound-socket transport factory — required, not defaulted. See
   * `WasmH3ServerEventLoopOptions.transportFactory`'s identical doc comment.
   */
  transportFactory: (port: number, host: string) => Promise<DatagramServerTransport>;
}

/**
 * Implements the `QuicServerEventLoopLike` contract over the wasm core. See
 * the module doc comment for why this doesn't literally `implements` the
 * imported interface.
 */
export class WasmQuicServerEventLoop {
  private readonly core: Http3WasmCore;
  private readonly opts: WasmQuicServerEventLoopOptions;
  private readonly dispatch: (events: WasmEvent[]) => void;

  private handle = 0;
  private transport: DatagramServerTransport | null = null;
  private outPtrCell = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private armedAbsoluteDeadlineMs: number | null = null;
  private closed = false;
  private closePromise: Promise<void> | null = null;

  constructor(opts: WasmQuicServerEventLoopOptions, dispatch: (events: WasmEvent[]) => void) {
    this.opts = opts;
    this.dispatch = dispatch;
    this.core = opts.core;
  }

  /** Bind the server's UDP socket and construct the wasm server handle. Returns the bound local address. */
  async listen(port: number, host: string): Promise<DatagramServerTransportAddress> {
    const transport = await this.opts.transportFactory(port, host);
    this.transport = transport;
    const local = transport.localAddress();

    const optsJson: Record<string, unknown> = {
      ...buildCommonServerOptionsJson(this.opts),
      ...(this.opts.alpn && { alpn: this.opts.alpn }),
      localAddr: formatLocalAddr(local.address, local.family, local.port),
      retryTokenKeyHex: randomRetryTokenKeyHex(),
    };

    const { ptr, len } = this.core.writeUtf8(JSON.stringify(optsJson));
    const handle = this.core.exports.qs_new(ptr, len);
    this.core.free(ptr, len);

    if (handle === 0) {
      const message = this.core.readLastError(this.core.exports.qs_last_error, 0);
      await transport.close();
      this.transport = null;
      throw new Error(message);
    }

    this.handle = handle;
    this.outPtrCell = this.core.allocOutPtrCell();
    transport.onMessage((datagram, peerAddr) => {
      this.onDatagram(datagram, peerAddr);
    });

    return local;
  }

  // ---- Per-connection operations (QuicServerEventLoopLike) ----

  streamSend(connHandle: number, streamId: number, data: Uint8Array, fin: boolean): number {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.qs_stream_send(this.handle, connHandle, BigInt(streamId), ptr, len, fin ? 1 : 0));
    this.core.free(ptr, len);
    this.pump();
    return result < 0 ? 0 : result;
  }

  streamClose(connHandle: number, streamId: number, errorCode: number): void {
    this.core.exports.qs_stream_close(this.handle, connHandle, BigInt(streamId), errorCode);
    this.pump();
  }

  // ---- Additional per-connection operations (mirrors WorkerEventLoop's
  // QuicWorkerEventLoop sibling in lib/quic-server.ts — not part of
  // QuicServerEventLoopLike, which only needs streamSend/streamClose, but
  // QuicServerSession's own methods need these). ----

  closeSession(connHandle: number, errorCode: number, reason: string): void {
    const { ptr, len } = this.core.writeUtf8(reason);
    this.core.exports.qs_close_connection(this.handle, connHandle, errorCode, ptr, len);
    this.core.free(ptr, len);
    this.pump();
  }

  sendDatagram(connHandle: number, data: Uint8Array): boolean {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.qs_send_datagram(this.handle, connHandle, ptr, len));
    this.core.free(ptr, len);
    this.pump();
    return result >= 0;
  }

  getSessionMetrics(connHandle: number): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  } {
    const len = Number(this.core.exports.qs_session_metrics(this.handle, connHandle, this.outPtrCell));
    if (len <= 0) {
      throw new Error(`failed to read wasm QUIC session metrics for connHandle=${String(connHandle)}`);
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

  pingSession(connHandle: number): boolean {
    const result = Number(this.core.exports.qs_ping(this.handle, connHandle));
    this.pump();
    return result >= 0;
  }

  // ---- Whole-server lifecycle ----

  localAddress(): DatagramServerTransportAddress {
    return this.transport?.localAddress() ?? { address: '0.0.0.0', family: 'IPv4', port: 0 };
  }

  /** Graceful shutdown of the whole server. See `WasmH3ServerEventLoop.close()`'s identical doc comment. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.closePromise) return this.closePromise;
    this.closePromise = this.doClose();
    return this.closePromise;
  }

  private async doClose(): Promise<void> {
    if (this.handle !== 0) {
      this.core.exports.qs_shutdown(this.handle);
      this.pump();

      const deadline = Date.now() + CLOSE_DRAIN_DEADLINE_MS;
      while (this.core.exports.qs_is_done(this.handle) === 0 && Date.now() < deadline) {
        await sleep(CLOSE_DRAIN_POLL_MS);
        this.core.exports.qs_on_timeout(this.handle);
        this.pump();
      }

      if (this.timer) {
        clearTimeout(this.timer);
        this.timer = null;
      }
      this.core.free(this.outPtrCell, 4);
      this.core.exports.qs_free(this.handle);
      this.handle = 0;
    }

    if (this.transport) {
      await this.transport.close();
      this.transport = null;
    }
  }

  // ---- Pump discipline — mirrors h3-server-event-loop.ts's WasmH3ServerEventLoop exactly. ----

  private onDatagram(datagram: Uint8Array, peerAddr: string): void {
    if (this.handle === 0) return;
    const rxPtr = this.core.exports.qs_rx_buffer(this.handle);
    this.core.writeAt(rxPtr, datagram);
    const { ptr, len } = this.core.writeUtf8(peerAddr);
    this.core.exports.qs_recv(this.handle, datagram.length, ptr, len);
    this.core.free(ptr, len);
    this.pump();
  }

  private onTimerFire(): void {
    this.timer = null;
    // Must clear the stale deadline *before* rearmTimer()'s dedup check —
    // see WasmH3ServerEventLoop.onTimerFire()'s identical, more-detailed
    // comment (the same bug class already found and fixed in both client
    // event loops).
    this.armedAbsoluteDeadlineMs = null;
    if (this.handle === 0) return;
    this.core.exports.qs_on_timeout(this.handle);
    this.pump();
  }

  private flushSends(): void {
    if (!this.transport || this.handle === 0) return;
    for (;;) {
      const len = Number(this.core.exports.qs_next_send(this.handle));
      if (len <= 0) break;
      const txPtr = this.core.exports.qs_tx_buffer(this.handle);
      const payload = this.core.copyOut(txPtr, len);

      const destLen = Number(this.core.exports.qs_next_send_dest(this.handle, this.outPtrCell));
      if (destLen <= 0) break;
      const dest = this.core.readOutPtrResultUtf8(this.outPtrCell, destLen);
      this.transport.send(payload, dest);
    }
  }

  private pump(): void {
    if (this.handle === 0) return;

    this.flushSends();

    const len = Number(this.core.exports.qs_drain_events(this.handle, this.outPtrCell));
    const json = len > 0 ? this.core.readOutPtrResultUtf8(this.outPtrCell, len) : '[]';
    const events = decodeServerEventBatch(this.core, json);

    this.flushSends();

    if (events.length > 0) {
      queueMicrotask(() => {
        this.dispatch(events);
      });
    }

    this.rearmTimer();
  }

  private rearmTimer(): void {
    if (this.handle === 0) return;
    const relativeMs = Number(this.core.exports.qs_timeout_ms(this.handle));

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
