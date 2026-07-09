/**
 * Host-agnostic **server**-side datagram transport interface — the
 * bound/unconnected-socket sibling of `datagram-transport.ts`'s
 * `DatagramTransport` (which models a single connected client flow).
 *
 * A server binds one UDP socket and receives from many different peers, so
 * (unlike the client transport) every inbound datagram callback reports the
 * sender's address, and every outbound send specifies its destination
 * explicitly — the socket itself is never "connected" to one fixed peer.
 *
 * `WasmH3ServerEventLoop`/`WasmQuicServerEventLoop` are written only against
 * this interface — never against `node:dgram` directly — so a future
 * workerd-side adapter (should one ever make sense — see
 * `lib/wasm/index.workerd.ts`'s doc comment on why servers are Node-only for
 * now: Workers has no inbound-listening-socket model at all, not just no
 * outbound UDP) would be a drop-in second implementation, not a rewrite.
 *
 * This file itself imports nothing host-specific. The sole Node
 * implementation lives in `lib/wasm/node-udp-server-adapter.ts`.
 */

export interface DatagramServerTransportAddress {
  address: string;
  family: string;
  port: number;
}

export interface DatagramServerTransport {
  /**
   * Send one datagram to `dest` — a `"ip:port"`/`"[v6]:port"` string in the
   * exact format `hs_next_send_dest`/`qs_next_send_dest` return (Rust's
   * `SocketAddr::to_string()`), matching `lib/wasm/wasm-options.ts`'s
   * `parseSocketAddress` parser.
   */
  send(datagram: Uint8Array, dest: string): void;
  /**
   * Register the (single) receive callback for inbound datagrams from any
   * peer. `peerAddr` is that datagram's sender address, in the same
   * `"ip:port"`/`"[v6]:port"` format `hs_recv`/`qs_recv` expect.
   */
  onMessage(cb: (datagram: Uint8Array, peerAddr: string) => void): void;
  /** The transport's local bound address. */
  localAddress(): DatagramServerTransportAddress;
  /** Release the underlying socket/resource. Idempotent. */
  close(): Promise<void>;
}
