import {
  Http3ClientSessionBase,
  type GoawayInfo,
  type SessionCloseInfo,
  sessionCloseInfoFromEvent,
} from './session.js';
import { ClientHttp3Stream } from './stream.js';
import { incomingHeadersToNativeHeaders, nativeHeadersToIncomingHeaders } from './stream.js';
import type { IncomingHeaders } from './stream.js';
import { ClientEventLoop, EVENT_SHUTDOWN_COMPLETE, binding } from './event-loop.js';
import type { NativeEvent } from './event-loop.js';
import type { ConnectionEndpoint } from './endpoint.js';
import { abortSignalError, resolveConnectionEndpoint, stringifyConnectionEndpoint } from './endpoint.js';
import {
  Http3Error,
  ERR_HTTP3_GOAWAY,
  ERR_HTTP3_INVALID_STATE,
  ERR_HTTP3_SESSION_ERROR,
  ERR_HTTP3_STREAM_BLOCKED,
  ERR_HTTP3_STREAM_ERROR,
} from './errors.js';
import { toSessionError, toStreamError } from './error-map.js';
import { prepareKeylogFile, subscribeKeylog } from './keylog.js';
import type { RuntimeInfo, RuntimeOptions } from './runtime.js';
import { runWithRuntimeSelection, setPendingRuntimeInfo } from './runtime.js';

// Event type constants (must match Rust)
const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_RESET = 6;
const EVENT_SESSION_CLOSE = 7;
const EVENT_DRAIN = 8;
const EVENT_STREAM_BLOCKED = 16;
const EVENT_WRITE_READY = 18;
const EVENT_GOAWAY = 9;
const EVENT_ERROR = 10;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_SESSION_TICKET = 12;
const EVENT_DATAGRAM = 14;
const EVENT_PING_ACK = 17;

function normalizeCaOption(ca?: string | Buffer | Array<string | Buffer>): Buffer | undefined {
  if (!ca) return undefined;
  const first = Array.isArray(ca) ? ca[0] : ca;
  return typeof first === 'string' ? Buffer.from(first) : first;
}

function isNativeStreamBlockedError(err: unknown): err is Error {
  return err instanceof Error && err.message.includes('StreamBlocked');
}

function isRequestStreamBlockedError(err: unknown): boolean {
  return err instanceof Http3Error && err.code === ERR_HTTP3_STREAM_BLOCKED;
}

function requestStreamBlockedError(cause: Error): Http3Error {
  return new Http3Error(
    `request stream is flow-control blocked (StreamBlocked); retry with requestAsync()`,
    ERR_HTTP3_STREAM_BLOCKED,
    { cause },
  );
}

function normalizeRequestTimeoutMs(timeoutMs: number | undefined): number {
  if (timeoutMs === undefined) return 30_000;
  if (!Number.isFinite(timeoutMs) || timeoutMs < 0) {
    throw new Http3Error('request timeout must be a non-negative finite number', ERR_HTTP3_INVALID_STATE);
  }
  return Math.floor(timeoutMs);
}

function normalizeConnectTimeoutMs(timeoutMs: number | undefined): number {
  if (timeoutMs === undefined) return 30_000;
  if (!Number.isFinite(timeoutMs) || timeoutMs < 0) {
    throw new Http3Error('connect timeout must be a non-negative finite number', ERR_HTTP3_INVALID_STATE);
  }
  return Math.floor(timeoutMs);
}

function requestRetryDelayMs(attempt: number, deadlineMs: number): number {
  const base = Math.min(100, 2 ** Math.min(attempt, 6));
  const remaining = deadlineMs - Date.now();
  if (remaining <= 0) return 0;
  return Math.max(1, Math.min(base, remaining));
}

