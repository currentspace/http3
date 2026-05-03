import { EventEmitter } from 'node:events';
import type { Http2Session } from 'node:http2';
import type { NativeEvent, ServerEventLoopLike, ClientEventLoop } from './event-loop.js';
import type { RuntimeInfo } from './runtime.js';

/** QUIC transport tuning parameters shared by server and client. */
export interface SessionOptions {
  /** Idle timeout in milliseconds before the connection is closed. Default: 30 000. */
  maxIdleTimeoutMs?: number;
  /** Maximum UDP payload size in bytes. Default: 1350. */
  maxUdpPayloadSize?: number;
  /** Connection-level flow control window in bytes. Default: 10 MiB. */
  initialMaxData?: number;
  /** Per-stream flow control window for locally-initiated bidi streams. Default: 1 MiB. */
  initialMaxStreamDataBidiLocal?: number;
  /** Maximum concurrent bidirectional streams. Default: 100. */
  initialMaxStreamsBidi?: number;
  /** Disable QUIC active connection migration. Default: true. */
  disableActiveMigration?: boolean;
  /** Enable QUIC DATAGRAM extension (RFC 9221). Default: false. */
  enableDatagrams?: boolean;
  /** QPACK dynamic table capacity. Default: 0 (disabled). */
  qpackMaxTableCapacity?: number;
  /** Maximum QPACK blocked streams. Default: 0. */
  qpackBlockedStreams?: number;
}

/** Point-in-time snapshot of session-level transport metrics. */
export interface SessionMetrics {
  /** Total QUIC packets received. */
  packetsIn: number;
  /** Total QUIC packets sent. */
  packetsOut: number;
  /** Total bytes received (UDP payload). */
  bytesIn: number;
  /** Total bytes sent (UDP payload). */
  bytesOut: number;
  /** Time to complete the TLS handshake, in milliseconds. */
  handshakeTimeMs: number;
  /** Smoothed round-trip time estimate in milliseconds. */
  rttMs: number;
  /** Current congestion window in bytes. */
  cwnd: number;
  /** Current outbound DATAGRAM retry queue depth. */
  datagramQueueDepth: number;
}

export type PingCallback = (err: Error | null, duration: number) => void;

/** Details supplied with H3/raw QUIC session close events. */
export interface SessionCloseInfo {
  errorCode: number;
  reason: string;
}

/** Details supplied with H3 GOAWAY events. */
export interface GoawayInfo {
  lastStreamId: number;
}

export function defaultSessionCloseInfo(): SessionCloseInfo {
  return { errorCode: 0, reason: '' };
}

/** @internal */
export function sessionCloseInfoFromEvent(event: NativeEvent): SessionCloseInfo {
  return {
    errorCode: event.meta?.errorCode ?? 0,
    reason: event.meta?.errorReason ?? '',
  };
}

/**
 * Typed event declarations for {@link Http3Session}.
 */
export interface Http3Session {
  on(event: 'close', listener: (info: SessionCloseInfo) => void): this;
  on(event: 'error', listener: (err: Error) => void): this;
  on(event: 'goaway', listener: (info: GoawayInfo) => void): this;
  on(event: 'metrics', listener: (metrics: SessionMetrics) => void): this;
  on(event: 'keylog', listener: (line: Buffer) => void): this;
  on(event: 'datagram', listener: (data: Buffer) => void): this;
  on(event: 'runtime', listener: (info: RuntimeInfo) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
}

/**
 * Base class for HTTP/3 sessions (both server-side and client-side).
 *
 * Wraps a single QUIC connection and exposes transport metrics, keylog
 * support, and lifecycle events.
 */
export class Http3Session extends EventEmitter {
  /** @internal */
  _alpnProtocol = 'h3';

  /** @internal */
  _connHandle = -1;
  /** @internal */
  _remoteAddress = '';
  /** @internal */
  _remotePort = 0;
  /** @internal */
  _handshakeComplete = false;
  /** @internal */
  _lastMetrics: SessionMetrics | null = null;
  /** @internal */
  _metricsTimer: NodeJS.Timeout | null = null;
  /** @internal */
  _metricsPolling = false;
  /** @internal */
  _keylogUnsubscribe: (() => void) | null = null;
  /** @internal */
  _pendingPingCallbacks: PingCallback[] = [];
  /** @internal */
  _runtimeInfo: RuntimeInfo | null = null;
  /** @internal */
  _closeEmitted = false;
  /** @internal */
  _closeInfo: SessionCloseInfo | null = null;

