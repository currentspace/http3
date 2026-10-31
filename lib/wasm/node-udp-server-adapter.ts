/**
 * Node `node:dgram` implementation of {@link DatagramServerTransport}
 * (the bound/unconnected-socket sibling of `node-udp-adapter.ts`'s
 * connected-socket client transport). This is the **only** file under
 * `lib/wasm/` that binds an unconnected UDP socket — enforced by the
 * `lib/wasm/**` ESLint zone in `eslint.config.mjs`, same convention as
 * `node-udp-adapter.ts`'s existing `node:dgram` exception.
 *
 * A server socket is fundamentally different from the client's connected
 * socket: it is bound (not connected) to a local address, receives from
 * many different peers (each `'message'` event carries the sender's
 * `rinfo`), and every send specifies its destination explicitly (the socket
 * has no fixed peer for the kernel to route to automatically).
 */

import dgram from 'node:dgram';
import { isIP } from 'node:net';
import type { DatagramServerTransport, DatagramServerTransportAddress } from './datagram-server-transport.js';
import { formatLocalAddr, parseSocketAddress } from './wasm-options.js';

export interface BindNodeUdpServerOptions {
  /** UDP socket receive buffer size, in bytes. Default: 4 MiB. */
  recvBufferSize?: number;
}

const DEFAULT_RECV_BUFFER_SIZE = 4 * 1024 * 1024;

/**
 * Bind a Node UDP socket to `(host, port)`. Unlike
 * `node-udp-adapter.ts`'s `connectNodeUdp`, this socket is never
 * `.connect()`-ed: it stays bound-and-unconnected so it can receive from
 * (and reply to) any peer, which is exactly what a server needs.
 */
export async function bindNodeUdpServer(
  port: number,
  host: string,
  opts: BindNodeUdpServerOptions = {},
): Promise<DatagramServerTransport> {
  const type = isIP(host) === 6 ? 'udp6' : 'udp4';
  const socket = dgram.createSocket({
    type,
    ipv6Only: false,
    recvBufferSize: opts.recvBufferSize ?? DEFAULT_RECV_BUFFER_SIZE,
  });

  let onMessage: ((datagram: Uint8Array, peerAddr: string) => void) | null = null;
  socket.on('message', (msg, rinfo) => {
    onMessage?.(msg, formatLocalAddr(rinfo.address, rinfo.family, rinfo.port));
  });

  // Keep late errors observed even after startup has failed. QUIC handles
  // post-bind UDP errors through loss recovery rather than adapter callbacks.
  socket.on('error', () => {});
  await new Promise<void>((resolve, reject) => {
    let settled = false;
    const cleanup = (): void => {
      socket.off('error', onBindError);
      socket.off('listening', onListening);
    };
    const onBindError = (err: Error): void => {
      if (settled) return;
      settled = true;
      cleanup();
      onMessage = null;
      // A failed bind still owns an open dgram handle. Finish releasing it
      // before rejecting, preserving the bind failure rather than close errors.
      try { socket.close(() => reject(err)); }
      catch { reject(err); }
    };
    const onListening = (): void => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve();
    };
    socket.once('error', onBindError);
    socket.once('listening', onListening);
    try { socket.bind(port, host); }
    catch (error) { onBindError(error instanceof Error ? error : new Error(String(error))); }
  });

  let closed = false;

  return {
    send(datagram: Uint8Array, dest: string): void {
      const { host: destHost, port: destPort } = parseSocketAddress(dest);
      // Fire-and-forget by design — see connectNodeUdp's identical comment:
      // no delivery confirmation at this layer, quiche's own loss recovery
      // is the retransmission mechanism.
      socket.send(datagram, destPort, destHost, (): void => {});
    },

    onMessage(cb: (datagram: Uint8Array, peerAddr: string) => void): void {
      onMessage = cb;
    },

    localAddress(): DatagramServerTransportAddress {
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