/** Options for connecting to an HTTP/3 server. */
export interface ConnectOptions {
  /** Abort the connect attempt (DNS lookup + handshake). Audit finding #15. */
  signal?: AbortSignal;
  /** Runtime selection mode. Default: `'auto'`. */
  runtimeMode?: RuntimeOptions['runtimeMode'];
  /** Runtime fallback policy. Default: `'warn-and-fallback'`. */
  fallbackPolicy?: RuntimeOptions['fallbackPolicy'];
  /** Callback invoked when runtime selection resolves or falls back. */
  onRuntimeEvent?: RuntimeOptions['onRuntimeEvent'];
  /** PEM-encoded CA certificate(s) to trust. */
  ca?: string | Buffer | Array<string | Buffer>;
  /** If `false`, accept self-signed certificates. Default: `true`. */
  rejectUnauthorized?: boolean;
  /** Override the SNI hostname sent during TLS handshake. */
  servername?: string;
  /** Idle timeout in milliseconds. Default: 30 000. */
  maxIdleTimeoutMs?: number;
  /** Maximum time to wait for QUIC/TLS handshake completion. Default: 30 000; 0 disables. */
  connectTimeoutMs?: number;
  /** Maximum UDP payload size. Default: 1350. */
  maxUdpPayloadSize?: number;
  /** Connection-level flow control window. Default: 100_000_000 bytes. */
  initialMaxData?: number;
  /** Per-stream bidi flow control window. Default: 2_000_000 bytes. */
  initialMaxStreamDataBidiLocal?: number;
  /** Maximum concurrent bidirectional streams. Default: 10_000. */
  initialMaxStreamsBidi?: number;
  /** TLS 1.3 session ticket for 0-RTT resumption. */
  sessionTicket?: Buffer;
  /** Enable 0-RTT early data. Default: `false`. */
  allow0RTT?: boolean;
  /** Enable TLS keylog; `true` for auto-path, or a string file path. */
  keylog?: boolean | string;
  /** Enable QUIC DATAGRAM extension (RFC 9221). Default: `false`. */
  enableDatagrams?: boolean;
  /** Allow non-safe HTTP methods (POST, etc.) in 0-RTT. Default: `false`. */
  allowUnsafe0RTTMethods?: boolean;
  /** Hook called before sending 0-RTT requests; return `true` to allow. */
  onEarlyData?: (headers: IncomingHeaders) => boolean;
  /** Interval in ms for emitting `'metrics'` events. Default: 1000. */
  metricsIntervalMs?: number;
  /** Directory for qlog output files. */
  qlogDir?: string;
  /** qlog verbosity level. */
  qlogLevel?: string;
}

/** Options for creating a new HTTP/3 request stream. */
export interface RequestOptions {
  /** If `true`, send the request with FIN (no request body). */
  endStream?: boolean;
}

/** Options for awaitable HTTP/3 request creation. */
export interface RequestAsyncOptions extends RequestOptions {
  /** Abort waiting for request stream capacity. */
  signal?: AbortSignal;
  /** Maximum time to wait for request stream capacity. Default: 30 000. */
  timeoutMs?: number;
}

/**
 * Typed event declarations for {@link Http3ClientSession}.
 */
