import { Duplex } from 'node:stream';
import type { Http3Session } from './session.js';
import type { ServerEventLoopLike, ClientEventLoopLike } from './event-loop.js';
import {
  type BackpressureState,
  BoundedStreamWrite,
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
type EndChunk = Buffer | Uint8Array | string;

function resolveEndArgs(
  chunk?: EndChunk | (() => void),
  encoding?: BufferEncoding | (() => void),
  callback?: () => void,
): { finalChunk?: EndChunk; finalEncoding?: BufferEncoding; finalCallback?: () => void } {
  if (typeof chunk === 'function') {
    return { finalCallback: chunk };
  }
  if (typeof encoding === 'function') {
    return { finalChunk: chunk, finalCallback: encoding };
  }
  return { finalChunk: chunk, finalEncoding: encoding, finalCallback: callback };
}

function bufferFromEndChunk(chunk: EndChunk, encoding?: BufferEncoding): Buffer {
  if (Buffer.isBuffer(chunk)) return chunk;
  if (chunk instanceof Uint8Array) return Buffer.from(chunk);
  return Buffer.from(chunk, encoding);
}

/** HTTP header map where each value is a string or string array. */
export type IncomingHeaders = Record<string, string | string[]>;

/** Flags indicating the stream state when headers are received. */
export interface StreamFlags {
  /** True if the sender has finished sending data (no more body follows). */
  endStream: boolean;
}

/** Options for {@link ServerHttp3Stream.respond}. */
export interface RespondOptions {
  /** If true, send headers with FIN (no body will follow). */
  endStream?: boolean;
}

/** @internal Append a header value while preserving duplicate field lines. */
export function appendIncomingHeader(headers: IncomingHeaders, name: string, value: string): void {
  if (!Object.prototype.hasOwnProperty.call(headers, name)) {
    headers[name] = value;
    return;
  }
  const current = headers[name];
  if (Array.isArray(current)) {
    current.push(value);
    return;
  }
  headers[name] = [current, value];
}

/** @internal Convert native header pairs to an `IncomingHeaders` map. */
export function nativeHeadersToIncomingHeaders(
  headers: Array<{ name: string; value: string }>,
): IncomingHeaders {
  const out: IncomingHeaders = {};
  for (const header of headers) {
    appendIncomingHeader(out, header.name, header.value);
  }
  return out;
}

/** @internal Convert `IncomingHeaders` into native header pairs, preserving arrays. */
export function incomingHeadersToNativeHeaders(
  headers: IncomingHeaders,
): Array<{ name: string; value: string }> {
  const out: Array<{ name: string; value: string }> = [];
  for (const [name, value] of Object.entries(headers)) {
    if (Array.isArray(value)) {
      for (const item of value) {
        out.push({ name, value: item });
      }
      continue;
    }
    out.push({ name, value });
  }
  return out;
}

/**
 * Typed event declarations for {@link ServerHttp3Stream}.
 */
export interface ServerHttp3Stream {
  on(event: 'data', listener: (chunk: Buffer) => void): this;
  on(event: 'end', listener: () => void): this;
  on(event: 'drain', listener: () => void): this;
  on(event: 'trailers', listener: (trailers: IncomingHeaders) => void): this;
  on(event: 'timeout', listener: () => void): this;
  on(event: 'aborted', listener: () => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * A server-side HTTP/3 request/response stream (Duplex).
 *
 * The readable side receives the request body; the writable side sends
 * the response body.  Call {@link respond} before writing.
 *
 * @example
 * ```ts
 * stream.respond({ ':status': '200', 'content-type': 'text/plain' });
 * stream.end('Hello, HTTP/3!');
 * ```
 */
export class ServerHttp3Stream extends Duplex {
  /** The connection that owns this request stream, including peer address metadata. */
  session: Http3Session | null = null;
  /** @internal Fetch adapter receive spill limit; raw streams retain their existing contract. */
  _maxBufferedReadBytes = Infinity;
  /** @internal */ _connHandle = -1;
  /** @internal */ _streamId = -1;
  /** @internal */ _eventLoop: ServerEventLoopLike | null = null;
  /** @internal */ _headersSent = false;
  /** @internal */ _finSent = false;
  /**
   * Set by EVENT_STREAM_BLOCKED (audit #8/#16/#29). Native worker emits
   * this when a chunk could not be fully accepted by quiche flow control
   * and was buffered into pending_writes. While true, `_writeChunk` and
   * `_final` route directly to drainCallbacks instead of calling
   * streamSend — surfacing real backpressure so `Duplex.write()` returns
   * false. Cleared by EVENT_DRAIN.
   * @internal
   */
  _blocked = false;
  /** @internal */ _bp: BackpressureState | null = createBackpressureState();
  /** @internal */ _nativeWriteWindow = createNativeWriteWindow(this.writableHighWaterMark);
  /** @internal */ _timeoutMs = 0;
  /** @internal */ _timeout: NodeJS.Timeout | null = null;
  private _uncreditedReadBytes = 0;
  private _creditRetry: NodeJS.Immediate | undefined;
  private _closeSent = false;
  private _finalChunk: Buffer | null = null;
  private _boundedWrite: BoundedStreamWrite | null = null;
  private _queuedWasmTrailers: Array<{ name: string; value: string }> | null = null;
  private _trailerCallback: ((error?: Error | null) => void) | null = null;
  private _trailerContinuation: NodeJS.Immediate | undefined;

  /** The HTTP/3 stream ID. */
  get id(): number { return this._streamId; }

  /**
   * Send response headers to the client.
   *
   * @example
   * ```ts
   * stream.respond({ ':status': '200', 'content-type': 'application/json' });
   * ```
   */
  respond(headers: IncomingHeaders, options?: RespondOptions): void {
    if (this._headersSent) return;
    this._headersSent = true;

    const h = incomingHeadersToNativeHeaders(headers);
    // Audit finding #17: auto-inject :status: 200 when the caller didn't
    // supply one. Brings H3 to parity with the H2 adapter (which already
    // defaults to 200 in toHttp2OutgoingHeaders) and avoids a quiche-
    // level rejection that surfaces only as a generic stream error.
    if (!h.some((entry) => entry.name === ':status')) {
      h.unshift({ name: ':status', value: '200' });
    }

    this._eventLoop?.sendResponseHeaders(
      this._connHandle,
      this._streamId,
      h,
      options?.endStream ?? false,
    );
  }

  /**
   * Send response headers, body, and FIN in a single NAPI call.
   * This is an optimization for the common respond-then-end pattern.
   * Equivalent to `respond(headers); end(body);` but avoids 2 extra
   * NAPI boundary crossings.
   */
  respondWithBody(headers: IncomingHeaders, body: Buffer | string): void {
    if (this._headersSent) return;
    const buf = typeof body === 'string' ? Buffer.from(body) : body;
    const limit = this._eventLoop?.maxStreamSendBytes;
    if (limit && buf.length > limit) {
      try {
        this.respond(headers);
        this.end(buf);
      } catch (error) {
        this.destroy(error instanceof Error ? error : new Error(String(error)));
      }
      return;
    }
    this._headersSent = true;
    this._finSent = true;

    const h = incomingHeadersToNativeHeaders(headers);
    if (!h.some((entry) => entry.name === ':status')) {
      h.unshift({ name: ':status', value: '200' });
    }

    try {
      this._eventLoop?.sendResponse(
        this._connHandle,
        this._streamId,
        h,
        buf,
        true,
      );
    } catch (error) {
      this.destroy(error instanceof Error ? error : new Error(String(error)));
      return;
    }

    // End the Duplex writable side. _final will be a no-op since _finSent is true.
    this.end();
  }

  /** Send trailing headers after the response body is complete. */
  sendTrailers(trailers: IncomingHeaders): void {
    const headers = incomingHeadersToNativeHeaders(trailers);
    if (this._eventLoop?.maxStreamSendBytes && !this.writableEnded &&
        (this.writableLength > 0 || this._boundedWrite || this._blocked)) {
      // WASM may still hold most of a large write in JS. Finalize the writable
      // queue before sending trailing HEADERS (which also carry FIN).
      this._queuedWasmTrailers = headers;
      this.end();
      return;
    }
    this._finSent = true;
    this._eventLoop?.sendTrailers(this._connHandle, this._streamId, headers);
  }

  private _flushWasmTrailers(): void {
    this._trailerContinuation = undefined;
    if (!this._trailerCallback || !this._queuedWasmTrailers) return;
    if (this._blocked) {
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push(error => {
        if (error) this._completeWasmTrailers(error);
        else this._flushWasmTrailers();
      });
      return;
    }
    try {
      this._eventLoop?.sendTrailers(this._connHandle, this._streamId, this._queuedWasmTrailers);
      this._completeWasmTrailers();
    } catch (error) {
      this._completeWasmTrailers(error instanceof Error ? error : new Error(String(error)));
    }
  }

  private _completeWasmTrailers(error?: Error | null): void {
    clearImmediate(this._trailerContinuation);
    this._trailerContinuation = undefined;
    this._queuedWasmTrailers = null;
    const callback = this._trailerCallback;
    this._trailerCallback = null;
    callback?.(error);
  }

  /**
   * Close this stream, optionally sending an HTTP/3 error code.
   * @param code - HTTP/3 error code (default `0` / H3_NO_ERROR).
   */
  close(code?: number): void {
    if (this.destroyed) return;
    this._closeTransport(code ?? 0);
    rejectDrainCallbacks(this._bp, new Error('stream closed'));
    this._clearTimeout();
    this.destroy();
  }

  /** @internal Terminate remaining transport work once, preserving the first close code. */
  _closeTransport(code: number): void {
    if (this._closeSent) return;
    this._closeSent = true;
    this._eventLoop?.streamClose(this._connHandle, this._streamId, code);
  }

  /**
   * Set an inactivity timeout on this stream.
   * @param ms - Timeout in milliseconds; 0 disables.
   * @param cb - Optional callback invoked on timeout (equivalent to `stream.once('timeout', cb)`).
   */
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
    this._blocked = false;
    fireDrainCallbacks(this._bp);
  }

  /** @internal — native command queue has room; do not clear QUIC flow-control state. */
  _onNativeWriteReady(): void {
    this._onActivity();
    fireDrainCallbacks(this._bp);
  }

  /**
   * @internal — called by event dispatcher when the native worker has
   * buffered a chunk into pending_writes (quiche flow control). Future
   * writes route to drainCallbacks until EVENT_DRAIN clears the flag.
   */
  _onNativeBlocked(): void {
    this._blocked = true;
  }

  _read(_size: number): void {
    this._onActivity();
    drainPendingReads(this, this._bp);
    this._scheduleReadCredit();
  }

  /** @internal — push data respecting Readable backpressure. */
  _pushData(chunk: Buffer | null): void {
    this._uncreditedReadBytes += chunk?.length ?? 0;
    this._bp = pushData(this, this._bp, chunk, this._maxBufferedReadBytes);
    this._scheduleReadCredit();
  }

  private _scheduleReadCredit(): void {
    if (this._creditRetry || this.destroyed || this._bp?.readBackpressure || this._uncreditedReadBytes === 0 || !this._eventLoop?.streamReadCredit) return;
    // Coalesce a poll turn's consumed chunks without expanding the native
    // receive window. Recheck backpressure and destruction before granting.
    this._creditRetry = setImmediate(() => { this._creditRetry = undefined; this._returnReadCredit(); });
  }

  private _returnReadCredit(): void {
    if (this.destroyed || this._bp?.readBackpressure || this._uncreditedReadBytes === 0 || !this._eventLoop?.streamReadCredit) return;
    if (this._eventLoop.streamReadCredit(this._connHandle, this._streamId, this._uncreditedReadBytes)) {
      this._uncreditedReadBytes = 0;
    } else if (!this._creditRetry) {
      // A full worker command queue must delay credit, never silently lose it.
      this._creditRetry = setImmediate(() => { this._creditRetry = undefined; this._returnReadCredit(); });
    }
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (!this._eventLoop) {
      callback(new Error('stream not connected'));
      return;
    }
    this._writeChunk(chunk, callback);
  }

  override end(
    chunk?: EndChunk | (() => void),
    encoding?: BufferEncoding | (() => void),
    callback?: () => void,
  ): this {
    const { finalChunk, finalEncoding, finalCallback } = resolveEndArgs(chunk, encoding, callback);
    if (finalChunk != null) {
      this._finalChunk = bufferFromEndChunk(finalChunk, finalEncoding);
    }
    return super.end(finalCallback);
  }

  private _writeBounded(chunk: Buffer, limit: number, fin: boolean, callback: (error?: Error | null) => void): void {
    this._boundedWrite = new BoundedStreamWrite(chunk, limit, fin,
      (slice, final, next) => {
        if (final) this._writeFinalChunk(slice, next);
        else this._writeChunk(slice, next);
      }, error => {
        this._boundedWrite = null;
        callback(error);
      });
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const limit = this._eventLoop?.maxStreamSendBytes;
    if (limit && chunk.length > limit) {
      this._writeBounded(chunk, limit, false, callback);
      return;
    }
    this._onActivity();
    if (this._blocked) {
      // Native is backed up — skip streamSend (it would just queue more
      // into pending_writes) and wait for EVENT_DRAIN.
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeChunk(chunk, callback);
      });
      return;
    }
    let written: number;
    try {
      written = this._eventLoop?.streamSend(
        this._connHandle,
        this._streamId,
        chunk,
        false,
      ) ?? 0;
    } catch (error) {
      callback(error instanceof Error ? error : new Error(String(error)));
      return;
    }

    if (written >= chunk.length) {
      completeNativeWrite(this._nativeWriteWindow, written, callback);
    } else {
      // Partial write or fully blocked — retry remainder on drain. If the
      // stream closes before drain, the closure is invoked with an Error
      // so the user's callback fires instead of hanging.
      const remaining = chunk.subarray(written);
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeChunk(remaining, callback);
      });
    }
  }

  private _writeFinalChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const limit = this._eventLoop?.maxStreamSendBytes;
    if (limit && chunk.length > limit) {
      this._writeBounded(chunk, limit, true, callback);
      return;
    }
    this._onActivity();
    let written: number;
    try {
      written = this._blocked
        ? 0
        : this._eventLoop?.streamSend(this._connHandle, this._streamId, chunk, true) ?? 0;
    } catch (error) {
      callback(error instanceof Error ? error : new Error(String(error)));
      return;
    }

    if (chunk.length === 0) {
      if (written > 0) {
        completeNativeWrite(this._nativeWriteWindow, written, callback);
        return;
      }
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeFinalChunk(chunk, callback);
      });
      return;
    }

    if (written >= chunk.length) {
      completeNativeWrite(this._nativeWriteWindow, written, callback);
      return;
    }

    const remaining = chunk.subarray(written);
    this._bp = ensureBackpressureState(this._bp);
    this._bp.drainCallbacks.push((err) => {
      if (err) { callback(err); return; }
      this._writeFinalChunk(remaining, callback);
    });
  }

  _final(callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (this._queuedWasmTrailers) {
      this._finSent = true;
      this._trailerCallback = callback;
      // The final body slice may have just queued a deferred BLOCKED event.
      this._trailerContinuation = setImmediate(() => this._flushWasmTrailers());
      return;
    }
    if (this._finSent || !this._eventLoop) {
      callback();
      return;
    }
    this._finSent = true;
    const finalChunk = this._takeFinalChunk();
    this._writeFinalChunk(finalChunk, callback);
  }

  /** @internal */
  protected _takeFinalChunk(): Buffer {
    const finalChunk = this._finalChunk ?? EMPTY_BUFFER;
    this._finalChunk = null;
    return finalChunk;
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

  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    this._boundedWrite?.cancel(error ?? new Error('stream destroyed'));
    this._finalChunk = null;
    this._completeWasmTrailers(error ?? new Error('stream destroyed'));
    // Automatic destruction after both halves finish must leave admitted
    // response bytes in the transport; finish is not a delivery acknowledgement.
    if (!this.readableEnded || !this.writableFinished) this._closeTransport(0x010c);

    clearImmediate(this._creditRetry);
    rejectDrainCallbacks(this._bp, error ?? new Error('stream destroyed'));
    rejectNativeWriteWindow(this._nativeWriteWindow, error ?? new Error('stream destroyed'));
    this._clearTimeout();
    callback(error);
  }
}

