import { Http3Error, ERR_HTTP3_SESSION_ERROR, ERR_HTTP3_STREAM_ERROR } from './errors.js';
import type { NativeEvent } from './event-loop.js';

/**
 * Convert a native stream-error event into an {@link Http3Error}.
 * @internal
 */
export function toStreamError(event: NativeEvent, fallback = 'stream error'): Http3Error {
  const message = event.meta?.errorReason ?? fallback;
  return new Http3Error(message, ERR_HTTP3_STREAM_ERROR, {
    h3Code: event.meta?.errorCode,
  });
}

/**
 * Convert a native session-error event into an {@link Http3Error}.
 * @internal
 */
export function toSessionError(event: NativeEvent, fallback = 'session error'): Http3Error {
  const message = event.meta?.errorReason ?? fallback;
  return new Http3Error(message, ERR_HTTP3_SESSION_ERROR, {
    quicCode: event.meta?.errorCode,
  });
}
