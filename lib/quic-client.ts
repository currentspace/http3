import { EventEmitter } from 'node:events';
import { binding } from './event-loop.js';
import type { NativeEvent, NativeQuicClientBinding } from './event-loop.js';
import type { ConnectionEndpoint } from './endpoint.js';
import { resolveConnectionEndpoint } from './endpoint.js';
import { toSessionError } from './error-map.js';
import { ERR_HTTP3_TLS_CONFIG_ERROR, Http3Error } from './errors.js';
import { QuicStream } from './quic-stream.js';
import type { QuicClientEventLoopLike } from './quic-stream.js';
import type { RuntimeInfo, RuntimeOptions } from './runtime.js';
import { runWithRuntimeSelection, setPendingRuntimeInfo } from './runtime.js';

const EVENT_NEW_STREAM = 2;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_RESET = 6;
const EVENT_SESSION_CLOSE = 7;
const EVENT_DRAIN = 8;
const EVENT_ERROR = 10;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_SESSION_TICKET = 12;
const EVENT_DATAGRAM = 14;

/** Options for connecting to a raw QUIC server. */
export interface QuicConnectOptions {
  /** Runtime selection mode. Default: `'auto'`. */
  runtimeMode?: RuntimeOptions['runtimeMode'];
  /** Runtime fallback policy. Default: `'warn-and-fallback'`. */
  fallbackPolicy?: RuntimeOptions['fallbackPolicy'];
  /** Callback invoked when runtime selection resolves or falls back. */
  onRuntimeEvent?: RuntimeOptions['onRuntimeEvent'];
  /** PEM-encoded CA certificate to trust. */
  ca?: Buffer | string;
  /** PEM-encoded client certificate chain for mutual TLS. */
  cert?: Buffer | string;
  /** PEM-encoded client private key for mutual TLS. */
  key?: Buffer | string;
  /** If `false`, accept self-signed certificates. Default: `true`. */
  rejectUnauthorized?: boolean;
  /** ALPN protocol strings. Default: `['quic']`. */
  alpn?: string[];
  /** Override the SNI hostname sent during TLS handshake. */
  servername?: string;
  /** Idle timeout in milliseconds. Default: 30_000. */
  maxIdleTimeoutMs?: number;
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
  /** Enable QUIC DATAGRAM extension (RFC 9221). Default: `false`. */
  enableDatagrams?: boolean;
  /** Enable TLS keylog. Default: `false`. */
  keylog?: boolean;
  /** Directory for qlog output. */
  qlogDir?: string;
  /** qlog verbosity level. */
  qlogLevel?: string;
}

interface NormalizedQuicClientTlsOptions {
  ca?: Buffer;
  cert?: Buffer;
  key?: Buffer;
}

class QuicClientEventLoop implements QuicClientEventLoopLike {
  private readonly worker: NativeQuicClientBinding;
  private closed = false;

  constructor(worker: NativeQuicClientBinding) {
    this.worker = worker;
  }

  async connect(serverAddr: string, serverName: string): Promise<void> {
    this.worker.connect(serverAddr, serverName);
    await Promise.resolve();
  }

  streamSend(streamId: number, data: Buffer, fin: boolean): number {
    this.worker.streamSend(streamId, data, fin);
    return Math.max(data.length, fin ? 1 : 0);
  }

  streamClose(streamId: number, errorCode: number): boolean {
    return this.worker.streamClose(streamId, errorCode);
  }

  sendDatagram(data: Buffer): boolean {
    return this.worker.sendDatagram(data);
  }

  getSessionMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  } {
    return this.worker.getSessionMetrics();
  }

  ping(): boolean {
    return this.worker.ping();
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    const queued = this.worker.close(0, 'client close');
    if (queued) {
      await new Promise<void>((resolve) => { setTimeout(resolve, 20); });
    }
    this.worker.shutdown();
  }
}

/**
 * Typed event declarations for {@link QuicClientSession}.
 */
