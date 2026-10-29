import { EventEmitter } from 'node:events';
import { lookup } from 'node:dns';
import { isIP } from 'node:net';
import type { Socket } from 'node:net';
import { validateHeaderName, validateHeaderValue } from 'node:http';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { Readable, Writable } from 'node:stream';
import { createSecureServer } from './server.js';
import type { AddressInfo, ServerOptions } from './server.js';
import type { IncomingHeaders, ServerHttp3Stream, StreamFlags } from './stream.js';
import type { Http3ServerSession } from './session.js';

type HeaderValue = string | number | string[];
type Headers = Record<string, HeaderValue>;
type StoredHeaders = Partial<Headers>;

type Handler = (request: IncomingMessage, response: ServerResponse) => void;
const forbiddenHeaders = new Set(['connection', 'keep-alive', 'proxy-connection', 'transfer-encoding', 'upgrade']);

function headerError(): Error {
  return Object.assign(new Error('Cannot change headers after they are sent'), { code: 'ERR_HTTP_HEADERS_SENT' });
}

/** Stream-scoped socket metadata. A QUIC stream is not a TCP socket. */
class RequestSocket extends EventEmitter {
  readonly encrypted = true;
  constructor(readonly stream: ServerHttp3Stream) { super(); }
  get remoteAddress(): string | undefined { return this.stream.session?.remoteAddress; }
  get remotePort(): number | undefined { return this.stream.session?.remotePort; }
  get remoteFamily(): string { return isIP(this.remoteAddress ?? '') === 6 ? 'IPv6' : 'IPv4'; }
  get alpnProtocol(): string { return this.stream.session?.alpnProtocol ?? 'h3'; }
  get destroyed(): boolean { return this.stream.destroyed; }
  get readable(): boolean { return this.stream.readable; }
  get writable(): boolean { return this.stream.writable; }
  get readableEnded(): boolean { return this.stream.readableEnded; }
  get writableEnded(): boolean { return this.stream.writableEnded; }
  setTimeout(ms: number, callback?: () => void): this {
    if (callback) this.once('timeout', callback);
    this.stream.setTimeout(ms);
    return this;
  }
  destroy(): this { this.stream.close(this.alpnProtocol === 'h2' ? 8 : 0x10c); return this; }
}

/** Node-style request backed directly by an H3/H2 stream. */
export class Http3IncomingMessage extends Readable {
  readonly socket: RequestSocket;
  readonly headers: IncomingHeaders = {};
  readonly rawHeaders: string[] = [];
  trailers: IncomingHeaders = {};
  rawTrailers: string[] = [];
  readonly httpVersionMajor: number;
  readonly httpVersionMinor = 0;
  readonly httpVersion: string;
  method: string;
  url: string;
  complete: boolean;
  aborted = false;

  constructor(readonly stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) {
    super({ autoDestroy: false });
    this.socket = new RequestSocket(stream);
    this.httpVersionMajor = stream.session?.alpnProtocol === 'h2' ? 2 : 3;
    this.httpVersion = `${this.httpVersionMajor}.0`;
    this.method = String(headers[':method'] ?? 'GET');
    this.url = String(headers[':path'] ?? '/');
    this.complete = flags.endStream;
    for (const [name, value] of Object.entries(headers)) {
      if (name.startsWith(':')) continue;
      // Define an own property so names such as __proto__ cannot mutate the prototype.
      Object.defineProperty(this.headers, name.toLowerCase(), {
        value, writable: true, enumerable: true, configurable: true,
      });
      for (const item of Array.isArray(value) ? value : [value]) this.rawHeaders.push(name, item);
    }
    if (Object.hasOwn(headers, ':authority')) this.headers.host = headers[':authority'];
    stream.pause();
    stream.on('data', (chunk: Buffer) => { if (!this.push(chunk)) stream.pause(); });
    stream.once('end', () => { if (!this.aborted) this.complete = true; this.push(null); });
    stream.once('aborted', () => {
      this.aborted = true;
      this.emit('aborted');
      this.destroy();
    });
    stream.on('trailers', (trailers: IncomingHeaders) => {
      this.trailers = trailers;
      this.rawTrailers = Object.entries(trailers).flatMap(([name, value]) =>
        (Array.isArray(value) ? value : [value]).flatMap(item => [name, item]));
    });
    stream.on('timeout', () => { this.emit('timeout'); this.socket.emit('timeout'); });
    stream.once('close', () => {
      if (!this.complete && !this.aborted) { this.aborted = true; this.emit('aborted'); }
      this.socket.emit('close');
      this.destroy();
    });
    // Error events still reach body parsers, while requests with no parser remain safe.
    this.on('error', () => {});
    stream.on('error', error => { this.aborted ||= !this.complete; this.destroy(error); });
    if (flags.endStream) this.push(null);
  }
  get connection(): RequestSocket { return this.socket; }
  override _read(): void { this.stream.resume(); }
  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    if (!this.complete && !this.stream.destroyed) this.socket.destroy();
    callback(error);
  }
  setTimeout(ms: number, callback?: () => void): this {
    if (callback) this.once('timeout', callback);
    this.stream.setTimeout(ms);
    return this;
  }
}

