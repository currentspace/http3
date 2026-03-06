import { Duplex } from 'node:stream';
import { constants as http2Constants } from 'node:http2';
import type { IncomingHttpHeaders, OutgoingHttpHeaders, ServerHttp2Stream } from 'node:http2';
import type { ServerEventLoopLike, ClientEventLoop } from './event-loop.js';

export type IncomingHeaders = Record<string, string | string[]>;

export interface StreamFlags {
  endStream: boolean;
}

export interface RespondOptions {
  endStream?: boolean;
}

function firstHeaderValue(value: string | string[]): string {
  return Array.isArray(value) ? value[0] : value;
}

export function normalizeIncomingHeaders(headers: IncomingHttpHeaders): IncomingHeaders {
  const normalized: IncomingHeaders = {};
  for (const [name, value] of Object.entries(headers)) {
    if (typeof value === 'undefined') continue;
    if (Array.isArray(value)) {
      normalized[name] = value;
      continue;
    }
    normalized[name] = typeof value === 'number' ? String(value) : value;
  }
  return normalized;
}

export function toHttp2OutgoingHeaders(headers: IncomingHeaders): OutgoingHttpHeaders {
  const out: OutgoingHttpHeaders = {};
  for (const [name, value] of Object.entries(headers)) {
    const singleValue = firstHeaderValue(value);
    if (name === ':status') {
      const status = Number.parseInt(singleValue, 10);
      out[name] = Number.isFinite(status) ? status : 200;
      continue;
    }
    out[name] = singleValue;
  }
  return out;
}

export class ServerHttp3Stream extends Duplex {
  /** @internal */ _connHandle = -1;
  /** @internal */ _streamId = -1;
  /** @internal */ _eventLoop: ServerEventLoopLike | null = null;
  /** @internal */ _headersSent = false;
  /** @internal */ _drainCallbacks: Array<() => void> = [];
  /** @internal */ _timeoutMs = 0;
  /** @internal */ _timeout: NodeJS.Timeout | null = null;

  get id(): number { return this._streamId; }

  respond(headers: IncomingHeaders, options?: RespondOptions): void {
    if (this._headersSent) return;
    this._headersSent = true;

    const h = Object.entries(headers).map(([name, value]) => ({
      name,
      value: Array.isArray(value) ? value[0] : value,
    }));

    this._eventLoop?.sendResponseHeaders(
      this._connHandle,
      this._streamId,
      h,
      options?.endStream ?? false,
    );
  }

  sendTrailers(trailers: IncomingHeaders): void {
    const h = Object.entries(trailers).map(([name, value]) => ({
      name,
      value: Array.isArray(value) ? value[0] : value,
    }));
    this._eventLoop?.sendTrailers(this._connHandle, this._streamId, h);
  }

  close(code?: number): void {
    this._eventLoop?.streamClose(this._connHandle, this._streamId, code ?? 0);
    // Flush any pending drain callbacks before destroying
    for (const cb of this._drainCallbacks) {
      cb();
    }
    this._drainCallbacks.length = 0;
    this._clearTimeout();
    this.destroy();
  }

  setTimeout(ms: number, cb?: () => void): this {
    if (cb) this.once('timeout', cb);
    if (!Number.isFinite(ms) || ms <= 0) {
      this._timeoutMs = 0;
      this._clearTimeout();
      return this;
    }
    this._timeoutMs = Math.floor(ms);
    this._refreshTimeout();
    return this;
  }

  /** @internal — called by event dispatcher when flow control window opens */
  _onNativeDrain(): void {
    this._onActivity();
    const cbs = this._drainCallbacks.splice(0);
    for (const cb of cbs) {
      cb();
    }
  }

  _read(_size: number): void {
    // Data is pushed by the event dispatcher — no pull needed
    this._onActivity();
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (!this._eventLoop) {
      callback(new Error('stream not connected'));
      return;
    }
    this._writeChunk(chunk, callback);
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    this._onActivity();
    const written = this._eventLoop?.streamSend(
      this._connHandle,
      this._streamId,
      chunk,
      false,
    ) ?? 0;

    if (written >= chunk.length) {
      callback();
    } else {
      // Partial write or fully blocked — retry remainder on drain
      const remaining = chunk.subarray(written);
      this._drainCallbacks.push(() => {
        this._writeChunk(remaining, callback);
      });
    }
  }

  _final(callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (!this._eventLoop) {
      callback();
      return;
    }
    const written = this._eventLoop.streamSend(
      this._connHandle,
      this._streamId,
      Buffer.alloc(0),
      true,
    );
    if (written === 0) {
      this._drainCallbacks.push(() => {
        this._eventLoop?.streamSend(
          this._connHandle,
          this._streamId,
          Buffer.alloc(0),
          true,
        );
        callback();
      });
    } else {
      callback();
    }
  }

  /** @internal */
  _onActivity(): void {
    this._refreshTimeout();
  }

  private _refreshTimeout(): void {
    if (this._timeoutMs <= 0) return;
    this._clearTimeout();
    this._timeout = setTimeout(() => {
      this.emit('timeout');
    }, this._timeoutMs);
    this._timeout.unref();
  }

  private _clearTimeout(): void {
    if (!this._timeout) return;
    clearTimeout(this._timeout);
    this._timeout = null;
  }
}

