import { constants as http2Constants } from 'node:http2';
import type { IncomingHttpHeaders, OutgoingHttpHeaders, ServerHttp2Stream } from 'node:http2';
import { ServerHttp3Stream } from './stream.js';
import type { IncomingHeaders, RespondOptions } from './stream.js';
import {
  rejectDrainCallbacks,
  rejectNativeWriteWindow,
} from './stream-backpressure.js';

function errorCode(error: unknown): string | undefined {
  const code = (error as { code?: unknown }).code;
  return typeof code === 'string' ? code : undefined;
}

function isExpectedH2CloseError(error: unknown): boolean {
  const code = errorCode(error);
  if (
    code === 'ERR_STREAM_WRITE_AFTER_END' ||
    code === 'ERR_STREAM_DESTROYED' ||
    code === 'ERR_HTTP2_STREAM_CLOSED' ||
    code === 'ERR_HTTP2_INVALID_STREAM'
  ) {
    return true;
  }

  if (!(error instanceof Error)) return false;
  return (
    error.message.includes('write after end') ||
    error.message.includes('stream closed') ||
    error.message.includes('stream destroyed')
  );
}

function firstHeaderValue(value: string | string[]): string {
  return Array.isArray(value) ? value[0] : value;
}

/** @internal Convert node:http2 incoming headers to the flat `IncomingHeaders` map. */
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

/** @internal Convert `IncomingHeaders` to node:http2 outgoing header format. */
export function toHttp2OutgoingHeaders(headers: IncomingHeaders): OutgoingHttpHeaders {
  const out: OutgoingHttpHeaders = {};
  for (const [name, value] of Object.entries(headers)) {
    const singleValue = firstHeaderValue(value);
    if (name === ':status') {
      const status = Number.parseInt(singleValue, 10);
      out[name] = Number.isFinite(status) ? status : 200;
      continue;
    }
    out[name] = value;
  }
  return out;
}

/**
 * Adapter that wraps a `node:http2` {@link ServerHttp2Stream} as a
 * {@link ServerHttp3Stream}, enabling transparent H2/H3 fallback.
 * @internal
 */
export class ServerHttp2StreamAdapter extends ServerHttp3Stream {
  private readonly _h2Stream: ServerHttp2Stream;
  private _pendingTrailers: OutgoingHttpHeaders | null = null;
  private _waitingForTrailers = false;
  private _h2Closed = false;
  private _h2Aborted = false;

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
      this._h2Aborted = true;
      this.emit('aborted');
      this._closeAdapter();
    });
    this._h2Stream.on('wantTrailers', () => {
      this._waitingForTrailers = true;
      this._flushPendingTrailers();
    });
    this._h2Stream.on('drain', () => {
      this.emit('drain');
    });
    this._h2Stream.on('error', (err: Error) => {
      this._h2Closed = true;
      if (isExpectedH2CloseError(err)) {
        this._closeAdapter();
        return;
      }
      this.destroy(err);
    });
    this._h2Stream.on('close', () => {
      this._h2Closed = true;
      this._closeAdapter();
    });
  }

  private _h2WritableClosed(): boolean {
    return (
      this._h2Closed ||
      this._h2Aborted ||
      this.destroyed ||
      this._h2Stream.closed ||
      this._h2Stream.destroyed ||
      this._h2Stream.writableEnded
    );
  }

  private _closeAdapter(): void {
    this._h2Closed = true;
    rejectDrainCallbacks(this._bp, new Error('stream closed'));
    rejectNativeWriteWindow(this._nativeWriteWindow, new Error('stream closed'));
    if (!this.destroyed) this.destroy();
  }

  private _finishExpectedClose(callback: (error?: Error | null) => void): void {
    this._closeAdapter();
    callback();
  }

  private _waitForH2Drain(callback: (error?: Error | null) => void): void {
    const cleanup = (): void => {
      this._h2Stream.off('drain', onDrain);
      this._h2Stream.off('close', onClose);
      this._h2Stream.off('aborted', onClose);
      this._h2Stream.off('error', onError);
    };
    const onDrain = (): void => {
      cleanup();
      callback();
    };
    const onClose = (): void => {
      cleanup();
      this._finishExpectedClose(callback);
    };
    const onError = (err: Error): void => {
      cleanup();
      if (isExpectedH2CloseError(err)) {
        this._finishExpectedClose(callback);
        return;
      }
      callback(err);
    };

    this._h2Stream.once('drain', onDrain);
    this._h2Stream.once('close', onClose);
    this._h2Stream.once('aborted', onClose);
    this._h2Stream.once('error', onError);
  }

  override respond(headers: IncomingHeaders, options?: RespondOptions): void {
    if (this._headersSent) return;
    if (this._h2WritableClosed()) {
      this._headersSent = true;
      this._closeAdapter();
      return;
    }
    try {
      this._h2Stream.respond(toHttp2OutgoingHeaders(headers), {
        endStream: options?.endStream ?? false,
        waitForTrailers: true,
      });
      this._headersSent = true;
    } catch (err: unknown) {
      if (isExpectedH2CloseError(err)) {
        this._headersSent = true;
        this._closeAdapter();
        return;
      }
      throw err;
    }
  }

  override sendTrailers(trailers: IncomingHeaders): void {
    this._pendingTrailers = toHttp2OutgoingHeaders(trailers);
    this._flushPendingTrailers();
  }

  private _flushPendingTrailers(): void {
    if (!this._waitingForTrailers) return;
    const trailers = this._pendingTrailers ?? {};
    try {
      if (this._h2WritableClosed()) {
        this._closeAdapter();
        return;
      }
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
    rejectDrainCallbacks(this._bp, new Error('stream closed'));
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
    if (this._h2WritableClosed()) {
      this._finishExpectedClose(callback);
      return;
    }
    if (!this._headersSent) {
      this.respond({ ':status': '200' });
    }
    if (this._h2WritableClosed()) {
      this._finishExpectedClose(callback);
      return;
    }
    let written: boolean;
    try {
      written = this._h2Stream.write(chunk);
    } catch (err: unknown) {
      if (isExpectedH2CloseError(err)) {
        this._finishExpectedClose(callback);
        return;
      }
      callback(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    if (written) {
      callback();
      return;
    }
    this._waitForH2Drain(callback);
  }

  override _final(callback: (error?: Error | null) => void): void {
    if (this._finSent) {
      callback();
      return;
    }
    this._finSent = true;
    if (!this._headersSent) {
      this.respond({ ':status': '200' });
    }
    const finalChunk = this._takeFinalChunk();
    const finish = (): void => {
      if (this._h2WritableClosed()) {
        this._finishExpectedClose(callback);
        return;
      }
      try {
        this._h2Stream.end(() => {
          callback();
        });
      } catch (err: unknown) {
        if (isExpectedH2CloseError(err)) {
          this._finishExpectedClose(callback);
          return;
        }
        callback(err instanceof Error ? err : new Error(String(err)));
      }
    };

    if (finalChunk.length === 0) {
      finish();
      return;
    }

    try {
      if (this._h2WritableClosed()) {
        this._finishExpectedClose(callback);
        return;
      }
      if (this._h2Stream.write(finalChunk)) {
        finish();
        return;
      }
      this._waitForH2Drain((err?: Error | null) => {
        if (err) {
          callback(err);
          return;
        }
        finish();
      });
    } catch (err: unknown) {
      if (isExpectedH2CloseError(err)) {
        this._finishExpectedClose(callback);
        return;
      }
      callback(err instanceof Error ? err : new Error(String(err)));
    }
  }
}
