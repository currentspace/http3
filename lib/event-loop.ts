/**
 * Worker-thread event loop adapters for native HTTP/3 server and client.
 * All UDP I/O, polling, timeouts, and QUIC/H3 processing run in Rust.
 */

import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

/** Sentinel event emitted by each Rust worker thread before exit. */
export const EVENT_SHUTDOWN_COMPLETE = 15;

/**
 * Maximum time `close()` will wait for the worker's SHUTDOWN_COMPLETE
 * sentinel before falling through to `joinWorker()`. A wedged worker
 * shouldn't hang the host process indefinitely.
 */
export const SHUTDOWN_TIMEOUT_MS = 5000;

/**
 * Await worker shutdown without leaving the fallback timer referenced after
 * the sentinel wins. A referenced timer here keeps `node --test` alive even
 * though every stream/session has closed correctly.
 * @internal
 */
export async function waitForShutdownOrTimeout(sentinel: Promise<void>): Promise<void> {
  let timeout: NodeJS.Timeout | undefined;
  const timer = new Promise<void>((resolve) => {
    timeout = setTimeout(resolve, SHUTDOWN_TIMEOUT_MS);
  });

  try {
    await Promise.race([sentinel, timer]);
  } finally {
    if (timeout !== undefined) {
      clearTimeout(timeout);
    }
  }
}

// ----- Native binding type definitions -----

/**
 * A single event delivered from the native Rust worker thread.
 * @internal
 */
export interface NativeEvent {
  eventType: number;
  connHandle: number;
  streamId: number;
  headers?: Array<{ name: string; value: string }>;
  data?: Buffer;
  fin?: boolean;
  meta?: {
    errorCode?: number;
    errorReason?: string;
    errorCategory?: string;
    remoteAddr?: string;
    remotePort?: number;
    serverName?: string;
    reasonCode?: string;
    runtimeDriver?: string;
    runtimeMode?: string;
    requestedRuntimeMode?: string;
    fallbackOccurred?: boolean;
    errno?: number;
    syscall?: string;
    peerCertificatePresented?: boolean;
    peerCertificateChain?: Buffer[];
    durationMs?: number;
  };
  metrics?: {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
}

/** @internal */
export interface NativeOutboundPacket {
  data: Buffer;
  addr: string;
}

/** Native stream-send status: write command accepted into local native queue. */
export const STREAM_SEND_ACCEPTED = 0;
/** Native stream-send status: local native command/backpressure window is closed. */
export const STREAM_SEND_BACKPRESSURE = 1;

export type NativeStreamSendStatus =
  | typeof STREAM_SEND_ACCEPTED
  | typeof STREAM_SEND_BACKPRESSURE;

/**
 * Structured result from the native JS -> worker write-admission boundary.
 *
 * `written` is local admission units, not peer ACKed bytes. For FIN-only
 * writes it is `1` so `_final()` can distinguish success from backpressure.
 * @internal
 */
export interface NativeStreamSendOutcome {
  status: NativeStreamSendStatus;
  written: number;
}

/** @internal Convert native admission outcome to the stream-layer contract. */
export function streamSendOutcomeBytes(outcome: NativeStreamSendOutcome): number {
  return outcome.status === STREAM_SEND_ACCEPTED ? outcome.written : 0;
}

/**
 * N-API binding for the worker-mode HTTP/3 server.
 * @internal
 */
export interface NativeWorkerServerBinding {
  listen(port: number, host: string): { address: string; family: string; port: number };
  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): boolean;
  sendResponse(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, data: Buffer, fin: boolean): boolean;
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): NativeStreamSendOutcome;
  streamClose(connHandle: number, streamId: number, errorCode: number): boolean;
  sendTrailers(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>): boolean;
  closeSession(connHandle: number, errorCode: number, reason: string): boolean;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
  getRemoteSettings(connHandle: number): Array<{ id: number; value: number }>;
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  localAddress(): { address: string; family: string; port: number };
  /** Audit #14: release credit on the native outstanding-events gauge. */
  ackEventBatch(count: number): void;
  requestShutdown(): boolean;
  joinWorker(): void;
  shutdown(): void;
}

