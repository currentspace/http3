import { EventEmitter } from 'node:events';
import { createSecureServer as createHttp2SecureServer, constants as http2Constants } from 'node:http2';
import type { Http2SecureServer, Http2Session, IncomingHttpHeaders, ServerHttp2Stream } from 'node:http2';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Http2ServerSessionAdapter, Http3ServerSession } from './session.js';
import { ServerHttp2StreamAdapter, ServerHttp3Stream, normalizeIncomingHeaders } from './stream.js';
import type { IncomingHeaders, StreamFlags } from './stream.js';
import { WorkerEventLoop, EVENT_SHUTDOWN_COMPLETE, binding } from './event-loop.js';
import type { NativeEvent, NativeWorkerServerBinding, ServerEventLoopLike } from './event-loop.js';
import {
  Http3Error,
  ERR_HTTP3_INVALID_STATE,
  ERR_HTTP3_STREAM_ERROR,
  ERR_HTTP3_TLS_CONFIG_ERROR,
} from './errors.js';
import { toSessionError, toStreamError } from './error-map.js';
import { prepareKeylogFile, subscribeKeylog } from './keylog.js';
import type { RuntimeInfo, RuntimeOptions } from './runtime.js';
import { runWithRuntimeSelectionSync, setPendingRuntimeInfo } from './runtime.js';

// Event type constants (must match Rust EventType enum)
const EVENT_NEW_SESSION = 1;
const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_RESET = 6;
const EVENT_SESSION_CLOSE = 7;
const EVENT_DRAIN = 8;
const EVENT_GOAWAY = 9;
const EVENT_ERROR = 10;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_DATAGRAM = 14;

/** TLS credential options accepted by the server. */
export interface TlsOptions {
  /** PEM-encoded private key. */
  key?: string | Buffer;
  /** PEM-encoded certificate chain. */
  cert?: string | Buffer;
  /** PEM-encoded CA certificate(s) for client verification. */
  ca?: string | Buffer | Array<string | Buffer>;
  /** PKCS#12 bundle (alternative to key/cert). */
  pfx?: Buffer;
  /** Passphrase for the PFX bundle. */
  passphrase?: string;
  /** Override negotiated ALPN protocols. */
  alpnProtocols?: string[];
  /** TLS 1.3 session ticket encryption keys. */
  sessionTicketKeys?: Buffer;
  /** Enable TLS keylog; `true` for auto-path, or a string file path. */
  keylog?: boolean | string;
}

/** Combined TLS + QUIC transport options for the HTTP/3 server. */
export interface ServerOptions extends TlsOptions {
  /** Runtime selection mode. Default: `'auto'`. */
  runtimeMode?: RuntimeOptions['runtimeMode'];
  /** Runtime fallback policy. Default: `'warn-and-fallback'`. */
  fallbackPolicy?: RuntimeOptions['fallbackPolicy'];
  /** Callback invoked when runtime selection resolves or falls back. */
  onRuntimeEvent?: RuntimeOptions['onRuntimeEvent'];
  /** Idle timeout in milliseconds. Default: 30 000. */
  maxIdleTimeoutMs?: number;
  /** Maximum UDP payload size. Default: 1350. */
  maxUdpPayloadSize?: number;
  /** Connection-level flow control window. Default: 100_000_000 bytes. */
  initialMaxData?: number;
  /** Per-stream bidi flow control window. Default: 2_000_000 bytes. */
  initialMaxStreamDataBidiLocal?: number;
  /** Maximum concurrent bidirectional streams. Default: 10_000. */
  initialMaxStreamsBidi?: number;
  /** Disable QUIC active connection migration. Default: `true` (migration disabled). Set `false` to enable. */
  disableActiveMigration?: boolean;
  /** Enable QUIC DATAGRAM extension (RFC 9221). Default: false. */
  enableDatagrams?: boolean;
  /** QPACK dynamic table capacity. */
  qpackMaxTableCapacity?: number;
  /** Maximum QPACK blocked streams. */
  qpackBlockedStreams?: number;
  /** Number of UDP packets to receive per syscall. */
  recvBatchSize?: number;
  /** Number of UDP packets to send per syscall. */
  sendBatchSize?: number;
  /** Directory for qlog output files. */
  qlogDir?: string;
  /** qlog verbosity level (e.g. `'trace'`). */
  qlogLevel?: string;
  /** Interval in ms for emitting `'metrics'` events on sessions. Default: 1000. */
  metricsIntervalMs?: number;
  /** Allow HTTP/1.1 fallback via the TLS ALPN h2 server. Default: `false`. */
  allowHTTP1?: boolean;
  /** HTTP/2 settings passed to the fallback h2 server. */
  settings?: Record<string, number>;
  /** Disable QUIC Retry token validation. Default: `false`. */
  disableRetry?: boolean;
  /** Maximum number of concurrent QUIC connections. Default: 10_000. */
  maxConnections?: number;
  /**
   * Interval in milliseconds between automatic QUIC PING frames per session.
   * Keeps idle connections alive when streams are open but no data is flowing.
   * Must be less than `maxIdleTimeoutMs` to be effective. Default: disabled.
   */
  keepAliveIntervalMs?: number;
  /** Enable SO_REUSEPORT for the UDP socket. Default: `false`. */
  reusePort?: boolean;
  /** Enable QUIC-LB connection-ID routing. Requires `serverId`. Default: `false`. */
  quicLb?: boolean;
  /** 8-byte server identifier for QUIC-LB (hex string or Buffer). */
  serverId?: Buffer | string;
}

