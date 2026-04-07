import { EventEmitter } from 'node:events';
import { connect } from './client.js';
import type { ConnectOptions } from './client.js';
import type { Http3ClientSession } from './client.js';
import type { ClientHttp3Stream, IncomingHeaders } from './stream.js';

/** Options for creating an {@link Http3EventSource}. Extends {@link ConnectOptions}. */
export interface EventSourceInit extends ConnectOptions {
  /** Extra request headers sent with the SSE GET request. */
  headers?: Record<string, string>;
  /** Whether to automatically reconnect on disconnect. Default: `true`. */
  reconnect?: boolean;
  /** Initial reconnection delay in milliseconds. Default: 1000. */
  initialRetryMs?: number;
  /** Maximum reconnection delay in milliseconds. Default: 30 000. */
  maxRetryMs?: number;
}

/** A parsed SSE message delivered via the `'message'` or named event. */
export interface EventSourceMessage {
  /** Event type (`'message'` if unnamed). */
  type: string;
  /** Concatenated `data:` field lines. */
  data: string;
  /** The last `id:` field value received. */
  lastEventId: string;
  /** The origin URL of the SSE endpoint. */
  origin: string;
}

const CONNECTING = 0;
const OPEN = 1;
const CLOSED = 2;

/**
 * Typed event declarations for {@link Http3EventSource}.
 */
export interface Http3EventSource {
  on(event: 'open', listener: (e: Event) => void): this;
  on(event: 'message', listener: (msg: EventSourceMessage) => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * An EventSource (SSE) client that connects over HTTP/3.
 *
 * Follows W3C EventSource semantics: automatic reconnection, `Last-Event-ID`
 * tracking, and named event dispatch.
 */
export class Http3EventSource extends EventEmitter {
  static readonly CONNECTING = CONNECTING;
  static readonly OPEN = OPEN;
  static readonly CLOSED = CLOSED;

  readonly url: string;
  readyState = CONNECTING;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: EventSourceMessage) => void) | null = null;
  onerror: ((error: Error) => void) | null = null;

  private readonly _url: URL;
  private readonly _options: EventSourceInit;
  private _session: Http3ClientSession | null = null;
  private _stream: ClientHttp3Stream | null = null;
  private _decoder = new TextDecoder();
  private _buffer = '';
  private _currentEvent = 'message';
  private _currentData: string[] = [];
  private _lastEventId = '';
  private _retryMs: number;
  private readonly _maxRetryMs: number;
  private _reconnectTimer: NodeJS.Timeout | null = null;
  private _closed = false;
  private _closePromise: Promise<void> | null = null;
  private _sessionClosePromise: Promise<void> | null = null;

  constructor(url: string, options?: EventSourceInit) {
    super();
    this.url = url;
    this._url = new URL(url);
    this._options = options ?? {};
    this._retryMs = options?.initialRetryMs ?? 1000;
    this._maxRetryMs = options?.maxRetryMs ?? 30000;
    void this._startConnection();
  }

  addEventListener(event: string, listener: (...args: unknown[]) => void): void {
    this.on(event, listener);
  }

  removeEventListener(event: string, listener: (...args: unknown[]) => void): void {
    this.off(event, listener);
  }

  /** Close the EventSource and stop reconnecting. */
  close(): void {
    if (this._closed) return;
    this._closed = true;
    this.readyState = CLOSED;
    if (this._reconnectTimer) {
      clearTimeout(this._reconnectTimer);
      this._reconnectTimer = null;
    }
    this._closePromise = this._finalizeClose();
    void this._closePromise;
  }

  private async _startConnection(): Promise<void> {
    if (this._closed) return;
    this.readyState = CONNECTING;
    await this._closeSession();
    if (this._closed) return;

    const authority = `${this._url.hostname}:${this._url.port || '443'}`;
    this._session = connect(authority, {
      ...this._options,
      servername: this._options.servername ?? this._url.hostname,
    });

    this._session.once('connect', () => {
      this._openStream();
    });
    this._session.on('error', (err: Error) => {
      this._emitError(err);
      this._scheduleReconnect();
    });
    this._session.on('close', () => {
      if (!this._closed) {
        this._scheduleReconnect();
      }
    });
  }