/**
 * N-API binding for the worker-mode HTTP/3 client.
 * @internal
 */
export interface NativeWorkerClientBinding {
  connect(serverAddr: string, serverName: string): { address: string; family: string; port: number };
  sendRequest(headers: Array<{ name: string; value: string }>, fin: boolean): number;
  streamSend(streamId: number, data: Buffer, fin: boolean): NativeStreamSendOutcome;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
  getRemoteSettings(): Array<{ id: number; value: number }>;
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode: number, reason: string): boolean;
  localAddress(): { address: string; family: string; port: number };
  /** Audit #14: release credit on the native outstanding-events gauge. */
  ackEventBatch(count: number): void;
  requestShutdown(): boolean;
  joinWorker(): void;
  shutdown(): void;
}

/**
 * Options passed to the native QUIC server constructor.
 * @internal
 */
export interface NativeQuicServerOptions {
  key: Buffer;
  cert: Buffer;
  ca?: Buffer;
  clientAuth?: 'none' | 'request' | 'require';
  alpn?: string[];
  runtimeMode?: 'fast' | 'portable';
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  disableActiveMigration?: boolean;
  enableDatagrams?: boolean;
  maxConnections?: number;
  disableRetry?: boolean;
  qlogDir?: string;
  qlogLevel?: string;
  keylog?: boolean;
}

/**
 * Options passed to the native QUIC client constructor.
 * @internal
 */
export interface NativeQuicClientOptions {
  ca?: Buffer;
  cert?: Buffer;
  key?: Buffer;
  rejectUnauthorized?: boolean;
  alpn?: string[];
  runtimeMode?: 'fast' | 'portable';
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Buffer;
  allow0Rtt?: boolean;
  enableDatagrams?: boolean;
  keylog?: boolean;
  qlogDir?: string;
  qlogLevel?: string;
}

/**
 * N-API binding for the raw QUIC server (no HTTP/3 framing).
 * @internal
 */
export interface NativeQuicServerBinding {
  listen(port: number, host: string): { address: string; family: string; port: number };
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): NativeStreamSendOutcome;
  streamClose(connHandle: number, streamId: number, errorCode: number): boolean;
  closeSession(connHandle: number, errorCode: number, reason: string): boolean;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number; datagramQueueDepth: number;
  };
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  localAddress(): { address: string; family: string; port: number };
  /** Audit #14: release credit on the native outstanding-events gauge. */
  ackEventBatch(count: number): void;
  requestShutdown(): boolean;
  joinWorker(): void;
  shutdown(): void;
}

/**
 * N-API binding for the raw QUIC client (no HTTP/3 framing).
 * @internal
 */
export interface NativeQuicClientBinding {
  connect(serverAddr: string, serverName: string): { address: string; family: string; port: number };
  openStream(): number;
  streamSend(streamId: number, data: Buffer, fin: boolean): NativeStreamSendOutcome;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number; datagramQueueDepth: number;
  };
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode: number, reason: string): boolean;
  localAddress(): { address: string; family: string; port: number };
  /** Audit #14: release credit on the native outstanding-events gauge. */
  ackEventBatch(count: number): void;
  requestShutdown(): boolean;
  joinWorker(): void;
  shutdown(): void;
}

interface NativeServerOptions {
  key: Buffer;
  cert: Buffer;
  ca?: Buffer;
  runtimeMode?: 'fast' | 'portable';
  quicLb?: boolean;
  serverId?: Buffer;
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  disableActiveMigration?: boolean;
  enableDatagrams?: boolean;
  qpackMaxTableCapacity?: number;
  qpackBlockedStreams?: number;
  recvBatchSize?: number;
  sendBatchSize?: number;
  qlogDir?: string;
  qlogLevel?: string;
  sessionTicketKeys?: Buffer;
  maxConnections?: number;
  disableRetry?: boolean;
  reusePort?: boolean;
  keylog?: boolean;
}

