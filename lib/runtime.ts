import { EventEmitter } from 'node:events';
import {
  Http3Error,
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  WARN_HTTP3_RUNTIME_FALLBACK,
} from './errors.js';

export type RuntimeMode = 'auto' | 'fast' | 'portable';
export type SelectedRuntimeMode = Exclude<RuntimeMode, 'auto'>;
export type FallbackPolicy = 'error' | 'warn-and-fallback';
export type RuntimeDriver = 'io_uring' | 'poll' | 'kqueue';
export type RuntimeReasonCode =
  | 'requested-fast'
  | 'requested-portable'
  | 'auto-selected-fast'
  | 'fast-path-unavailable'
  | 'platform-default'
  | 'runtime-error';

export interface RuntimeAttemptFailure {
  code: string;
  message: string;
  driver?: RuntimeDriver;
  errno?: number;
  syscall?: string;
}

export interface RuntimeInfo {
  requestedMode: RuntimeMode;
  fallbackPolicy: FallbackPolicy;
  selectedMode: SelectedRuntimeMode | null;
  driver: RuntimeDriver | null;
  fallbackOccurred: boolean;
  reasonCode: string | null;
  message?: string;
  errno?: number;
  syscall?: string;
  warningCode?: string;
  fastAttempt?: RuntimeAttemptFailure | null;
}

export interface RuntimeOptions {
  runtimeMode?: RuntimeMode;
  fallbackPolicy?: FallbackPolicy;
  onRuntimeEvent?: (info: RuntimeInfo) => void;
}

export interface RuntimeAware extends EventEmitter {
  _runtimeInfo: RuntimeInfo | null;
}

export const DEFAULT_RUNTIME_MODE: RuntimeMode = 'auto';
export const DEFAULT_FALLBACK_POLICY: FallbackPolicy = 'warn-and-fallback';

type NativeRuntimeErrorCode =
  | typeof ERR_HTTP3_FAST_PATH_UNAVAILABLE
  | typeof ERR_HTTP3_RUNTIME_UNSUPPORTED;

type CachedFastPathFailure = RuntimeAttemptFailure & {
  code: typeof ERR_HTTP3_FAST_PATH_UNAVAILABLE;
};

export interface ParsedNativeRuntimeError {
  code: NativeRuntimeErrorCode;
  message: string;
  driver?: RuntimeDriver;
  errno?: number;
  syscall?: string;
  reasonCode?: string;
}

const NATIVE_RUNTIME_ERROR_PREFIX = /^(ERR_HTTP3_[A-Z_]+)\s+([^:]+):\s+(.*)$/u;
let cachedFastPathFailure: CachedFastPathFailure | null = null;
const emittedRuntimeWarnings = new Set<string>();

function parseMetadataSegment(segment: string): Record<string, string> {
  const meta: Record<string, string> = {};
  for (const token of segment.split(/\s+/u)) {
    const equals = token.indexOf('=');
    if (equals <= 0) continue;
    const key = token.slice(0, equals);
    const value = token.slice(equals + 1);
    meta[key] = value;
  }
  return meta;
}

export function normalizeRuntimeMode(mode?: RuntimeMode): RuntimeMode {
  return mode ?? DEFAULT_RUNTIME_MODE;
}

export function normalizeFallbackPolicy(policy?: FallbackPolicy): FallbackPolicy {
  return policy ?? DEFAULT_FALLBACK_POLICY;
}

export function normalizeRuntimeOptions(options?: RuntimeOptions): Required<Pick<RuntimeOptions, 'runtimeMode' | 'fallbackPolicy'>> & Pick<RuntimeOptions, 'onRuntimeEvent'> {
  return {
    runtimeMode: normalizeRuntimeMode(options?.runtimeMode),
    fallbackPolicy: normalizeFallbackPolicy(options?.fallbackPolicy),
    onRuntimeEvent: options?.onRuntimeEvent,
  };
}

export function createPendingRuntimeInfo(options?: RuntimeOptions): RuntimeInfo {
  const runtime = normalizeRuntimeOptions(options);
  return {
    requestedMode: runtime.runtimeMode,
    fallbackPolicy: runtime.fallbackPolicy,
    selectedMode: null,
    driver: null,
    fallbackOccurred: false,
    reasonCode: null,
    fastAttempt: null,
  };
}

export function setPendingRuntimeInfo(target: RuntimeAware, options?: RuntimeOptions): RuntimeInfo {
  const info = createPendingRuntimeInfo(options);
  target._runtimeInfo = info;
  return info;
}

