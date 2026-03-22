import { Duplex } from 'node:stream';
import {
  type BackpressureState,
  createBackpressureState,
  pushData,
  drainPendingReads,
  fireDrainCallbacks,
  flushDrainCallbacks,
} from './stream-backpressure.js';

/**
 * Event loop interface for QUIC server-side stream commands.
 * @internal
 */
export interface QuicServerEventLoopLike {
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number;
  streamClose(connHandle: number, streamId: number, errorCode: number): void;
}

/**
 * Event loop interface for QUIC client-side stream commands.
 * @internal
 */
export interface QuicClientEventLoopLike {
  streamSend(streamId: number, data: Buffer, fin: boolean): number;
  streamClose(streamId: number, errorCode: number): boolean;
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
  /** @internal */ _bp: BackpressureState = createBackpressureState();
  private _finalChunk: Buffer | null = null;

  constructor(opts?: { highWaterMark?: number }) {
    super(opts?.highWaterMark != null ? { highWaterMark: opts.highWaterMark } : undefined);
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
    flushDrainCallbacks(this._bp);
    this.destroy();
  }

  /** @internal — called by event dispatcher when flow control window opens */
  _onNativeDrain(): void {
    fireDrainCallbacks(this._bp);
  }

  _read(_size: number): void {
    drainPendingReads(this, this._bp);
  }

  /** @internal — push data respecting Readable backpressure. */
  _pushData(chunk: Buffer | null): void {
    pushData(this, this._bp, chunk);
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._writeChunk(chunk, callback);
  }

  override end(chunk?: any, encoding?: any, callback?: any): this {
    let finalChunk = chunk;
    let finalEncoding = encoding;
    let finalCallback = callback;

    if (typeof finalChunk === 'function') {
      finalCallback = finalChunk;
      finalChunk = undefined;
      finalEncoding = undefined;
    } else if (typeof finalEncoding === 'function') {
      finalCallback = finalEncoding;
      finalEncoding = undefined;
    }

    if (finalChunk != null) {
      if (Buffer.isBuffer(finalChunk)) {
        this._finalChunk = finalChunk;
      } else if (finalChunk instanceof Uint8Array) {
        this._finalChunk = Buffer.from(finalChunk);
      } else {
        this._finalChunk = Buffer.from(String(finalChunk), finalEncoding);
      }
      return (super.end as any)(undefined, undefined, finalCallback);
    }

    return (super.end as any)(undefined, undefined, finalCallback);
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const written = this._doSend(chunk, false);
    if (written >= chunk.length) {
      callback();
    } else {
      const remaining = chunk.subarray(written);
      this._bp.drainCallbacks.push(() => {
        this._writeChunk(remaining, callback);
      });
    }
  }

  private _writeFinalChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const written = this._doSend(chunk, true);
    if (chunk.length === 0) {
      if (written > 0) {
        callback();
      } else {
        this._bp.drainCallbacks.push(() => {
          this._writeFinalChunk(chunk, callback);
        });
      }
      return;
    }
    if (written >= chunk.length) {
      callback();
    } else {
      this._bp.drainCallbacks.push(() => {
        this._writeFinalChunk(chunk.subarray(written), callback);
      });
    }
  }

  _final(callback: (error?: Error | null) => void): void {
    const finalChunk = this._finalChunk ?? Buffer.alloc(0);
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
}