export class ClientHttp3Stream extends Duplex {
  /** @internal */ _streamId = -1;
  /** @internal */ _eventLoop: ClientEventLoop | null = null;
  /** @internal */ _drainCallbacks: Array<() => void> = [];
  /** @internal */ _timeoutMs = 0;
  /** @internal */ _timeout: NodeJS.Timeout | null = null;

  get id(): number { return this._streamId; }

  close(code?: number): void {
    const closeCode = code ?? 0;
    this._eventLoop?.streamClose(this._streamId, closeCode);
    for (const cb of this._drainCallbacks) {
      cb();
    }
    this._drainCallbacks.length = 0;
    this._clearTimeout();
    this.destroy();
  }

  setTimeout(ms: number, cb?: () => void): this {
    if (cb) this.once('timeout', cb);
    if (!Number.isFinite(ms) || ms <= 0) {
      this._timeoutMs = 0;
      this._clearTimeout();
      return this;
    }
    this._timeoutMs = Math.floor(ms);
    this._refreshTimeout();
    return this;
  }

  /** @internal */
  _onNativeDrain(): void {
    this._onActivity();
    const cbs = this._drainCallbacks.splice(0);
    for (const cb of cbs) {
      cb();
    }
  }

  _read(_size: number): void {
    // Data is pushed by the event dispatcher
    this._onActivity();
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (!this._eventLoop) {
      callback(new Error('stream not connected'));
      return;
    }
    this._writeChunk(chunk, callback);
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    this._onActivity();
    const written = this._eventLoop?.streamSend(this._streamId, chunk, false) ?? 0;
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
    this._onActivity();
    if (!this._eventLoop) {
      callback();
      return;
    }
    const written = this._eventLoop.streamSend(this._streamId, Buffer.alloc(0), true);
    if (written === 0) {
      this._drainCallbacks.push(() => {
        this._eventLoop?.streamSend(this._streamId, Buffer.alloc(0), true);
        callback();
      });
    } else {
      callback();
    }
  }

  /** @internal */
  _onActivity(): void {
    this._refreshTimeout();
  }

  private _refreshTimeout(): void {
    if (this._timeoutMs <= 0) return;
    this._clearTimeout();
    this._timeout = setTimeout(() => {
      this.emit('timeout');
    }, this._timeoutMs);
    this._timeout.unref();
  }

  private _clearTimeout(): void {
    if (!this._timeout) return;
    clearTimeout(this._timeout);
    this._timeout = null;
  }
}

export class ServerHttp2StreamAdapter extends ServerHttp3Stream {
  private readonly _h2Stream: ServerHttp2Stream;
  private _pendingTrailers: OutgoingHttpHeaders | null = null;
  private _waitingForTrailers = false;

  constructor(h2Stream: ServerHttp2Stream) {
    super();
    this._h2Stream = h2Stream;
    this._bindH2Events();
  }

  private _bindH2Events(): void {
    this._h2Stream.on('data', (chunk: Buffer) => {
      this.push(Buffer.from(chunk));
    });
    this._h2Stream.on('end', () => {
      this.push(null);
    });
    this._h2Stream.on('trailers', (trailers: IncomingHttpHeaders) => {
      this.emit('trailers', normalizeIncomingHeaders(trailers));
    });
    this._h2Stream.on('aborted', () => {
      this.emit('aborted');
    });
    this._h2Stream.on('wantTrailers', () => {
      this._waitingForTrailers = true;
      this._flushPendingTrailers();
    });
    this._h2Stream.on('error', (err: Error) => {
      this.destroy(err);
    });
    this._h2Stream.on('close', () => {
      if (!this.destroyed) this.destroy();
    });
  }

  override respond(headers: IncomingHeaders, options?: RespondOptions): void {
    if (this._headersSent) return;
    this._headersSent = true;
    this._h2Stream.respond(toHttp2OutgoingHeaders(headers), {
      endStream: options?.endStream ?? false,
      waitForTrailers: true,
    });
  }

  override sendTrailers(trailers: IncomingHeaders): void {
    this._pendingTrailers = toHttp2OutgoingHeaders(trailers);
    this._flushPendingTrailers();
  }

  private _flushPendingTrailers(): void {
    if (!this._waitingForTrailers) return;
    const trailers = this._pendingTrailers ?? {};
    try {
      this._h2Stream.sendTrailers(trailers);
      this._pendingTrailers = null;
      this._waitingForTrailers = false;
    } catch {
      // sendTrailers can throw when called before the stream is ready.
    }
  }

  override close(code?: number): void {
    try {
      this._h2Stream.close(code ?? http2Constants.NGHTTP2_NO_ERROR);
    } catch {
      // Ignore close errors while cleaning up.
    }
    for (const cb of this._drainCallbacks) {
      cb();
    }
    this._drainCallbacks.length = 0;
    this.destroy();
  }

  override setTimeout(ms: number, cb?: () => void): this {
    this._h2Stream.setTimeout(ms, () => {
      this.emit('timeout');
      cb?.();
    });
    return this;
  }

  override _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    if (!this._headersSent) {
      this.respond({ ':status': '200' });
    }
    const written = this._h2Stream.write(chunk);
    if (written) {
      callback();
      return;
    }
    this._h2Stream.once('drain', () => {
      callback();
    });
  }

  override _final(callback: (error?: Error | null) => void): void {
    if (!this._headersSent) {
      this.respond({ ':status': '200' });
    }
    this._h2Stream.end(() => {
      callback();
    });
  }
}
