import { EventEmitter } from 'node:events';
import { X509Certificate } from 'node:crypto';
import { EVENT_SHUTDOWN_COMPLETE, binding } from './event-loop.js';
import type { NativeEvent, NativeQuicServerBinding } from './event-loop.js';
import { QuicStream } from './quic-stream.js';
import type { QuicServerEventLoopLike } from './quic-stream.js';
import { toSessionError } from './error-map.js';
import type { RuntimeInfo, RuntimeOptions } from './runtime.js';
import { runWithRuntimeSelectionSync, setPendingRuntimeInfo } from './runtime.js';
import { resolveServerClientAuthMode } from './tls-client-auth.js';

const EVENT_NEW_SESSION = 1;
const EVENT_NEW_STREAM = 2;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_RESET = 6;
const EVENT_SESSION_CLOSE = 7;
const EVENT_DRAIN = 8;
const EVENT_ERROR = 10;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_DATAGRAM = 14;

const peerCertificatePresentedState = new WeakMap<QuicServerSession, boolean>();
const peerCertificateChainRawState = new WeakMap<QuicServerSession, Buffer[]>();
const peerCertificateChainState = new WeakMap<QuicServerSession, X509Certificate[]>();

function setSessionPeerCertificateInfo(
  session: QuicServerSession,
  presented: boolean,
  chain?: Buffer[],
): void {
  peerCertificatePresentedState.set(session, presented);
  peerCertificateChainRawState.set(
    session,
    chain ? chain.map((certificate) => Buffer.from(certificate)) : [],
  );
  peerCertificateChainState.delete(session);
}

/** Options for creating a raw QUIC server (no HTTP/3 framing). */
export interface QuicServerOptions {
  /** Runtime selection mode. Default: `'auto'`. */
  runtimeMode?: RuntimeOptions['runtimeMode'];
  /** Runtime fallback policy. Default: `'warn-and-fallback'`. */
  fallbackPolicy?: RuntimeOptions['fallbackPolicy'];
  /** Callback invoked when runtime selection resolves or falls back. */
  onRuntimeEvent?: RuntimeOptions['onRuntimeEvent'];
  /** PEM-encoded private key. */
  key: Buffer | string;
  /** PEM-encoded certificate chain. */
  cert: Buffer | string;
  /** PEM-encoded CA certificate used to verify client certificates. */
  ca?: Buffer | string;
  /** Client certificate policy. Default: `'require'` when `ca` is set, otherwise `'none'`. */
  clientAuth?: 'none' | 'request' | 'require';
  /** ALPN protocol strings. Default: `['quic']`. */
  alpn?: string[];
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
  /** Disable QUIC active connection migration. Default: `true`. */
  disableActiveMigration?: boolean;
  /** Enable QUIC DATAGRAM extension (RFC 9221). Default: `false`. */
  enableDatagrams?: boolean;
  /** Maximum concurrent QUIC connections. Default: 10_000. */
  maxConnections?: number;
  /** Disable QUIC Retry token validation. Default: `true`. */
  disableRetry?: boolean;
  /** Directory for qlog output. */
  qlogDir?: string;
  /** qlog verbosity level. */
  qlogLevel?: string;
  /** Enable TLS keylog. Default: `false`. */
  keylog?: boolean;
}

class QuicWorkerEventLoop implements QuicServerEventLoopLike {
  private readonly worker: NativeQuicServerBinding;
  private closed = false;
  private _shutdownObserved = false;
  private _shutdownResolve: (() => void) | null = null;

  constructor(worker: NativeQuicServerBinding) {
    this.worker = worker;
  }

  /** @internal */
  _onShutdownSentinel(): void {
    this._shutdownObserved = true;
    if (this._shutdownResolve) {
      const resolve = this._shutdownResolve;
      this._shutdownResolve = null;
      resolve();
    }
  }

  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number {
    this.worker.streamSend(connHandle, streamId, data, fin);
    return Math.max(data.length, fin ? 1 : 0);
  }

  streamClose(connHandle: number, streamId: number, errorCode: number): void {
    this.worker.streamClose(connHandle, streamId, errorCode);
  }

