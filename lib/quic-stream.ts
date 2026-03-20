import { Duplex } from 'node:stream';

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
  /** @internal */ _drainCallbacks: Array<() => void> = [];
  /** @internal — buffered data when push() returned false (backpressure). */
  /** @internal */ _pendingReads: Array<Buffer | null> = [];
  /** @internal */ _readBackpressure = false;

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
    for (const cb of this._drainCallbacks) {
      cb();
    }
    this._drainCallbacks.length = 0;
    this.destroy();
  }

  /** @internal — called by event dispatcher when flow control window opens */
  _onNativeDrain(): void {
    const cbs = this._drainCallbacks.splice(0);
    for (const cb of cbs) {
      cb();
    }
  }

  _read(_size: number): void {
    // Drain buffered data that was held back when push() returned false.
    this._readBackpressure = false;
    while (this._pendingReads.length > 0) {
      const chunk = this._pendingReads.shift()!;
      if (!this.push(chunk)) {
        this._readBackpressure = true;
        break;
      }
      if (chunk === null) break; // EOF
    }
  }

  /**
   * Push data respecting Node.js Readable backpressure.
   * When the internal buffer is full (push returns false), subsequent
   * chunks are queued in _pendingReads and drained when _read() fires.
   * @internal
   */
  _pushData(chunk: Buffer | null): void {
    if (this._readBackpressure) {
      this._pendingReads.push(chunk);
      return;
    }
    if (!this.push(chunk)) {
      this._readBackpressure = true;
    }
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._writeChunk(chunk, callback);
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const written = this._doSend(chunk, false);
    if (written >= chunk.length) {
      callback();
    } else {
      const remaining = chunk.subarray(written);
      this._drainCallbacks.push(() => {
        this._writeChunk(remaining, callback);
      });
    }
  }

  _final(callback: (error?: Error | null) => void): void {
    const written = this._doSend(Buffer.alloc(0), true);
    if (written === 0) {
      this._drainCallbacks.push(() => {
        this._doSend(Buffer.alloc(0), true);
        callback();
      });
    } else {
      callback();
    }
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
