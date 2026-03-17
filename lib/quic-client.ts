import { EventEmitter } from 'node:events';
import { binding } from './event-loop.js';
import type { NativeEvent, NativeQuicClientBinding } from './event-loop.js';
import { QuicStream } from './quic-stream.js';
import type { QuicClientEventLoopLike } from './quic-stream.js';

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
  /** PEM-encoded CA certificate to trust. */
  ca?: Buffer | string;
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
  private _readySettled = false;
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
    this._cleanupStreams();
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
  }

  /** @internal */
  _setEventLoop(loop_: QuicClientEventLoop): void {
    this._eventLoop = loop_;
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
            this.emit('sessionTicket', Buffer.from(event.data));
          }
          break;
        case EVENT_DATAGRAM:
          if (event.data) {
            this.emit('datagram', Buffer.from(event.data));
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
      stream.push(Buffer.from(event.data));
    }
    if (event.fin) {
      stream.push(null);
    }
    this.emit('stream', stream);
  }

  private _onData(event: NativeEvent): void {
    if (!event.data) return;
    const stream = this._getOrCreateStream(event.streamId);
    stream.push(Buffer.from(event.data));
  }

  private _onFinished(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream.push(null);
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
      this.emit('error', new Error(event.meta?.errorReason ?? 'session error'));
      if (!this._handshakeComplete) {
        this._markReadyError(new Error(event.meta?.errorReason ?? 'session error'));
      }
    }
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

  private _markReady(): void {
    if (this._readySettled) return;
    this._readySettled = true;
    this._resolveReady?.();
    this._resolveReady = null;
    this._rejectReady = null;
  }

  private _markReadyError(err: Error): void {
    if (this._readySettled) return;
    this._readySettled = true;
    this._rejectReady?.(err);
    this._resolveReady = null;
    this._rejectReady = null;
  }
}

function normalizeCa(ca?: string | Buffer): Buffer | undefined {
  if (!ca) return undefined;
  return typeof ca === 'string' ? Buffer.from(ca) : ca;
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
export function connectQuic(authority: string, options?: QuicConnectOptions): QuicClientSession {
  let host: string;
  let port: number;
  let servername: string;

  try {
    const url = new URL(authority.includes('://') ? authority : `quic://${authority}`);
    host = url.hostname;
    port = parseInt(url.port || '4433', 10);
    servername = options?.servername ?? host;
  } catch {
    const parts = authority.split(':');
    host = parts[0];
    port = parseInt(parts[1] ?? '4433', 10);
    servername = options?.servername ?? host;
  }

  const session = new QuicClientSession();
  const NativeQuicClient = getNativeQuicClientConstructor();

  const nativeClient = new NativeQuicClient(
    {
      ca: normalizeCa(options?.ca),
      rejectUnauthorized: options?.rejectUnauthorized,
      alpn: options?.alpn,
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

  void (async (): Promise<void> => {
    try {
      await eventLoop.connect(`${host}:${port}`, servername);
    } catch (err: unknown) {
      const error = err instanceof Error ? err : new Error(String(err));
      session.emit('error', error);
    }
  })();

  return session;
}

/**
 * Connect to a raw QUIC server and wait for the handshake to complete.
 * Convenience wrapper around {@link connectQuic} + `session.ready()`.
 */
export async function connectQuicAsync(authority: string, options?: QuicConnectOptions): Promise<QuicClientSession> {
  const session = connectQuic(authority, options);
  await session.ready();
  return session;
}