interface NativeClientOptions {
  ca?: Buffer;
  rejectUnauthorized?: boolean;
  runtimeMode?: 'fast' | 'portable';
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Buffer;
  allow0Rtt?: boolean;
  enableDatagrams?: boolean;
  keylog?: boolean;
  qlogDir?: string;
  qlogLevel?: string;
}

export interface ReactorTelemetrySnapshot {
  driverSetupAttemptsTotal: number;
  driverSetupSuccessTotal: number;
  driverSetupFailureTotal: number;
  ioUringDriverSetupAttempts: number;
  ioUringDriverSetupSuccesses: number;
  ioUringDriverSetupFailures: number;
  pollDriverSetupAttempts: number;
  pollDriverSetupSuccesses: number;
  pollDriverSetupFailures: number;
  kqueueDriverSetupAttempts: number;
  kqueueDriverSetupSuccesses: number;
  kqueueDriverSetupFailures: number;
  workerThreadSpawnsTotal: number;
  workerThreadStopsTotal: number;
  workerLoopExitByCommandTotal: number;
  workerLoopExitByHandlerDoneTotal: number;
  workerLoopExitBySinkCloseTotal: number;
  workerLoopExitByRuntimeErrorTotal: number;
  shutdownCompleteEmittedTotal: number;
  eventBatchFlushesTotal: number;
  eventBatchAttemptedEventsTotal: number;
  eventBatchDeliveredEventsTotal: number;
  eventBatchDroppedEventsTotal: number;
  eventBatchSinkErrorsTotal: number;
  eventBatchMaxSizeHighWatermark: number;
  eventBatchAckedEventsTotal: number;
  eventBatchOutstanding: number;
  eventBatchOutstandingHighWatermark: number;
  eventBatchSelfHealedTotal: number;
  eventBatchRxPausesTotal: number;
  ecnRecvNotEctTotal: number;
  ecnRecvEct0Total: number;
  ecnRecvEct1Total: number;
  ecnRecvCeTotal: number;
  rawQuicServerWorkerSpawns: number;
  rawQuicClientDedicatedWorkerSpawns: number;
  rawQuicClientSharedWorkersCreated: number;
  rawQuicClientSharedWorkerReuses: number;
  h3ServerWorkerSpawns: number;
  h3ClientDedicatedWorkerSpawns: number;
  h3ClientSharedWorkersCreated: number;
  h3ClientSharedWorkerReuses: number;
  clientLocalPortReuseHits: number;
  rawQuicClientSessionsOpened: number;
  rawQuicClientSessionsClosed: number;
  rawQuicFinObservations: number;
  rawQuicFinishedEventEmits: number;
  rawQuicDrainEventEmits: number;
  rawQuicBlockedStreamHighWatermark: number;
  rawQuicClientPendingWriteHighWatermark: number;
  rawQuicClientReapsWithPendingWrites: number;
  rawQuicClientReapsWithBlockedStreams: number;
  rawQuicClientReapsWithKnownStreams: number;
  rawQuicClientCloseByPacket: number;
  rawQuicClientCloseByTimeout: number;
  rawQuicClientCloseByShutdown: number;
  rawQuicClientCloseByRelease: number;
  rawQuicServerSessionsOpened: number;
  rawQuicServerSessionsClosed: number;
  h3ClientSessionsOpened: number;
  h3ClientSessionsClosed: number;
  h3ServerSessionsOpened: number;
  h3ServerSessionsClosed: number;
  ioUringRxInFlightHighWatermark: number;
  ioUringTxInFlightHighWatermark: number;
  ioUringPendingTxHighWatermark: number;
  ioUringRetryableSendCompletions: number;
  ioUringSubmitCalls: number;
  ioUringSubmitWithArgsCalls: number;
  ioUringSubmittedSqesTotal: number;
  ioUringCompletionTotal: number;
  ioUringCompletionBatchHighWatermark: number;
  ioUringWakeCompletions: number;
  ioUringWakeWrites: number;
  ioUringTimeoutPolls: number;
  ioUringRxDatagramsTotal: number;
  ioUringTxDatagramsSubmittedTotal: number;
  ioUringTxDatagramsCompletedTotal: number;
  ioUringSqFullEvents: number;
  kqueueUnsentHighWatermark: number;
  kqueueWouldBlockSends: number;
  kqueueWriteWakeups: number;
  outboundIngressBufferReuses: number;
  outboundIngressBufferAllocations: number;
  outboundIngressCopiedBytes: number;
  outboundStreamJsAdmittedBytesTotal: number;
  outboundStreamJsAdmittedWritesTotal: number;
  outboundCommandQueuedBytes: number;
  outboundCommandQueuedBytesHighWatermark: number;
  outboundPendingWriteBytes: number;
  outboundPendingWriteBytesHighWatermark: number;
  outboundAdmissionBackpressureEventsTotal: number;
  outboundAdmissionReleasedUnitsTotal: number;
  outboundAdmissionWriteReadyEventsTotal: number;
  txBuffersRecycled: number;
}