/**
 * Typed event declarations for {@link ClientHttp3Stream}.
 */
export interface ClientHttp3Stream {
  on(event: 'data', listener: (chunk: Buffer) => void): this;
  on(event: 'end', listener: () => void): this;
  on(event: 'drain', listener: () => void): this;
  on(event: 'response', listener: (headers: IncomingHeaders, flags: StreamFlags) => void): this;
  on(event: 'trailers', listener: (trailers: IncomingHeaders) => void): this;
  on(event: 'timeout', listener: () => void): this;
  on(event: 'aborted', listener: () => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * A client-side HTTP/3 request/response stream (Duplex).
 *
 * The writable side sends the request body; the readable side receives
 * the response body.  Response headers arrive via the `'response'` event.
 */
export class ClientHttp3Stream extends Duplex {
  /** @internal */ _streamId = -1;
  /** @internal */ _eventLoop: ClientEventLoopLike | null = null;
  /** @internal */ _bp: BackpressureState | null = createBackpressureState();
  /** @internal */ _nativeWriteWindow = createNativeWriteWindow(this.writableHighWaterMark);
  /** @internal */ _timeoutMs = 0;
  /** @internal */ _timeout: NodeJS.Timeout | null = null;
  /** @internal — see ServerHttp3Stream._blocked. */ _blocked = false;
  /** @internal */ _finSent = false;
  private _uncreditedReadBytes = 0;
  private _creditRetry: NodeJS.Immediate | undefined;
  private _closeSent = false;
  private _finalChunk: Buffer | null = null;
  private _boundedWrite: BoundedStreamWrite | null = null;
  /**
   * @internal — set after the response HEADERS arrive. Subsequent
   * HEADERS frames are treated as trailing headers and emitted as
   * `'trailers'` instead of `'response'`, matching node:http2 semantics.
   */
  _responseSeen = false;

  /** The HTTP/3 stream ID. */
  get id(): number { return this._streamId; }

  /**
   * Close this stream, optionally sending an HTTP/3 error code.
   * @param code - HTTP/3 error code (default `0` / H3_NO_ERROR).
   */
  close(code?: number): void {
    const closeCode = code ?? 0;
    if (!this._closeSent) {
      this._closeSent = true;
      this._eventLoop?.streamClose(this._streamId, closeCode);
    }
    rejectDrainCallbacks(this._bp, new Error('stream closed'));
    this._clearTimeout();
    this.destroy();
  }

  /**
   * Set an inactivity timeout on this stream.
   * @param ms - Timeout in milliseconds; 0 disables.
   * @param cb - Optional callback invoked on timeout (equivalent to `stream.once('timeout', cb)`).
   */
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
    this._blocked = false;
    fireDrainCallbacks(this._bp);
  }

  /** @internal — native command queue has room; do not clear QUIC flow-control state. */
  _onNativeWriteReady(): void {
    this._onActivity();
    fireDrainCallbacks(this._bp);
  }

  /** @internal — see ServerHttp3Stream._onNativeBlocked. */
  _onNativeBlocked(): void {
    this._blocked = true;
  }

  _read(_size: number): void {
    this._onActivity();
    drainPendingReads(this, this._bp);
    this._scheduleReadCredit();
  }

  /** @internal — push data respecting Readable backpressure. */
  _pushData(chunk: Buffer | null): void {
    this._uncreditedReadBytes += chunk?.length ?? 0;
    this._bp = pushData(this, this._bp, chunk);
    this._scheduleReadCredit();
  }

  private _scheduleReadCredit(): void {
    if (this._creditRetry || this.destroyed || this._bp?.readBackpressure || this._uncreditedReadBytes === 0 || !this._eventLoop?.streamReadCredit) return;
    // Coalesce a poll turn's consumed chunks without expanding the native
    // receive window. Recheck backpressure and destruction before granting.
    this._creditRetry = setImmediate(() => { this._creditRetry = undefined; this._returnReadCredit(); });
  }

  private _returnReadCredit(): void {
    if (this.destroyed || this._bp?.readBackpressure || this._uncreditedReadBytes === 0 || !this._eventLoop?.streamReadCredit) return;
    if (this._eventLoop.streamReadCredit(this._streamId, this._uncreditedReadBytes)) {
      this._uncreditedReadBytes = 0;
    } else if (!this._creditRetry) {
      this._creditRetry = setImmediate(() => { this._creditRetry = undefined; this._returnReadCredit(); });
    }
  }

  _write(chunk: Buffer, _encoding: string, callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (!this._eventLoop) {
      callback(new Error('stream not connected'));
      return;
    }
    this._writeChunk(chunk, callback);
  }

  override end(
    chunk?: EndChunk | (() => void),
    encoding?: BufferEncoding | (() => void),
    callback?: () => void,
  ): this {
    const { finalChunk, finalEncoding, finalCallback } = resolveEndArgs(chunk, encoding, callback);
    if (finalChunk != null) {
      this._finalChunk = bufferFromEndChunk(finalChunk, finalEncoding);
    }
    return super.end(finalCallback);
  }

  private _writeBounded(chunk: Buffer, limit: number, fin: boolean, callback: (error?: Error | null) => void): void {
    this._boundedWrite = new BoundedStreamWrite(chunk, limit, fin,
      (slice, final, next) => {
        if (final) this._writeFinalChunk(slice, next);
        else this._writeChunk(slice, next);
      }, error => {
        this._boundedWrite = null;
        callback(error);
      });
  }

  private _writeChunk(chunk: Buffer, callback: (error?: Error | null) => void): void {
    const limit = this._eventLoop?.maxStreamSendBytes;
    if (limit && chunk.length > limit) {
      this._writeBounded(chunk, limit, false, callback);
      return;
    }
    this._onActivity();
    if (this._blocked) {
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeChunk(chunk, callback);
      });
      return;
    }
    let written: number;
    try {
      written = this._eventLoop?.streamSend(this._streamId, chunk, false) ?? 0;
    } catch (error) {
      callback(error instanceof Error ? error : new Error(String(error)));
      return;
    }
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
    const limit = this._eventLoop?.maxStreamSendBytes;
    if (limit && chunk.length > limit) {
      this._writeBounded(chunk, limit, true, callback);
      return;
    }
    this._onActivity();
    let written: number;
    try {
      written = this._blocked
        ? 0
        : this._eventLoop?.streamSend(this._streamId, chunk, true) ?? 0;
    } catch (error) {
      callback(error instanceof Error ? error : new Error(String(error)));
      return;
    }

