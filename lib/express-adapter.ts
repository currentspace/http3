import { Readable, Writable } from 'node:stream';
import type { IncomingHeaders, ServerHttp3Stream, StreamFlags } from './stream.js';

/** Minimal Express-style request interface. */
export interface ExpressLikeRequest extends Readable {
  method: string;
  url: string;
  headers: Record<string, string | string[]>;
  httpVersion: string;
}

/** Minimal Express-style response interface. */
export interface ExpressLikeResponse extends Writable {
  statusCode: number;
  writeHead(status: number, headers?: Record<string, string>): void;
  setHeader(name: string, value: string): void;
}

/** An Express-style `(req, res)` handler function. */
export type ExpressLikeHandler = (req: ExpressLikeRequest, res: ExpressLikeResponse) => void;

/**
 * Wrap an Express-style handler as an HTTP/3 stream listener.
 * Translates each H3 stream into Express-like `req`/`res` objects.
 */
export function createExpressAdapter(
  app: ExpressLikeHandler,
): (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void {
  return (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
    const req = new Readable({ read() {} }) as ExpressLikeRequest;
    req.method = (headers[':method'] as string | undefined) ?? 'GET';
    req.url = (headers[':path'] as string | undefined) ?? '/';
    req.httpVersion = '3.0';
    req.headers = {};

    for (const [key, value] of Object.entries(headers)) {
      if (!key.startsWith(':')) {
        req.headers[key] = value;
      }
    }

    if (!flags.endStream) {
      stream.on('data', (chunk: Buffer) => { req.push(chunk); });
      stream.on('end', () => { req.push(null); });
    } else {
      req.push(null);
    }

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
        for (const [name, value] of Object.entries(hdrs)) {
          responseHeaders[name.toLowerCase()] = value;
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
      stream.respond({
        ':status': String(res.statusCode),
        ...responseHeaders,
      });
    }

    try {
      app(req, res);
    } catch (err: unknown) {
      const error = err instanceof Error ? err : new Error(String(err));
      const responseStarted = headersSent;
      // Express-style handlers can synchronously send headers before throwing.
      // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition
      if (!responseStarted) {
        headersSent = true;
        stream.respond({ ':status': '500' });
        stream.end();
        return;
      }
      stream.destroy(error);
    }
  };
}