export interface QuicClientSession {
  on(event: 'connect', listener: () => void): this;
  on(event: 'stream', listener: (stream: QuicStream) => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'runtime', listener: (info: RuntimeInfo) => void): this;
  on(event: 'sessionTicket', listener: (ticket: Buffer) => void): this;
  on(event: 'datagram', listener: (data: Buffer) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * Client-side raw QUIC session (no HTTP/3 framing).
 *
 * Obtain an instance via {@link connectQuic} or {@link connectQuicAsync}.
 */
export class QuicClientSession extends EventEmitter {
  private _eventLoop: QuicClientEventLoop | null = null;
  private readonly _streams = new Map<number, QuicStream>();
  private _handshakeComplete = false;
  /** @internal */
  _runtimeInfo: RuntimeInfo | null = null;
  /** @internal */
  _closeRequested = false;
  private _readySettled = false;
  private _readyHandle: NodeJS.Immediate | null = null;
  private readonly _readyPromise: Promise<void>;
  private _resolveReady: (() => void) | null = null;
  private _rejectReady: ((err: Error) => void) | null = null;
  private _nextBidiStreamId = 0; // Client-initiated bidi: 0, 4, 8, ...

  constructor() {
    super();
    this._readyPromise = new Promise<void>((resolve, reject) => {
      this._resolveReady = resolve;
      this._rejectReady = reject;
    });
    void this._readyPromise.catch(() => undefined);
  }

  /** Whether the QUIC/TLS handshake has completed. */
  get handshakeComplete(): boolean {
    return this._handshakeComplete;
  }
  /** Runtime mode/driver information for this session, when available. */
  get runtimeInfo(): RuntimeInfo | null {
    return this._runtimeInfo;
  }

  /** Resolves when the QUIC handshake completes. Rejects on connection failure. */
  async ready(): Promise<void> {
    return this._readyPromise;
  }

  /** Open a new client-initiated bidirectional stream. */
  openStream(): QuicStream {
    if (!this._handshakeComplete) {
      throw new Error('QUIC handshake not complete — await session.ready() first');
    }
    if (!this._eventLoop) {
      throw new Error('QUIC session not connected');
    }
    const streamId = this._nextBidiStreamId;
    this._nextBidiStreamId += 4;
    const hwm = this._streams.size < 100 ? 256 * 1024 : 16 * 1024;
    const stream = new QuicStream({ highWaterMark: hwm });
    stream._streamId = streamId;
    stream._clientLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
  }

  /** Send an unreliable DATAGRAM frame (RFC 9221). */
  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (!this._eventLoop) return false;
    return this._eventLoop.sendDatagram(Buffer.from(data));
  }

  /** Return a transport metrics snapshot, or `null` on failure. */
  getMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  } | null {
    try {
      return this._eventLoop?.getSessionMetrics() ?? null;
    } catch {
      return null;
    }
  }

  /** Send a PING frame. Returns `true` if the command was queued. */
  ping(): boolean {
    return this._eventLoop?.ping() ?? false;
  }

  /** Close the session and destroy all streams. */
  async close(): Promise<void> {
    this._closeRequested = true;
    if (!this._handshakeComplete) {
      this._markReadyError(new Error('session closed before handshake'));
    }
    this._cleanupStreams();
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
  }

  /** @internal */
  _setEventLoop(loop_: QuicClientEventLoop | null): void {
    this._eventLoop = loop_;
  }

  /** @internal */
  _dispatchEvents(events: NativeEvent[]): void {
    for (const event of events) {
      switch (event.eventType) {
        case EVENT_HANDSHAKE_COMPLETE:
          this._scheduleHandshakeReady();
          break;
        case EVENT_NEW_STREAM:
          this._onNewStream(event);
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
        case EVENT_SESSION_CLOSE:
          if (!this._handshakeComplete) {
            this._markReadyError(new Error('session closed before handshake'));
          }
          this._cleanupStreams();
          this.emit('close');
          break;
        case EVENT_DRAIN:
          this._onDrain(event);
          break;
        case EVENT_ERROR:
          this._onError(event);
          break;
        case EVENT_SESSION_TICKET:
          if (event.data) {
            this.emit('sessionTicket', event.data);
          }
          break;
        case EVENT_DATAGRAM:
          if (event.data) {
            this.emit('datagram', event.data);
          }
          break;
        default:
          break;
      }
    }
  }

  private _onNewStream(event: NativeEvent): void {
    const stream = this._getOrCreateStream(event.streamId);
    // Coalesced first data from Rust: push inline to avoid extra TSFN event
    if (event.data) {
      stream._pushData(event.data);
    }
    if (event.fin) {
      stream._pushData(null);
    }
    this.emit('stream', stream);
  }

  private _onData(event: NativeEvent): void {
    if (!event.data) return;
    const stream = this._getOrCreateStream(event.streamId);
    stream._pushData(event.data);
  }

