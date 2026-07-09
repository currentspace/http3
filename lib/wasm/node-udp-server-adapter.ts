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
    recvBufferSize: opts.recvBufferSize ?? DEFAULT_RECV_BUFFER_SIZE,
  });

  let onMessage: ((datagram: Uint8Array, peerAddr: string) => void) | null = null;
  socket.on('message', (msg, rinfo) => {
    onMessage?.(msg, formatLocalAddr(rinfo.address, rinfo.family, rinfo.port));
  });

  await new Promise<void>((resolve, reject) => {
    const onBindError = (err: Error): void => {
      reject(err);
    };
    socket.once('error', onBindError);
    socket.bind(port, host, () => {
      socket.removeListener('error', onBindError);
      resolve();
    });
  });

  // Mandatory (mirrors connectNodeUdp's identical, more-commented handler):
  // an EventEmitter with an 'error' event and no listener throws, crashing
  // the whole process, for conditions that are routine at the protocol
  // layer (e.g. an ICMP port-unreachable from a peer that has since gone
  // away). `DatagramServerTransport` has no error-reporting method by
  // design — every post-bind error is swallowed.
  socket.on('error', () => {
    /* swallowed intentionally — see comment above */
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