export interface Http3ClientSession {
  on(event: 'connect', listener: () => void): this;
  on(event: 'goaway', listener: (info: GoawayInfo) => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'runtime', listener: (info: RuntimeInfo) => void): this;
  on(event: 'sessionTicket', listener: (ticket: Buffer) => void): this;
  on(event: 'datagram', listener: (data: Buffer) => void): this;
  on(event: 'close', listener: (info: SessionCloseInfo) => void): this;
  on(event: 'keylog', listener: (line: Buffer) => void): this;
  on(event: 'metrics', listener: (metrics: import('./session.js').SessionMetrics) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * Client-side HTTP/3 session for sending requests to a remote server.
 *
 * Obtain an instance via {@link connect} or {@link connectAsync}.
 */
export class Http3ClientSession extends Http3ClientSessionBase {
  private readonly _authority: string;
  private readonly _streams = new Map<number, ClientHttp3Stream>();
  private _allow0RTT = false;
  private _allowUnsafe0RTTMethods = false;
  private _onEarlyData: ((headers: IncomingHeaders) => boolean) | undefined;
  /** @internal */
  _closeRequested = false;
  private _readySettled = false;
  private _goawayReceived = false;
  private _goawayLastStreamId: number | null = null;
  private readonly _readyPromise: Promise<void>;
  private _resolveReady: (() => void) | null = null;
  private _rejectReady: ((err: Error) => void) | null = null;
  private _connectAbortCleanup: (() => void) | null = null;
  private _connectTimer: NodeJS.Timeout | null = null;
  private readonly _requestDrainResolvers = new Set<() => void>();

  constructor(authority: string, options?: Pick<ConnectOptions, 'allow0RTT' | 'allowUnsafe0RTTMethods' | 'onEarlyData'>) {
    super();
    this._authority = authority;
    this._allow0RTT = options?.allow0RTT ?? false;
    this._allowUnsafe0RTTMethods = options?.allowUnsafe0RTTMethods ?? false;
    this._onEarlyData = options?.onEarlyData;
    this._readyPromise = new Promise<void>((resolve, reject) => {
      this._resolveReady = resolve;
      this._rejectReady = reject;
    });
    // ready() is optional for event-driven callers; swallow unobserved
    // rejections via an explicit try/await (project rule: no `.catch`).
    const readyPromise = this._readyPromise;
    void (async (): Promise<void> => {
      try {
        await readyPromise;
      } catch {
        /* unobserved — caller chose event-driven flow */
      }
    })();
  }

  /** The authority (host:port) this session is connected to. */
  get authority(): string {
    return this._authority;
  }

  /** Resolves when the QUIC handshake completes. Rejects on connection failure. */
  async ready(): Promise<void> {
    return this._readyPromise;
  }

  override async close(code?: number, reason?: string): Promise<void> {
    this._closeRequested = true;
    this._clearConnectTimer();
    if (!this._handshakeComplete) {
      this._markReadyError(new Http3Error('session closed before handshake completed', ERR_HTTP3_INVALID_STATE));
    }
    await super.close(code, reason);
  }

  override async destroy(err?: Error): Promise<void> {
    this._closeRequested = true;
    this._clearConnectTimer();
    if (!this._handshakeComplete) {
      this._markReadyError(err ?? new Http3Error('session destroyed before handshake completed', ERR_HTTP3_INVALID_STATE));
    }
    await super.destroy(err);
  }

  /** @internal */
  _setConnectAbortCleanup(cleanup: (() => void) | null): void {
    this._connectAbortCleanup?.();
    this._connectAbortCleanup = cleanup;
  }

  /** @internal */
  _setConnectTimer(timer: NodeJS.Timeout | null): void {
    this._clearConnectTimer();
    this._connectTimer = timer;
  }

  /** @internal */
  _clearConnectTimer(): void {
    if (!this._connectTimer) return;
    clearTimeout(this._connectTimer);
    this._connectTimer = null;
  }

  /** @internal */
  _abortConnect(err: Error): void {
    this._closeRequested = true;
    this._clearConnectTimer();
    this._markReadyError(err);
    this._cleanupStreams(err);
    this._notifyRequestDrain();
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    const loop_ = this._eventLoop;
    this._eventLoop = null;
    if (loop_) {
      void (async (): Promise<void> => {
        try {
          await loop_.close();
        } catch {
          /* best-effort abort cleanup */
        }
      })();
    }
    this._emitSessionError(err);
  }

  /**
   * Open a new HTTP/3 request stream.
   * @param headers - Pseudo-headers (`:method`, `:path`, etc.) and regular headers.
   * @param options - Stream options (e.g. `endStream` for body-less requests).
   * @returns A {@link ClientHttp3Stream} duplex for reading the response.
   */
  request(headers: IncomingHeaders, options?: RequestOptions): ClientHttp3Stream {
    if (!this._handshakeComplete) {
      if (!this._allow0RTT) {
        throw new Http3Error('handshake not complete — wait for "connect" event', ERR_HTTP3_INVALID_STATE);
      }
      const method = String(headers[':method'] ?? 'GET').toUpperCase();
      const safeMethod = method === 'GET' || method === 'HEAD';
      const allowedByHook = this._onEarlyData?.(headers) ?? false;
      if (!safeMethod && !this._allowUnsafe0RTTMethods && !allowedByHook) {
        throw new Http3Error(
          `0-RTT is restricted to safe methods (GET/HEAD), got ${method}`,
          ERR_HTTP3_INVALID_STATE,
        );
      }
    }
    if (!this._eventLoop) {
      throw new Http3Error('not connected', ERR_HTTP3_INVALID_STATE);
    }
    if (this._goawayReceived) {
      const suffix = this._goawayLastStreamId === null ? '' : ` (last stream ID ${this._goawayLastStreamId})`;
      throw new Http3Error(`session received GOAWAY${suffix}`, ERR_HTTP3_GOAWAY);
    }

    const h = incomingHeadersToNativeHeaders(headers);

    let streamId: number;
    try {
      streamId = this._eventLoop.sendRequest(h, options?.endStream ?? false);
    } catch (err: unknown) {
      if (isNativeStreamBlockedError(err)) {
        throw requestStreamBlockedError(err);
      }
      throw err;
    }
    const stream = new ClientHttp3Stream();
    stream._streamId = streamId;
    stream._eventLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
  }

  /**
   * Open a new HTTP/3 request stream, waiting when quiche reports that the
   * request HEADERS cannot yet fit in the QUIC flow-control window.
   *
   * `request()` remains the synchronous node:http2-style primitive. This
   * helper is for high-concurrency clients that want request creation itself
   * to participate in backpressure instead of handling transient
   * `ERR_HTTP3_STREAM_BLOCKED` failures manually.
   */
  async requestAsync(headers: IncomingHeaders, options?: RequestAsyncOptions): Promise<ClientHttp3Stream> {
    if (!this._handshakeComplete && !this._allow0RTT) {
      await this.ready();
    }

    const timeoutMs = normalizeRequestTimeoutMs(options?.timeoutMs);
    const deadlineMs = timeoutMs === 0 ? Number.POSITIVE_INFINITY : Date.now() + timeoutMs;
    let attempt = 0;

    for (;;) {
      if (options?.signal?.aborted) {
        throw abortSignalError(options.signal);
      }

      try {
        return this.request(headers, options);
      } catch (err: unknown) {
        if (!isRequestStreamBlockedError(err)) {
          throw err;
        }
        const delayMs = requestRetryDelayMs(attempt, deadlineMs);
        if (delayMs <= 0) {
          throw new Http3Error('timed out waiting for HTTP/3 request stream capacity', ERR_HTTP3_STREAM_BLOCKED, {
            cause: err instanceof Error ? err : undefined,
          });
        }
        attempt += 1;
        await this._waitForRequestRetry(delayMs, options?.signal);
      }
    }
  }

  /** @internal */
  _dispatchEvents(events: NativeEvent[]): void {
    for (const event of events) {
      switch (event.eventType) {
        case EVENT_HANDSHAKE_COMPLETE:
          this._handshakeComplete = true;
          this._markReady();
          this.emit('connect');
          break;
        case EVENT_HEADERS:
          this._onHeaders(event);
          break;
        case EVENT_DATA:
          this._onData(event);
          break;
        case EVENT_FINISHED:
          this._onFinished(event);
          break;
        case EVENT_RESET:
          this._onReset(event);
          break;
        case EVENT_SESSION_CLOSE: {
          const closeInfo = sessionCloseInfoFromEvent(event);
          const closeErr = toSessionError(event, 'session closed');
          if (!this._handshakeComplete) {
            this._markReadyError(new Http3Error('session closed before handshake completed', ERR_HTTP3_INVALID_STATE));
          }
          this._cleanupStreams(closeErr);
          this._notifyRequestDrain();
          this._stopMetricsEmitter();
          this._stopKeylogEmitter();
          this._rejectPendingPings(closeErr);
          this._emitClose(closeInfo);
          this._cleanupEventLoopAfterNativeClose();
          break;
        }
        case EVENT_DRAIN:
          this._onDrain(event);
          break;
        case EVENT_STREAM_BLOCKED:
          this._onStreamBlocked(event);
          break;
        case EVENT_WRITE_READY:
          this._onWriteReady();
          break;
        case EVENT_GOAWAY:
          this._goawayReceived = true;
          this._goawayLastStreamId = event.streamId;
          this._notifyRequestDrain();
          this.emit('goaway', { lastStreamId: event.streamId });
          break;
        case EVENT_ERROR:
          this._onError(event);
          break;
        case EVENT_SESSION_TICKET:
          this._onSessionTicket(event);
          break;
        case EVENT_DATAGRAM:
          this._onDatagram(event);
          break;
        case EVENT_PING_ACK:
          this._onPingAck(event.meta?.durationMs ?? 0);
          break;
        default:
          break;
      }
    }
  }

  private _cleanupStreams(err = new Error('session closed')): void {
    for (const stream of this._streams.values()) {
      if (stream.listenerCount('error') > 0) {
        stream.destroy(err);
      } else {
        stream.destroy();
      }
    }
    this._streams.clear();
  }

  private _cleanupEventLoopAfterNativeClose(): void {
    this._closeRequested = true;
    this._clearConnectTimer();
    const loop_ = this._eventLoop;
    this._eventLoop = null;
    if (!loop_) return;
    void (async (): Promise<void> => {
      try {
        await loop_.close();
      } catch {
        /* native close cleanup is best-effort after SESSION_CLOSE */
      }
    })();
  }

  private _onHeaders(event: NativeEvent): void {
    if (!event.headers) return;
    const stream = this._streams.get(event.streamId);
    if (!stream) return;

    const headers = nativeHeadersToIncomingHeaders(event.headers);

    if (stream._responseSeen) {
      // Subsequent HEADERS frames after the response are trailing headers
      // (H3 spec §4.1). Emit them as 'trailers' to match node:http2.
      stream.emit('trailers', headers);
      return;
    }
    stream._responseSeen = true;
    const flags = { endStream: event.fin ?? false };
    stream.emit('response', headers, flags);
  }

  private _onData(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream && event.data) {
      stream._onActivity();
      stream._pushData(event.data);
    }
  }

  private _onFinished(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream._pushData(null);
      // If the readable side was never consumed, resume so the Duplex can
      // fully destroy via 'close'. Mirrors the server pattern in server.ts.
      if (stream.readableFlowing === null) {
        stream.resume();
      }
      // Don't delete from _streams here: the writable side may still have
      // pending drain callbacks that need EVENT_DRAIN to route through.
      // Map cleanup happens on 'close' instead. (Audit finding #2.)
      stream.once('close', () => {
        this._streams.delete(event.streamId);
      });
    }
  }

