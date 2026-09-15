import type { Readable } from 'node:stream';

export const DEFAULT_MAX_BODY_BYTES = 16 * 1024 * 1024;
const BODY_HIGH_WATER_MARK = 64 * 1024;

export class RequestBodyTooLargeError extends Error {
  constructor() { super('request body exceeds maxBodyBytes'); }
}

export function validateBodyLimit(limit: number): number {
  if (!Number.isSafeInteger(limit) || limit < 0) {
    throw new RangeError('maxBodyBytes must be a non-negative safe integer');
  }
  return limit;
}

/** A bounded Web stream over a Node readable; pausing propagates to H1/H2. */
export function createRequestBody(
  source: Readable,
  signal: AbortSignal,
  limit: number,
): { body: ReadableStream<Uint8Array>; dispose: () => void } {
  let received = 0;
  let finished = false;
  let controller: ReadableStreamDefaultController<Uint8Array>;
  const dispose = (): void => {
    source.pause();
    source.off('data', onData);
    source.off('end', onEnd);
    source.off('error', onError);
    source.off('close', onClose);
    signal.removeEventListener('abort', onAbort);
  };
  const fail = (error: unknown): void => {
    if (finished) return;
    finished = true;
    dispose();
    controller.error(error);
  };
  const onData = (chunk: Buffer): void => {
    received += chunk.byteLength;
    if (received > limit) { fail(new RequestBodyTooLargeError()); return; }
    controller.enqueue(new Uint8Array(chunk));
    if ((controller.desiredSize ?? 0) <= 0) source.pause();
  };
  const onEnd = (): void => {
    if (finished) return;
    finished = true;
    dispose();
    controller.close();
  };
  const onError = (error: Error): void => fail(error);
  const onClose = (): void => {
    if (source.readableEnded) onEnd();
    else fail(new Error('request stream closed'));
  };
  const onAbort = (): void => fail(signal.reason);
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
      source.on('data', onData);
      source.once('end', onEnd);
      source.once('error', onError);
      source.once('close', onClose);
      source.pause();
      signal.addEventListener('abort', onAbort, { once: true });
      if (signal.aborted) onAbort();
      else if (source.readableEnded) onEnd();
      else if (source.destroyed) onClose();
    },
    pull() { if (!finished) source.resume(); },
    cancel() { finished = true; dispose(); },
  }, { highWaterMark: BODY_HIGH_WATER_MARK, size: chunk => chunk.byteLength });
  return { body, dispose };
}
