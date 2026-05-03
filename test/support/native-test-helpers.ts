/**
 * Shared utilities for FFI boundary tests that bypass the TS wrapper layer.
 *
 * These helpers load the raw NAPI binding directly and provide event
 * collection, QUIC/H3 pair creation, and clean shutdown for tests that
 * exercise the native Rust ↔ Node.js boundary without the TypeScript
 * abstractions from lib/.
 */

import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { generateTestCerts, generateMutualTlsTestCerts } from './generate-certs.js';

// Re-export cert generators so FFI tests only need one import.
export { generateTestCerts, generateMutualTlsTestCerts };

// ---- Native event type constants ----

export const EVENT_NEW_SESSION = 1;
export const EVENT_NEW_STREAM = 2;
export const EVENT_HEADERS = 3;
export const EVENT_DATA = 4;
export const EVENT_FINISHED = 5;
export const EVENT_RESET = 6;
export const EVENT_SESSION_CLOSE = 7;
export const EVENT_DRAIN = 8;
export const EVENT_GOAWAY = 9;
export const EVENT_ERROR = 10;
export const EVENT_HANDSHAKE_COMPLETE = 11;
export const EVENT_SESSION_TICKET = 12;
export const EVENT_METRICS = 13;
export const EVENT_DATAGRAM = 14;
export const EVENT_SHUTDOWN_COMPLETE = 15;

export function isH3BodyDataEvent(evt: any): boolean {
  return (evt?.eventType === EVENT_DATA || evt?.eventType === EVENT_HEADERS) && !!evt.data;
}

export function h3BodyDataEvents(events: any[]): any[] {
  return events.filter(isH3BodyDataEvent);
}

export function concatH3BodyData(events: any[]): Buffer {
  return Buffer.concat(h3BodyDataEvents(events).map((evt: any) => Buffer.from(evt.data)));
}

export function waitForH3BodyData(
  source: any[] | { allEvents: any[] },
  timeoutMs = 5000,
): Promise<any> {
  const events = Array.isArray(source) ? source : source.allEvents;
  const existing = events.find(isH3BodyDataEvent);
  if (existing) return Promise.resolve(existing);

  return new Promise<any>((resolve, reject) => {
    const startedAt = Date.now();
    const check = (): void => {
      const evt = events.find(isH3BodyDataEvent);
      if (evt) {
        resolve(evt);
        return;
      }
      if (Date.now() - startedAt > timeoutMs) {
        reject(new Error(`Timed out waiting for H3 body data after ${timeoutMs}ms`));
        return;
      }
      setTimeout(check, 10);
    };
    check();
  });
}

// ---- Binding loader ----

/**
 * Load the raw native NAPI binding, bypassing the TypeScript wrapper layer.
 *
 * This uses the same `index.js` that NAPI-RS generates at the package root,
 * mirroring the strategy in `lib/event-loop.ts`.
 */
export function loadBinding(): any {
  // Walk up from __dirname (which may be dist-test/test/support/ at runtime)
  // until we find the package root containing both package.json and index.js.
  // This mirrors the strategy used in lib/event-loop.ts.
  const searched: string[] = [];
  let dir = __dirname;
  for (let i = 0; i < 6; i++) {
    const candidate = join(dir, 'index.js');
    searched.push(candidate);
    if (existsSync(candidate) && existsSync(join(dir, 'package.json'))) {
      const require_ = createRequire(join(dir, 'package.json'));
      return require_(candidate);
    }
    dir = resolve(dir, '..');
  }
  throw new Error(
    `Cannot find native binding index.js. Searched:\n${searched.map((p) => `  - ${p}`).join('\n')}`,
  );
}

// ---- Event collector ----

export interface EventCollector {
  /** The raw callback to pass into native constructors. */
  callback: (err: Error | null, events: any[]) => void;
  /** All events received so far. */
  allEvents: any[];
  /** Wait until an event with the given `eventType` appears. */
  waitForEvent(eventType: number, timeoutMs?: number): Promise<any>;
  /** Wait until any one of the given event types appears. */
  waitForAnyEvent(eventTypes: number[], timeoutMs?: number): Promise<any>;
  /** Wait until N events of the given `eventType` have been collected. */
  waitForNEvents(eventType: number, count: number, timeoutMs?: number): Promise<any[]>;
  /** Wait until a SHUTDOWN_COMPLETE (15) event is observed. */
  waitForShutdown(timeoutMs?: number): Promise<void>;
  /** Clear accumulated events. */
  reset(): void;
}

/**
 * Create an EventCollector that accumulates native events delivered via the
 * TSFN callback and provides promise-based waiters.
 */
