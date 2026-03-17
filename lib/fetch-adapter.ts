import type { IncomingMessage, ServerResponse } from 'node:http';
import type { ServerHttp3Stream, IncomingHeaders, StreamFlags } from './stream.js';
import type { ServerOptions, StreamListener } from './server.js';
import { createSecureServer, Http3SecureServer } from './server.js';
import { createSseReadableStream, sseHeaders } from './sse.js';
import type { SseEvent } from './sse.js';

/** A Web Fetch API-compatible request handler. */
export type FetchHandler = (req: Request) => Response | Promise<Response>;

/** An object with a `fetch` method (e.g. a Hono app). */
export interface FetchApp {
  fetch: FetchHandler;
}

function isSseContentType(value: string | null): boolean {
  return typeof value === 'string' && value.toLowerCase().includes('text/event-stream');
}

type WritableLike = {
  once(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  off?(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  removeListener?(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  destroyed?: boolean;
  writableEnded?: boolean;
};

function removeWritableListener(
  writable: WritableLike,
  event: 'drain' | 'close' | 'error',
  listener: (...args: unknown[]) => void,
): void {
  if (typeof writable.off === 'function') {
    writable.off(event, listener);
    return;
  }
  if (typeof writable.removeListener === 'function') {
    writable.removeListener(event, listener);
  }
}

async function waitForDrainOrAbort(writable: WritableLike, signal: AbortSignal): Promise<void> {
  if (signal.aborted || writable.destroyed || writable.writableEnded) {
    throw new Error('writable is closed');
  }

  await new Promise<void>((resolve, reject) => {
    const cleanup = (): void => {
      removeWritableListener(writable, 'drain', onDrain);
      removeWritableListener(writable, 'close', onClose);
      removeWritableListener(writable, 'error', onError);
      signal.removeEventListener('abort', onAbort);
    };

    const onDrain = (): void => {
      cleanup();
      resolve();
    };
    const onClose = (): void => {
      cleanup();
      reject(new Error('writable closed before drain'));
    };
    const onError = (err?: unknown): void => {
      cleanup();
      reject(err instanceof Error ? err : new Error('writable errored before drain'));
    };
    const onAbort = (): void => {
      cleanup();
      reject(new Error('request aborted before drain'));
    };

    writable.once('drain', onDrain);
    writable.once('close', onClose);
    writable.once('error', onError);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

async function cancelReaderQuietly(reader: ReadableStreamDefaultReader<Uint8Array>, reason: unknown): Promise<void> {
  try {
    await reader.cancel(reason);
  } catch {
    // Ignore reader cancellation errors during disconnect paths.
  }
}

/**
 * Wrap a Fetch API handler (or {@link FetchApp}) as an HTTP/3 {@link StreamListener}.
 * Converts each H3 stream into a `Request`, invokes the handler, and writes the
 * `Response` back to the stream.
 */
export function createFetchHandler(appOrFetch: FetchApp | FetchHandler): StreamListener {
  const handler: FetchHandler = typeof appOrFetch === 'function'
    ? appOrFetch
    : appOrFetch.fetch.bind(appOrFetch);

  return (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
    void handleStream(handler, stream, headers, flags);
  };
}

function toBufferChunk(chunk: unknown): Buffer {
  if (Buffer.isBuffer(chunk)) {
    return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }
  if (typeof chunk === 'string') {
    return Buffer.from(chunk);
  }
  if (chunk instanceof ArrayBuffer) {
    return Buffer.from(new Uint8Array(chunk));
  }
  if (ArrayBuffer.isView(chunk)) {
    return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }
  throw new TypeError(`unsupported request body chunk type: ${typeof chunk}`);
}

async function readNodeRequestBody(req: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of req as AsyncIterable<unknown>) {
    chunks.push(toBufferChunk(chunk));
  }
  return Buffer.concat(chunks);
}

async function handleHttp1Request(handler: FetchHandler, req: IncomingMessage, res: ServerResponse): Promise<void> {
  const abortController = new AbortController();
  const abort = (): void => abortController.abort();
  req.once('aborted', abort);
  req.once('error', abort);
  res.once('close', abort);
  res.once('error', abort);

  const cleanupAbortHandlers = (): void => {
    req.removeListener('aborted', abort);
    req.removeListener('error', abort);
    res.removeListener('close', abort);
    res.removeListener('error', abort);
  };

  const method = req.method ?? 'GET';
  const authority = req.headers.host ?? 'localhost';
  const path = req.url ?? '/';
  const url = `https://${authority}${path}`;

  const reqHeaders = new Headers();
  for (const [key, value] of Object.entries(req.headers)) {
    if (typeof value === 'undefined') continue;
    if (Array.isArray(value)) {
      for (const v of value) reqHeaders.append(key, v);
    } else {
      reqHeaders.set(key, value);
    }
  }

  const hasBody = method !== 'GET' && method !== 'HEAD';
  const body = hasBody ? new Uint8Array(await readNodeRequestBody(req)) : undefined;
  const requestInit: RequestInit & { duplex?: 'half' } = {
    method,
    headers: reqHeaders,
    body,
    duplex: hasBody ? 'half' : undefined,
    signal: abortController.signal,
  };
  const request = new Request(url, requestInit);

  try {
    const response = await handler(request);
    const sseResponse = isSseContentType(response.headers.get('content-type'));
    res.statusCode = response.status;
    response.headers.forEach((value, key) => {
      res.setHeader(key, value);
    });

    if (sseResponse) {
      const defaults = sseHeaders();
      for (const [key, value] of Object.entries(defaults)) {
        if (key.startsWith(':')) continue;
        if (!res.hasHeader(key)) {
          res.setHeader(key, Array.isArray(value) ? value[0] : value);
        }
      }
      res.removeHeader('content-length');
    }

    if (method === 'HEAD' || !response.body) {
      res.end();
      return;
    }

    const reader = response.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!res.write(Buffer.from(value))) {
          await waitForDrainOrAbort(res, abortController.signal);
        }
      }
    } catch (err: unknown) {
      if (abortController.signal.aborted || res.destroyed || res.writableEnded) {
        await cancelReaderQuietly(reader, err);
        return;
      }
      throw err;
    } finally {
      reader.releaseLock();
    }
    if (!abortController.signal.aborted && !res.destroyed && !res.writableEnded) {
      res.end();
    }
  } catch (err: unknown) {
    if (abortController.signal.aborted || res.destroyed || res.writableEnded) {
      return;
    }
    if (!res.headersSent) {
      res.statusCode = 500;
      res.setHeader('content-type', 'text/plain; charset=utf-8');
    }
    res.end(err instanceof Error ? err.message : String(err));
  } finally {
    cleanupAbortHandlers();
  }
}

async function handleStream(
  handler: FetchHandler,
  stream: ServerHttp3Stream,
  headers: IncomingHeaders,
  flags: StreamFlags,
): Promise<void> {
  const abortController = new AbortController();
  const abort = (): void => abortController.abort();
  stream.once('close', abort);
  stream.once('error', abort);

  const cleanupAbortHandlers = (): void => {
    stream.removeListener('close', abort);
    stream.removeListener('error', abort);
  };

  const method = (headers[':method'] as string | undefined) ?? 'GET';
  const scheme = (headers[':scheme'] as string | undefined) ?? 'https';
  const authority = (headers[':authority'] as string | undefined) ?? 'localhost';
  const path = (headers[':path'] as string | undefined) ?? '/';

  const url = `${scheme}://${authority}${path}`;

  const reqHeaders = new Headers();
  for (const [key, value] of Object.entries(headers)) {
    if (key.startsWith(':')) continue;
    if (Array.isArray(value)) {
      for (const v of value) reqHeaders.append(key, v);
    } else {
      reqHeaders.set(key, value);
    }
  }

  const hasBody = method !== 'GET' && method !== 'HEAD' && !flags.endStream;
  const body = hasBody
    ? new ReadableStream({
        start(controller) {
          stream.on('data', (chunk: Buffer) => { controller.enqueue(new Uint8Array(chunk)); });
          stream.on('end', () => { controller.close(); });
          stream.on('close', () => { controller.error(new Error('request stream closed')); });
          stream.on('error', (err: unknown) => { controller.error(err); });
        },
      })
    : null;

  const request = new Request(url, {
    method,
    headers: reqHeaders,
    body,
    signal: abortController.signal,
    // @ts-expect-error duplex is needed for streaming request bodies
    duplex: hasBody ? 'half' : undefined,
  });

  try {
    const response = await handler(request);
    const sseResponse = isSseContentType(response.headers.get('content-type'));

    const resHeaders: IncomingHeaders = { ':status': String(response.status) };
    response.headers.forEach((value, key) => {
      resHeaders[key] = value;
    });
    if (sseResponse) {
      const defaults = sseHeaders();
      for (const [key, value] of Object.entries(defaults)) {
        if (key.startsWith(':')) continue;
        if (typeof resHeaders[key] === 'undefined') {
          resHeaders[key] = value;
        }
      }
      delete resHeaders['content-length'];
    }
    stream.respond(resHeaders);

    if (response.body) {
      const reader = response.body.getReader();
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          if (!stream.write(Buffer.from(value))) {
            await waitForDrainOrAbort(stream, abortController.signal);
          }
        }
      } catch (err: unknown) {
        if (abortController.signal.aborted || stream.destroyed || stream.writableEnded) {
          await cancelReaderQuietly(reader, err);
          return;
        }
        throw err;
      } finally {
        reader.releaseLock();
      }
    }
    if (!abortController.signal.aborted && !stream.destroyed && !stream.writableEnded) {
      stream.end();
    }
  } catch (err: unknown) {
    if (abortController.signal.aborted || stream.destroyed || stream.writableEnded) {
      return;
    }
    stream.destroy(err instanceof Error ? err : new Error(String(err)));
  } finally {
    cleanupAbortHandlers();
  }
}