  private _onReset(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream.emit('aborted');
      stream.destroy(new Http3Error('stream reset', ERR_HTTP3_STREAM_ERROR, {
        h3Code: event.meta?.errorCode,
      }));
      this._streams.delete(event.streamId);
    }
  }

  private _onDrain(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream._onNativeDrain();
      return;
    }
    this._notifyRequestDrain();
  }

  private _onStreamBlocked(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream._onNativeBlocked();
    }
  }

  private _onWriteReady(): void {
    for (const stream of this._streams.values()) {
      stream._onNativeWriteReady();
    }
  }

  private _onError(event: NativeEvent): void {
    if (event.streamId >= 0) {
      const stream = this._streams.get(event.streamId);
      if (stream) {
        stream.destroy(toStreamError(event));
      }
    } else {
      this._emitSessionError(toSessionError(event));
      this._notifyRequestDrain();
      if (!this._handshakeComplete) {
        this._markReadyError(toSessionError(event));
      }
    }
  }

  /** @internal */
  _emitSessionError(err: Error): void {
    if (!this._handshakeComplete && this.listenerCount('error') === 0) {
      process.nextTick(() => {
        if (this.listenerCount('error') > 0) {
          this.emit('error', err);
        }
      });
      return;
    }

    this.emit('error', err);
  }

  private _onSessionTicket(event: NativeEvent): void {
    if (!event.data) return;
    this.emit('sessionTicket', event.data);
  }

  private _onDatagram(event: NativeEvent): void {
    if (!event.data) return;
    this.emit('datagram', event.data);
  }

  private _notifyRequestDrain(): void {
    const resolvers = Array.from(this._requestDrainResolvers);
    this._requestDrainResolvers.clear();
    for (const resolve of resolvers) {
      resolve();
    }
  }

  private _waitForRequestRetry(delayMs: number, signal: AbortSignal | undefined): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      if (signal?.aborted) {
        reject(abortSignalError(signal));
        return;
      }

      let settled = false;
      let timer: NodeJS.Timeout | null = null;
      let onAbort: (() => void) | null = null;
      let onDrain: (() => void) | null = null;

      const cleanup = (): void => {
        if (onDrain) {
          this._requestDrainResolvers.delete(onDrain);
        }
        if (timer) {
          clearTimeout(timer);
          timer = null;
        }
        if (signal && onAbort) {
          signal.removeEventListener('abort', onAbort);
          onAbort = null;
        }
      };

      const settle = (err?: Error): void => {
        if (settled) return;
        settled = true;
        cleanup();
        if (err) {
          reject(err);
        } else {
          resolve();
        }
      };

      onDrain = (): void => {
        settle();
      };

      onAbort = (): void => {
        settle(abortSignalError(signal));
      };

      this._requestDrainResolvers.add(onDrain);
      timer = setTimeout(() => {
        settle();
      }, delayMs);
      signal?.addEventListener('abort', onAbort, { once: true });
    });
  }

  /** @internal */
  _markReady(): void {
    if (this._readySettled) return;
    this._clearConnectTimer();
    this._setConnectAbortCleanup(null);
    this._readySettled = true;
    this._resolveReady?.();
    this._resolveReady = null;
    this._rejectReady = null;
  }

  /** @internal */
  _markReadyError(err: Error): void {
    if (this._readySettled) return;
    this._clearConnectTimer();
    this._setConnectAbortCleanup(null);
    this._readySettled = true;
    this._rejectReady?.(err);
    this._resolveReady = null;
    this._rejectReady = null;
  }
}

