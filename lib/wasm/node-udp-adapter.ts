/**
 * Node `node:dgram` implementation of {@link DatagramTransport}
 * (docs/WASM_CLIENT_PLAN.md §6.5). This is the **only** file under
 * `lib/wasm/` that imports `node:dgram` (enforced by the `lib/wasm/**`
 * ESLint zone in `eslint.config.mjs`) — a future workerd adapter implements
 * the same `DatagramTransport` interface over whatever ships from
 * `cloudflare/workerd#4463`, as a second file, not a rewrite of this one.
 */

import dgram from 'node:dgram';
import { isIP } from 'node:net';
import type { DatagramTransport, DatagramTransportAddress } from './datagram-transport.js';

export interface ConnectNodeUdpOptions {
  /** UDP socket receive buffer size, in bytes. Default: 4 MiB. */
  recvBufferSize?: number;
}

const DEFAULT_RECV_BUFFER_SIZE = 4 * 1024 * 1024;

/**
 * Open a connected Node UDP socket to `(host, port)`. "Connected" here is
 * the UDP sense (RFC 8085): the kernel filters `recv` to datagrams from
 * this one peer and `send`/`write` no longer need a destination — matching
 * `DatagramTransport`'s single-fixed-peer contract.
 */
export async function connectNodeUdp(host: string, port: number, opts: ConnectNodeUdpOptions = {}): Promise<DatagramTransport> {
  const type = isIP(host) === 6 ? 'udp6' : 'udp4';
  const socket = dgram.createSocket({
    type,
    recvBufferSize: opts.recvBufferSize ?? DEFAULT_RECV_BUFFER_SIZE,
  });

  // Mandatory: an ICMP port-unreachable (the peer isn't listening, or a
  // path change occurs) surfaces asynchronously as an 'error' event on a
  // *connected* UDP socket. An EventEmitter with an 'error' event and no
  // listener throws — crashing the whole process — for a condition that is
  // routine and already handled at the protocol layer: quiche's own idle
  // timeout is the real failure signal here (it will emit EVENT_SESSION_CLOSE
  // once retries are exhausted), not this socket. `DatagramTransport` has no
  // error-reporting method by design (docs/WASM_CLIENT_PLAN.md §6.5), so
  // every error is swallowed rather than partially surfaced.
  socket.on('error', () => {
    /* swallowed intentionally — see comment above */
  });

  let onMessage: ((datagram: Uint8Array) => void) | null = null;
  socket.on('message', (msg) => {
    onMessage?.(msg);
  });

  await new Promise<void>((resolve) => {
    socket.connect(port, host, () => {
      resolve();
    });
  });

  let closed = false;

  return {
    send(datagram: Uint8Array): void {
      // Fire-and-forget by design: DatagramTransport has no delivery
      // confirmation (matches raw UDP semantics), and quiche's own loss
      // recovery is the retransmission mechanism. A late callback error
      // after the transport has already been asked to close is expected
      // (e.g. teardown in flight) and intentionally ignored for the same
      // reason as the 'error' handler above.
      socket.send(datagram, (): void => {});
    },

    onMessage(cb: (datagram: Uint8Array) => void): void {
      onMessage = cb;
    },

    localAddress(): DatagramTransportAddress {
      const addr = socket.address();
      return { address: addr.address, family: addr.family, port: addr.port };
    },

    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      onMessage = null;
      await new Promise<void>((resolve) => {
        socket.close(() => {
          resolve();
        });
      });
    },
  };
}