export function createEventCollector(): EventCollector {
  const allEvents: any[] = [];
  const waiters: Array<{ eventType: number; resolve: (evt: any) => void }> = [];

  const callback = (_err: Error | null, events: any[]): void => {
    for (const evt of events) {
      allEvents.push(evt);
      for (let i = waiters.length - 1; i >= 0; i--) {
        const waiter = waiters[i];
        if (!waiter) continue;
        if (waiter.eventType === evt.eventType) {
          waiters.splice(i, 1);
          waiter.resolve(evt);
        }
      }
    }
  };

  function waitForEvent(eventType: number, timeoutMs = 5000): Promise<any> {
    // Check already-collected events first.
    const existing = allEvents.find((e) => e.eventType === eventType);
    if (existing) return Promise.resolve(existing);

    return new Promise<any>((resolve, reject) => {
      const timer = setTimeout(() => {
        const idx = waiters.findIndex((w) => w.resolve === resolve);
        if (idx !== -1) waiters.splice(idx, 1);
        reject(new Error(`Timed out waiting for eventType=${eventType} after ${timeoutMs}ms`));
      }, timeoutMs);

      waiters.push({
        eventType,
        resolve: (evt: any) => {
          clearTimeout(timer);
          resolve(evt);
        },
      });
    });
  }

  function waitForAnyEvent(eventTypes: number[], timeoutMs = 5000): Promise<any> {
    const wanted = new Set(eventTypes);
    const existing = allEvents.find((e) => wanted.has(e.eventType));
    if (existing) return Promise.resolve(existing);

    return new Promise<any>((resolve, reject) => {
      let settled = false;
      const group = Symbol('waitForAnyEvent');
      const timer = setTimeout(() => {
        settled = true;
        for (let i = waiters.length - 1; i >= 0; i--) {
          if ((waiters[i] as any).__anyEventGroup === group) waiters.splice(i, 1);
        }
        reject(new Error(`Timed out waiting for eventTypes=${eventTypes.join(',')} after ${timeoutMs}ms`));
      }, timeoutMs);

      const complete = (evt: any): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        for (let i = waiters.length - 1; i >= 0; i--) {
          if ((waiters[i] as any).__anyEventGroup === group) waiters.splice(i, 1);
        }
        resolve(evt);
      };

      for (const eventType of eventTypes) {
        waiters.push({ eventType, resolve: complete, __anyEventGroup: group } as any);
      }
    });
  }

  function waitForShutdown(timeoutMs = 5000): Promise<void> {
    return waitForEvent(EVENT_SHUTDOWN_COMPLETE, timeoutMs).then(() => {});
  }

  function reset(): void {
    allEvents.length = 0;
  }

  function waitForNEvents(eventType: number, count: number, timeoutMs = 10000): Promise<any[]> {
    return new Promise<any[]>((resolve, reject) => {
      const collected: any[] = [];
      // Check already-collected events first.
      for (const e of allEvents) {
        if (e.eventType === eventType) collected.push(e);
      }
      if (collected.length >= count) {
        resolve(collected.slice(0, count));
        return;
      }

      const timer = setTimeout(() => {
        // Remove our waiter entries
        for (let i = waiters.length - 1; i >= 0; i--) {
          if ((waiters[i] as any).__nEventsGroup === group) waiters.splice(i, 1);
        }
        reject(new Error(`Timed out waiting for ${count} events of type ${eventType} (got ${collected.length}) after ${timeoutMs}ms`));
      }, timeoutMs);

      const group = Symbol('waitForNEvents');

      const check = (evt: any): void => {
        collected.push(evt);
        if (collected.length >= count) {
          clearTimeout(timer);
          // Remove remaining waiters from this group
          for (let i = waiters.length - 1; i >= 0; i--) {
            if ((waiters[i] as any).__nEventsGroup === group) waiters.splice(i, 1);
          }
          resolve(collected.slice(0, count));
        }
      };

      // Register enough waiters to collect the remaining events
      const remaining = count - collected.length;
      for (let i = 0; i < remaining; i++) {
        const waiter = {
          eventType,
          resolve: check,
          __nEventsGroup: group,
        };
        waiters.push(waiter as any);
      }
    });
  }

  return { callback, allEvents, waitForEvent, waitForAnyEvent, waitForNEvents, waitForShutdown, reset };
}

// ---- QUIC pair ----

export interface QuicPair {
  server: any;
  client: any;
  serverEvents: EventCollector;
  clientEvents: EventCollector;
  serverAddr: { address: string; port: number };
  cleanup(): Promise<void>;
}

/**
 * Create a raw QUIC server+client pair using native bindings directly.
 * The server listens on localhost with an ephemeral port; the client
 * connects immediately. Both use self-signed test certs.
 */
export async function createQuicPair(opts?: { enableDatagrams?: boolean }): Promise<QuicPair> {
  const binding = loadBinding();
  const certs = generateTestCerts();
  const serverEvents = createEventCollector();
  const clientEvents = createEventCollector();

  const server = new binding.NativeQuicServer(
    {
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      enableDatagrams: opts?.enableDatagrams ?? false,
      runtimeMode: 'portable',
    },
    serverEvents.callback,
  );

  const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

  const client = new binding.NativeQuicClient(
    {
      rejectUnauthorized: false,
      enableDatagrams: opts?.enableDatagrams ?? false,
      runtimeMode: 'portable',
    },
    clientEvents.callback,
  );

  client.connect(`127.0.0.1:${addr.port}`, 'localhost');

  // Wait for the handshake to complete on the client side.
  await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE);

  return {
    server,
    client,
    serverEvents,
    clientEvents,
    serverAddr: addr,
    async cleanup() {
      try { client.close(0, 'test cleanup'); } catch { /* already closed */ }
      try { client.requestShutdown(); } catch { /* already shut down */ }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { client.joinWorker(); } catch { /* already joined */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    },
  };
}

