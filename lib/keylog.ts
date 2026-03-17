import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { randomUUID } from 'node:crypto';

/** @internal */
type KeylogListener = (line: Buffer) => void;

interface TailState {
  listeners: Set<KeylogListener>;
  offset: number;
  partial: string;
  timer: NodeJS.Timeout;
  reading: boolean;
}

const tails = new Map<string, TailState>();

function emitLines(state: TailState, chunk: string): void {
  const text = `${state.partial}${chunk}`;
  const lines = text.split(/\r?\n/);
  state.partial = lines.pop() ?? '';
  for (const line of lines) {
    if (line.length === 0) continue;
    const payload = Buffer.from(`${line}\n`);
    for (const listener of state.listeners) {
      listener(payload);
    }
  }
}

function pollFile(path: string, state: TailState): void {
  if (state.reading) return;
  state.reading = true;
  try {
    const buf = readFileSync(path);
    if (buf.length <= state.offset) return;
    const delta = buf.subarray(state.offset);
    state.offset = buf.length;
    emitLines(state, delta.toString('utf8'));
  } catch {
    // File may not exist yet; keep polling.
  } finally {
    state.reading = false;
  }
}

/**
 * Resolve and prepare the SSLKEYLOGFILE path for TLS key logging.
 * @internal
 */
export function prepareKeylogFile(value: boolean | string | undefined): string | null {
  if (!value) return null;

  const filePath = typeof value === 'string'
    ? resolve(value)
    : resolve(process.env.SSLKEYLOGFILE ?? `${tmpdir()}/http3-keylog-${randomUUID()}.log`);

  mkdirSync(dirname(filePath), { recursive: true });
  writeFileSync(filePath, '', { flag: 'a' });
  process.env.SSLKEYLOGFILE = filePath;
  return filePath;
}

/**
 * Subscribe to new lines appended to an SSLKEYLOGFILE via polling.
 * Returns an unsubscribe function.
 * @internal
 */
export function subscribeKeylog(path: string, listener: KeylogListener): () => void {
  const resolved = resolve(path);
  let state = tails.get(resolved);
  if (!state) {
    state = {
      listeners: new Set(),
      offset: 0,
      partial: '',
      timer: setInterval(() => {
        const current = tails.get(resolved);
        if (current) pollFile(resolved, current);
      }, 100),
      reading: false,
    };
    state.timer.unref();
    tails.set(resolved, state);
  }
  state.listeners.add(listener);
  // Try to emit any existing lines right away for sessions created mid-flight.
  pollFile(resolved, state);

  return (): void => {
    const tail = tails.get(resolved);
    if (!tail) return;
    tail.listeners.delete(listener);
    if (tail.listeners.size === 0) {
      clearInterval(tail.timer);
      tails.delete(resolved);
    }
  };
}
