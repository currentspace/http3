/**
 * `WasmH3ServerEventLoop` — implements `ServerEventLoopLike`
 * (`lib/event-loop.ts`) over the `http3-wasm` core (`core-loader.ts`) and a
 * `DatagramServerTransport` (`datagram-server-transport.ts`).
 *
 * This is the server-side sibling of `h3-client-event-loop.ts`
 * (`WasmH3ClientEventLoop`) — same pump/close/buffer-lifetime discipline,
 * adapted for a server handle that multiplexes many connections instead of
 * a client handle that is exactly one:
 *
 *  - The socket is bound, not connected: each inbound datagram is routed
 *    into `hs_recv(handle, len, peerAddrPtr, peerAddrLen)` with *that*
 *    datagram's sender address (`onDatagram`), and each outbound datagram
 *    is sent to whatever destination `hs_next_send_dest` reports for the
 *    packet `hs_next_send` just produced (`flushSends`) — never a fixed
 *    peer.
 *  - Every per-connection method (`sendResponseHeaders`, `streamSend`, ...)
 *    takes a `connHandle` parameter that passes straight through to the
 *    corresponding `hs_*(handle, connHandle, ...)` ABI call — there is no
 *    per-connection JS-side session object at this layer, exactly like the
 *    native `WorkerEventLoop` class (`lib/event-loop.ts`) already works:
 *    `Http3SecureServer`'s own `Map` of sessions (keyed by connHandle) is
 *    unaffected by which event loop implementation feeds it events.
 *
 * Like the client event loop, this file does not literally `implements
 * ServerEventLoopLike` — doing so would require importing
 * `lib/event-loop.ts`, which the `lib/wasm/**` ESLint zone forbids. This
 * class's method surface is *structurally* identical to `ServerEventLoopLike`
 * (bivariant method parameter checking means `Uint8Array`-typed parameters
 * here still satisfy an interface declaring `Buffer`), which is all
 * `lib/server.ts` needs to use an instance of this class wherever a
 * `ServerEventLoopLike` is expected.
 *
 * Host-agnostic by construction (mirrors the Phase 5 client design,
 * docs/WASM_CLIENT_PLAN.md §9): takes an already-instantiated
 * {@link Http3WasmCore} and a caller-supplied `transportFactory` — never
 * touches `node:fs`/`node:dgram` itself. `lib/server.ts` builds both via
 * `lib/wasm/node-core-loader.ts` + `lib/wasm/node-udp-server-adapter.ts`.
 */

import type { Http3WasmCore } from './core-loader.js';
import { decodeServerEventBatch } from './events.js';
import type { WasmEvent } from './events.js';
import type { DatagramServerTransport, DatagramServerTransportAddress } from './datagram-server-transport.js';
import { buildCommonServerOptionsJson, formatLocalAddr, randomRetryTokenKeyHex } from './wasm-options.js';
import type { CommonWasmServerOptions } from './wasm-options.js';

/** Bounded wait for `close()`'s "pump until is_done" step. There is no cross-thread sentinel to synchronize here (unlike native's WorkerEventLoop) — this loop directly drives every live connection's graceful CONNECTION_CLOSE to completion. */
const CLOSE_DRAIN_DEADLINE_MS = 2000;
const CLOSE_DRAIN_POLL_MS = 5;

/** See `h3-client-event-loop.ts`'s identical function for why the parameter is `ReturnType<typeof setTimeout>` rather than `NodeJS.Timeout`. */
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