/** Node-style response with streaming writes, headers and trailers. */
export class Http3ServerResponse extends Writable {
  statusCode = 200;
  statusMessage = '';
  sendDate = true;
  headersSent = false;
  private readonly headers: StoredHeaders = Object.create(null) as StoredHeaders;
  private trailers: IncomingHeaders | null = null;

  constructor(readonly req: Http3IncomingMessage) {
    super();
    req.stream.once('error', error => this.destroy(error));
    req.stream.once('close', () => {
      if (!this.writableFinished) this.destroy(new Error('Request stream closed before response completed'));
    });
    this.on('error', () => {});
    this.once('finish', () => req.resume());
  }
  get socket(): RequestSocket { return this.req.socket; }
  get connection(): RequestSocket { return this.socket; }
  get finished(): boolean { return this.writableEnded; }
  setHeader(name: string, value: HeaderValue): this {
    if (this.headersSent) throw headerError();
    validateHeaderName(name);
    for (const item of Array.isArray(value) ? value : [value]) validateHeaderValue(name, typeof item === 'number' ? String(item) : item);
    this.headers[name.toLowerCase()] = Array.isArray(value) ? [...value] : value;
    return this;
  }
  getHeader(name: string): HeaderValue | undefined { return this.headers[name.toLowerCase()]; }
  getHeaders(): StoredHeaders { return { ...this.headers }; }
  getHeaderNames(): string[] { return Object.keys(this.headers); }
  hasHeader(name: string): boolean { return Object.hasOwn(this.headers, name.toLowerCase()); }
  removeHeader(name: string): void {
    if (this.headersSent) throw headerError();
    if (name.toLowerCase() === 'date') this.sendDate = false;
    Reflect.deleteProperty(this.headers, name.toLowerCase());
  }
  appendHeader(name: string, value: HeaderValue): this {
    const previous = this.getHeader(name);
    return this.setHeader(name, [...(previous === undefined ? [] : Array.isArray(previous) ? previous : [String(previous)]),
      ...(Array.isArray(value) ? value : [String(value)])]);
  }
  writeHead(statusCode: number, statusMessageOrHeaders?: string | Headers, headers?: Headers): this {
    if (this.headersSent) throw headerError();
    if (!Number.isInteger(statusCode) || statusCode < 200 || statusCode > 599) throw new RangeError('Invalid final status code');
    this.statusCode = statusCode;
    const values = typeof statusMessageOrHeaders === 'string' ? headers : statusMessageOrHeaders;
    for (const [name, value] of Object.entries(values ?? {})) this.setHeader(name, value);
    this.commitHeaders();
    return this;
  }
  /** Compatibility hooks used by Node middleware such as compression/on-headers. */
  get _header(): string | null { return this.headersSent ? 'sent' : null; }
  _implicitHeader(): void { this.writeHead(this.statusCode); }
  flushHeaders(): void {
    // Dispatch through writeHead so middleware gets to prepare headers exactly once.
    if (!this.headersSent) this._implicitHeader();
  }
  private commitHeaders(): void {
    if (!Number.isInteger(this.statusCode) || this.statusCode < 200 || this.statusCode > 599) throw new RangeError('Invalid final status code');
    const headers: IncomingHeaders = { ':status': String(this.statusCode) };
    // Node middleware may produce HTTP/1 hop-by-hop fields. Never transmit these on H2/H3.
    const forbidden = new Set(forbiddenHeaders);
    for (const name of String(this.headers.connection ?? '').split(',')) forbidden.add(name.trim().toLowerCase());
    for (const [name, value] of Object.entries(this.headers)) {
      if (value !== undefined && !forbidden.has(name)) headers[name] = Array.isArray(value) ? [...value] : String(value);
    }
    if (this.statusCode === 204) delete headers['content-length'];
    if (this.sendDate && !headers.date) headers.date = new Date().toUTCString();
    this.req.stream.respond(headers, {
      endStream: this.req.httpVersionMajor === 2 && this.req.method === 'HEAD',
    });
    this.headersSent = true;
  }
  addTrailers(headers: Headers): void {
    this.trailers ??= {};
    for (const [name, value] of Object.entries(headers)) {
      validateHeaderName(name);
      for (const item of Array.isArray(value) ? value : [value]) validateHeaderValue(name, typeof item === 'number' ? String(item) : item);
      if (forbiddenHeaders.has(name.toLowerCase()) || ['content-length', 'host'].includes(name.toLowerCase())) throw new TypeError('Invalid trailer');
      this.trailers[name.toLowerCase()] = Array.isArray(value) ? [...value] : String(value);
    }
  }
  setTimeout(ms: number, callback?: () => void): this {
    this.req.stream.setTimeout(ms, callback);
    return this;
  }
  override _write(chunk: Buffer, _encoding: BufferEncoding, callback: (error?: Error | null) => void): void {
    try {
      this.flushHeaders();
      if (this.req.method === 'HEAD' || this.statusCode === 204 || this.statusCode === 304) callback();
      else this.req.stream.write(chunk, callback);
    } catch (error) { callback(error as Error); }
  }
  override _final(callback: (error?: Error | null) => void): void {
    try {
      this.flushHeaders();
      if (this.trailers) this.req.stream.sendTrailers(this.trailers);
      this.req.stream.end(callback);
    } catch (error) { callback(error as Error); }
  }
  override _destroy(error: Error | null, callback: (error?: Error | null) => void): void {
    if (error && !this.req.stream.destroyed) this.socket.destroy();
    callback(error);
  }
}

