import { Duplex } from 'node:stream';
import {
  type BackpressureState,
  createBackpressureState,
  ensureBackpressureState,
  createNativeWriteWindow,
  completeNativeWrite,
  pushData,
  drainPendingReads,
  fireDrainCallbacks,
  rejectDrainCallbacks,
  rejectNativeWriteWindow,
} from './stream-backpressure.js';

const EMPTY_BUFFER = Buffer.alloc(0);

/**
 * Event loop interface for QUIC server-side session + stream commands.
 *
 * Widened (mirroring {@link QuicClientEventLoopLike}'s identical note) to
 * cover the full surface {@link QuicServerSession} needs (not just the
 * stream-command subset {@link QuicStream} uses: `closeSession`,
 * `sendDatagram`, `getSessionMetrics`, `pingSession`, `close`), so
 * `QuicServerSession._eventLoop`/`QuicServer._eventLoop` can also be typed
 * against this interface instead of the concrete `QuicWorkerEventLoop`
 * class (`lib/quic-server.ts`) — which has private fields, so TS's class
 * typing would otherwise reject any structurally-equivalent substitute
 * (e.g. `WasmQuicServerEventLoop`).
 * @internal
 */
export interface QuicServerEventLoopLike {
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number;
  streamClose(connHandle: number, streamId: number, errorCode: number): void;
  closeSession(connHandle: number, errorCode: number, reason: string): void;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number; datagramQueueDepth: number;
  };
  pingSession(connHandle: number): boolean;
  close(): Promise<void>;
}

/**
 * Event loop interface for QUIC client-side session + stream commands.
 *
 * Widened to cover the full surface {@link QuicClientSession} needs (not
 * just the stream-command subset {@link QuicStream} uses), so
 * `QuicClientSession._eventLoop` can also be typed against this interface
 * instead of the concrete `QuicClientEventLoop` class — which has private
 * fields, so TS's class typing would otherwise reject any
 * structurally-equivalent substitute (e.g. a future non-native event loop
 * implementation).
 * @internal
 */
export interface QuicClientEventLoopLike {
  connect(serverAddr: string, serverName: string): Promise<void>;
  openStream(): number;
  streamSend(streamId: number, data: Buffer, fin: boolean): number;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number; datagramQueueDepth: number;
  };
  ping(): boolean;
  close(errorCode?: number, reason?: string): Promise<void>;
}

/**
 * A bidirectional QUIC stream exposed as a Node.js {@link Duplex}.
 *
 * Data written to the writable side is sent over QUIC; data received
 * from the peer is pushed to the readable side.  Flow control is
 * handled transparently via native drain callbacks.
 */
export class QuicStream extends Duplex {
  /** @internal */ _connHandle = -1;
  /** @internal */ _streamId = -1;
  /** @internal */ _serverLoop: QuicServerEventLoopLike | null = null;
  /** @internal */ _clientLoop: QuicClientEventLoopLike | null = null;
  /** @internal */ _bp: BackpressureState | null = createBackpressureState();
  /** @internal — see ServerHttp3Stream._blocked. */ _blocked = false;
  /** @internal */ _nativeWriteWindow = createNativeWriteWindow(this.writableHighWaterMark);
  private _finalChunk: Buffer | null = null;

  constructor(opts?: { highWaterMark?: number }) {
    super(opts?.highWaterMark != null ? { highWaterMark: opts.highWaterMark } : undefined);
    this._nativeWriteWindow.highWaterMark = this.writableHighWaterMark;
  }

  /** The QUIC stream ID assigned by the protocol (0, 1, 4, 5, ...). */
  get id(): number {
    return this._streamId;
  }

