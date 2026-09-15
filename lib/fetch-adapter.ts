import { createRequestBody, DEFAULT_MAX_BODY_BYTES, RequestBodyTooLargeError, validateBodyLimit } from './request-body.js';
import { isWritableClosed, waitForDrainOrAbort } from './writable-lifecycle.js';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { runDetached } from './run-detached.js';
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

function errorCode(error: unknown): string | undefined {
  const code = (error as { code?: unknown }).code;
  return typeof code === 'string' ? code : undefined;
}

function isExpectedWritableCloseError(error: unknown): boolean {
  const code = errorCode(error);
  if (
    code === 'ERR_STREAM_WRITE_AFTER_END' ||
    code === 'ERR_STREAM_DESTROYED' ||
    code === 'ERR_HTTP2_STREAM_CLOSED' ||
    code === 'ERR_HTTP2_INVALID_STREAM' ||
    code === 'ABORT_ERR'
  ) {
    return true;
  }

  if (!(error instanceof Error)) return false;
  return (
    error.message.includes('write after end') ||
    error.message.includes('writable is closed') ||
    error.message.includes('writable closed before drain') ||
    error.message.includes('request aborted before drain') ||
    error.message.includes('stream closed') ||
    error.message.includes('stream destroyed')
  );
}

async function cancelReaderQuietly(reader: ReadableStreamDefaultReader<Uint8Array>, reason: unknown): Promise<void> {
  try {
    await reader.cancel(reason);
  } catch {
    // Ignore reader cancellation errors during disconnect paths.
  }
}

function cancelOnAbort(reader: ReadableStreamDefaultReader<Uint8Array>, signal: AbortSignal): () => void {
  const abort = (): void => {
    // cancel() settles pending read() immediately, even if the source's
    // cooperative cleanup is asynchronous. Always observe that cleanup.
    runDetached(cancelReaderQuietly(reader, signal.reason), (err) => { console.error(err); });
  };
  signal.addEventListener('abort', abort, { once: true });
  if (signal.aborted) abort();
  return () => signal.removeEventListener('abort', abort);
}

/**
 * Wrap a Fetch API handler (or {@link FetchApp}) as an HTTP/3 {@link StreamListener}.
 * Converts each H3 stream into a `Request`, invokes the handler, and writes the
 * `Response` back to the stream.
 */
export interface FetchHandlerOptions {
  /** Maximum consumed request body size in bytes. Default: 16 MiB. */
  maxBodyBytes?: number;
}

export function createFetchHandler(appOrFetch: FetchApp | FetchHandler, options: FetchHandlerOptions = {}): StreamListener {
  const maxBodyBytes = validateBodyLimit(options.maxBodyBytes ?? DEFAULT_MAX_BODY_BYTES);
  const handler: FetchHandler = typeof appOrFetch === 'function'
    ? appOrFetch
    : appOrFetch.fetch.bind(appOrFetch);

  return (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags) => {
    stream._maxBufferedReadBytes = 1024 * 1024;
    // handleStream() already has its own try/catch/finally (reports
    // failures via stream.destroy(err)), so this onError is a defensive
    // backstop, not the primary handling path.
    runDetached(handleStream(handler, stream, headers, flags, maxBodyBytes), (err) => {
      console.error('unhandled error in fetch adapter stream handler:', err);
    });
  };
}

async function handleHttp1Request(handler: FetchHandler, req: IncomingMessage, res: ServerResponse, maxBodyBytes: number): Promise<void> {
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

  let disposeBody: (() => void) | undefined;
  try {
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
    if (Number(req.headers['content-length']) > maxBodyBytes) throw new RequestBodyTooLargeError();
    const requestBody = hasBody ? createRequestBody(req, abortController.signal, maxBodyBytes) : undefined;
    disposeBody = requestBody?.dispose;
    const body = requestBody?.body;
    const requestInit: RequestInit & { duplex?: 'half' } = {
      method,
      headers: reqHeaders,
      body,
      duplex: hasBody ? 'half' : undefined,
      signal: abortController.signal,
    };
    const request = new Request(url, requestInit);

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
    const stopReading = cancelOnAbort(reader, abortController.signal);
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!res.write(Buffer.from(value))) {
          await waitForDrainOrAbort(res, abortController.signal);
        }
      }
    } catch (err: unknown) {
      if (abortController.signal.aborted || isWritableClosed(res) || isExpectedWritableCloseError(err)) {
        await cancelReaderQuietly(reader, err);
        return;
      }
      throw err;
    } finally {
      stopReading();
      reader.releaseLock();
    }
    if (!abortController.signal.aborted && !isWritableClosed(res)) {
      res.end();
    }
  } catch (err: unknown) {
    if (abortController.signal.aborted || isWritableClosed(res) || isExpectedWritableCloseError(err)) {
      return;
    }
    if (!res.headersSent) {
      res.statusCode = err instanceof RequestBodyTooLargeError ? 413 : 500;
      res.setHeader('content-type', 'text/plain; charset=utf-8');
    }
    res.end(err instanceof Error ? err.message : String(err));
  } finally {
    disposeBody?.();
    cleanupAbortHandlers();
  }
}