function installHttp3ConnectAbortHandler(
  session: Http3ClientSession,
  signal: AbortSignal | undefined,
): void {
  if (!signal) return;

  const onAbort = (): void => {
    session._abortConnect(abortSignalError(signal));
  };

  if (signal.aborted) {
    onAbort();
    return;
  }

  signal.addEventListener('abort', onAbort, { once: true });
  session._setConnectAbortCleanup(() => {
    signal.removeEventListener('abort', onAbort);
  });
}

/**
 * Connect to an HTTP/3 server and return a session immediately.
 *
 * The session begins the QUIC handshake asynchronously. Wait for the
 * `'connect'` event or call `session.ready()` before sending requests
 * (unless 0-RTT is enabled).
 *
 * @example
 * ```ts
 * import { connect } from '@currentspace/http3';
 *
 * const session = connect('https://localhost:443', {
 *   ca: readFileSync('ca.pem'),
 * });
 * session.on('connect', () => {
 *   const stream = session.request({ ':method': 'GET', ':path': '/' });
 *   stream.on('response', (headers) => console.log(headers[':status']));
 *   stream.end();
 * });
 * ```
 */
export function connect(authority: ConnectionEndpoint, options?: ConnectOptions): Http3ClientSession {
  const authorityString = typeof authority === 'string'
    ? authority
    : stringifyConnectionEndpoint(authority);
  const session = new Http3ClientSession(authorityString, {
    allow0RTT: options?.allow0RTT,
    allowUnsafe0RTTMethods: options?.allowUnsafe0RTTMethods,
    onEarlyData: options?.onEarlyData,
  });
  setPendingRuntimeInfo(session, options);
  session._qlogPath = options?.qlogDir ?? null;
  const keylogPath = prepareKeylogFile(options?.keylog);
  if (keylogPath) {
    session._setKeylogUnsubscribe(subscribeKeylog(keylogPath, (line) => {
      session.emit('keylog', line);
    }));
    process.nextTick(() => {
      session.emit('keylog', Buffer.from(`# keylog enabled ${keylogPath}\n`));
    });
  }
  const shouldAbortConnect = (): boolean => session._closeRequested;
  installHttp3ConnectAbortHandler(session, options?.signal);
  const connectTimeoutMs = normalizeConnectTimeoutMs(options?.connectTimeoutMs);
  if (connectTimeoutMs > 0) {
    session._setConnectTimer(setTimeout(() => {
      if (session._closeRequested) return;
      session._abortConnect(new Http3Error(
        `HTTP/3 connect timed out after ${connectTimeoutMs} ms`,
        ERR_HTTP3_SESSION_ERROR,
        {
          reasonCode: 'connect-timeout',
          endpoint: authorityString,
        },
      ));
    }, connectTimeoutMs));
  }

  void (async (): Promise<void> => {
    try {
      const resolved = await resolveConnectionEndpoint(authority, {
        defaultScheme: 'https',
        defaultPort: 443,
        signal: options?.signal,
      });
      if (shouldAbortConnect()) {
        return;
      }

      await runWithRuntimeSelection(session, options, async (runtimeMode) => {
        const nativeClient = new binding.NativeWorkerClient({
          ca: normalizeCaOption(options?.ca),
          rejectUnauthorized: options?.rejectUnauthorized,
          runtimeMode,
          maxIdleTimeoutMs: options?.maxIdleTimeoutMs,
          maxUdpPayloadSize: options?.maxUdpPayloadSize,
          initialMaxData: options?.initialMaxData,
          initialMaxStreamDataBidiLocal: options?.initialMaxStreamDataBidiLocal,
          initialMaxStreamsBidi: options?.initialMaxStreamsBidi,
          sessionTicket: options?.sessionTicket,
          allow0Rtt: options?.allow0RTT,
          keylog: Boolean(keylogPath),
          enableDatagrams: options?.enableDatagrams,
          qlogDir: options?.qlogDir,
          qlogLevel: options?.qlogLevel,
        }, (_err: Error | null, events: NativeEvent[]) => {
          try {
            let hasShutdown = false;
            for (const event of events) {
              if (event.eventType === EVENT_SHUTDOWN_COMPLETE) {
                hasShutdown = true;
              }
            }
            if (!hasShutdown) {
              session._dispatchEvents(events);
            } else {
              session._dispatchEvents(events.filter(e => e.eventType !== EVENT_SHUTDOWN_COMPLETE));
              eventLoop._onShutdownSentinel();
            }
          } finally {
            // Audit #14: release credit on the outstanding-events gauge even
            // when user event handlers throw during dispatch.
            nativeClient.ackEventBatch(events.length);
          }
        });

        const eventLoop = new ClientEventLoop(nativeClient);
        session._eventLoop = eventLoop;
        if (shouldAbortConnect()) {
          await eventLoop.close();
          session._eventLoop = null;
          return;
        }
        try {
          await eventLoop.connect(resolved.socketAddress, options?.servername ?? resolved.servername);
        } catch (error: unknown) {
          session._eventLoop = null;
          throw error;
        }
        if (shouldAbortConnect()) {
          await eventLoop.close();
          session._eventLoop = null;
          return;
        }
        session._startMetricsEmitter(options?.metricsIntervalMs ?? 1000, () => session.getMetrics());
      });
    } catch (err: unknown) {
      if (shouldAbortConnect()) {
        return;
      }
      const error = err instanceof Error ? err : new Error(String(err));
      session._markReadyError(error);
      session._emitSessionError(error);
    }
  })();

  return session;
}

/**
 * Connect to an HTTP/3 server and wait for the handshake to complete.
 * Convenience wrapper around {@link connect} + `session.ready()`.
 */
export async function connectAsync(authority: ConnectionEndpoint, options?: ConnectOptions): Promise<Http3ClientSession> {
  const session = connect(authority, options);
  await session.ready();
  return session;
}