  private _onFinished(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream._pushData(null);
    }
  }

  private _onReset(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream.destroy(new Error(`stream reset: ${event.meta?.errorCode ?? 0}`));
      this._streams.delete(event.streamId);
    }
  }

  private _onDrain(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream._onNativeDrain();
    }
  }

  private _onError(event: NativeEvent): void {
    if (event.streamId >= 0) {
      const stream = this._streams.get(event.streamId);
      if (stream) {
        stream.destroy(new Error(event.meta?.errorReason ?? 'stream error'));
      }
    } else {
      this._emitSessionError(toSessionError(event));
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

  private _getOrCreateStream(streamId: number): QuicStream {
    let stream = this._streams.get(streamId);
    if (!stream) {
      const hwm = this._streams.size < 100 ? 256 * 1024 : 16 * 1024;
      stream = new QuicStream({ highWaterMark: hwm });
      stream._streamId = streamId;
      stream._clientLoop = this._eventLoop;
      this._streams.set(streamId, stream);
    }
    return stream;
  }

  private _cleanupStreams(): void {
    for (const stream of this._streams.values()) {
      stream.destroy();
    }
    this._streams.clear();
  }

  private _scheduleHandshakeReady(): void {
    if (this._readySettled || this._readyHandle) return;
    // Give the next poll cycle a chance to deliver an immediate server-side
    // policy close before we expose the session as successfully connected.
    this._readyHandle = setImmediate(() => {
      this._readyHandle = setImmediate(() => {
        this._readyHandle = null;
        if (this._readySettled) return;
        this._handshakeComplete = true;
        this._markReady();
        this.emit('connect');
      });
    });
  }

  private _markReady(): void {
    if (this._readySettled) return;
    if (this._readyHandle) {
      clearImmediate(this._readyHandle);
      this._readyHandle = null;
    }
    this._readySettled = true;
    this._resolveReady?.();
    this._resolveReady = null;
    this._rejectReady = null;
  }

  /** @internal */
  _markReadyError(err: Error): void {
    if (this._readySettled) return;
    if (this._readyHandle) {
      clearImmediate(this._readyHandle);
      this._readyHandle = null;
    }
    this._readySettled = true;
    this._rejectReady?.(err);
    this._resolveReady = null;
    this._rejectReady = null;
  }
}

function normalizePem(value?: string | Buffer): Buffer | undefined {
  if (!value) return undefined;
  return typeof value === 'string' ? Buffer.from(value) : value;
}

function assertPemBlock(field: 'cert' | 'key', value: Buffer): void {
  const pem = value.toString('utf8');
  const pattern = field === 'cert'
    ? /-----BEGIN CERTIFICATE-----[\s\S]+-----END CERTIFICATE-----/
    : /-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]+-----END [A-Z0-9 ]*PRIVATE KEY-----/;
  if (!pattern.test(pem)) {
    const description = field === 'cert' ? 'certificate chain' : 'private key';
    throw new Http3Error(
      `invalid QUIC client TLS ${field}: expected PEM-encoded ${description}`,
      ERR_HTTP3_TLS_CONFIG_ERROR,
    );
  }
}

function normalizeQuicClientTlsOptions(options?: QuicConnectOptions): NormalizedQuicClientTlsOptions {
  const ca = normalizePem(options?.ca);
  const cert = normalizePem(options?.cert);
  const key = normalizePem(options?.key);

  if (cert && !key) {
    throw new Http3Error(
      'invalid QUIC client TLS options: `key` is required when `cert` is provided',
      ERR_HTTP3_TLS_CONFIG_ERROR,
    );
  }
  if (key && !cert) {
    throw new Http3Error(
      'invalid QUIC client TLS options: `cert` is required when `key` is provided',
      ERR_HTTP3_TLS_CONFIG_ERROR,
    );
  }
  if (cert) {
    assertPemBlock('cert', cert);
  }
  if (key) {
    assertPemBlock('key', key);
  }

  return { ca, cert, key };
}

function isQuicClientTlsConfigMessage(message: string): boolean {
  const normalized = message.toLowerCase();
  return normalized.includes('invalid quic client tls')
    || normalized.includes('client certificate')
    || normalized.includes('private key pem')
    || normalized.includes('certificate pem');
}