export function updateRuntimeInfo(
  target: RuntimeAware,
  info: RuntimeInfo,
  onRuntimeEvent?: (info: RuntimeInfo) => void,
): RuntimeInfo {
  const snapshot: RuntimeInfo = {
    ...info,
    fastAttempt: info.fastAttempt ? { ...info.fastAttempt } : null,
  };
  target._runtimeInfo = snapshot;
  onRuntimeEvent?.(snapshot);
  target.emit('runtime', snapshot);

  if (snapshot.fallbackOccurred && snapshot.fallbackPolicy === 'warn-and-fallback') {
    const warningKey = createRuntimeWarningKey(snapshot);
    if (!emittedRuntimeWarnings.has(warningKey)) {
      emittedRuntimeWarnings.add(warningKey);
      process.emitWarning(formatRuntimeWarning(snapshot), {
        code: snapshot.warningCode ?? WARN_HTTP3_RUNTIME_FALLBACK,
        detail: JSON.stringify(snapshot),
      });
    }
  }

  return snapshot;
}

export function driverForMode(mode: SelectedRuntimeMode, platform = process.platform): RuntimeDriver {
  if (platform === 'linux') {
    return mode === 'fast' ? 'io_uring' : 'poll';
  }

  return 'kqueue';
}

export function parseNativeRuntimeError(err: unknown): ParsedNativeRuntimeError | null {
  if (!(err instanceof Error)) return null;

  const match = err.message.match(NATIVE_RUNTIME_ERROR_PREFIX);
  if (!match) return null;

  const [, code, rawMeta, message] = match;
  if (code !== ERR_HTTP3_FAST_PATH_UNAVAILABLE && code !== ERR_HTTP3_RUNTIME_UNSUPPORTED) {
    return null;
  }

  const meta = parseMetadataSegment(rawMeta);
  const errno = typeof meta.errno === 'string' ? Number.parseInt(meta.errno, 10) : undefined;
  return {
    code,
    message,
    driver: meta.driver as RuntimeDriver | undefined,
    errno: Number.isFinite(errno) ? errno : undefined,
    syscall: meta.syscall,
    reasonCode: meta.reason_code,
  };
}

export function isFastPathUnavailableError(err: unknown): boolean {
  if (err instanceof Http3Error) {
    return err.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE;
  }
  return parseNativeRuntimeError(err)?.code === ERR_HTTP3_FAST_PATH_UNAVAILABLE;
}

export function toRuntimeAttemptFailure(err: unknown): RuntimeAttemptFailure {
  const parsed = parseNativeRuntimeError(err);
  if (parsed) {
    return {
      code: parsed.code,
      message: parsed.message,
      driver: parsed.driver,
      errno: parsed.errno,
      syscall: parsed.syscall,
    };
  }

  if (err instanceof Http3Error) {
    return {
      code: err.code,
      message: err.message,
      driver: err.driver,
      errno: err.errno,
      syscall: err.syscall,
    };
  }

  if (err instanceof Error) {
    return {
      code: ERR_HTTP3_RUNTIME_UNSUPPORTED,
      message: err.message,
    };
  }

  return {
    code: ERR_HTTP3_RUNTIME_UNSUPPORTED,
    message: String(err),
  };
}

function cacheFastPathFailure(failure: RuntimeAttemptFailure): void {
  if (failure.code !== ERR_HTTP3_FAST_PATH_UNAVAILABLE) return;
  cachedFastPathFailure = {
    ...failure,
    code: ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  };
}

function getCachedFastPathFailure(): CachedFastPathFailure | null {
  if (!cachedFastPathFailure) return null;
  return { ...cachedFastPathFailure };
}

function createAutoFallbackInfo(
  fallbackPolicy: FallbackPolicy,
  fastAttempt: RuntimeAttemptFailure,
): RuntimeInfo {
  return createSelectedRuntimeInfo('auto', fallbackPolicy, 'portable', 'fast-path-unavailable', {
    fallbackOccurred: true,
    message: fastAttempt.message,
    errno: fastAttempt.errno,
    syscall: fastAttempt.syscall,
    fastAttempt,
  });
}

function createCachedFastPathFailureInfo(
  fallbackPolicy: FallbackPolicy,
  fastAttempt: CachedFastPathFailure,
): RuntimeInfo {
  return {
    requestedMode: 'auto',
    fallbackPolicy,
    selectedMode: null,
    driver: fastAttempt.driver ?? driverForMode('fast'),
    fallbackOccurred: false,
    reasonCode: 'fast-path-unavailable',
    message: fastAttempt.message,
    errno: fastAttempt.errno,
    syscall: fastAttempt.syscall,
    fastAttempt,
  };
}

function createCachedFastPathError(
  info: RuntimeInfo,
  fastAttempt: CachedFastPathFailure,
): Http3Error {
  return new Http3Error(fastAttempt.message, ERR_HTTP3_FAST_PATH_UNAVAILABLE, {
    requestedMode: info.requestedMode,
    selectedMode: info.selectedMode ?? undefined,
    driver: fastAttempt.driver ?? info.driver ?? undefined,
    reasonCode: 'fast-path-unavailable',
    errno: fastAttempt.errno ?? info.errno,
    syscall: fastAttempt.syscall ?? info.syscall,
    fallbackOccurred: info.fallbackOccurred,
    runtimeInfo: info,
  });
}