  closeSession(connHandle: number, errorCode: number, reason: string): void {
    this.worker.closeSession(connHandle, errorCode, reason);
  }

  sendDatagram(connHandle: number, data: Buffer): boolean {
    return this.worker.sendDatagram(connHandle, data);
  }

  getSessionMetrics(connHandle: number): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  } {
    return this.worker.getSessionMetrics(connHandle);
  }

  pingSession(connHandle: number): boolean {
    return this.worker.pingSession(connHandle);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this._shutdownObserved) return;
    const settled = new Promise<void>((resolve) => {
      if (this._shutdownObserved) {
        resolve();
        return;
      }
      this._shutdownResolve = resolve;
    });
    this.worker.shutdown();
    await settled;
  }
}

/**
 * Typed event declarations for {@link QuicServerSession}.
 */
export interface QuicServerSession {
  on(event: 'stream', listener: (stream: QuicStream) => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'datagram', listener: (data: Buffer) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * A server-side QUIC session representing a single accepted connection.
 * Emits `'stream'` for each new peer-initiated stream.
 */
export class QuicServerSession extends EventEmitter {
  readonly connHandle: number;
  readonly remoteAddress: string;
  readonly remotePort: number;
  private readonly _eventLoop: QuicWorkerEventLoop;
  /** @internal */ readonly _streams = new Map<number, QuicStream>();
  /** @internal */ _nextBidiStreamId = 1; // Server-initiated bidi: 1, 5, 9, ...

  /** @internal */
  constructor(connHandle: number, remoteAddress: string, remotePort: number, eventLoop: QuicWorkerEventLoop) {
    super();
    this.connHandle = connHandle;
    this.remoteAddress = remoteAddress;
    this.remotePort = remotePort;
    this._eventLoop = eventLoop;
  }

  /** Whether the peer presented a verified certificate during the QUIC handshake. */
  get peerCertificatePresented(): boolean {
    return peerCertificatePresentedState.get(this) ?? false;
  }

  /** Open a new server-initiated bidirectional stream. */
  openStream(): QuicStream {
    const streamId = this._nextBidiStreamId;
    this._nextBidiStreamId += 4;
    const hwm = this._streams.size < 100 ? 256 * 1024 : 16 * 1024;
    const stream = new QuicStream({ highWaterMark: hwm });
    stream._connHandle = this.connHandle;
    stream._streamId = streamId;
    stream._serverLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
  }

  /** Close the session with an optional application error code and reason. */
  close(errorCode?: number, reason?: string): void {
    this._eventLoop.closeSession(this.connHandle, errorCode ?? 0, reason ?? '');
  }

  /** Send an unreliable DATAGRAM frame (RFC 9221). */
  sendDatagram(data: Buffer | Uint8Array): boolean {
    return this._eventLoop.sendDatagram(this.connHandle, Buffer.from(data));
  }

  /** Return a transport metrics snapshot, or `null` on failure. */
  getMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  } | null {
    try {
      return this._eventLoop.getSessionMetrics(this.connHandle);
    } catch {
      return null;
    }
  }

  /** Send a PING frame. Returns `true` if the command was queued. */
  ping(): boolean {
    return this._eventLoop.pingSession(this.connHandle);
  }

  /** Return the peer leaf certificate, or `null` when the peer did not present one. */
  getPeerCertificate(): X509Certificate | null {
    return this.getPeerCertificateChain()[0] ?? null;
  }

  /** Return the peer certificate chain as `X509Certificate` objects, leaf first. */
  getPeerCertificateChain(): readonly X509Certificate[] {
    const chainRaw = peerCertificateChainRawState.get(this) ?? [];
    if (chainRaw.length === 0) {
      return [];
    }
    let chain = peerCertificateChainState.get(this);
    if (!chain) {
      chain = chainRaw.map((certificate) => new X509Certificate(certificate));
      peerCertificateChainState.set(this, chain);
    }
    return [...chain];
  }

  /** @internal */
  _getOrCreateStream(streamId: number): QuicStream {
    let stream = this._streams.get(streamId);
    if (!stream) {
      const hwm = this._streams.size < 100 ? 256 * 1024 : 16 * 1024;
      stream = new QuicStream({ highWaterMark: hwm });
      stream._connHandle = this.connHandle;
      stream._streamId = streamId;
      stream._serverLoop = this._eventLoop;
      this._streams.set(streamId, stream);
    }
    return stream;
  }

  /** @internal */
  _cleanup(): void {
    for (const stream of this._streams.values()) {
      stream.destroy();
    }
    this._streams.clear();
  }
}

/**
 * Typed event declarations for {@link QuicServer}.
 */
export interface QuicServer {
  on(event: 'listening', listener: () => void): this;
  on(event: 'session', listener: (session: QuicServerSession) => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'runtime', listener: (info: RuntimeInfo) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * A raw QUIC server (no HTTP/3 framing).
 *
 * Accepts QUIC connections and emits {@link QuicServerSession} instances
 * via the `'session'` event.
 */
export class QuicServer extends EventEmitter {
  private _eventLoop: QuicWorkerEventLoop | null = null;
  private readonly _sessions = new Map<number, QuicServerSession>();
  private readonly _options: QuicServerOptions;
  private _address: { address: string; family: string; port: number } | null = null;
  /** @internal */
  _runtimeInfo: RuntimeInfo | null = null;

  constructor(options: QuicServerOptions) {
    super();
    this._options = options;
    setPendingRuntimeInfo(this, options);
  }

  /** Runtime mode/driver information for this server, when available. */
  get runtimeInfo(): RuntimeInfo | null {
    return this._runtimeInfo;
  }

  /**
   * Bind the QUIC server to a port and start accepting connections.
   * @param port - UDP port to bind.
   * @param host - Bind address (default `'127.0.0.1'`).
   */
  async listen(port: number, host?: string): Promise<{ address: string; family: string; port: number }> {
    const opts = this._options;
    const NativeQuicServer = getNativeQuicServerConstructor();
    const clientAuth = resolveServerClientAuthMode({
      ca: opts.ca,
      clientAuth: opts.clientAuth,
    });
    const addr = runWithRuntimeSelectionSync(this, opts, (runtimeMode) => {
      const native = new NativeQuicServer(
        {
          key: typeof opts.key === 'string' ? Buffer.from(opts.key) : opts.key,
          cert: typeof opts.cert === 'string' ? Buffer.from(opts.cert) : opts.cert,
          ca: opts.ca ? (typeof opts.ca === 'string' ? Buffer.from(opts.ca) : opts.ca) : undefined,
          clientAuth,
          alpn: opts.alpn,
          runtimeMode,
          maxIdleTimeoutMs: opts.maxIdleTimeoutMs,
          maxUdpPayloadSize: opts.maxUdpPayloadSize,
          initialMaxData: opts.initialMaxData,
          initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal,
          initialMaxStreamsBidi: opts.initialMaxStreamsBidi,
          disableActiveMigration: opts.disableActiveMigration,
          enableDatagrams: opts.enableDatagrams,
          maxConnections: opts.maxConnections,
          disableRetry: opts.disableRetry,
          qlogDir: opts.qlogDir,
          qlogLevel: opts.qlogLevel,
          keylog: opts.keylog,
        },
        (_err: Error | null, events: NativeEvent[]) => {
          let hasShutdown = false;
          for (const event of events) {
            if (event.eventType === EVENT_SHUTDOWN_COMPLETE) {
              hasShutdown = true;
            }
          }
          if (!hasShutdown) {
            this._dispatchEvents(events);
          } else {
            this._dispatchEvents(events.filter(e => e.eventType !== EVENT_SHUTDOWN_COMPLETE));
            eventLoop._onShutdownSentinel();
          }
        },
      );

      const eventLoop = new QuicWorkerEventLoop(native);
      const addr = native.listen(port, host ?? '127.0.0.1');
      this._eventLoop = eventLoop;
      return addr;
    });

    this._address = addr;
    this.emit('listening');
    await Promise.resolve();
    return addr;
  }

  /** Return the bound address, or `null` if not listening. */
  address(): { address: string; family: string; port: number } | null {
    return this._address;
  }

  /** Gracefully shut down the server and destroy all sessions. */
  async close(): Promise<void> {
    for (const session of this._sessions.values()) {
      session._cleanup();
    }
    this._sessions.clear();
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
    this.emit('close');
  }

  private _dispatchEvents(events: NativeEvent[]): void {
    for (const event of events) {
      switch (event.eventType) {
        case EVENT_NEW_SESSION:
          this._onNewSession(event);
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
          this._onSessionClose(event);
          break;
        case EVENT_DRAIN:
          this._onDrain(event);
          break;
        case EVENT_HANDSHAKE_COMPLETE:
          this._onHandshakeComplete(event);
          break;
        case EVENT_ERROR:
          this._onError(event);
          break;
        case EVENT_DATAGRAM:
          this._onDatagram(event);
          break;
        default:
          break;
      }
    }
  }

  private _onNewSession(event: NativeEvent): void {
    if (!this._eventLoop) return;
    const session = new QuicServerSession(
      event.connHandle,
      event.meta?.remoteAddr ?? '',
      event.meta?.remotePort ?? 0,
      this._eventLoop,
    );
    this._sessions.set(event.connHandle, session);
  }

  private _onHandshakeComplete(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      setSessionPeerCertificateInfo(
        session,
        event.meta?.peerCertificatePresented ?? false,
        event.meta?.peerCertificateChain,
      );
      this.emit('session', session);
    }
  }

  private _onNewStream(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._getOrCreateStream(event.streamId);
    if (event.data) {
      stream._pushData(event.data);
    }
    if (event.fin) {
      stream._pushData(null);
    }
    session.emit('stream', stream);
  }

  private _onData(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session || !event.data) return;
    const stream = session._getOrCreateStream(event.streamId);
    stream._pushData(event.data);
  }

