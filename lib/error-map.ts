import type { RuntimeDriver, RuntimeMode, SelectedRuntimeMode } from './runtime.js';
import {
  Http3Error,
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  ERR_HTTP3_SESSION_ERROR,
  ERR_HTTP3_STREAM_ERROR,
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
