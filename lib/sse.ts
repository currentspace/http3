import { once } from 'node:events';
import type { IncomingHeaders, ServerHttp3Stream } from './stream.js';

export interface SseEvent {
  data: string | string[];
  event?: string;
  id?: string;
  retry?: number;
}

export interface SseStreamOptions {
  headers?: IncomingHeaders;
  heartbeatIntervalMs?: number;
  heartbeatComment?: string;
}

export const SSE_HEADERS: IncomingHeaders = {
  ':status': '200',
  'content-type': 'text/event-stream; charset=utf-8',
  'cache-control': 'no-cache, no-transform',
  'x-accel-buffering': 'no',
};

export function encodeSseEvent(event: SseEvent | string): string {
  const payload: SseEvent = typeof event === 'string' ? { data: event } : event;
  const lines: string[] = [];

  if (typeof payload.event === 'string' && payload.event.length > 0) {
    lines.push(`event: ${payload.event}`);
  }
  if (typeof payload.id === 'string') {
    lines.push(`id: ${payload.id}`);
  }
  if (typeof payload.retry === 'number' && Number.isFinite(payload.retry)) {
    lines.push(`retry: ${Math.max(0, Math.floor(payload.retry))}`);
  }

  const dataLines = Array.isArray(payload.data)
    ? payload.data
    : payload.data.split(/\r?\n/u);
  for (const line of dataLines) {
    lines.push(`data: ${line}`);
  }

  return `${lines.join('\n')}\n\n`;
}

export function encodeSseComment(comment = ''): string {
  const lines = comment.split(/\r?\n/u);
  return `${lines.map(line => `: ${line}`).join('\n')}\n\n`;
}

export function sseHeaders(extraHeaders?: IncomingHeaders): IncomingHeaders {
  return {
    ...SSE_HEADERS,
    ...(extraHeaders ?? {}),
  };
}

export class ServerSentEventStream {
  private readonly _stream: ServerHttp3Stream;
  private _closed = false;
  private _heartbeatTimer: NodeJS.Timeout | null = null;

  constructor(stream: ServerHttp3Stream, options?: SseStreamOptions) {
    this._stream = stream;
    this._stream.respond(sseHeaders(options?.headers));
    this._stream.once('close', () => this._cleanup());
    this._stream.once('error', () => this._cleanup());
    if (options?.heartbeatIntervalMs && options.heartbeatIntervalMs > 0) {
      this.heartbeat(options.heartbeatIntervalMs, options.heartbeatComment);
    }
  }

  async send(event: SseEvent | string): Promise<void> {
    await this._writeFrame(encodeSseEvent(event));
  }

  async comment(text = ''): Promise<void> {
    await this._writeFrame(encodeSseComment(text));
  }

  heartbeat(intervalMs = 15000, comment = 'keepalive'): void {
    this._clearHeartbeat();
    this._heartbeatTimer = setInterval(() => {
      void this.comment(comment).catch(() => {
        this.close();
      });
    }, intervalMs);
    this._heartbeatTimer.unref();
  }

  close(): void {
    if (this._closed) return;
    this._closed = true;
    this._cleanup();
    if (!this._stream.writableEnded && !this._stream.destroyed) {
      this._stream.end();
    }
  }

  private async _writeFrame(frame: string): Promise<void> {
    if (this._closed || this._stream.destroyed) return;
    if (this._stream.write(frame)) return;
    await once(this._stream, 'drain');
  }

  private _clearHeartbeat(): void {
    if (!this._heartbeatTimer) return;
    clearInterval(this._heartbeatTimer);
    this._heartbeatTimer = null;
  }

  private _cleanup(): void {
    this._clearHeartbeat();
  }
}

export function createSseStream(stream: ServerHttp3Stream, options?: SseStreamOptions): ServerSentEventStream {
  return new ServerSentEventStream(stream, options);
}

export function createSseReadableStream(source: AsyncIterable<SseEvent | string>): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  const iterator = source[Symbol.asyncIterator]();

  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      const item = await iterator.next();
      if (item.done) {
        controller.close();
        return;
      }
      controller.enqueue(encoder.encode(encodeSseEvent(item.value)));
    },
    async cancel() {
      if (typeof iterator.return === 'function') {
        await iterator.return();
      }
    },
  });
}