  private _onFinished(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._streams.get(event.streamId);
    if (stream) {
      stream._pushData(null);
      // Don't delete from map yet — drain callbacks may still be pending.
      // The stream will be cleaned up when the session closes.
    }
  }

  private _onReset(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._streams.get(event.streamId);
    if (stream) {
      stream.destroy(new Error(`stream reset: ${event.meta?.errorCode ?? 0}`));
      session._streams.delete(event.streamId);
    }
  }

  private _onSessionClose(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      session._cleanup();
      session.emit('close');
      this._sessions.delete(event.connHandle);
    }
  }

  private _onDrain(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._streams.get(event.streamId);
    if (stream) {
      stream._onNativeDrain();
    }
  }

  private _onError(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) {
      this.emit('error', toSessionError(event));
      return;
    }
    if (event.streamId >= 0) {
      const stream = session._streams.get(event.streamId);
      if (stream) {
        stream.destroy(new Error(event.meta?.errorReason ?? 'stream error'));
      }
    } else {
      session.emit('error', toSessionError(event));
    }
  }

  private _onDatagram(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session && event.data) {
      session.emit('datagram', event.data);
    }
  }
}

function getNativeQuicServerConstructor(): typeof binding.NativeQuicServer {
  const NativeQuicServer = (binding as Partial<typeof binding>).NativeQuicServer;
  if (typeof NativeQuicServer !== 'function') {
    throw new Error(
      'The loaded @currentspace/http3 native binding is missing `NativeQuicServer`. '
      + 'This install appears to contain an incomplete prebuild, so raw QUIC server APIs '
      + '(`createQuicServer`) are unavailable. Reinstall a fixed package version or rebuild from source.',
    );
  }
  return NativeQuicServer;
}

/**
 * Create a new raw QUIC server (no HTTP/3 framing).
 *
 * @example
 * ```ts
 * import { createQuicServer } from '@currentspace/http3';
 * import { readFileSync } from 'node:fs';
 *
 * const key = readFileSync('key.pem');
 * const cert = readFileSync('cert.pem');
 * const server = createQuicServer({ key, cert, alpn: ['myproto'] });
 * server.on('session', (session) => {
 *   session.on('stream', (stream) => {
 *     stream.pipe(stream); // echo
 *   });
 * });
 * await server.listen(4433);
 * ```
 */
export function createQuicServer(options: QuicServerOptions): QuicServer {
  return new QuicServer(options);
}
