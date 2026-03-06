import { Readable, Writable } from 'node:stream';
import type { IncomingHeaders, ServerHttp3Stream, StreamFlags } from './stream.js';

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

export type ExpressLikeHandler = (req: ExpressLikeRequest, res: ExpressLikeResponse) => void;

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

    app(req, res);
  };
}
