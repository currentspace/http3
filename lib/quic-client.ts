import { EventEmitter } from 'node:events';
import { binding } from './event-loop.js';
import type { NativeEvent } from './event-loop.js';
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

export interface QuicConnectOptions {
  ca?: Buffer | string;
  rejectUnauthorized?: boolean;
  alpn?: string[];
  servername?: string;
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Buffer;
  allow0RTT?: boolean;
  enableDatagrams?: boolean;
  keylog?: boolean;
  qlogDir?: string;
  qlogLevel?: string;
}

interface NativeQuicClientBinding {
  connect(serverAddr: string, serverName: string): { address: string; family: string; port: number };
  streamSend(streamId: number, data: Buffer, fin: boolean): boolean;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  };
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode: number, reason: string): boolean;
  localAddress(): { address: string; family: string; port: number };
  shutdown(): void;
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

  get handshakeComplete(): boolean {
    return this._handshakeComplete;
  }

  async ready(): Promise<void> {
    return this._readyPromise;
  }

  openStream(): QuicStream {
    if (!this._handshakeComplete) {
      throw new Error('QUIC handshake not complete — await session.ready() first');
    }
    if (!this._eventLoop) {
      throw new Error('QUIC session not connected');
    }
    const streamId = this._nextBidiStreamId;
    this._nextBidiStreamId += 4;
    const stream = new QuicStream();
    stream._streamId = streamId;
    stream._clientLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
  }

  sendDatagram(data: Buffer | Uint8Array): boolean {
    if (!this._eventLoop) return false;
    return this._eventLoop.sendDatagram(Buffer.from(data));
  }

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

  ping(): boolean {
    return this._eventLoop?.ping() ?? false;
  }

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
      stream = new QuicStream();
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

  const nativeClient = new (binding as any).NativeQuicClient(
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
  ) as NativeQuicClientBinding;

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

export async function connectQuicAsync(authority: string, options?: QuicConnectOptions): Promise<QuicClientSession> {
  const session = connectQuic(authority, options);
  await session.ready();
  return session;
}
