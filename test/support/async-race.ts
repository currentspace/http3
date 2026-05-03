import type { EventEmitter } from 'node:events';

type TimeoutValue<T> = T | (() => T);

function resolveTimeoutValue<T>(value: TimeoutValue<T>): T {
  return typeof value === 'function' ? (value as () => T)() : value;
}

export async function withTimeoutValue<T, U>(
  promise: Promise<T>,
  timeoutMs: number,
  timeoutValue: TimeoutValue<U>,
): Promise<T | U> {
  let timer: NodeJS.Timeout | null = null;
  const timeout = new Promise<U>((resolve) => {
    timer = setTimeout(() => {
      resolve(resolveTimeoutValue(timeoutValue));
    }, timeoutMs);
  });

  try {
    return await Promise.race([promise, timeout]);
  } finally {
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}

export async function withTimeoutError<T>(
  promise: Promise<T>,
  timeoutMs: number,
  error: TimeoutValue<Error>,
): Promise<T> {
  let timer: NodeJS.Timeout | null = null;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      reject(resolveTimeoutValue(error));
    }, timeoutMs);
  });

  try {
    return await Promise.race([promise, timeout]);
  } finally {
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}

export async function onceEventWithTimeout<T>(
  emitter: Pick<EventEmitter, 'once' | 'off'>,
  eventName: string,
  timeoutMs: number,
  timeoutValue: TimeoutValue<T>,
): Promise<T> {
  let timer: NodeJS.Timeout | null = null;
  let cleanup = (): void => {};
  const event = new Promise<T>((resolve) => {
    const onEvent = (value: T): void => {
      resolve(value);
    };
    cleanup = () => {
      emitter.off(eventName, onEvent);
    };
    emitter.once(eventName, onEvent);
  });
  const timeout = new Promise<T>((resolve) => {
    timer = setTimeout(() => {
      resolve(resolveTimeoutValue(timeoutValue));
    }, timeoutMs);
  });

  try {
    return await Promise.race([event, timeout]);
  } finally {
    cleanup();
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}

export async function anyEventWithTimeout<T>(
  emitter: Pick<EventEmitter, 'once' | 'off'>,
  events: Array<{ name: string; value: T }>,
  timeoutMs: number,
  timeoutValue: TimeoutValue<T>,
): Promise<T> {
  let timer: NodeJS.Timeout | null = null;
  const cleanups: Array<() => void> = [];
  const event = new Promise<T>((resolve) => {
    for (const candidate of events) {
      const onEvent = (): void => {
        resolve(candidate.value);
      };
      cleanups.push(() => {
        emitter.off(candidate.name, onEvent);
      });
      emitter.once(candidate.name, onEvent);
    }
  });
  const timeout = new Promise<T>((resolve) => {
    timer = setTimeout(() => {
      resolve(resolveTimeoutValue(timeoutValue));
    }, timeoutMs);
  });

  try {
    return await Promise.race([event, timeout]);
  } finally {
    for (const cleanup of cleanups) {
      cleanup();
    }
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}