async function handleStream(
  handler: FetchHandler,
  stream: ServerHttp3Stream,
  headers: IncomingHeaders,
  flags: StreamFlags,
  maxBodyBytes: number,
): Promise<void> {
  const abortController = new AbortController();
  const abort = (): void => abortController.abort();
  stream.once('aborted', abort);
  stream.once('close', abort);
  stream.once('error', abort);

  const cleanupAbortHandlers = (): void => {
    stream.removeListener('aborted', abort);
    stream.removeListener('close', abort);
    stream.removeListener('error', abort);
  };

  let disposeBody: (() => void) | undefined;
  try {
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
    if (Number(headers['content-length']) > maxBodyBytes) throw new RequestBodyTooLargeError();
    const requestBody = hasBody ? createRequestBody(stream, abortController.signal, maxBodyBytes) : undefined;
    disposeBody = requestBody?.dispose;
    const body = requestBody?.body ?? null;

    const request = new Request(url, {
      method,
      headers: reqHeaders,
      body,
      signal: abortController.signal,
      // @ts-expect-error duplex is needed for streaming request bodies
      duplex: hasBody ? 'half' : undefined,
    });

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
      const stopReading = cancelOnAbort(reader, abortController.signal);
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          let accepted = false;
          try {
            accepted = stream.write(Buffer.from(value));
          } catch (err: unknown) {
            if (abortController.signal.aborted || isWritableClosed(stream) || isExpectedWritableCloseError(err)) {
              await cancelReaderQuietly(reader, err);
              return;
            }
            throw err;
          }
          if (!accepted) {
            await waitForDrainOrAbort(stream, abortController.signal);
          }
        }
      } catch (err: unknown) {
        if (abortController.signal.aborted || isWritableClosed(stream) || isExpectedWritableCloseError(err)) {
          await cancelReaderQuietly(reader, err);
          return;
        }
        throw err;
      } finally {
        stopReading();
        reader.releaseLock();
      }
    }
    if (!abortController.signal.aborted && !isWritableClosed(stream)) {
      stream.end();
    }
  } catch (err: unknown) {
    if (abortController.signal.aborted || isWritableClosed(stream) || isExpectedWritableCloseError(err)) {
      return;
    }
    if (err instanceof RequestBodyTooLargeError && !stream._headersSent) {
      stream.respond({ ':status': '413' });
      stream.end(err.message);
      return;
    }
    stream.destroy(err instanceof Error ? err : new Error(String(err)));
  } finally {
    disposeBody?.();
    // The response may finish before the peer finishes uploading. Keep
    // the error guard until close, and discard the unread tail without
    // retaining it in either the Web queue or the native spill queue.
    if (!stream.destroyed) {
      stream.once('close', cleanupAbortHandlers);
      stream.resume();
    } else {
      cleanupAbortHandlers();
    }
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
export interface ServeFetchOptions extends ServerOptions, FetchHandlerOptions {
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
  const { port, host, fetch: appOrFetch, maxBodyBytes = DEFAULT_MAX_BODY_BYTES, ...serverOptions } = options;
  const fetchHandler: FetchHandler = typeof appOrFetch === 'function'
    ? appOrFetch
    : appOrFetch.fetch.bind(appOrFetch);
  const handler = createFetchHandler(appOrFetch, { maxBodyBytes });
  const server = createSecureServer(serverOptions, handler);
  server.on('request', (req: IncomingMessage, res: ServerResponse) => {
    // handleHttp1Request() already has its own try/catch/finally (reports
    // failures via res.end(...)), so this onError is a defensive backstop.
    runDetached(handleHttp1Request(fetchHandler, req, res, maxBodyBytes), (err) => {
      console.error('unhandled error in fetch adapter HTTP/1.1 handler:', err);
    });
  });
  server.listen(port, host);
  return server;
}