/**
 * Create a `Response` that streams Server-Sent Events from an async iterable.
 * Suitable for returning from a {@link FetchHandler}.
 */
export function createSseFetchResponse(events: AsyncIterable<SseEvent | string>, init?: ResponseInit): Response {
  const headers = new Headers(init?.headers);
  const defaults = sseHeaders();
  for (const [name, value] of Object.entries(defaults)) {
    if (name.startsWith(':')) continue;
    if (!headers.has(name)) {
      headers.set(name, Array.isArray(value) ? value[0] : value);
    }
  }
  headers.delete('content-length');
  return new Response(createSseReadableStream(events), {
    ...init,
    status: init?.status ?? 200,
    headers,
  });
}

/** Options for the {@link serveFetch} convenience function. */
export interface ServeFetchOptions extends ServerOptions {
  /** UDP/TCP port to listen on. */
  port: number;
  /** Bind address (default `'0.0.0.0'`). */
  host?: string;
  /** The Fetch handler or app to serve. */
  fetch: FetchHandler | FetchApp;
}

/**
 * One-liner to start an HTTP/3 server powered by a Fetch API handler.
 * Creates the server, attaches the handler, and starts listening.
 */
export function serveFetch(options: ServeFetchOptions): Http3SecureServer {
  const { port, host, fetch: appOrFetch, ...serverOptions } = options;
  const fetchHandler: FetchHandler = typeof appOrFetch === 'function'
    ? appOrFetch
    : appOrFetch.fetch.bind(appOrFetch);
  const handler = createFetchHandler(appOrFetch);
  const server = createSecureServer(serverOptions, handler);
  server.on('request', (req: IncomingMessage, res: ServerResponse) => {
    void handleHttp1Request(fetchHandler, req, res);
  });
  server.listen(port, host);
  return server;
}