  /** Negotiated ALPN protocol (`'h3'` or `'h2'`). */
  get alpnProtocol(): string { return this._alpnProtocol; }
  /** Remote peer IP address. */
  get remoteAddress(): string { return this._remoteAddress; }
  /** Remote peer UDP port. */
  get remotePort(): number { return this._remotePort; }
  /** Whether the QUIC/TLS handshake has completed. */
  get handshakeComplete(): boolean { return this._handshakeComplete; }
  /** Runtime mode/driver information for this session, when available. */
  get runtimeInfo(): RuntimeInfo | null { return this._runtimeInfo; }
  /** Whether the session has reached a terminal close state. */
  get closed(): boolean { return this._closeEmitted; }

  /** @internal */
  _startMetricsEmitter(
    intervalMs: number,
    pollMetrics: () => SessionMetrics | Promise<SessionMetrics | null> | null,
  ): void {
    this._stopMetricsEmitter();
    if (!Number.isFinite(intervalMs) || intervalMs <= 0) return;
    this._metricsTimer = setInterval(() => {
      if (this._metricsPolling) return;
      this._metricsPolling = true;
      void (async () => {
        try {
          const snapshot = await pollMetrics();
          if (!snapshot) return;
          this._lastMetrics = snapshot;
          this.emit('metrics', snapshot);
        } catch {
          // Metrics polling is best-effort and must not crash the session.
        } finally {
          this._metricsPolling = false;
        }
      })();
    }, intervalMs);
    this._metricsTimer.unref();
  }

  /** @internal */
  _stopMetricsEmitter(): void {
    if (!this._metricsTimer) return;
    clearInterval(this._metricsTimer);
    this._metricsTimer = null;
    this._metricsPolling = false;
  }

  /** @internal */
  _setKeylogUnsubscribe(unsubscribe: (() => void) | null): void {
    this._keylogUnsubscribe?.();
    this._keylogUnsubscribe = unsubscribe;
  }

  /** @internal */
  _stopKeylogEmitter(): void {
    this._keylogUnsubscribe?.();
    this._keylogUnsubscribe = null;
  }

  /** Return the most recent transport metrics snapshot, or `null` if unavailable. */
  getMetrics(): SessionMetrics | null {
    return this._lastMetrics;
  }

  /** Send a PING frame and return the last known RTT in milliseconds. */
  ping(cb?: PingCallback): number {
    const snapshot = this._lastMetrics?.rttMs ?? 0;
    if (cb) process.nextTick(cb, null, snapshot);
    return snapshot;
  }

  /** @internal */
  _onPingAck(durationMs: number): void {
    const cb = this._pendingPingCallbacks.shift();
    if (cb) {
      cb(null, durationMs);
    }
  }

  /** @internal */
  _rejectPendingPings(err: Error): void {
    const callbacks = this._pendingPingCallbacks.splice(0);
    for (const cb of callbacks) {
      cb(err, 0);
    }
  }

  /** @internal */
  _emitClose(info: SessionCloseInfo = defaultSessionCloseInfo()): void {
    this._closeInfo = info;
    if (this._closeEmitted) return;
    this._closeEmitted = true;
    this.emit('close', info);
  }

  /** Return the peer's advertised QUIC transport settings, or `null`. */
  getRemoteSettings(): Record<string, number | boolean> | null {
    return null;
  }

  /** Return the filesystem path to the qlog file for this session, or `null`. */
  exportQlog(): string | null {
    return null;
  }

  // eslint-disable-next-line @typescript-eslint/require-await
  async close(_code?: number): Promise<void> {
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(new Error('session closed'));
    this._emitClose();
  }

  // eslint-disable-next-line @typescript-eslint/require-await
  async destroy(_err?: Error): Promise<void> {
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(_err ?? new Error('session destroyed'));
    this._emitClose();
  }
}

/**
 * Server-side HTTP/3 session representing one accepted QUIC connection.
 *
 * Created automatically by {@link Http3SecureServer} when a new client connects.
 */
export class Http3ServerSession extends Http3Session {
  /** @internal */
  _serverName = '';
  /** @internal */
  _eventLoop: ServerEventLoopLike | null = null;
  /** @internal */
  _h2Session: Http2Session | null = null;
  /** @internal */
  _closing = false;
  /** @internal */
  _qlogPath: string | null = null;

  /** The SNI hostname presented by the client during TLS handshake. */
  get serverName(): string { return this._serverName; }