export interface WasmH3ServerEventLoopOptions extends CommonWasmServerOptions {
  /** An already-instantiated wasm core — see `WasmH3ClientEventLoopOptions.core`'s identical doc comment. */
  core: Http3WasmCore;
  /** QPACK dynamic table capacity. */
  qpackMaxTableCapacity?: number;
  /** Maximum QPACK blocked streams. */
  qpackBlockedStreams?: number;
  /** Enable QUIC-LB connection-ID routing. Requires `serverId`. */
  quicLb?: boolean;
  /** 8-byte server identifier for QUIC-LB. */
  serverId?: Uint8Array;
  /**
   * The bound-socket transport factory — required, not defaulted (same
   * reasoning as the client's `transportFactory`: this class has no
   * built-in notion of "the Node way" to bind a socket). `lib/server.ts`
   * passes `lib/wasm/node-udp-server-adapter.ts`'s `bindNodeUdpServer`.
   */
  transportFactory: (port: number, host: string) => Promise<DatagramServerTransport>;
}

/**
 * Implements the `ServerEventLoopLike` contract over the wasm core. See the
 * module doc comment for why this doesn't literally `implements` the
 * imported interface.
 */
export class WasmH3ServerEventLoop {
  private readonly core: Http3WasmCore;
  private readonly opts: WasmH3ServerEventLoopOptions;
  private readonly dispatch: (events: WasmEvent[]) => void;

  private handle = 0;
  private transport: DatagramServerTransport | null = null;
  private outPtrCell = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private armedAbsoluteDeadlineMs: number | null = null;
  private closed = false;
  private closePromise: Promise<void> | null = null;

  /**
   * @param dispatch Receives already-decoded event batches (never
   *   dispatched synchronously from inside a command call — deferred via
   *   `queueMicrotask`, same discipline as the client event loops).
   */
  constructor(opts: WasmH3ServerEventLoopOptions, dispatch: (events: WasmEvent[]) => void) {
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
      localAddr: formatLocalAddr(local.address, local.family, local.port),
      retryTokenKeyHex: randomRetryTokenKeyHex(),
    };
    if (this.opts.qpackMaxTableCapacity !== undefined) optsJson.qpackMaxTableCapacity = this.opts.qpackMaxTableCapacity;
    if (this.opts.qpackBlockedStreams !== undefined) optsJson.qpackBlockedStreams = this.opts.qpackBlockedStreams;
    if (this.opts.quicLb !== undefined) optsJson.quicLb = this.opts.quicLb;
    if (this.opts.serverId) optsJson.serverId = Array.from(this.opts.serverId, (b) => b.toString(16).padStart(2, '0')).join('');

    const { ptr, len } = this.core.writeUtf8(JSON.stringify(optsJson));
    const handle = this.core.exports.hs_new(ptr, len);
    this.core.free(ptr, len);