  private _openStream(): void {
    if (!this._session || this._closed) return;

    const path = `${this._url.pathname}${this._url.search}`;
    const headers: IncomingHeaders = {
      ':method': 'GET',
      ':path': path,
      ':authority': this._url.host,
      ':scheme': this._url.protocol.replace(':', ''),
      accept: 'text/event-stream',
      'cache-control': 'no-cache',
    };
    if (this._lastEventId.length > 0) {
      headers['last-event-id'] = this._lastEventId;
    }
    if (this._options.headers) {
      for (const [name, value] of Object.entries(this._options.headers)) {
        headers[name.toLowerCase()] = value;
      }
    }

    this._stream = this._session.request(headers, { endStream: true });
    this._stream.on('response', (responseHeaders: IncomingHeaders) => {
      const status = responseHeaders[':status'];
      const contentType = String(responseHeaders['content-type'] ?? '');
      if (status !== '200' || !contentType.toLowerCase().includes('text/event-stream')) {
        this._emitError(new Error(`EventSource expected 200 text/event-stream, got status=${String(status)} content-type=${contentType}`));
        this._scheduleReconnect();
        return;
      }
      this.readyState = OPEN;
      const openEvent = new Event('open');
      this.emit('open', openEvent);
      this.onopen?.(openEvent);
    });
    this._stream.on('data', (chunk: Buffer) => {
      this._onChunk(chunk);
    });
    this._stream.on('end', () => {
      if (!this._closed) {
        this._scheduleReconnect();
      }
    });
    this._stream.on('aborted', () => {
      if (!this._closed) {
        this._scheduleReconnect();
      }
    });
    this._stream.on('error', (err: Error) => {
      this._emitError(err);
      this._scheduleReconnect();
    });
  }

  private _onChunk(chunk: Buffer): void {
    this._buffer += this._decoder.decode(chunk, { stream: true });
    for (;;) {
      const newlineIndex = this._buffer.indexOf('\n');
      if (newlineIndex < 0) break;
      const rawLine = this._buffer.slice(0, newlineIndex);
      this._buffer = this._buffer.slice(newlineIndex + 1);
      const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
      this._processLine(line);
    }
  }

  private _processLine(line: string): void {
    if (line.length === 0) {
      this._dispatchMessage();
      return;
    }
    if (line.startsWith(':')) {
      return;
    }

    const separator = line.indexOf(':');
    const field = separator >= 0 ? line.slice(0, separator) : line;
    let value = separator >= 0 ? line.slice(separator + 1) : '';
    if (value.startsWith(' ')) {
      value = value.slice(1);
    }

    switch (field) {
      case 'event':
        this._currentEvent = value || 'message';
        break;
      case 'data':
        this._currentData.push(value);
        break;
      case 'id':
        if (!value.includes('\0')) {
          this._lastEventId = value;
        }
        break;
      case 'retry': {
        const retry = Number.parseInt(value, 10);
        if (Number.isFinite(retry) && retry >= 0) {
          this._retryMs = Math.min(retry, this._maxRetryMs);
        }
        break;
      }
      default:
        break;
    }
  }

  private _dispatchMessage(): void {
    if (this._currentData.length === 0) {
      this._currentEvent = 'message';
      return;
    }

    const message: EventSourceMessage = {
      type: this._currentEvent,
      data: this._currentData.join('\n'),
      lastEventId: this._lastEventId,
      origin: this._url.origin,
    };

    this.emit(this._currentEvent, message);
    if (this._currentEvent === 'message') {
      this.onmessage?.(message);
    }
    this._currentData = [];
    this._currentEvent = 'message';
  }

  private _scheduleReconnect(): void {
    if (this._closed) return;
    if (this._options.reconnect === false) {
      this.close();
      return;
    }
    if (this._reconnectTimer) return;
    this.readyState = CONNECTING;
    this._reconnectTimer = setTimeout(() => {
      this._reconnectTimer = null;
      void this._startConnection();
    }, this._retryMs);
    this._reconnectTimer.unref();
  }

  private _emitError(err: Error): void {
    if (this.listenerCount('error') > 0) {
      this.emit('error', err);
    } else {
      this.emit('sessionError', err);
    }
    this.onerror?.(err);
  }

  private async _closeSession(): Promise<void> {
    if (this._sessionClosePromise) {
      await this._sessionClosePromise;
      return;
    }

    const stream = this._stream;
    this._stream = null;
    const session = this._session;
    this._session = null;
    if (!stream && !session) {
      return;
    }

    this._sessionClosePromise = (async () => {
      if (stream) {
        stream.removeAllListeners();
        if (!stream.destroyed) {
          stream.destroy();
        }
      }

      if (session) {
        await session.close();
      }
    })();

    try {
      await this._sessionClosePromise;
    } finally {
      this._sessionClosePromise = null;
    }
  }

  private async _finalizeClose(): Promise<void> {
    try {
      await this._closeSession();
    } catch (error: unknown) {
      this._emitError(error instanceof Error ? error : new Error(String(error)));
    } finally {
      this.emit('close');
    }
  }
}

/** Create an {@link Http3EventSource} connected to the given SSE URL. */
export function createEventSource(url: string, options?: EventSourceInit): Http3EventSource {
  return new Http3EventSource(url, options);
}