  /**
   * Gracefully close the session. Sends CONNECTION_CLOSE frame and waits
   * for the QUIC draining period to complete before resolving.
   */
  async close(code?: number, reason = ''): Promise<void> {
    if (this._closing || this._closeEmitted) return;
    this._closing = true;

    const closeObserved = new Promise<void>((resolve) => {
      const timeout = setTimeout(() => {
        this.removeListener('close', onClose);
        resolve();
      }, 5000);
      timeout.unref();

      const onClose = (): void => {
        clearTimeout(timeout);
        resolve();
      };
      this.once('close', onClose);
    });

    if (this._h2Session) {
      if (code && code !== 0) {
        try {
          this._h2Session.goaway(code);
        } catch {
          // Ignore GOAWAY errors while shutting down.
        }
      }
      this._h2Session.close();
    } else if (this._eventLoop) {
      this._eventLoop.closeSession(this._connHandle, code ?? 0, reason);
    }

    // Wait for the native SESSION_CLOSE event (emitted by server._onSessionClose
    // when quiche finishes draining). Times out after 5s to prevent hanging.
    await closeObserved;
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(new Error('session closed'));
  }

  /**
   * Immediately destroy the session without waiting for draining.
   */
  // eslint-disable-next-line @typescript-eslint/require-await
  async destroy(err?: Error): Promise<void> {
    if (this._h2Session) {
      this._h2Session.destroy(err);
      this._stopMetricsEmitter();
      this._stopKeylogEmitter();
      this._rejectPendingPings(err ?? new Error('session destroyed'));
      this._emitClose();
      return;
    }
    if (this._eventLoop) this._eventLoop.closeSession(this._connHandle, 0, err?.message ?? '');
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(err ?? new Error('session destroyed'));
    this._emitClose({ errorCode: 0, reason: err?.message ?? '' });
  }

  override getMetrics(): SessionMetrics | null {
    if (this._h2Session) return this._lastMetrics;
    if (!this._eventLoop) return this._lastMetrics;
    try {
      const metrics = this._eventLoop.getSessionMetrics(this._connHandle);
      this._lastMetrics = {
        packetsIn: metrics.packetsIn,
        packetsOut: metrics.packetsOut,
        bytesIn: metrics.bytesIn,
        bytesOut: metrics.bytesOut,
        handshakeTimeMs: metrics.handshakeTimeMs,
        rttMs: metrics.rttMs,
        cwnd: metrics.cwnd,
        datagramQueueDepth: metrics.datagramQueueDepth,
      };
    } catch {
      // Keep the last known snapshot on transient native-read failures.
    }
    return this._lastMetrics;
  }

  override ping(cb?: PingCallback): number {
    if (this._h2Session) {
      this._h2Session.ping((err: Error | null, duration: number) => {
        if (this._lastMetrics) {
          this._lastMetrics.rttMs = duration;
        }
        cb?.(err, duration);
      });
      return this._lastMetrics?.rttMs ?? 0;
    }
    if (!this._eventLoop) return super.ping(cb);
    const snapshot = this.getMetrics()?.rttMs ?? 0;
    if (cb) {
      this._pendingPingCallbacks.push(cb);
    }
    try {
      if (!this._eventLoop.pingSession(this._connHandle) && cb) {
        const pending = this._pendingPingCallbacks.pop();
        process.nextTick(pending ?? cb, new Error('ping was not queued'), 0);
      }
    } catch {
      if (cb) {
        const pending = this._pendingPingCallbacks.pop();
        process.nextTick(pending ?? cb, new Error('ping failed'), 0);
      }
    }
    return snapshot;
  }

  override getRemoteSettings(): Record<string, number | boolean> | null {
    if (this._h2Session) {
      return this._h2Session.remoteSettings as Record<string, number>;
    }
    if (!this._eventLoop) return null;
    let settings: Array<{ id: number; value: number }>;
    try {
      settings = this._eventLoop.getRemoteSettings(this._connHandle);
    } catch {
      return null;
    }
    const out: Record<string, number> = {};
    for (const setting of settings) {
      out[`setting_${String(setting.id)}`] = setting.value;
    }
    return out;
  }

  override exportQlog(): string | null {
    if (this._h2Session) return null;
    if (this._eventLoop) {
      try {
        this._qlogPath = this._eventLoop.getQlogPath(this._connHandle);
      } catch {
        return this._qlogPath;
      }
    }
    return this._qlogPath;
  }

  /** Send an unreliable DATAGRAM frame (RFC 9221). Returns `false` if unsupported. */
  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (this._h2Session || !this._eventLoop) return false;
    return this._eventLoop.sendDatagram(this._connHandle, Buffer.from(data));
  }
}

/**
 * Adapter that wraps a `node:http2` server session to expose the same
 * interface as {@link Http3ServerSession}, enabling transparent H2/H3 fallback.
 */