export interface LifecycleTraceEvent {
  seq: number;
  timestampMs: number;
  component: string;
  action: string;
  driver?: string;
  batchSize?: number;
  pendingTx?: number;
  note?: string;
}

export interface LifecycleTraceSnapshot {
  enabled: boolean;
  capacity: number;
  droppedEvents: number;
  eventCount: number;
  events: LifecycleTraceEvent[];
}

/**
 * Shape of the loaded native N-API module. Exported (rather than kept
 * file-private) so callers that need to reference a specific constructor's
 * type (e.g. `NativeBinding['NativeQuicClient']`) can do so without also
 * importing the concrete {@link binding} value.
 * @internal
 */
export interface NativeBinding {
  NativeWorkerServer: new (
    options: NativeServerOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeWorkerServerBinding;
  NativeWorkerClient: new (
    options: NativeClientOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeWorkerClientBinding;
  NativeQuicServer: new (
    options: NativeQuicServerOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeQuicServerBinding;
  NativeQuicClient: new (
    options: NativeQuicClientOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeQuicClientBinding;
  version(): string;
  runtimeTelemetry(): ReactorTelemetrySnapshot;
  resetRuntimeTelemetry(): void;
  setLifecycleTraceEnabled(enabled: boolean): void;
  resetLifecycleTrace(): void;
  lifecycleTraceSnapshot(): LifecycleTraceSnapshot;
}

// ----- Binding loader -----

function findBinding(): string {
  const searched: string[] = [];
  let dir = __dirname;
  for (let i = 0; i < 5; i++) {
    const candidate = join(dir, 'index.js');
    searched.push(candidate);
    if (existsSync(candidate) && existsSync(join(dir, 'package.json'))) {
      return candidate;
    }
    dir = resolve(dir, '..');
  }
  throw new Error(
    `Cannot find native binding index.js. Searched:\n${searched.map(p => `  - ${p}`).join('\n')}`,
  );
}

let cachedBinding: NativeBinding | null = null;

/**
 * Resolve and cache the native N-API binding.
 *
 * The filesystem walk + `require()` only run on the first call, and the
 * result is memoized for every call after. Merely importing this module (or
 * anything that transitively imports it) therefore never touches the
 * filesystem or throws for a missing/unbuildable native binding — only
 * calling `getBinding()` does. This is the seam a future non-native (e.g.
 * wasm) runtime needs: it can import shared types/helpers from this module
 * without requiring a native build to exist, as long as it never calls this
 * function. Native behavior is unchanged: the first real call still resolves
 * (and throws on failure) exactly as the old eager `require` did.
 */
export function getBinding(): NativeBinding {
  if (!cachedBinding) {
    // eslint-disable-next-line @typescript-eslint/no-require-imports, @typescript-eslint/no-unsafe-assignment
    const loaded: NativeBinding = require(findBinding());
    cachedBinding = loaded;
    return loaded;
  }
  return cachedBinding;
}

/**
 * Compatibility alias for {@link getBinding}(). New lib/ code should call
 * {@link getBinding}() directly; this export exists so pre-existing direct
 * `binding.*` call sites (tests, benches) keep working unchanged: property
 * access transparently resolves through {@link getBinding}(), so the
 * underlying `require` still only happens lazily, on first property access
 * rather than at module-import time. Every proxied member is either a plain
 * stateless function or a constructor, so forwarding through `getBinding()`
 * per access is behaviorally identical to holding a direct reference.
 * @internal
 */
export const binding: NativeBinding = new Proxy({} as NativeBinding, {
  get(_target, prop) {
    // eslint-disable-next-line @typescript-eslint/no-unsafe-return
    return Reflect.get(getBinding(), prop);
  },
  // Forward writes to the real resolved binding too (rather than the
  // discarded empty proxy target) so tests that stub out a constructor
  // (e.g. `binding.NativeQuicClient = undefined`) to simulate an
  // incomplete prebuild keep observing the mutation everywhere, including
  // through `getBinding()`.
  set(_target, prop, value) {
    return Reflect.set(getBinding(), prop, value);
  },
});

/** Callback signature for receiving batches of native events. */
export type EventCallback = (events: NativeEvent[]) => void;

// ----- Common interface for server command adapters -----

/** Common interface for worker-based server command adapters. */
export interface ServerEventLoopLike {
  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void;
  sendResponse(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, data: Buffer, fin: boolean): void;
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number;
  streamClose(connHandle: number, streamId: number, errorCode: number): void;
  sendTrailers(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>): void;
  closeSession(connHandle: number, errorCode: number, reason: string): void;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
  getRemoteSettings(connHandle: number): Array<{ id: number; value: number }>;
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  close(): Promise<void>;
}

/** Event loop adapter for worker thread mode.
 * Commands are sent via crossbeam channel; events arrive via TSFN callback.
 * No Node dgram/timer work — Rust handles all UDP I/O.
 */
export class WorkerEventLoop implements ServerEventLoopLike {
  private readonly worker: NativeWorkerServerBinding;
  private closed = false;
  private _shutdownObserved = false;
  private _shutdownResolve: (() => void) | null = null;

  constructor(worker: NativeWorkerServerBinding) {
    this.worker = worker;
  }

  /**
   * Called by the TSFN callback when a SHUTDOWN_COMPLETE sentinel arrives.
   * Resolves the promise that close() is awaiting.
   * @internal
   */
  _onShutdownSentinel(): void {
    this._shutdownObserved = true;
    if (this._shutdownResolve) {
      const resolve = this._shutdownResolve;
      this._shutdownResolve = null;
      resolve();
    }
  }

  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void {
    this.worker.sendResponseHeaders(connHandle, streamId, headers, fin);
  }

  sendResponse(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, data: Buffer, fin: boolean): void {
    this.worker.sendResponse(connHandle, streamId, headers, data, fin);
  }

  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number {
    return streamSendOutcomeBytes(this.worker.streamSend(connHandle, streamId, data, fin));
  }

  streamClose(connHandle: number, streamId: number, errorCode: number): void {
    this.worker.streamClose(connHandle, streamId, errorCode);
  }

  sendTrailers(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>): void {
    this.worker.sendTrailers(connHandle, streamId, headers);
  }

  closeSession(connHandle: number, errorCode: number, reason: string): void {
    this.worker.closeSession(connHandle, errorCode, reason);
  }

  sendDatagram(connHandle: number, data: Buffer): boolean {
    return this.worker.sendDatagram(connHandle, data);
  }

  getSessionMetrics(connHandle: number): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  } {
    return this.worker.getSessionMetrics(connHandle);
  }

  getRemoteSettings(connHandle: number): Array<{ id: number; value: number }> {
    return this.worker.getRemoteSettings(connHandle);
  }

  pingSession(connHandle: number): boolean {
    return this.worker.pingSession(connHandle);
  }

  getQlogPath(connHandle: number): string | null {
    return this.worker.getQlogPath(connHandle);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;

    // Audit finding #33: await the SHUTDOWN_COMPLETE sentinel before
    // joinWorker so late TSFN events can't land on cleared maps.
    // Bounded by SHUTDOWN_TIMEOUT_MS so a wedged worker can't hang the
    // host process indefinitely.
    const sentinel = this._shutdownObserved
      ? Promise.resolve()
      : new Promise<void>((resolve) => {
          this._shutdownResolve = resolve;
        });

    this.worker.requestShutdown();

    await waitForShutdownOrTimeout(sentinel);
    this._shutdownResolve = null;

    this.worker.joinWorker();
  }
}

// ----- Client Event Loop (worker mode) -----

/**
 * Common interface for worker-based client command adapters.
 *
 * Extracted from {@link ClientEventLoop} so consumers (`Http3ClientSessionBase`,
 * `ClientHttp3Stream`) can be typed against a structural contract instead of
 * the concrete class. `ClientEventLoop` has private fields, so without this
 * interface TS's class typing would reject any structurally-equivalent
 * substitute (e.g. a future non-native event loop implementation).
 */
export interface ClientEventLoopLike {
  connect(serverAddr: string, serverName: string): Promise<void>;
  sendRequest(headers: Array<{ name: string; value: string }>, fin: boolean): number;
  streamSend(streamId: number, data: Buffer, fin: boolean): number;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  };
  getRemoteSettings(): Array<{ id: number; value: number }>;
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode?: number, reason?: string): Promise<void>;
}

/**
 * Event loop adapter for the HTTP/3 client in worker-thread mode.
 * Mirrors {@link WorkerEventLoop} but for the client side.
 */
export class ClientEventLoop implements ClientEventLoopLike {
  private readonly worker: NativeWorkerClientBinding;
  private closed = false;
  private _shutdownObserved = false;
  private _shutdownResolve: (() => void) | null = null;

