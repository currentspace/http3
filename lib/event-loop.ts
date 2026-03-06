/**
 * Worker-thread event loop adapters for native HTTP/3 server and client.
 * All UDP I/O, polling, timeouts, and QUIC/H3 processing run in Rust.
 */

import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

// ----- Native binding type definitions -----

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
    remoteAddr?: string;
    remotePort?: number;
    serverName?: string;
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

export interface NativeOutboundPacket {
  data: Buffer;
  addr: string;
}

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

interface NativeServerOptions {
  key: Buffer;
  cert: Buffer;
  ca?: Buffer;
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

interface NativeBinding {
  NativeWorkerServer: new (
    options: NativeServerOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeWorkerServerBinding;
  NativeWorkerClient: new (
    options: NativeClientOptions,
    callback: (err: Error | null, events: NativeEvent[]) => void,
  ) => NativeWorkerClientBinding;
  version(): string;
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

export { binding };

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

  constructor(worker: NativeWorkerServerBinding) {
    this.worker = worker;
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
    this.worker.shutdown();
    await Promise.resolve();
  }
}

// ----- Client Event Loop (worker mode) -----

export class ClientEventLoop {
  private readonly worker: NativeWorkerClientBinding;
  private closed = false;

  constructor(worker: NativeWorkerClientBinding) {
    this.worker = worker;
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
    const queued = this.worker.close(0, 'client close');
    if (queued) {
      // Give the worker a brief chance to flush CONNECTION_CLOSE packets
      // before forcing shutdown.
      await new Promise<void>((resolve) => { setTimeout(resolve, 20); });
    }
    this.worker.shutdown();
  }
}
