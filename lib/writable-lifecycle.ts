type WritableLike = {
  once(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  off?(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  removeListener?(event: 'drain' | 'close' | 'error', listener: (...args: unknown[]) => void): unknown;
  closed?: boolean;
  destroyed?: boolean;
  writableEnded?: boolean;
};

export function isWritableClosed(writable: WritableLike): boolean {
  return Boolean(writable.closed || writable.destroyed || writable.writableEnded);
}

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

export async function waitForDrainOrAbort(writable: WritableLike, signal: AbortSignal): Promise<void> {
  if (signal.aborted || isWritableClosed(writable)) {
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