export class Http2ServerSessionAdapter extends Http3ServerSession {
  constructor(h2Session: Http2Session) {
    super();
    const socket = h2Session.socket;
    this._alpnProtocol = 'h2';
    this._h2Session = h2Session;
    this._handshakeComplete = true;
    this._remoteAddress = socket.remoteAddress ?? '';
    this._remotePort = socket.remotePort ?? 0;
    this._serverName = 'servername' in socket && typeof socket.servername === 'string'
      ? socket.servername
      : '';

    h2Session.once('close', () => {
      this._emitClose();
    });
    h2Session.on('error', (err: Error) => {
      this.emit('error', err);
    });
    h2Session.on('goaway', (_errorCode: number, lastStreamId: number) => {
      this.emit('goaway', { lastStreamId });
    });
    this._startMetricsEmitter(1000, () => this.getMetrics());
  }

  override getMetrics(): SessionMetrics | null {
    const socket = this._h2Session?.socket;
    if (!socket) return this._lastMetrics;
    this._lastMetrics = {
      packetsIn: 0,
      packetsOut: 0,
      bytesIn: socket.bytesRead,
      bytesOut: socket.bytesWritten,
      handshakeTimeMs: 0,
      rttMs: 0,
      cwnd: 0,
      datagramQueueDepth: 0,
    };
    return this._lastMetrics;
  }
}

/**
 * Base class for client-side HTTP/3 sessions.
 *
 * Provides transport metric access, datagram support, and session lifecycle
 * management over the client worker event loop.
 */
export class Http3ClientSessionBase extends Http3Session {
  /** @internal */
  _eventLoop: ClientEventLoop | null = null;
  /** @internal */
  _qlogPath: string | null = null;

  /** Gracefully close the client session and release resources. */
  async close(code?: number, reason = 'client close'): Promise<void> {
    if (this._eventLoop) {
      await this._eventLoop.close(code ?? 0, reason);
      this._eventLoop = null;
    }
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(new Error('session closed'));
    this._emitClose({ errorCode: code ?? 0, reason });
  }

  /** Immediately destroy the session without waiting for draining. */
  async destroy(_err?: Error): Promise<void> {
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this._rejectPendingPings(_err ?? new Error('session destroyed'));
    this._emitClose({ errorCode: 0, reason: _err?.message ?? '' });
  }

  override getMetrics(): SessionMetrics | null {
    if (!this._eventLoop) return this._lastMetrics;
    try {
      const metrics = this._eventLoop.getSessionMetrics();
      this._lastMetrics = {
        packetsIn: metrics.packetsIn,
        packetsOut: metrics.packetsOut,
        bytesIn: metrics.bytesIn,
        bytesOut: metrics.bytesOut,
        handshakeTimeMs: metrics.handshakeTimeMs,
        rttMs: metrics.rttMs,
        cwnd: metrics.cwnd,
        datagramQueueDepth: metrics.datagramQueueDepth,
      };
    } catch {
      // Keep the previous snapshot on transient native-read failures.
    }
    return this._lastMetrics;
  }

  override ping(cb?: PingCallback): number {
    if (!this._eventLoop) return super.ping(cb);
    const snapshot = this.getMetrics()?.rttMs ?? 0;
    if (cb) {
      this._pendingPingCallbacks.push(cb);
    }
    try {
      if (!this._eventLoop.ping() && cb) {
        const pending = this._pendingPingCallbacks.pop();
        process.nextTick(pending ?? cb, new Error('ping was not queued'), 0);
      }
    } catch {
      if (cb) {
        const pending = this._pendingPingCallbacks.pop();
        process.nextTick(pending ?? cb, new Error('ping failed'), 0);
      }
    }
    return snapshot;
  }

  override getRemoteSettings(): Record<string, number | boolean> | null {
    if (!this._eventLoop) return null;
    let settings: Array<{ id: number; value: number }>;
    try {
      settings = this._eventLoop.getRemoteSettings();
    } catch {
      return null;
    }
    const out: Record<string, number> = {};
    for (const setting of settings) {
      out[`setting_${String(setting.id)}`] = setting.value;
    }
    return out;
  }

  override exportQlog(): string | null {
    if (this._eventLoop) {
      try {
        this._qlogPath = this._eventLoop.getQlogPath();
      } catch {
        return this._qlogPath;
      }
    }
    return this._qlogPath;
  }

  /** Send an unreliable DATAGRAM frame (RFC 9221). Returns `false` if unsupported. */
  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (!this._eventLoop) return false;
    return this._eventLoop.sendDatagram(Buffer.from(data));
  }
}
