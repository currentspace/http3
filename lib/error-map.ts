import type { RuntimeDriver, RuntimeMode, SelectedRuntimeMode } from './runtime.js';
import {
  Http3Error,
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  ERR_HTTP3_SESSION_ERROR,
  ERR_HTTP3_STREAM_ERROR,
  ERR_HTTP3_INVALID_STATE,
} from './errors.js';
import type { NativeEvent } from './event-loop.js';

function runtimeCodeForEvent(event: NativeEvent): string {
  if (event.meta?.reasonCode === 'fast-path-unavailable') {
    return ERR_HTTP3_FAST_PATH_UNAVAILABLE;
  }

  return ERR_HTTP3_RUNTIME_UNSUPPORTED;
}

function runtimeOptionsFromEvent(event: NativeEvent): ConstructorParameters<typeof Http3Error>[2] {
  return {
    requestedMode: event.meta?.requestedRuntimeMode as RuntimeMode | undefined,
    selectedMode: event.meta?.runtimeMode as SelectedRuntimeMode | undefined,
    driver: event.meta?.runtimeDriver as RuntimeDriver | undefined,
    reasonCode: event.meta?.reasonCode,
    errno: event.meta?.errno,
    syscall: event.meta?.syscall,
    fallbackOccurred: event.meta?.fallbackOccurred,
  };
}

/**
 * Convert a native stream-error event into an {@link Http3Error}.
 * @internal
 */
export function toStreamError(event: NativeEvent, fallback = 'stream error'): Http3Error {
  const message = event.meta?.errorReason ?? fallback;
  if (event.meta?.errorCategory === 'runtime') {
    return new Http3Error(message, runtimeCodeForEvent(event), runtimeOptionsFromEvent(event));
  }
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
  if (event.meta?.errorCategory === 'runtime') {
    return new Http3Error(message, runtimeCodeForEvent(event), runtimeOptionsFromEvent(event));
  }
  return new Http3Error(message, ERR_HTTP3_SESSION_ERROR, {
    quicCode: event.meta?.errorCode,
  });
}

/**
 * Parse the structured prefix that the Rust side stamps onto napi::Error
 * messages (audit finding #30). Format:
 *
 *     [h3:<category>|key=value|key=value|…] <human message>
 *
 * Returns `null` when the input is not a tagged napi error so callers
 * can fall through to default Error handling.
 *
 * @internal
 */
export function fromNapiError(value: unknown): Http3Error | null {
  if (!(value instanceof Error)) return null;
  const match = /^\[h3:([^\]|]+)((?:\|[^\]]*)?)\]\s*(.*)$/s.exec(value.message);
  if (!match) return null;
  const category = match[1];
  const meta: Partial<Record<string, string>> = {};
  if (match[2]) {
    for (const part of match[2].split('|').filter(Boolean)) {
      const eq = part.indexOf('=');
      if (eq > 0) {
        meta[part.slice(0, eq)] = part.slice(eq + 1);
      }
    }
  }
  const message = match[3];

  const parseInt = (v: string | undefined): number | undefined =>
    v === undefined ? undefined : Number.parseInt(v, 10);

  switch (category) {
    case 'fast-path':
      return new Http3Error(message, ERR_HTTP3_FAST_PATH_UNAVAILABLE, {
        driver: meta.driver as RuntimeDriver | undefined,
        syscall: meta.syscall,
        errno: parseInt(meta.errno),
        reasonCode: 'fast-path-unavailable',
      });
    case 'runtime-io':
      return new Http3Error(message, ERR_HTTP3_RUNTIME_UNSUPPORTED, {
        driver: meta.driver as RuntimeDriver | undefined,
        syscall: meta.syscall,
        errno: parseInt(meta.errno),
        reasonCode: meta.reason,
      });
    case 'invalid-state':
    case 'config':
    case 'not-found':
      return new Http3Error(message, ERR_HTTP3_INVALID_STATE);
    case 'quic':
      return new Http3Error(message, ERR_HTTP3_SESSION_ERROR, {
        quicCode: parseInt(meta.h3code),
      });
    case 'h3':
      return new Http3Error(message, ERR_HTTP3_STREAM_ERROR, {
        h3Code: parseInt(meta.h3code),
      });
    case 'io':
      return new Http3Error(message, ERR_HTTP3_RUNTIME_UNSUPPORTED, {
        errno: parseInt(meta.errno),
      });
    default:
      return null;
  }
}