export interface NodeServerOptions extends ServerOptions {
  /** Maximum time to drain active responses during close. Default: 30 seconds. */
  shutdownTimeoutMs?: number;
  /** Maximum unread H3 request bytes in the transport stream. Default: 1 MiB. */
  maxBufferedRequestBytes?: number;
}

/**
 * Node-style server facade for frameworks with a custom server factory.
 * HTTP/1 upgrades are forwarded on the TLS fallback listener. QUIC raw socket
 * writes and informational responses are not supported.
 */
export class Http3NodeServer extends EventEmitter {
  private readonly transport;
  private readonly active = new Set<object>();
  private readonly sessions = new Set<Http3ServerSession>();
  private readonly http1Sockets = new Set<Socket>();
  private closing = false;
  private starting = false;
  private closeStarted = false;
  private draining = false;
  private closed = false;
  private readonly closeCallbacks: ((error?: Error) => void)[] = [];
  private closeTimer: NodeJS.Timeout | undefined;
  listening = false;
  constructor(private readonly options: NodeServerOptions, handler: Handler) {
    super();
    if (options.shutdownTimeoutMs !== undefined &&
      (!Number.isFinite(options.shutdownTimeoutMs) || options.shutdownTimeoutMs < 0 || options.shutdownTimeoutMs > 2 ** 31 - 1)) {
      throw new RangeError('shutdownTimeoutMs must be between 0 and 2147483647');
    }
    if (options.maxBufferedRequestBytes !== undefined &&
      (!Number.isSafeInteger(options.maxBufferedRequestBytes) || options.maxBufferedRequestBytes < 1)) {
      throw new RangeError('maxBufferedRequestBytes must be a positive safe integer');
    }
    this.transport = createSecureServer(options, (stream, headers, flags) => {
      if (this.closing) { stream.respond({ ':status': '503' }); stream.end(); return; }
      stream._maxBufferedReadBytes = options.maxBufferedRequestBytes ?? 1024 * 1024;
      const request = new Http3IncomingMessage(stream, headers, flags);
      const response = new Http3ServerResponse(request);
      response.setHeader('alt-svc', `h3=":${this.address()?.port ?? 443}"; ma=60`);
      this.trackResponse(response);
      try {
        // The facade intentionally implements the stream-based subset used by frameworks.
        handler(request as unknown as IncomingMessage, response as unknown as ServerResponse);
      } catch (error) {
        if (response.headersSent) response.destroy(error as Error);
        else { response.statusCode = 500; response.end(); }
      }
    });
    this.transport.on('request', (request, response) => {
      this.trackHttp1Socket(request.socket);
      if (this.closing) { response.writeHead(503); response.end(); return; }
      response.setHeader('alt-svc', `h3=":${this.address()?.port ?? 443}"; ma=60`);
      this.trackResponse(response);
      handler(request, response);
    });
    this.transport.on('upgrade', (request: IncomingMessage, socket: Socket, head: Buffer) => {
      this.trackHttp1Socket(socket);
      if (this.closing || !this.emit('upgrade', request, socket, head)) socket.destroy();
    });
    this.transport.on('session', session => {
      this.sessions.add(session);
      session.once('close', () => this.sessions.delete(session));
    });
    this.transport.on('error', error => this.startFailed(error));
    this.transport.on('listening', () => {
      this.starting = false;
      this.listening = true;
      this.emit('listening');
      if (this.closing) this.drain();
    });
  }
  private trackHttp1Socket(socket: Socket): void {
    if (this.http1Sockets.has(socket)) return;
    this.http1Sockets.add(socket);
    socket.once('close', () => this.http1Sockets.delete(socket));
  }
  private trackResponse(response: Writable): void {
    this.active.add(response);
    const finished = (): void => {
      response.off('finish', finished);
      response.off('close', finished);
      this.active.delete(response);
      if (this.closing) this.drain();
    };
    response.once('finish', finished);
    response.once('close', finished);
  }
  private startFailed(error: Error): void {
    this.starting = false;
    if (this.closing) this.closeTransport();
    this.emit('error', error);
  }
  address(): AddressInfo | null { return this.transport.address(); }
  listen(options: { port?: number; host?: string; path?: string }, callback?: () => void): this {
    if (options.path) throw new TypeError('HTTP/3 requires an IP port, not a Unix socket');
    if (this.starting || this.listening || this.closing) throw new Error('Server is already started or closing');
    this.starting = true;
    if (callback) this.once('listening', callback);
    const start = (host: string): void => {
      try {
        const port = options.port ?? 0;
        if (!Number.isInteger(port) || port < 0 || port > 65535) throw new RangeError('port must be an integer between 0 and 65535');
        this.transport.listen(port, host);
      }
      catch (error) { process.nextTick(() => this.startFailed(error as Error)); }
    };
    const host = options.host ?? '127.0.0.1';
    if (isIP(host)) start(host);
    else lookup(host, (error, address) => {
      if (error) this.startFailed(error);
      else start(address);
    });
    return this;
  }
  close(callback?: (error?: Error) => void): this {
    if (this.closed) { if (callback) process.nextTick(callback); return this; }
    if (callback) this.closeCallbacks.push(callback);
    if (this.closing) return this;
    this.closing = true;
    this.listening = false;
    if (this.starting) return this;
    this.drain();
    return this;
  }
  private drain(): void {
    this.closeTimer ??= setTimeout(() => {
      for (const socket of this.http1Sockets) socket.destroy();
      this.closeTransport();
    }, this.options.shutdownTimeoutMs ?? 30_000);
    if (this.active.size > 0 || this.draining || this.closeStarted) return;
    this.draining = true;
    for (const socket of this.http1Sockets) socket.end();
    // Use the transport's session shutdown (GOAWAY) before stopping its reactor.
    // Writable.finish alone only means writes were handed to the transport.
    void Promise.allSettled([...this.sessions].map(async session => session.close()))
      .then(() => this.closeTransport());
  }
  private closeTransport(): void {
    if (this.closeStarted) return;
    this.closeStarted = true;
    void this.transport.close(error => {
      clearTimeout(this.closeTimer);
      this.closed = true;
      this.listening = false;
      this.emit('close');
      for (const callback of this.closeCallbacks.splice(0)) callback(error);
      if (error && this.listenerCount('error') > 0) this.emit('error', error);
    });
  }
}

/** Build an opt-in serverFactory for Fastify or another Node-style framework. */
export function createNodeServerFactory(options: NodeServerOptions): (handler: Handler) => Http3NodeServer {
  return handler => new Http3NodeServer(options, handler);
}
