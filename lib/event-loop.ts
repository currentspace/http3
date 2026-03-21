/**
 * Worker-thread event loop adapters for native HTTP/3 server and client.
 * All UDP I/O, polling, timeouts, and QUIC/H3 processing run in Rust.
 */

import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

/** Sentinel event emitted by each Rust worker thread before exit. */
export const EVENT_SHUTDOWN_COMPLETE = 15;

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
  };
  metrics?: {
    packetsIn: number;
    packetsOut: number;
    bytesIn: number;
    bytesOut: number;
    handshakeTimeMs: number;
    rttMs: number;
    cwnd: number;
  };
}

/** @internal */
export interface NativeOutboundPacket {
  data: Buffer;
  addr: string;
}

/**
 * N-API binding for the worker-mode HTTP/3 server.
 * @internal
 */
export interface NativeWorkerServerBinding {
  listen(port: number, host: string): { address: string; family: string; port: number };
  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): boolean;
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): boolean;
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
  };
  getRemoteSettings(connHandle: number): Array<{ id: number; value: number }>;
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  localAddress(): { address: string; family: string; port: number };
  shutdown(): void;
}

/**
 * N-API binding for the worker-mode HTTP/3 client.
 * @internal
 */
export interface NativeWorkerClientBinding {
  connect(serverAddr: string, serverName: string): { address: string; family: string; port: number };
  sendRequest(headers: Array<{ name: string; value: string }>, fin: boolean): number;
  streamSend(streamId: number, data: Buffer, fin: boolean): boolean;
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
  };
  getRemoteSettings(): Array<{ id: number; value: number }>;
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode: number, reason: string): boolean;
  localAddress(): { address: string; family: string; port: number };
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
  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): boolean;
  streamClose(connHandle: number, streamId: number, errorCode: number): boolean;
  closeSession(connHandle: number, errorCode: number, reason: string): boolean;
  sendDatagram(connHandle: number, data: Buffer): boolean;
  getSessionMetrics(connHandle: number): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  };
  pingSession(connHandle: number): boolean;
  getQlogPath(connHandle: number): string | null;
  localAddress(): { address: string; family: string; port: number };
  shutdown(): void;
}

/**
 * N-API binding for the raw QUIC client (no HTTP/3 framing).
 * @internal
 */
export interface NativeQuicClientBinding {
  connect(serverAddr: string, serverName: string): { address: string; family: string; port: number };
  streamSend(streamId: number, data: Buffer, fin: boolean): boolean;
  streamClose(streamId: number, errorCode: number): boolean;
  sendDatagram(data: Buffer): boolean;
  getSessionMetrics(): {
    packetsIn: number; packetsOut: number;
    bytesIn: number; bytesOut: number;
    handshakeTimeMs: number; rttMs: number; cwnd: number;
  };
  ping(): boolean;
  getQlogPath(): string | null;
  close(errorCode: number, reason: string): boolean;
  localAddress(): { address: string; family: string; port: number };
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
  txBuffersRecycled: number;
}

interface NativeBinding {
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

// eslint-disable-next-line @typescript-eslint/no-require-imports, @typescript-eslint/no-unsafe-assignment
const binding: NativeBinding = require(findBinding());

/** @internal Loaded native N-API binding. */
export { binding };

/** Callback signature for receiving batches of native events. */
export type EventCallback = (events: NativeEvent[]) => void;

// ----- Common interface for server command adapters -----

/** Common interface for worker-based server command adapters. */
export interface ServerEventLoopLike {
  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void;
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
    if (this._shutdownResolve) {
      const resolve = this._shutdownResolve;
      this._shutdownResolve = null;
      resolve();
    }
  }

  sendResponseHeaders(connHandle: number, streamId: number, headers: Array<{ name: string; value: string }>, fin: boolean): void {
    this.worker.sendResponseHeaders(connHandle, streamId, headers, fin);
  }

  streamSend(connHandle: number, streamId: number, data: Buffer, fin: boolean): number {
    // Command is queued to worker thread via unbounded channel — always succeeds.
    // The worker will generate drain events if flow-control blocks at the quiche level.
    this.worker.streamSend(connHandle, streamId, data, fin);
    // When sending FIN with empty data (stream._final), return 1 so the
    // caller knows the command was accepted (0 would look like a block).
    return Math.max(data.length, fin ? 1 : 0);
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
    // Wire up sentinel promise BEFORE shutdown so no race.
    const settled = new Promise<void>((resolve) => {
      this._shutdownResolve = resolve;
    });
    this.worker.shutdown(); // sync N-API: joins all Rust worker threads
    // All TSFN callbacks (including sentinels) are now queued in libuv.
    await settled;
  }
}

// ----- Client Event Loop (worker mode) -----

/**
 * Event loop adapter for the HTTP/3 client in worker-thread mode.
 * Mirrors {@link WorkerEventLoop} but for the client side.
 */
export class ClientEventLoop {
  private readonly worker: NativeWorkerClientBinding;
  private closed = false;
  private _shutdownResolve: (() => void) | null = null;

  constructor(worker: NativeWorkerClientBinding) {
    this.worker = worker;
  }

  /** @internal */
  _onShutdownSentinel(): void {
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
    this.worker.streamSend(streamId, data, fin);
    return Math.max(data.length, fin ? 1 : 0);
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

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.worker.close(0, 'client close');
    const settled = new Promise<void>((resolve) => {
      this._shutdownResolve = resolve;
    });
    this.worker.shutdown();
    await settled;
  }
}