    if (handle === 0) {
      const message = this.core.readLastError(this.core.exports.hs_last_error, 0);
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

  // ---- Per-connection operations (ServerEventLoopLike) ----

  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void {
    this.rawSendResponseHeaders(connHandle, streamId, headers, fin);
    this.pump();
  }

  /**
   * Headers + one body chunk in a single call — mirrors native's
   * `WorkerCommand::SendResponse` composition exactly (`src/worker.rs`):
   * headers are sent with `fin=false` (the body always follows), then the
   * body chunk carries the caller's requested `fin`. This composes cleanly
   * over the two separate ABI calls because both wrap the same underlying
   * `H3ServerHandler` methods native's single command composes internally
   * — including the blocked-headers retry path (a blocked
   * `hs_send_response_headers` already buffers a pending response
   * server-side; the following `hs_stream_send` call detects that pending
   * entry and appends its body chunk to it, exactly as
   * `H3ServerHandler::queue_stream_send`'s doc comment describes).
   */
  sendResponse(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, data: Uint8Array, fin: boolean): void {
    this.rawSendResponseHeaders(connHandle, streamId, headers, false);
    this.rawStreamSend(connHandle, streamId, data, fin);
    this.pump();
  }

  streamSend(connHandle: number, streamId: number, data: Uint8Array, fin: boolean): number {
    const result = this.rawStreamSend(connHandle, streamId, data, fin);
    this.pump();
    // Negative results (backpressure or an already-reported protocol error)
    // map to 0, matching the native streamSendOutcomeBytes convention
    // (lib/event-loop.ts) and the client wasm event loops' identical rule.
    return result < 0 ? 0 : result;
  }

  streamClose(connHandle: number, streamId: number, errorCode: number): void {
    this.core.exports.hs_stream_close(this.handle, connHandle, BigInt(streamId), errorCode);
    this.pump();
  }

  sendTrailers(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>): void {
    const { ptr, len } = this.core.writeUtf8(JSON.stringify(headers));
    this.core.exports.hs_send_trailers(this.handle, connHandle, BigInt(streamId), ptr, len);
    this.core.free(ptr, len);
    this.pump();
  }

  closeSession(connHandle: number, errorCode: number, reason: string): void {
    const { ptr, len } = this.core.writeUtf8(reason);
    this.core.exports.hs_close_connection(this.handle, connHandle, errorCode, ptr, len);
    this.core.free(ptr, len);
    this.pump();
  }

  sendDatagram(connHandle: number, data: Uint8Array): boolean {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.hs_send_datagram(this.handle, connHandle, ptr, len));
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
    const len = Number(this.core.exports.hs_session_metrics(this.handle, connHandle, this.outPtrCell));
    if (len <= 0) {
      throw new Error(`failed to read wasm H3 session metrics for connHandle=${String(connHandle)}`);
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

  getRemoteSettings(connHandle: number): Array<{ id: number; value: number }> {
    const len = Number(this.core.exports.hs_remote_settings(this.handle, connHandle, this.outPtrCell));
    if (len <= 0) return [];
    const json = this.core.readOutPtrResultUtf8(this.outPtrCell, len);
    return JSON.parse(json) as Array<{ id: number; value: number }>;
  }

  pingSession(connHandle: number): boolean {
    const result = Number(this.core.exports.hs_ping(this.handle, connHandle));
    this.pump();
    return result >= 0;
  }

  // N5: qlog is excluded from the wasm build.
  getQlogPath(_connHandle: number): string | null {
    return null;
  }

  // ---- Whole-server lifecycle ----

  localAddress(): DatagramServerTransportAddress {
    return this.transport?.localAddress() ?? { address: '0.0.0.0', family: 'IPv4', port: 0 };
  }

  /**
   * Graceful shutdown of the whole server. Unlike the native
   * `WorkerEventLoop.close()`, there is no separate worker thread whose
   * SHUTDOWN_COMPLETE sentinel must be relayed back across a TSFN boundary
   * — the wasm core runs synchronously in this same thread, so this method
   * directly drives `hs_shutdown` to completion (every live connection's
   * graceful CONNECTION_CLOSE flushed and observed) before resolving.
   * `lib/server.ts` awaits this promise directly; no dispatch-callback
   * sentinel-detection wiring is needed.
   */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.closePromise) return this.closePromise;
    this.closePromise = this.doClose();
    return this.closePromise;
  }

  private async doClose(): Promise<void> {
    if (this.handle !== 0) {
      this.core.exports.hs_shutdown(this.handle);
      this.pump();

      // Actively drive every connection's close forward each poll tick
      // (mirrors WasmH3ClientEventLoop.doClose's identical reasoning):
      // hs_on_timeout is a safe no-op when nothing is due yet.
      const deadline = Date.now() + CLOSE_DRAIN_DEADLINE_MS;
      while (this.core.exports.hs_is_done(this.handle) === 0 && Date.now() < deadline) {
        await sleep(CLOSE_DRAIN_POLL_MS);
        this.core.exports.hs_on_timeout(this.handle);
        this.pump();
      }

      if (this.timer) {
        clearTimeout(this.timer);
        this.timer = null;
      }
      this.core.free(this.outPtrCell, 4);
      this.core.exports.hs_free(this.handle);
      this.handle = 0;
    }

    if (this.transport) {
      await this.transport.close();
      this.transport = null;
    }
  }

  // ---- Internal per-connection ABI helpers (shared by sendResponseHeaders/sendResponse and streamSend/sendResponse) ----

