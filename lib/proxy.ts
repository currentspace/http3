import { request as httpRequest } from 'node:http';
import { request as httpsRequest } from 'node:https';
import type { IncomingMessage, ServerResponse, IncomingHttpHeaders, OutgoingHttpHeaders } from 'node:http';
import type { SecureContextOptions } from 'node:tls';

const HOP = new Set(['connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding', 'upgrade', 'http2-settings']);
function forwardedHeaders(input: IncomingHttpHeaders): OutgoingHttpHeaders {
  const connection = Array.isArray(input.connection) ? input.connection.join(',') : input.connection ?? '';
  const blocked = new Set([...HOP, ...connection.toLowerCase().split(',').map(v => v.trim())]);
  const output: OutgoingHttpHeaders = {};
  for (const [name, value] of Object.entries(input)) {
    if (!name.startsWith(':') && !blocked.has(name.toLowerCase()) && value !== undefined) output[name] = value;
  }
  return output;
}
export interface ProxyOptions {
  /** Rewrite only the request path; the configured upstream origin is fixed. */
  rewrite?: (path: string, request: IncomingMessage) => string;
  /** Upstream socket inactivity timeout. Default: 30 seconds; 0 disables it for quiet SSE. */
  timeoutMs?: number;
  /** CA for an HTTPS upstream. Certificate verification is always enabled. */
  ca?: SecureContextOptions['ca'];
}

/** Streaming HTTP(S) upstream proxy for Node-style handlers. No buffering or WebSocket upgrades. */
export function proxy(upstream: string | URL, options: ProxyOptions = {}): (req: IncomingMessage, res: ServerResponse) => void {
  const target = new URL(upstream);
  if (!['http:', 'https:'].includes(target.protocol) || target.username || target.password || target.search || target.hash) {
    throw new TypeError('proxy upstream must be an http(s) URL without credentials, query or fragment');
  }
  const timeoutMs = options.timeoutMs ?? 30_000;
  if (!Number.isFinite(timeoutMs) || timeoutMs < 0 || timeoutMs > 2147483647) throw new RangeError('proxy timeoutMs must be between 0 and 2147483647');
  return (req, res) => {
    let path: string;
    try {
      path = options.rewrite ? options.rewrite(req.url ?? '/', req) : target.pathname.replace(/\/$/u, '') + (req.url ?? '/');
      if (typeof path !== 'string' || !path.startsWith('/') || path.startsWith('//') || /[\r\n]/u.test(path)) throw new TypeError('proxy rewrite must return an origin-relative path');
    } catch {
      res.statusCode = 502; res.end('Invalid upstream path'); return;
    }
    const headers = forwardedHeaders(req.headers);
    // Replace caller-supplied forwarding metadata; this listener is the trusted hop.
    for (const name of Object.keys(headers)) if (name === 'forwarded' || name.startsWith('x-forwarded-')) Reflect.deleteProperty(headers, name);
    headers['x-forwarded-for'] = req.socket.remoteAddress ?? '';
    headers['x-forwarded-host'] = req.headers.host ?? '';
    headers['x-forwarded-proto'] = 'encrypted' in req.socket && req.socket.encrypted ? 'https' : 'http';
    headers.host = target.host;
    const send = target.protocol === 'https:' ? httpsRequest : httpRequest;
    const request = send(target, { method: req.method, path, headers, ca: options.ca, rejectUnauthorized: true });
    let response: IncomingMessage | undefined;
    const abort = (): void => { request.destroy(); response?.destroy(); };
    const downstreamClosed = (): void => { if (!res.writableFinished) abort(); };
    const cleanup = (): void => {
      req.off('aborted', abort); req.off('error', abort); res.off('close', downstreamClosed); res.off('error', abort);
    };
    req.once('aborted', abort);
    req.once('error', abort);
    res.once('error', abort);
    res.once('close', downstreamClosed);
    res.once('finish', cleanup);
    res.once('close', cleanup);
    request.setTimeout(timeoutMs, () => request.destroy(new Error('Upstream request timed out')));
    request.once('error', () => {
      req.unpipe(request);
      if (res.destroyed) return;
      if (res.headersSent) res.destroy();
      else { res.statusCode = 502; res.end('Upstream request failed'); req.resume(); }
    });
    request.once('response', incoming => {
      response = incoming;
      res.statusCode = incoming.statusCode ?? 502;
      for (const [name, value] of Object.entries(forwardedHeaders(incoming.headers))) {
        if (value !== undefined) res.setHeader(name, value);
      }
      // Send headers immediately so SSE clients can observe an idle stream.
      res.flushHeaders();
      incoming.once('error', () => res.destroy());
      incoming.pipe(res);
    });
    req.pipe(request);
  };
}
