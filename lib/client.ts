import { Http3ClientSessionBase } from './session.js';
import { ClientHttp3Stream } from './stream.js';
import type { IncomingHeaders } from './stream.js';
import { ClientEventLoop, binding } from './event-loop.js';
import type { NativeEvent } from './event-loop.js';
import { Http3Error, ERR_HTTP3_INVALID_STATE, ERR_HTTP3_STREAM_ERROR } from './errors.js';
import { toSessionError, toStreamError } from './error-map.js';
import { prepareKeylogFile, subscribeKeylog } from './keylog.js';

// Event type constants (must match Rust)
const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_RESET = 6;
const EVENT_SESSION_CLOSE = 7;
const EVENT_DRAIN = 8;
const EVENT_GOAWAY = 9;
const EVENT_ERROR = 10;
const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_SESSION_TICKET = 12;
const EVENT_DATAGRAM = 14;

function normalizeCaOption(ca?: string | Buffer | Array<string | Buffer>): Buffer | undefined {
  if (!ca) return undefined;
  const first = Array.isArray(ca) ? ca[0] : ca;
  return typeof first === 'string' ? Buffer.from(first) : first;
}

export interface ConnectOptions {
  ca?: string | Buffer | Array<string | Buffer>;
  rejectUnauthorized?: boolean;
  servername?: string;
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Buffer;
  allow0RTT?: boolean;
  keylog?: boolean | string;
  enableDatagrams?: boolean;
  allowUnsafe0RTTMethods?: boolean;
  onEarlyData?: (headers: IncomingHeaders) => boolean;
  metricsIntervalMs?: number;
  qlogDir?: string;
  qlogLevel?: string;
}

export interface RequestOptions {
  endStream?: boolean;
}

export class Http3ClientSession extends Http3ClientSessionBase {
  private readonly _authority: string;
  private readonly _streams = new Map<number, ClientHttp3Stream>();
  private _allow0RTT = false;
  private _allowUnsafe0RTTMethods = false;
  private _onEarlyData: ((headers: IncomingHeaders) => boolean) | undefined;
  private _readySettled = false;
  private readonly _readyPromise: Promise<void>;
  private _resolveReady: (() => void) | null = null;
  private _rejectReady: ((err: Error) => void) | null = null;

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
    // ready() is optional for event-driven callers; swallow unobserved rejections.
    void this._readyPromise.catch(() => undefined);
  }

  get authority(): string {
    return this._authority;
  }

  async ready(): Promise<void> {
    return this._readyPromise;
  }

  request(headers: IncomingHeaders, options?: RequestOptions): ClientHttp3Stream {
    if (!this._eventLoop) {
      throw new Http3Error('not connected', ERR_HTTP3_INVALID_STATE);
    }
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

    const h = Object.entries(headers).map(([name, value]) => ({
      name,
      value: Array.isArray(value) ? value[0] : value,
    }));

    const streamId = this._eventLoop.sendRequest(h, options?.endStream ?? false);
    const stream = new ClientHttp3Stream();
    stream._streamId = streamId;
    stream._eventLoop = this._eventLoop;
    this._streams.set(streamId, stream);
    return stream;
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
        case EVENT_SESSION_CLOSE:
          if (!this._handshakeComplete) {
            this._markReadyError(new Http3Error('session closed before handshake completed', ERR_HTTP3_INVALID_STATE));
          }
          this._cleanupStreams();
          this._stopMetricsEmitter();
          this._stopKeylogEmitter();
          this.emit('close');
          break;
        case EVENT_DRAIN:
          this._onDrain(event);
          break;
        case EVENT_GOAWAY:
          this.emit('goaway');
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
        default:
          break;
      }
    }
  }

  private _cleanupStreams(): void {
    for (const stream of this._streams.values()) {
      stream.destroy();
    }
    this._streams.clear();
  }

  private _onHeaders(event: NativeEvent): void {
    if (!event.headers) return;
    const stream = this._streams.get(event.streamId);
    if (!stream) return;

    const headers: IncomingHeaders = {};
    for (const h of event.headers) {
      headers[h.name] = h.value;
    }

    const flags = { endStream: event.fin ?? false };
    stream.emit('response', headers, flags);
  }

  private _onData(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream && event.data) {
      stream._onActivity();
      stream.push(Buffer.from(event.data));
    }
  }

  private _onFinished(event: NativeEvent): void {
    const stream = this._streams.get(event.streamId);
    if (stream) {
      stream.push(null);
      this._streams.delete(event.streamId);
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
    }
  }

  private _onError(event: NativeEvent): void {
    if (event.streamId >= 0) {
      const stream = this._streams.get(event.streamId);
      if (stream) {
        stream.destroy(toStreamError(event));
      }
    } else {
      this.emit('error', toSessionError(event));
      if (!this._handshakeComplete) {
        this._markReadyError(toSessionError(event));
      }
    }
  }

  private _onSessionTicket(event: NativeEvent): void {
    if (!event.data) return;
    this.emit('sessionTicket', Buffer.from(event.data));
  }

  private _onDatagram(event: NativeEvent): void {
    if (!event.data) return;
    this.emit('datagram', Buffer.from(event.data));
  }

  /** @internal */
  _markReady(): void {
    if (this._readySettled) return;
    this._readySettled = true;
    this._resolveReady?.();
    this._resolveReady = null;
    this._rejectReady = null;
  }

  /** @internal */
  _markReadyError(err: Error): void {
    if (this._readySettled) return;
    this._readySettled = true;
    this._rejectReady?.(err);
    this._resolveReady = null;
    this._rejectReady = null;
  }
}

export function connect(authority: string, options?: ConnectOptions): Http3ClientSession {
  // Parse authority: "https://host:port" or "host:port"
  let host: string;
  let port: number;
  let servername: string;

  try {
    const url = new URL(authority.includes('://') ? authority : `https://${authority}`);
    host = url.hostname;
    port = parseInt(url.port || '443', 10);
    servername = options?.servername ?? host;
  } catch {
    // Fallback: treat as host:port
    const parts = authority.split(':');
    host = parts[0];
    port = parseInt(parts[1] ?? '443', 10);
    servername = options?.servername ?? host;
  }

  const session = new Http3ClientSession(authority, {
    allow0RTT: options?.allow0RTT,
    allowUnsafe0RTTMethods: options?.allowUnsafe0RTTMethods,
    onEarlyData: options?.onEarlyData,
  });
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

  const nativeClient = new binding.NativeWorkerClient({
    ca: normalizeCaOption(options?.ca),
    rejectUnauthorized: options?.rejectUnauthorized,
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
    session._dispatchEvents(events);
  });
  const eventLoop = new ClientEventLoop(nativeClient);
  session._eventLoop = eventLoop;
  session._startMetricsEmitter(options?.metricsIntervalMs ?? 1000, () => session.getMetrics());

  void (async (): Promise<void> => {
    try {
      await eventLoop.connect(`${host}:${port}`, servername);
    } catch (err: unknown) {
      const error = err instanceof Error
        ? new Http3Error(err.message, ERR_HTTP3_INVALID_STATE)
        : new Http3Error(String(err), ERR_HTTP3_INVALID_STATE);
      session._markReadyError(error);
      session.emit('error', error);
    }
  })();

  return session;
}

export async function connectAsync(authority: string, options?: ConnectOptions): Promise<Http3ClientSession> {
  const session = connect(authority, options);
  await session.ready();
  return session;
}