function toQuicClientConnectError(error: unknown): Error {
  if (error instanceof Http3Error) {
    return error;
  }
  if (error instanceof Error && isQuicClientTlsConfigMessage(error.message)) {
    return new Http3Error(
      error.message.replace(/^config error:\s*/i, ''),
      ERR_HTTP3_TLS_CONFIG_ERROR,
      { cause: error },
    );
  }
  return error instanceof Error ? error : new Error(String(error));
}

function getNativeQuicClientConstructor(): typeof binding.NativeQuicClient {
  const NativeQuicClient = (binding as Partial<typeof binding>).NativeQuicClient;
  if (typeof NativeQuicClient !== 'function') {
    throw new Error(
      'The loaded @currentspace/http3 native binding is missing `NativeQuicClient`. '
      + 'This install appears to contain an incomplete prebuild, so raw QUIC client APIs '
      + '(`connectQuic`, `connectQuicAsync`) are unavailable. Reinstall a fixed package version '
      + 'or rebuild from source.',
    );
  }
  return NativeQuicClient;
}

/**
 * Connect to a raw QUIC server and return a session immediately.
 *
 * The session begins the QUIC handshake asynchronously. Wait for the
 * `'connect'` event or call `session.ready()` before opening streams.
 *
 * @example
 * ```ts
 * import { connectQuic } from '@currentspace/http3';
 * import { readFileSync } from 'node:fs';
 *
 * const session = connectQuic('localhost:4433', {
 *   ca: readFileSync('ca.pem'),
 *   alpn: ['myproto'],
 * });
 * await session.ready();
 * const stream = session.openStream();
 * stream.end('hello');
 * ```
 */
export function connectQuic(authority: ConnectionEndpoint, options?: QuicConnectOptions): QuicClientSession {
  const tls = normalizeQuicClientTlsOptions(options);
  const session = new QuicClientSession();
  setPendingRuntimeInfo(session, options);
  const NativeQuicClient = getNativeQuicClientConstructor();
  const shouldAbortConnect = (): boolean => session._closeRequested;

  void (async (): Promise<void> => {
    try {
      const resolved = await resolveConnectionEndpoint(authority, {
        defaultScheme: 'quic',
        defaultPort: 4433,
      });
      if (shouldAbortConnect()) {
        return;
      }

      await runWithRuntimeSelection(session, options, async (runtimeMode) => {
        const nativeClient = new NativeQuicClient(
          {
            ca: tls.ca,
            cert: tls.cert,
            key: tls.key,
            rejectUnauthorized: options?.rejectUnauthorized,
            alpn: options?.alpn,
            runtimeMode,
            maxIdleTimeoutMs: options?.maxIdleTimeoutMs,
            maxUdpPayloadSize: options?.maxUdpPayloadSize,
            initialMaxData: options?.initialMaxData,
            initialMaxStreamDataBidiLocal: options?.initialMaxStreamDataBidiLocal,
            initialMaxStreamsBidi: options?.initialMaxStreamsBidi,
            sessionTicket: options?.sessionTicket,
            allow0Rtt: options?.allow0RTT,
            enableDatagrams: options?.enableDatagrams,
            keylog: options?.keylog,
            qlogDir: options?.qlogDir,
            qlogLevel: options?.qlogLevel,
          },
          (_err: Error | null, events: NativeEvent[]) => {
            session._dispatchEvents(events);
          },
        );

        const eventLoop = new QuicClientEventLoop(nativeClient);
        session._setEventLoop(eventLoop);
        if (shouldAbortConnect()) {
          await eventLoop.close();
          session._setEventLoop(null);
          return;
        }
        try {
          await eventLoop.connect(resolved.socketAddress, options?.servername ?? resolved.servername);
        } catch (error: unknown) {
          session._setEventLoop(null);
          throw error;
        }
        if (shouldAbortConnect()) {
          await eventLoop.close();
          session._setEventLoop(null);
          return;
        }
      });
    } catch (err: unknown) {
      if (shouldAbortConnect()) {
        return;
      }
      const error = toQuicClientConnectError(err);
      session._markReadyError(error);
      session._emitSessionError(error);
    }
  })();

  return session;
}

/**
 * Connect to a raw QUIC server and wait for the handshake to complete.
 * Convenience wrapper around {@link connectQuic} + `session.ready()`.
 */
export async function connectQuicAsync(authority: ConnectionEndpoint, options?: QuicConnectOptions): Promise<QuicClientSession> {
  const session = connectQuic(authority, options);
  await session.ready();
  return session;
}
