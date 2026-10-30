export interface ShutdownOptions {
  /** Deadline for transport drain and application cleanup. Default: 30 seconds. */
  timeoutMs?: number;
  signals?: NodeJS.Signals[];
  onClose?: (signal?: NodeJS.Signals) => void | Promise<void>;
  /** Exit after a signal-triggered shutdown, including lingering framework watchers. Default: true. */
  exit?: boolean;
}
export interface ShutdownController {
  /** Idempotent manual shutdown. Never exits the process. */
  close: (signal?: NodeJS.Signals) => Promise<void>;
  /** Remove only this controller's signal listeners. */
  dispose: () => void;
}

/** @internal Shared process ownership for the Node facade and framework helpers. */
export function installShutdownHandlers(closeTransport: () => Promise<void>, options: ShutdownOptions): ShutdownController {
  const timeoutMs = options.timeoutMs ?? 30_000;
  if (!Number.isFinite(timeoutMs) || timeoutMs < 0 || timeoutMs > 2147483647) throw new RangeError('timeoutMs must be between 0 and 2147483647');
  const signals = [...new Set<NodeJS.Signals>(options.signals ?? ['SIGINT', 'SIGTERM'])];
  const listeners = new Map<NodeJS.Signals, () => void>();
  let closing: Promise<void> | undefined;
  const dispose = (): void => {
    for (const [signal, handler] of listeners) process.off(signal, handler);
    listeners.clear();
  };
  const close = async (signal?: NodeJS.Signals): Promise<void> => {
    if (closing) return closing;
    dispose();
    closing = (async () => {
      let timer: NodeJS.Timeout | undefined;
      try {
        await Promise.race([
          (async () => { await closeTransport(); await options.onClose?.(signal); })(),
          new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new Error(`Shutdown exceeded ${timeoutMs}ms`)), timeoutMs);
          }),
        ]);
      } finally { clearTimeout(timer); }
    })();
    return closing;
  };
  try {
    for (const signal of signals) {
      const handler = (): void => {
        void close(signal).then(() => {
          if (options.exit !== false) process.exit(0);
        }, (error: unknown) => {
          console.error(error);
          if (options.exit !== false) process.exit(1);
          else process.exitCode = 1;
        });
      };
      process.on(signal, handler);
      listeners.set(signal, handler);
    }
  } catch (error) { dispose(); throw error; }
  return { close, dispose };
}