/** Callback invoked for each new HTTP/3 request stream. */
export type StreamListener = (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void;

/** Network address information returned by {@link Http3SecureServer.address}. */
export interface AddressInfo {
  /** Bound IP address. */
  address: string;
  /** Address family (`'IPv4'` or `'IPv6'`). */
  family: string;
  /** Bound UDP port number. */
  port: number;
}

/**
 * Typed event declarations for {@link Http3SecureServer}.
 */
export interface Http3SecureServer {
  on(event: 'listening', listener: () => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'runtime', listener: (info: RuntimeInfo) => void): this;
  on(event: 'session', listener: (session: Http3ServerSession) => void): this;
  on(event: 'stream', listener: StreamListener): this;
  on(event: 'request', listener: (req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse) => void): this;
  on(event: 'close', listener: () => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * An HTTP/3 (+ HTTP/2 fallback) secure server.
 *
 * Binds a UDP socket for QUIC/H3 traffic and a TLS socket for H2 on the
 * same port.  Streams are emitted as {@link ServerHttp3Stream} instances.
 */
export class Http3SecureServer extends EventEmitter {
  private readonly _options: ServerOptions;
  private _eventLoop: ServerEventLoopLike | null = null;
  private _workerServer: NativeWorkerServerBinding | null = null;
  private _h2Server: Http2SecureServer | null = null;
  private _address: AddressInfo | null = null;
  private _starting = false;
  private _keylogPath: string | null = null;
  /** @internal */
  _runtimeInfo: RuntimeInfo | null = null;
  private readonly _sessions = new Map<number, Http3ServerSession>();
  private readonly _h2Sessions = new Map<Http2Session, Http2ServerSessionAdapter>();
  private readonly _h2SessionStreams = new Map<Http2Session, Set<ServerHttp3Stream>>();
  private readonly _streams = new Map<string, ServerHttp3Stream>();

  constructor(options: ServerOptions, onStream?: StreamListener) {
    super();
    this._options = options;
    setPendingRuntimeInfo(this, options);
    if (onStream) {
      this.on('stream', onStream);
    }
  }

  /** Runtime mode/driver information for this server, when available. */
  get runtimeInfo(): RuntimeInfo | null {
    return this._runtimeInfo;
  }

  /**
   * Start listening for QUIC and HTTP/2 connections.
   * @param port - UDP/TCP port to bind.
   * @param host - Bind address (default `'0.0.0.0'`).
   */
  listen(port: number, host?: string): this {
    if (this._eventLoop || this._h2Server || this._starting) {
      throw new Http3Error('server is already listening', ERR_HTTP3_INVALID_STATE);
    }

    const listenHost = host ?? '0.0.0.0';
    const { key, cert } = this._resolveNativeTlsCreds();
    const ca = this._getNativeCa();
    const serverId = this._resolveServerId();
    if (this._options.quicLb && !serverId) {
      throw new TypeError('serverId is required when quicLb is enabled');
    }
    if (!this._options.quicLb && serverId) {
      throw new TypeError('serverId requires quicLb=true');
    }
    this._keylogPath = prepareKeylogFile(this._options.keylog);
    let quicStart: {
      workerServer: NativeWorkerServerBinding;
      eventLoop: WorkerEventLoop;
      addrInfo: { address: string; family: string; port: number };
    };
    try {
      quicStart = runWithRuntimeSelectionSync(this, this._options, (runtimeMode) => {
        const workerServer = new binding.NativeWorkerServer({
          key,
          cert,
          ca,
          runtimeMode,
          quicLb: this._options.quicLb,
          serverId,
          keylog: Boolean(this._keylogPath),
          maxIdleTimeoutMs: this._options.maxIdleTimeoutMs,
          maxUdpPayloadSize: this._options.maxUdpPayloadSize,
          initialMaxData: this._options.initialMaxData,
          initialMaxStreamDataBidiLocal: this._options.initialMaxStreamDataBidiLocal,
          initialMaxStreamsBidi: this._options.initialMaxStreamsBidi,
          disableActiveMigration: this._options.disableActiveMigration,
          enableDatagrams: this._options.enableDatagrams,
          qpackMaxTableCapacity: this._options.qpackMaxTableCapacity,
          qpackBlockedStreams: this._options.qpackBlockedStreams,
          recvBatchSize: this._options.recvBatchSize,
          sendBatchSize: this._options.sendBatchSize,
          qlogDir: this._options.qlogDir,
          qlogLevel: this._options.qlogLevel,
          sessionTicketKeys: this._options.sessionTicketKeys,
          maxConnections: this._options.maxConnections,
          disableRetry: this._options.disableRetry,
          reusePort: this._options.reusePort,
        }, (_err: Error | null, events: NativeEvent[]) => {
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
        });

        const eventLoop = new WorkerEventLoop(workerServer);
        const addrInfo = workerServer.listen(port, listenHost);
        return { workerServer, eventLoop, addrInfo };
      });
    } catch (err: unknown) {
      this._keylogPath = null;
      throw err;
    }

    this._workerServer = quicStart.workerServer;
    this._eventLoop = quicStart.eventLoop;
    this._address = {
      address: quicStart.addrInfo.address,
      family: quicStart.addrInfo.family,
      port: quicStart.addrInfo.port,
    };
    this._starting = true;

    let h2Server: Http2SecureServer;
    try {
      h2Server = this._createH2Server();
    } catch (err: unknown) {
      void this._abortStartup(err);
      return this;
    }
    this._h2Server = h2Server;
    this._attachH2ServerListeners(h2Server);

    const onH2BindError = (err: Error): void => {
      void this._abortStartup(err);
    };
    h2Server.once('error', onH2BindError);
    h2Server.listen(quicStart.addrInfo.port, listenHost, () => {
      h2Server.off('error', onH2BindError);
      this._starting = false;
      process.nextTick(() => this.emit('listening'));
    });

    return this;
  }

  /** Gracefully shut down the server, closing all sessions and sockets. */
  async close(cb?: (err?: Error) => void): Promise<void> {
    try {
      this._starting = false;
      this._destroyH2Sessions();
      if (this._eventLoop) {
        await this._eventLoop.close();
        this._eventLoop = null;
      }
      this._workerServer = null;
      await this._closeH2Server();
      for (const session of this._sessions.values()) {
        session._stopKeylogEmitter();
      }
      this._sessions.clear();
      this._h2Sessions.clear();
      this._h2SessionStreams.clear();
      this._streams.clear();
      this._address = null;
      this._keylogPath = null;
      this.emit('close');
      cb?.();
    } catch (err: unknown) {
      const error = err instanceof Error ? err : new Error(String(err));
      cb?.(error);
    }
  }

  /** Return the bound address, or `null` if not listening. */
  address(): AddressInfo | null {
    return this._address;
  }

  private _dispatchEvents(events: NativeEvent[]): void {
    for (const event of events) {
      switch (event.eventType) {
        case EVENT_NEW_SESSION:
          this._onNewSession(event);
          break;
        case EVENT_HANDSHAKE_COMPLETE:
          this._onHandshakeComplete(event);
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
        case EVENT_SESSION_CLOSE:
          this._onSessionClose(event);
          break;
        case EVENT_DRAIN:
          this._onDrain(event);
          break;
        case EVENT_GOAWAY:
          this._onGoaway(event);
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

  private _createH2Server(): Http2SecureServer {
    const allowHTTP1 = this._options.allowHTTP1 ?? false;
    const alpnProtocols = this._resolveH2AlpnProtocols(allowHTTP1);
    return createHttp2SecureServer({
      key: this._options.key,
      cert: this._options.cert,
      ca: this._options.ca,
      pfx: this._options.pfx,
      passphrase: this._options.passphrase,
      allowHTTP1,
      ALPNProtocols: alpnProtocols,
      settings: this._options.settings,
    });
  }

  private _resolveH2AlpnProtocols(allowHTTP1: boolean): string[] {
    const requested = this._options.alpnProtocols
      ?.filter(protocol => protocol !== 'h3' && protocol !== 'h3-29');
    const defaults = allowHTTP1 ? ['h2', 'http/1.1'] : ['h2'];
    const protocols = requested && requested.length > 0 ? requested : defaults;
    if (!protocols.includes('h2')) {
      protocols.unshift('h2');
    }
    return allowHTTP1 ? protocols : protocols.filter(protocol => protocol === 'h2');
  }

  private _getNativeCa(): Buffer | undefined {
    const { ca } = this._options;
    if (!ca) return undefined;
    const first = Array.isArray(ca) ? ca[0] : ca;
    return typeof first === 'string' ? Buffer.from(first) : first;
  }

  private _resolveServerId(): Buffer | undefined {
    const { serverId } = this._options;
    if (typeof serverId === 'undefined') return undefined;
    if (Buffer.isBuffer(serverId)) {
      if (serverId.length !== 8) {
        throw new TypeError('serverId must be exactly 8 bytes');
      }
      return serverId;
    }

    const trimmed = serverId.trim();
    const normalized = trimmed.startsWith('0x') || trimmed.startsWith('0X')
      ? trimmed.slice(2)
      : trimmed;
    if (!/^[0-9a-fA-F]{16}$/.test(normalized)) {
      throw new TypeError('serverId string must be 16 hex chars (optionally prefixed with 0x)');
    }

    return Buffer.from(normalized.toLowerCase(), 'hex');
  }

  private _resolveNativeTlsCreds(): { key: Buffer; cert: Buffer } {
    if (this._options.key && this._options.cert) {
      const key = typeof this._options.key === 'string'
        ? Buffer.from(this._options.key)
        : this._options.key;
      const cert = typeof this._options.cert === 'string'
        ? Buffer.from(this._options.cert)
        : this._options.cert;
      return { key, cert };
    }

    if (this._options.pfx) {
      const tempDir = mkdtempSync(join(tmpdir(), 'http3-pfx-'));
      try {
        const pfxPath = join(tempDir, 'bundle.p12');
        writeFileSync(pfxPath, this._options.pfx);

        const passIn = `pass:${this._options.passphrase ?? ''}`;
        const keyDump = execFileSync('openssl', [
          'pkcs12',
          '-in',
          pfxPath,
          '-nocerts',
          '-nodes',
          '-passin',
          passIn,
        ], { encoding: 'utf8' });

        const certDump = execFileSync('openssl', [
          'pkcs12',
          '-in',
          pfxPath,
          '-nokeys',
          '-passin',
          passIn,
        ], { encoding: 'utf8' });

        const keyBlocks = keyDump.match(/-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g) ?? [];
        const certBlocks = certDump.match(/-----BEGIN CERTIFICATE-----[\s\S]*?-----END CERTIFICATE-----/g) ?? [];

        if (keyBlocks.length === 0 || certBlocks.length === 0) {
          throw new Error('PKCS#12 archive missing usable private key or certificate');
        }

        return {
          key: Buffer.from(`${keyBlocks.join('\n')}\n`, 'utf8'),
          cert: Buffer.from(`${certBlocks.join('\n')}\n`, 'utf8'),
        };
      } catch (err: unknown) {
        throw new Http3Error(
          `invalid pfx/passphrase: ${err instanceof Error ? err.message : String(err)}`,
          ERR_HTTP3_TLS_CONFIG_ERROR,
        );
      } finally {
        rmSync(tempDir, { recursive: true, force: true });
      }
    }

    throw new Http3Error(
      'missing TLS credentials: provide key/cert or pfx',
      ERR_HTTP3_TLS_CONFIG_ERROR,
    );
  }

  private _attachH2ServerListeners(h2Server: Http2SecureServer): void {
    h2Server.on('error', (err: Error) => {
      if (!this._starting) {
        this.emit('error', err);
      }
    });
    h2Server.on('request', (req: IncomingMessage, res: ServerResponse) => {
      if (req.httpVersionMajor !== 1) return;
      this.emit('request', req, res);
    });
    h2Server.on('session', (session: Http2Session) => {
      this._ensureH2Session(session);
    });
    h2Server.on('stream', (stream: ServerHttp2Stream, headers: IncomingHttpHeaders, flags: number) => {
      const streamSession = stream.session;
      if (!streamSession) {
        stream.close(http2Constants.NGHTTP2_INTERNAL_ERROR);
        return;
      }
      const session = this._ensureH2Session(streamSession);
      const adapter = new ServerHttp2StreamAdapter(stream);
      this._trackH2Stream(streamSession, adapter);

      const incoming = normalizeIncomingHeaders(headers);
      const streamFlags: StreamFlags = {
        endStream: (flags & http2Constants.NGHTTP2_FLAG_END_STREAM) !== 0,
      };
      this.emit('stream', adapter, incoming, streamFlags);
      void session;
    });
  }

  private _ensureH2Session(h2Session: Http2Session): Http2ServerSessionAdapter {
    const existing = this._h2Sessions.get(h2Session);
    if (existing) return existing;

    const session = new Http2ServerSessionAdapter(h2Session);
    this._h2Sessions.set(h2Session, session);
    session.once('close', () => {
      this._h2Sessions.delete(h2Session);
      this._destroyH2SessionStreams(h2Session);
    });
    this.emit('session', session);
    return session;
  }

  private _trackH2Stream(h2Session: Http2Session, stream: ServerHttp3Stream): void {
    let streams = this._h2SessionStreams.get(h2Session);
    if (!streams) {
      streams = new Set<ServerHttp3Stream>();
      this._h2SessionStreams.set(h2Session, streams);
    }
    streams.add(stream);
    stream.once('close', () => {
      const current = this._h2SessionStreams.get(h2Session);
      if (!current) return;
      current.delete(stream);
      if (current.size === 0) {
        this._h2SessionStreams.delete(h2Session);
      }
    });
  }

  private _destroyH2SessionStreams(h2Session: Http2Session): void {
    const streams = this._h2SessionStreams.get(h2Session);
    if (!streams) return;
    const err = new Error('session closed');
    for (const stream of streams) {
      if (stream.listenerCount('error') > 0) {
        stream.destroy(err);
      } else {
        stream.destroy();
      }
    }
    this._h2SessionStreams.delete(h2Session);
  }

  private _destroyH2Sessions(): void {
    for (const [h2Session, session] of this._h2Sessions) {
      this._destroyH2SessionStreams(h2Session);
      h2Session.destroy();
      session.emit('close');
    }
    this._h2Sessions.clear();
  }

  private async _closeH2Server(): Promise<void> {
    if (!this._h2Server) return;
    const h2Server = this._h2Server;
    this._h2Server = null;
    const closeAllConnections = (h2Server as Http2SecureServer & {
      closeAllConnections?: () => void;
      closeIdleConnections?: () => void;
      unref?: () => void;
    }).closeAllConnections;
    const closeIdleConnections = (h2Server as Http2SecureServer & {
      closeAllConnections?: () => void;
      closeIdleConnections?: () => void;
      unref?: () => void;
    }).closeIdleConnections;
    const unref = (h2Server as Http2SecureServer & {
      closeAllConnections?: () => void;
      closeIdleConnections?: () => void;
      unref?: () => void;
    }).unref;

    // Best-effort: once shutdown begins, don't let stale H2 sockets pin the
    // process if Node keeps the TLS server referenced longer than expected.
    try { closeAllConnections?.call(h2Server); } catch { /* ignore */ }
    try { closeIdleConnections?.call(h2Server); } catch { /* ignore */ }
    try { unref?.call(h2Server); } catch { /* ignore */ }
    await new Promise<void>((resolve, reject) => {
      try {
        h2Server.close((err?: Error) => {
          if (!err || this._isServerNotRunning(err)) {
            resolve();
            return;
          }
          reject(err);
        });
      } catch (err: unknown) {
        if (err instanceof Error && this._isServerNotRunning(err)) {
          resolve();
          return;
        }
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  private _isServerNotRunning(err: Error): boolean {
    return err.name === 'ERR_SERVER_NOT_RUNNING';
  }

  private async _abortStartup(err: unknown): Promise<void> {
    const error = err instanceof Error ? err : new Error(String(err));
    this._starting = false;
    this._address = null;
    try {
      if (this._eventLoop) {
        await this._eventLoop.close();
      }
      await this._closeH2Server();
    } catch {
      // Best-effort shutdown for partial startup failures.
    } finally {
      this._eventLoop = null;
      this._workerServer = null;
      for (const session of this._sessions.values()) {
        session._stopKeylogEmitter();
      }
      this._sessions.clear();
      this._h2Sessions.clear();
      this._h2SessionStreams.clear();
      this._streams.clear();
      this._keylogPath = null;
    }
    process.nextTick(() => this.emit('error', error));
  }

  private _onNewSession(event: NativeEvent): void {
    const session = new Http3ServerSession();
    session._connHandle = event.connHandle;
    session._remoteAddress = event.meta?.remoteAddr ?? '';
    session._remotePort = event.meta?.remotePort ?? 0;
    session._serverName = event.meta?.serverName ?? '';
    session._qlogPath = this._options.qlogDir ?? null;
    session._eventLoop = this._eventLoop;
    if (this._keylogPath) {
      session._setKeylogUnsubscribe(subscribeKeylog(this._keylogPath, (line) => {
        session.emit('keylog', line);
      }));
    }
    session._startMetricsEmitter(this._options.metricsIntervalMs ?? 1000, () => session.getMetrics());
    this._sessions.set(event.connHandle, session);
    this.emit('session', session);
    if (this._keylogPath) {
      process.nextTick(() => {
        session.emit('keylog', Buffer.from(`# keylog enabled ${this._keylogPath}\n`));
      });
    }
  }

  private _onHandshakeComplete(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      session._handshakeComplete = true;
    }
  }

  private _onHeaders(event: NativeEvent): void {
    if (!event.headers) return;
    const streamKey = `${event.connHandle}:${event.streamId}`;

    const existing = this._streams.get(streamKey);
    if (existing) {
      // Duplicate headers on the same stream — these are trailers
      const trailers: IncomingHeaders = {};
      for (const h of event.headers) {
        trailers[h.name] = h.value;
      }
      existing.emit('trailers', trailers);
      return;
    }

    // New stream — create and emit
    const stream = new ServerHttp3Stream();
    stream._connHandle = event.connHandle;
    stream._streamId = event.streamId;
    stream._eventLoop = this._eventLoop;
    this._streams.set(streamKey, stream);

    const headers: IncomingHeaders = {};
    for (const h of event.headers) {
      headers[h.name] = h.value;
    }

    const flags: StreamFlags = { endStream: event.fin ?? false };
    this.emit('stream', stream, headers, flags);
  }

  private _onData(event: NativeEvent): void {
    const streamKey = `${event.connHandle}:${event.streamId}`;
    const stream = this._streams.get(streamKey);
    if (stream && event.data) {
      stream._onActivity();
      stream._pushData(event.data);
    }
  }

  private _onFinished(event: NativeEvent): void {
    const streamKey = `${event.connHandle}:${event.streamId}`;
    const stream = this._streams.get(streamKey);
    if (stream) {
      stream._pushData(null); // EOF on readable side
      // If the readable side was never consumed (e.g., a GET handler that
      // only writes a response), it stays in non-flowing mode and 'close'
      // never fires.  Resume it so Node's Duplex can fully destroy.
      if (stream.readableFlowing === null) {
        stream.resume();
      }
      // Don't remove from _streams yet — the writable side may still
      // have pending drain callbacks. Stream is cleaned up on session close
      // or when both sides complete naturally via 'close' event.
      stream.once('close', () => {
        this._streams.delete(streamKey);
      });
    }
  }

  private _onReset(event: NativeEvent): void {
    const streamKey = `${event.connHandle}:${event.streamId}`;
    const stream = this._streams.get(streamKey);
    if (stream) {
      stream.emit('aborted');
      stream.destroy(new Http3Error('stream reset', ERR_HTTP3_STREAM_ERROR, {
        h3Code: event.meta?.errorCode,
      }));
      this._streams.delete(streamKey);
    }
  }

  private _onSessionClose(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      session._stopMetricsEmitter();
      session._stopKeylogEmitter();
      session.emit('close');
      this._sessions.delete(event.connHandle);
    }
    // Clean up any streams belonging to this session
    const err = new Error('session closed');
    for (const [key, stream] of this._streams) {
      if (key.startsWith(`${event.connHandle}:`)) {
        if (stream.listenerCount('error') > 0) {
          stream.destroy(err);
        } else {
          stream.destroy();
        }
        this._streams.delete(key);
      }
    }
  }

  private _onDrain(event: NativeEvent): void {
    const streamKey = `${event.connHandle}:${event.streamId}`;
    const stream = this._streams.get(streamKey);
    if (stream) {
      stream._onNativeDrain();
    }
  }

  private _onGoaway(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      session.emit('goaway');
    }
  }

  private _onError(event: NativeEvent): void {
    if (event.streamId >= 0) {
      const streamKey = `${event.connHandle}:${event.streamId}`;
      const stream = this._streams.get(streamKey);
      if (stream) {
        stream.destroy(toStreamError(event));
      }
    } else {
      const session = this._sessions.get(event.connHandle);
      if (session) {
        session.emit('error', toSessionError(event));
        return;
      }
      this.emit('error', toSessionError(event));
    }
  }

  private _onDatagram(event: NativeEvent): void {
    if (!event.data) return;
    const session = this._sessions.get(event.connHandle);
    if (session) {
      session.emit('datagram', event.data);
    }
  }
}

/**
 * Create a new HTTP/3 secure server.
 *
 * @example
 * ```ts
 * import { createSecureServer } from '@currentspace/http3';
 * import { readFileSync } from 'node:fs';
 *
 * const server = createSecureServer({
 *   key: readFileSync('key.pem'),
 *   cert: readFileSync('cert.pem'),
 * }, (stream, headers) => {
 *   stream.respond({ ':status': '200' });
 *   stream.end('Hello, HTTP/3!');
 * });
 * server.listen(443);
 * ```
 */
export function createSecureServer(options: ServerOptions, onStream?: StreamListener): Http3SecureServer {
  return new Http3SecureServer(options, onStream);
}
