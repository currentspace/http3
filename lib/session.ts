import { EventEmitter } from 'node:events';
import type { Http2Session } from 'node:http2';
import type { ServerEventLoopLike, ClientEventLoop } from './event-loop.js';

export interface SessionOptions {
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  disableActiveMigration?: boolean;
  enableDatagrams?: boolean;
  qpackMaxTableCapacity?: number;
  qpackBlockedStreams?: number;
}

export interface SessionMetrics {
  packetsIn: number;
  packetsOut: number;
  bytesIn: number;
  bytesOut: number;
  handshakeTimeMs: number;
  rttMs: number;
  cwnd: number;
}

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

  get alpnProtocol(): string { return this._alpnProtocol; }
  get remoteAddress(): string { return this._remoteAddress; }
  get remotePort(): number { return this._remotePort; }
  get handshakeComplete(): boolean { return this._handshakeComplete; }

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
      void Promise.resolve()
        .then(async () => pollMetrics())
        .then((snapshot) => {
          if (!snapshot) return;
          this._lastMetrics = snapshot;
          this.emit('metrics', snapshot);
        })
        .catch(() => {
          // Metrics polling is best-effort and must not crash the session.
        })
        .finally(() => {
          this._metricsPolling = false;
        });
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

  getMetrics(): SessionMetrics | null {
    return this._lastMetrics;
  }

  ping(): number {
    return this._lastMetrics?.rttMs ?? 0;
  }

  getRemoteSettings(): Record<string, number | boolean> | null {
    return null;
  }

  exportQlog(): string | null {
    return null;
  }

  // eslint-disable-next-line @typescript-eslint/require-await
  async close(_code?: number): Promise<void> {
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this.emit('close');
  }

  // eslint-disable-next-line @typescript-eslint/require-await
  async destroy(_err?: Error): Promise<void> {
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this.emit('close');
  }
}

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

  get serverName(): string { return this._serverName; }

  /**
   * Gracefully close the session. Sends CONNECTION_CLOSE frame and waits
   * for the QUIC draining period to complete before resolving.
   */
  async close(code?: number): Promise<void> {
    if (this._closing) return;
    this._closing = true;

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
      this._eventLoop.closeSession(this._connHandle, code ?? 0, '');
    }

    // Wait for the native SESSION_CLOSE event (emitted by server._onSessionClose
    // when quiche finishes draining). Times out after 5s to prevent hanging.
    await new Promise<void>((resolve) => {
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
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
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
      this.emit('close');
      return;
    }
    if (this._eventLoop) this._eventLoop.closeSession(this._connHandle, 0, err?.message ?? '');
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this.emit('close');
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
      };
    } catch {
      // Keep the last known snapshot on transient native-read failures.
    }
    return this._lastMetrics;
  }

  override ping(): number {
    if (this._h2Session) {
      this._h2Session.ping((_err: Error | null, duration: number) => {
        if (this._lastMetrics) {
          this._lastMetrics.rttMs = duration;
        }
      });
      return this._lastMetrics?.rttMs ?? 0;
    }
    if (!this._eventLoop) return 0;
    try {
      this._eventLoop.pingSession(this._connHandle);
    } catch {
      return this._lastMetrics?.rttMs ?? 0;
    }
    const metrics = this.getMetrics();
    return metrics?.rttMs ?? 0;
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

  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (this._h2Session || !this._eventLoop) return false;
    const eventLoop = this._eventLoop as {
      sendDatagram: (connHandle: number, payload: Buffer) => boolean;
    };
    return eventLoop.sendDatagram(this._connHandle, Buffer.from(data));
  }
}

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
      this.emit('close');
    });
    h2Session.on('error', (err: Error) => {
      this.emit('error', err);
    });
    h2Session.on('goaway', () => {
      this.emit('goaway');
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
    };
    return this._lastMetrics;
  }
}

export class Http3ClientSessionBase extends Http3Session {
  /** @internal */
  _eventLoop: ClientEventLoop | null = null;
  /** @internal */
  _qlogPath: string | null = null;

  async close(_code?: number): Promise<void> {
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this.emit('close');
  }

  async destroy(_err?: Error): Promise<void> {
    if (this._eventLoop) {
      await this._eventLoop.close();
      this._eventLoop = null;
    }
    this._stopMetricsEmitter();
    this._stopKeylogEmitter();
    this.emit('close');
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
      };
    } catch {
      // Keep the previous snapshot on transient native-read failures.
    }
    return this._lastMetrics;
  }

  override ping(): number {
    if (!this._eventLoop) return 0;
    try {
      this._eventLoop.ping();
    } catch {
      return this._lastMetrics?.rttMs ?? 0;
    }
    const metrics = this.getMetrics();
    return metrics?.rttMs ?? 0;
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

  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (!this._eventLoop) return false;
    const eventLoop = this._eventLoop as {
      sendDatagram: (payload: Buffer) => boolean;
    };
    return eventLoop.sendDatagram(Buffer.from(data));
  }
}
