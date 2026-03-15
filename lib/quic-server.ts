import { EventEmitter } from 'node:events';
import { binding } from './event-loop.js';
import type { NativeEvent } from './event-loop.js';
import { QuicStream } from './quic-stream.js';
import type { QuicServerEventLoopLike } from './quic-stream.js';

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

export interface QuicServerOptions {
  key: Buffer | string;
  cert: Buffer | string;
  ca?: Buffer | string;
  alpn?: string[];
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  disableActiveMigration?: boolean;
  enableDatagrams?: boolean;
  maxConnections?: number;
  disableRetry?: boolean;
  qlogDir?: string;
  qlogLevel?: string;
  keylog?: boolean;
}

interface NativeQuicServerBinding {
  listen(port: number, host: string): { address: string; family: string; port: number };
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): boolean;
  streamClose(connHandle: number, streamId: number, errorCode: number): boolean;
  closeSession(connHandle: number, errorCode: number, reason: string): boolean;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  };
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  localAddress(): { address: string; family: string; port: number };
  shutdown(): void;
}

class QuicWorkerEventLoop implements QuicServerEventLoopLike {
  private readonly worker: NativeQuicServerBinding;
  private closed = false;

  constructor(worker: NativeQuicServerBinding) {
    this.worker = worker;
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
    this.worker.shutdown();
    await Promise.resolve();
  }
}

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

  openStream(): QuicStream {
    const streamId = this._nextBidiStreamId;
    this._nextBidiStreamId += 4;
    const stream = new QuicStream();
    stream._connHandle = this.connHandle;
    stream._streamId = streamId;
    stream._serverLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
  }

  close(errorCode?: number, reason?: string): void {
    this._eventLoop.closeSession(this.connHandle, errorCode ?? 0, reason ?? '');
  }

  sendDatagram(data: Buffer | Uint8Array): boolean {
    return this._eventLoop.sendDatagram(this.connHandle, Buffer.from(data));
  }

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

  ping(): boolean {
    return this._eventLoop.pingSession(this.connHandle);
  }

  /** @internal */
  _getOrCreateStream(streamId: number): QuicStream {
    let stream = this._streams.get(streamId);
    if (!stream) {
      stream = new QuicStream();
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

export class QuicServer extends EventEmitter {
  private _eventLoop: QuicWorkerEventLoop | null = null;
  private readonly _sessions = new Map<number, QuicServerSession>();
  private readonly _options: QuicServerOptions;
  private _address: { address: string; family: string; port: number } | null = null;

  constructor(options: QuicServerOptions) {
    super();
    this._options = options;
  }

  async listen(port: number, host?: string): Promise<{ address: string; family: string; port: number }> {
    const opts = this._options;
    const native = new (binding as any).NativeQuicServer(
      {
        key: typeof opts.key === 'string' ? Buffer.from(opts.key) : opts.key,
        cert: typeof opts.cert === 'string' ? Buffer.from(opts.cert) : opts.cert,
        ca: opts.ca ? (typeof opts.ca === 'string' ? Buffer.from(opts.ca) : opts.ca) : undefined,
        alpn: opts.alpn,
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
        this._dispatchEvents(events);
      },
    ) as NativeQuicServerBinding;

    const eventLoop = new QuicWorkerEventLoop(native);
    this._eventLoop = eventLoop;

    const addr = native.listen(port, host ?? '127.0.0.1');
    this._address = addr;
    this.emit('listening');
    return addr;
  }

  address(): { address: string; family: string; port: number } | null {
    return this._address;
  }

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
      this._eventLoop as QuicWorkerEventLoop,
    );
    this._sessions.set(event.connHandle, session);
  }

  private _onHandshakeComplete(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session) {
      this.emit('session', session);
    }
  }

  private _onNewStream(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._getOrCreateStream(event.streamId);
    session.emit('stream', stream);
  }

  private _onData(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session || !event.data) return;
    const stream = session._getOrCreateStream(event.streamId);
    stream.push(Buffer.from(event.data));
  }

  private _onFinished(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (!session) return;
    const stream = session._streams.get(event.streamId);
    if (stream) {
      stream.push(null);
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
    if (!session) return;
    if (event.streamId >= 0) {
      const stream = session._streams.get(event.streamId);
      if (stream) {
        stream.destroy(new Error(event.meta?.errorReason ?? 'stream error'));
      }
    } else {
      session.emit('error', new Error(event.meta?.errorReason ?? 'session error'));
    }
  }

  private _onDatagram(event: NativeEvent): void {
    const session = this._sessions.get(event.connHandle);
    if (session && event.data) {
      session.emit('datagram', Buffer.from(event.data));
    }
  }
}

export function createQuicServer(options: QuicServerOptions): QuicServer {
  return new QuicServer(options);
}