// ---- H3 pair ----

export interface H3Pair {
  server: any;
  client: any;
  serverEvents: EventCollector;
  clientEvents: EventCollector;
  serverAddr: { address: string; port: number };
  cleanup(): Promise<void>;
}

/**
 * Create a raw HTTP/3 (worker-mode) server+client pair using native bindings.
 * The server listens on localhost with an ephemeral port; the client connects
 * immediately. Both use self-signed test certs.
 */
export async function createH3Pair(opts?: {
  enableDatagrams?: boolean;
  maxIdleTimeoutMs?: number;
  initialMaxStreamDataBidiLocal?: number;
}): Promise<H3Pair> {
  const binding = loadBinding();
  const certs = generateTestCerts();
  const serverEvents = createEventCollector();
  const clientEvents = createEventCollector();

  const server = new binding.NativeWorkerServer(
    {
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      runtimeMode: 'portable',
      enableDatagrams: opts?.enableDatagrams ?? false,
      ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
      ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
    },
    serverEvents.callback,
  );

  const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

  const client = new binding.NativeWorkerClient(
    {
      rejectUnauthorized: false,
      runtimeMode: 'portable',
      enableDatagrams: opts?.enableDatagrams ?? false,
      ...(opts?.maxIdleTimeoutMs != null && { maxIdleTimeoutMs: opts.maxIdleTimeoutMs }),
      ...(opts?.initialMaxStreamDataBidiLocal != null && { initialMaxStreamDataBidiLocal: opts.initialMaxStreamDataBidiLocal }),
    },
    clientEvents.callback,
  );

  client.connect(`127.0.0.1:${addr.port}`, 'localhost');

  // Wait for the handshake to complete on the client side.
  await clientEvents.waitForEvent(EVENT_HANDSHAKE_COMPLETE);

  return {
    server,
    client,
    serverEvents,
    clientEvents,
    serverAddr: addr,
    async cleanup() {
      try { client.close(0, 'test cleanup'); } catch { /* already closed */ }
      try { client.requestShutdown(); } catch { /* already shut down */ }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { client.joinWorker(); } catch { /* already joined */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    },
  };
}

// ---- Drain + shutdown helper ----

/**
 * Gracefully drain and shut down a native server or client instance.
 * Uses requestShutdown + joinWorker instead of blocking shutdown() to
 * avoid TSFN callback delivery issues with the shutdown sentinel.
 *
 * Both calls are synchronous NAPI — the try/catch guards against the
 * native handle already being consumed.
 */
export function drainAndShutdown(instance: any, _collector: EventCollector): void {
  try { instance.requestShutdown(); } catch { /* already shut down */ }
  try { instance.joinWorker(); } catch { /* already joined */ }
}

// ---- Memory snapshot helper ----

export interface MemorySnapshot {
  rss: number;
  heapUsed: number;
  heapTotal: number;
}

export type MemoryDriftSnapshot = Pick<MemorySnapshot, 'rss' | 'heapUsed'>;

export const MEMORY_DRIFT_LIMIT_BYTES = 50 * 1024 * 1024;

/**
 * Capture a point-in-time memory snapshot for leak detection in long-haul tests.
 */
export function snapshotMemory(): MemorySnapshot {
  const m = process.memoryUsage();
  return { rss: m.rss, heapUsed: m.heapUsed, heapTotal: m.heapTotal };
}

/**
 * Assert that post-warmup memory drift stays bounded. Long-haul tests use a
 * post-warmup baseline so startup and native allocator ramp do not look like
 * steady-state leaks.
 */
export function assertMemoryDriftWithinLimit(
  label: string,
  baseline: MemoryDriftSnapshot,
  final: MemoryDriftSnapshot,
  limitBytes = MEMORY_DRIFT_LIMIT_BYTES,
): void {
  const limitMB = (limitBytes / 1024 / 1024).toFixed(1);
  const rssDrift = final.rss - baseline.rss;
  const heapDrift = final.heapUsed - baseline.heapUsed;
  if (rssDrift > limitBytes) {
    throw new Error(
      `${label} RSS drift ${(rssDrift / 1024 / 1024).toFixed(1)}MB exceeds ${limitMB}MB ` +
      `(${(baseline.rss / 1024 / 1024).toFixed(1)}MB -> ${(final.rss / 1024 / 1024).toFixed(1)}MB)`,
    );
  }
  if (heapDrift > limitBytes) {
    throw new Error(
      `${label} heap drift ${(heapDrift / 1024 / 1024).toFixed(1)}MB exceeds ${limitMB}MB ` +
      `(${(baseline.heapUsed / 1024 / 1024).toFixed(1)}MB -> ${(final.heapUsed / 1024 / 1024).toFixed(1)}MB)`,
    );
  }
}