  /**
   * Gracefully close this stream, optionally sending an application error code.
   * @param code - QUIC application error code (default `0`).
   */
  close(code?: number): void {
    if (this.destroyed) return;
    const errorCode = code ?? 0;
    if (this._serverLoop) {
      this._serverLoop.streamClose(this._connHandle, this._streamId, errorCode);
    } else if (this._clientLoop) {
      this._clientLoop.streamClose(this._streamId, errorCode);
    }
    rejectDrainCallbacks(this._bp, new Error('stream closed'));
    this.destroy();
  }

  /** @internal — called by event dispatcher when flow control window opens */
  _onNativeDrain(): void {
    this._blocked = false;
    fireDrainCallbacks(this._bp);
  }

  /** @internal — native command queue has room; do not clear QUIC flow-control state. */
  _onNativeWriteReady(): void {
    fireDrainCallbacks(this._bp);
  }

  /** @internal — see ServerHttp3Stream._onNativeBlocked. */
  _onNativeBlocked(): void {
    this._blocked = true;
  }

  _read(_size: number): void {
    drainPendingReads(this, this._bp);
  }

  /** @internal — push data respecting Readable backpressure. */
  _pushData(chunk: Buffer | null): void {
    this._bp = pushData(this, this._bp, chunk);
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._writeChunk(chunk, callback);
  }

  override end(
    chunk?: Buffer | Uint8Array | string | (() => void),
    encoding?: BufferEncoding | (() => void),
    callback?: () => void,
  ): this {
    let finalChunk: Buffer | Uint8Array | string | undefined;
    let finalEncoding: BufferEncoding | undefined;
    let finalCallback: (() => void) | undefined;

    if (typeof chunk === 'function') {
      finalCallback = chunk;
    } else if (typeof encoding === 'function') {
      finalChunk = chunk;
      finalCallback = encoding;
    } else {
      finalChunk = chunk;
      finalEncoding = encoding;
      finalCallback = callback;
    }

    if (finalChunk != null) {
      if (Buffer.isBuffer(finalChunk)) {
        this._finalChunk = finalChunk;
      } else if (finalChunk instanceof Uint8Array) {
        this._finalChunk = Buffer.from(finalChunk);
      } else {
        this._finalChunk = Buffer.from(finalChunk, finalEncoding);
      }
    }

    return super.end(finalCallback);
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    if (this._blocked) {
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeChunk(chunk, callback);
      });
      return;
    }
    const written = this._doSend(chunk, false);
    if (written >= chunk.length) {
      completeNativeWrite(this._nativeWriteWindow, written, callback);
    } else {
      const remaining = chunk.subarray(written);
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeChunk(remaining, callback);
      });
    }
  }

  private _writeFinalChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const written = this._blocked ? 0 : this._doSend(chunk, true);
    if (chunk.length === 0) {
      if (written > 0) {
        completeNativeWrite(this._nativeWriteWindow, written, callback);
      } else {
        this._bp = ensureBackpressureState(this._bp);
        this._bp.drainCallbacks.push((err) => {
          if (err) { callback(err); return; }
          this._writeFinalChunk(chunk, callback);
        });
      }
      return;
    }
    if (written >= chunk.length) {
      completeNativeWrite(this._nativeWriteWindow, written, callback);
    } else {
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeFinalChunk(chunk.subarray(written), callback);
      });
    }
  }

  _final(callback: (error?: Error | null) => void): void {
    const finalChunk = this._finalChunk ?? EMPTY_BUFFER;
    this._finalChunk = null;
    this._writeFinalChunk(finalChunk, callback);
  }

  private _doSend(data: Buffer, fin: boolean): number {
    if (this._serverLoop) {
      return this._serverLoop.streamSend(this._connHandle, this._streamId, data, fin);
    }
    if (this._clientLoop) {
      return this._clientLoop.streamSend(this._streamId, data, fin);
    }
    return 0;
  }

  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    rejectDrainCallbacks(this._bp, error ?? new Error('stream destroyed'));
    rejectNativeWriteWindow(this._nativeWriteWindow, error ?? new Error('stream destroyed'));
    callback(error);
  }
}