  constructor(worker: NativeWorkerClientBinding) {
    this.worker = worker;
  }

  /** @internal */
  _onShutdownSentinel(): void {
    this._shutdownObserved = true;
    if (this._shutdownResolve) {
      const resolve = this._shutdownResolve;
      this._shutdownResolve = null;
      resolve();
    }
  }

  async connect(serverAddr: string, serverName: string): Promise<void> {
    this.worker.connect(serverAddr, serverName);
    await Promise.resolve();
  }

  sendRequest(headers: Array<{ name: string; value: string }>, fin: boolean): number {
    return this.worker.sendRequest(headers, fin);
  }

  streamSend(streamId: number, data: Buffer, fin: boolean): number {
    return streamSendOutcomeBytes(this.worker.streamSend(streamId, data, fin));
  }

  streamClose(streamId: number, errorCode: number): boolean {
    return this.worker.streamClose(streamId, errorCode);
  }

  sendDatagram(data: Buffer): boolean {
    return this.worker.sendDatagram(data);
  }

  getSessionMetrics(): {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
    datagramQueueDepth: number;
  } {
    return this.worker.getSessionMetrics();
  }

  getRemoteSettings(): Array<{ id: number; value: number }> {
    return this.worker.getRemoteSettings();
  }

  ping(): boolean {
    return this.worker.ping();
  }

  getQlogPath(): string | null {
    return this.worker.getQlogPath();
  }

  async close(errorCode = 0, reason = 'client close'): Promise<void> {
    if (this.closed) return;
    this.closed = true;

    const sentinel = this._shutdownObserved
      ? Promise.resolve()
      : new Promise<void>((resolve) => {
          this._shutdownResolve = resolve;
        });

    this.worker.close(errorCode, reason);
    this.worker.requestShutdown();

    await waitForShutdownOrTimeout(sentinel);
    this._shutdownResolve = null;

    this.worker.joinWorker();
  }
}