/** @internal test hook */
export function resetRuntimeSelectionCacheForTests(): void {
  cachedFastPathFailure = null;
  emittedRuntimeWarnings.clear();
}

export function createSelectedRuntimeInfo(
  requestedMode: RuntimeMode,
  fallbackPolicy: FallbackPolicy,
  selectedMode: SelectedRuntimeMode,
  reasonCode: string,
  options?: {
    fallbackOccurred?: boolean;
    message?: string;
    errno?: number;
    syscall?: string;
    fastAttempt?: RuntimeAttemptFailure | null;
  },
): RuntimeInfo {
  return {
    requestedMode,
    fallbackPolicy,
    selectedMode,
    driver: driverForMode(selectedMode),
    fallbackOccurred: options?.fallbackOccurred ?? false,
    reasonCode,
    message: options?.message,
    errno: options?.errno,
    syscall: options?.syscall,
    warningCode: options?.fallbackOccurred ? WARN_HTTP3_RUNTIME_FALLBACK : undefined,
    fastAttempt: options?.fastAttempt ?? null,
  };
}

export function formatRuntimeWarning(info: RuntimeInfo): string {
  const selected = info.selectedMode ?? 'unknown';
  const driver = info.driver ?? 'unknown';
  const base = `@currentspace/http3 runtime fallback selected ${selected} (${driver})`;
  if (!info.message) return base;
  return `${base}: ${info.message}`;
}

function createRuntimeWarningKey(info: RuntimeInfo): string {
  return JSON.stringify({
    requestedMode: info.requestedMode,
    fallbackPolicy: info.fallbackPolicy,
    selectedMode: info.selectedMode,
    driver: info.driver,
    reasonCode: info.reasonCode,
    message: info.message,
    errno: info.errno,
    syscall: info.syscall,
    warningCode: info.warningCode,
    fastAttempt: info.fastAttempt ?? null,
  });
}

function buildFailureInfo(
  requestedMode: RuntimeMode,
  fallbackPolicy: FallbackPolicy,
  attemptedMode: SelectedRuntimeMode,
  error: unknown,
  options?: { fallbackOccurred?: boolean; fastAttempt?: RuntimeAttemptFailure | null },
): RuntimeInfo {
  if (error instanceof Http3Error) {
    return {
      requestedMode,
      fallbackPolicy,
      selectedMode: null,
      driver: error.driver ?? driverForMode(attemptedMode),
      fallbackOccurred: options?.fallbackOccurred ?? false,
      reasonCode: error.reasonCode ?? 'runtime-error',
      message: error.message,
      errno: error.errno,
      syscall: error.syscall,
      fastAttempt: options?.fastAttempt ?? null,
    };
  }

  const parsed = parseNativeRuntimeError(error);
  return {
    requestedMode,
    fallbackPolicy,
    selectedMode: null,
    driver: driverForMode(attemptedMode),
    fallbackOccurred: options?.fallbackOccurred ?? false,
    reasonCode: parsed?.reasonCode ?? 'runtime-error',
    message: parsed?.message ?? (error instanceof Error ? error.message : String(error)),
    errno: parsed?.errno,
    syscall: parsed?.syscall,
    fastAttempt: options?.fastAttempt ?? null,
  };
}

function enrichRuntimeError(error: unknown, info: RuntimeInfo): unknown {
  if (error instanceof Http3Error) {
    return new Http3Error(error.message, error.code, {
      quicCode: error.quicCode,
      h3Code: error.h3Code,
      requestedMode: error.requestedMode ?? info.requestedMode,
      selectedMode: error.selectedMode ?? info.selectedMode ?? undefined,
      driver: error.driver ?? info.driver ?? undefined,
      reasonCode: error.reasonCode ?? info.reasonCode ?? undefined,
      errno: error.errno ?? info.errno,
      syscall: error.syscall ?? info.syscall,
      endpoint: error.endpoint,
      host: error.host,
      port: error.port,
      servername: error.servername,
      fallbackOccurred: error.fallbackOccurred ?? info.fallbackOccurred,
      runtimeInfo: info,
      cause: error,
    });
  }

  const parsed = parseNativeRuntimeError(error);
  if (!parsed) {
    return error;
  }

  return new Http3Error(parsed.message, parsed.code, {
    requestedMode: info.requestedMode,
    selectedMode: info.selectedMode ?? undefined,
    driver: parsed.driver ?? info.driver ?? undefined,
    reasonCode: parsed.reasonCode,
    errno: parsed.errno,
    syscall: parsed.syscall,
    fallbackOccurred: info.fallbackOccurred,
    runtimeInfo: info,
    cause: error instanceof Error ? error : undefined,
  });
}

