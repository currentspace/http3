/**
 * Express adapter for HTTP/3.
 *
 * Creates req/res-like objects from HTTP/3 streams so that Express
 * middleware and route handlers work without modification.
 */

import { Readable, Writable } from 'node:stream';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from '../../dist/stream.js';

export interface ExpressLikeRequest extends Readable {
  method: string;
  url: string;
  headers: Record<string, string | string[]>;
  httpVersion: string;
}

export interface ExpressLikeResponse extends Writable {
  statusCode: number;
  writeHead(status: number, headers?: Record<string, string>): void;
  setHeader(name: string, value: string): void;
}

type ExpressHandler = (req: ExpressLikeRequest, res: ExpressLikeResponse) => void;

/**
 * Create a stream listener that adapts HTTP/3 streams to Express req/res.
 */
export function createExpressAdapter(
  app: ExpressHandler,
): (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void {
  return (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
    // Build request object
    const req = new Readable({ read() { /* data pushed from H3 stream */ } }) as ExpressLikeRequest;
    req.method = (headers[':method'] as string | undefined) ?? 'GET';
    req.url = (headers[':path'] as string | undefined) ?? '/';
    req.httpVersion = '3.0';
    req.headers = {};

    for (const [key, value] of Object.entries(headers)) {
      if (!key.startsWith(':')) {
        req.headers[key] = value;
      }
    }

    // Pipe H3 stream data to request readable
    if (!flags.endStream) {
      stream.on('data', (chunk: Buffer) => { req.push(chunk); });
      stream.on('end', () => { req.push(null); });
    } else {
      req.push(null);
    }

    // Build response object
    let headersSent = false;
    const responseHeaders: Record<string, string> = {};

    const res = new Writable({
      write(chunk: Buffer, _encoding: string, callback: (err?: Error | null) => void) {
        if (!headersSent) {
          sendHeaders();
        }
        if (!stream.write(chunk)) {
          stream.once('drain', () => { callback(); });
        } else {
          callback();
        }
      },
      final(callback: (err?: Error | null) => void) {
        if (!headersSent) {
          sendHeaders();
        }
        stream.end();
        callback();
      },
    }) as ExpressLikeResponse;

    res.statusCode = 200;

    res.writeHead = (status: number, hdrs?: Record<string, string>) => {
      res.statusCode = status;
      if (hdrs) {
        for (const [k, v] of Object.entries(hdrs)) {
          responseHeaders[k.toLowerCase()] = v;
        }
      }
      sendHeaders();
    };

    res.setHeader = (name: string, value: string) => {
      responseHeaders[name.toLowerCase()] = value;
    };

    function sendHeaders(): void {
      if (headersSent) return;
      headersSent = true;
      const h3Headers: IncomingHeaders = {
        ':status': String(res.statusCode),
        ...responseHeaders,
      };
      stream.respond(h3Headers);
    }

    app(req, res);
  };
}
