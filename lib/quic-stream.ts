import { Duplex } from 'node:stream';

/** Event loop interface for QUIC server-side stream commands. */
export interface QuicServerEventLoopLike {
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number;
  streamClose(connHandle: number, streamId: number, errorCode: number): void;
}

/** Event loop interface for QUIC client-side stream commands. */
export interface QuicClientEventLoopLike {
  streamSend(streamId: number, data: Buffer, fin: boolean): number;
  streamClose(streamId: number, errorCode: number): boolean;
}

/** A bidirectional QUIC stream exposed as a Node.js Duplex. */
export class QuicStream extends Duplex {
  /** @internal */ _connHandle = -1;
  /** @internal */ _streamId = -1;
  /** @internal */ _serverLoop: QuicServerEventLoopLike | null = null;
  /** @internal */ _clientLoop: QuicClientEventLoopLike | null = null;
  /** @internal */ _drainCallbacks: Array<() => void> = [];

  get id(): number {
    return this._streamId;
  }

  close(code?: number): void {
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
    // Data is pushed by the event dispatcher — no pull needed
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