  private rawSendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void {
    const { ptr, len } = this.core.writeUtf8(JSON.stringify(headers));
    this.core.exports.hs_send_response_headers(this.handle, connHandle, BigInt(streamId), ptr, len, fin ? 1 : 0);
    this.core.free(ptr, len);
  }

  private rawStreamSend(connHandle: number, streamId: number, data: Uint8Array, fin: boolean): number {
    const { ptr, len } = this.core.writeBytes(data);
    const result = Number(this.core.exports.hs_stream_send(this.handle, connHandle, BigInt(streamId), ptr, len, fin ? 1 : 0));
    this.core.free(ptr, len);
    return result;
  }

  // ---- Pump discipline — mirrors h3-client-event-loop.ts's WasmH3ClientEventLoop exactly, adapted for a bound/multi-peer socket. ----

  private onDatagram(datagram: Uint8Array, peerAddr: string): void {
    if (this.handle === 0) return;
    const rxPtr = this.core.exports.hs_rx_buffer(this.handle);
    this.core.writeAt(rxPtr, datagram);
    const { ptr, len } = this.core.writeUtf8(peerAddr);
    this.core.exports.hs_recv(this.handle, datagram.length, ptr, len);
    this.core.free(ptr, len);
    this.pump();
  }

  private onTimerFire(): void {
    this.timer = null;
    // Must also forget the deadline the just-fired timer was armed for
    // *before* rearmTimer() runs its dedup check — otherwise a stale
    // `armedAbsoluteDeadlineMs` can make rearmTimer() wrongly believe a
    // timer is still armed for the new deadline and skip arming a real one,
    // permanently orphaning every connection this server holds with no
    // timer left to ever recheck any of their timeouts again. This is the
    // exact bug class already found and fixed in
    // WasmH3ClientEventLoop.onTimerFire()/WasmQuicClientEventLoop.onTimerFire()
    // (see their identical, more-detailed comments) — here it is even
    // higher-stakes, since one server handle's single timer represents the
    // soonest deadline across *every* connection it holds
    // (`hs_timeout_ms`'s doc comment), not just one.
    this.armedAbsoluteDeadlineMs = null;
    if (this.handle === 0) return;
    this.core.exports.hs_on_timeout(this.handle);
    this.pump();
  }

  private flushSends(): void {
    if (!this.transport || this.handle === 0) return;
    for (;;) {
      const len = Number(this.core.exports.hs_next_send(this.handle));
      if (len <= 0) break;
      const txPtr = this.core.exports.hs_tx_buffer(this.handle);
      // Copy the payload out (a real, independent copy) *before* the next
      // ABI call on this handle (hs_next_send_dest below) — the
      // buffer-lifetime rule applies to tx_buffer's contents exactly like
      // every other scratch buffer this crate hands back a pointer to.
      const payload = this.core.copyOut(txPtr, len);

      // Unlike the client (one fixed connected peer), a server's outbound
      // packets can each be addressed to a different peer — fetch this
      // packet's destination immediately, per hs_next_send_dest's doc
      // comment ("call immediately after each non-zero return").
      const destLen = Number(this.core.exports.hs_next_send_dest(this.handle, this.outPtrCell));
      if (destLen <= 0) break;
      const dest = this.core.readOutPtrResultUtf8(this.outPtrCell, destLen);
      this.transport.send(payload, dest);
    }
  }

  private pump(): void {
    if (this.handle === 0) return;

    this.flushSends();

    const len = Number(this.core.exports.hs_drain_events(this.handle, this.outPtrCell));
    const json = len > 0 ? this.core.readOutPtrResultUtf8(this.outPtrCell, len) : '[]';
    const events = decodeServerEventBatch(this.core, json);

    // Step 2's internal flush_pending_writes may have just released
    // previously-blocked stream data — flush again so it becomes an actual
    // wire packet in this same pump (same reasoning as the client loops).
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
    const relativeMs = Number(this.core.exports.hs_timeout_ms(this.handle));

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