    if (chunk.length === 0) {
      if (written > 0) {
        completeNativeWrite(this._nativeWriteWindow, written, callback);
        return;
      }
      this._bp = ensureBackpressureState(this._bp);
      this._bp.drainCallbacks.push((err) => {
        if (err) { callback(err); return; }
        this._writeFinalChunk(chunk, callback);
      });
      return;
    }

    if (written >= chunk.length) {
      completeNativeWrite(this._nativeWriteWindow, written, callback);
      return;
    }

    const remaining = chunk.subarray(written);
    this._bp = ensureBackpressureState(this._bp);
    this._bp.drainCallbacks.push((err) => {
      if (err) { callback(err); return; }
      this._writeFinalChunk(remaining, callback);
    });
  }

  _final(callback: (error?: Error | null) => void): void {
    this._onActivity();
    if (this._finSent || !this._eventLoop) {
      callback();
      return;
    }
    this._finSent = true;
    const finalChunk = this._finalChunk ?? EMPTY_BUFFER;
    this._finalChunk = null;
    this._writeFinalChunk(finalChunk, callback);
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

  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    this._boundedWrite?.cancel(error ?? new Error('stream destroyed'));
    this._finalChunk = null;
    if (!this._closeSent && (!this.readableEnded || !this.writableFinished)) {
      this._closeSent = true;
      this._eventLoop?.streamClose(this._streamId, 0x010c);
    }
    clearImmediate(this._creditRetry);

    rejectDrainCallbacks(this._bp, error ?? new Error('stream destroyed'));
    rejectNativeWriteWindow(this._nativeWriteWindow, error ?? new Error('stream destroyed'));
    this._clearTimeout();
    callback(error);
  }
}
