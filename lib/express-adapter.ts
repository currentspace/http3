import type { Readable, Writable } from 'node:stream';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { Http3IncomingMessage, Http3ServerResponse } from './node-server.js';
import type { IncomingHeaders, ServerHttp3Stream, StreamFlags } from './stream.js';

type NodeHandler = (req: IncomingMessage, res: ServerResponse) => void;

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
 * Copy the facade's transport/stream descriptors onto the instance. Express
 * replaces req/res prototypes with its own helpers (including in mounted apps).
 * Keeping these descriptors as own properties preserves the transport without
 * mutating Express globals or changing real HTTP/1 request/response objects.
 */
function facadeDescriptors(prototype: object): PropertyDescriptorMap {
  const descriptors: PropertyDescriptorMap = {};
  let current: object | null = prototype;
  while (current && current !== Object.prototype) {
    for (const key of Reflect.ownKeys(current)) {
      if (key === 'constructor' || Object.hasOwn(descriptors, key)) continue;
      const descriptor = Object.getOwnPropertyDescriptor(current, key);
      if (!descriptor) continue;
      Object.defineProperty(descriptors, key, {
        value: { ...descriptor, configurable: true }, enumerable: true,
      });
    }
    current = Object.getPrototypeOf(current) as object | null;
  }
  return descriptors;
}

const requestDescriptors = facadeDescriptors(Http3IncomingMessage.prototype);
const responseDescriptors = facadeDescriptors(Http3ServerResponse.prototype);

function preserveFacade(instance: object, descriptors: PropertyDescriptorMap): void {
  for (const key of Reflect.ownKeys(descriptors)) {
    if (!Object.hasOwn(instance, key)) Object.defineProperty(instance, key, descriptors[key]);
  }
}

function prepareRequest(req: Http3IncomingMessage): void {
  preserveFacade(req, requestDescriptors);
  // Express's type-is/body-parser detects bodies only through HTTP/1 framing
  // headers. H2/H3 streams can carry a body without Content-Length. This is a
  // request-local compatibility hint, never a header sent over the transport.
  if (!req.complete && !Object.hasOwn(req.headers, 'content-length') && !Object.hasOwn(req.headers, 'transfer-encoding')) {
    req.headers['transfer-encoding'] = 'chunked';
  }
}

/**
 * Adapt a real Express application for createNodeServerFactory(). H3/H2 facade
 * methods survive Express prototype decoration; native HTTP/1 objects pass
 * through unchanged. Does not add an Express dependency or promise Node's
 * entire HTTP API (socket writes, informational responses, etc.).
 */
export function createExpressHandler(app: NodeHandler): NodeHandler {
  return (req, res) => {
    if (req instanceof Http3IncomingMessage) prepareRequest(req);
    if (res instanceof Http3ServerResponse) preserveFacade(res, responseDescriptors);
    app(req, res);
  };
}

/** Wrap an Express-style handler as an HTTP/3 stream listener. */
export function createExpressAdapter(
  app: ExpressLikeHandler,
): (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => void {
  return (stream, headers, flags) => {
    const req = new Http3IncomingMessage(stream, headers, flags);
    const res = new Http3ServerResponse(req);
    prepareRequest(req);
    preserveFacade(res, responseDescriptors);
    try {
      app(req, res);
    } catch (err: unknown) {
      const error = err instanceof Error ? err : new Error(String(err));
      if (!res.headersSent) {
        res.statusCode = 500;
        res.end();
      } else {
        res.destroy(error);
      }
    }
  };
}