export function runWithRuntimeSelectionSync<T>(
  target: RuntimeAware,
  options: RuntimeOptions | undefined,
  attempt: (mode: SelectedRuntimeMode) => T,
): T {
  const runtime = normalizeRuntimeOptions(options);

  const run = (mode: SelectedRuntimeMode, reasonCode: string): T => {
    try {
      const result = attempt(mode);
      updateRuntimeInfo(
        target,
        createSelectedRuntimeInfo(runtime.runtimeMode, runtime.fallbackPolicy, mode, reasonCode),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (error: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, mode, error);
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }
  };

  if (runtime.runtimeMode === 'portable') {
    return run('portable', 'requested-portable');
  }

  if (runtime.runtimeMode === 'fast') {
    return run('fast', 'requested-fast');
  }

  const cachedFailure = getCachedFastPathFailure();
  if (cachedFailure) {
    if (runtime.fallbackPolicy === 'error') {
      const failure = createCachedFastPathFailureInfo(runtime.fallbackPolicy, cachedFailure);
      target._runtimeInfo = failure;
      throw createCachedFastPathError(failure, cachedFailure);
    }

    try {
      const result = attempt('portable');
      updateRuntimeInfo(
        target,
        createAutoFallbackInfo(runtime.fallbackPolicy, cachedFailure),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (portableError: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'portable', portableError, {
        fallbackOccurred: true,
        fastAttempt: cachedFailure,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(portableError, failure);
    }
  }

  try {
    return run('fast', 'auto-selected-fast');
  } catch (error: unknown) {
    const fastAttempt = toRuntimeAttemptFailure(error);
    if (!isFastPathUnavailableError(error)) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'fast', error, {
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }

    cacheFastPathFailure(fastAttempt);

    if (runtime.fallbackPolicy === 'error') {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'fast', error, {
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }

    try {
      const result = attempt('portable');
      updateRuntimeInfo(
        target,
        createAutoFallbackInfo(runtime.fallbackPolicy, fastAttempt),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (portableError: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'portable', portableError, {
        fallbackOccurred: true,
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(portableError, failure);
    }
  }
}

export async function runWithRuntimeSelection<T>(
  target: RuntimeAware,
  options: RuntimeOptions | undefined,
  attempt: (mode: SelectedRuntimeMode) => Promise<T>,
): Promise<T> {
  const runtime = normalizeRuntimeOptions(options);

  const run = async (mode: SelectedRuntimeMode, reasonCode: string): Promise<T> => {
    try {
      const result = await attempt(mode);
      updateRuntimeInfo(
        target,
        createSelectedRuntimeInfo(runtime.runtimeMode, runtime.fallbackPolicy, mode, reasonCode),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (error: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, mode, error);
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }
  };

  if (runtime.runtimeMode === 'portable') {
    return await run('portable', 'requested-portable');
  }

  if (runtime.runtimeMode === 'fast') {
    return await run('fast', 'requested-fast');
  }

  const cachedFailure = getCachedFastPathFailure();
  if (cachedFailure) {
    if (runtime.fallbackPolicy === 'error') {
      const failure = createCachedFastPathFailureInfo(runtime.fallbackPolicy, cachedFailure);
      target._runtimeInfo = failure;
      throw createCachedFastPathError(failure, cachedFailure);
    }

    try {
      const result = await attempt('portable');
      updateRuntimeInfo(
        target,
        createAutoFallbackInfo(runtime.fallbackPolicy, cachedFailure),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (portableError: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'portable', portableError, {
        fallbackOccurred: true,
        fastAttempt: cachedFailure,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(portableError, failure);
    }
  }

  try {
    return await run('fast', 'auto-selected-fast');
  } catch (error: unknown) {
    const fastAttempt = toRuntimeAttemptFailure(error);
    if (!isFastPathUnavailableError(error)) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'fast', error, {
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }

    cacheFastPathFailure(fastAttempt);

    if (runtime.fallbackPolicy === 'error') {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'fast', error, {
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(error, failure);
    }

    try {
      const result = await attempt('portable');
      updateRuntimeInfo(
        target,
        createAutoFallbackInfo(runtime.fallbackPolicy, fastAttempt),
        runtime.onRuntimeEvent,
      );
      return result;
    } catch (portableError: unknown) {
      const failure = buildFailureInfo(runtime.runtimeMode, runtime.fallbackPolicy, 'portable', portableError, {
        fallbackOccurred: true,
        fastAttempt,
      });
      target._runtimeInfo = failure;
      throw enrichRuntimeError(portableError, failure);
    }
  }
}
