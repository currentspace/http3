/**
 * Host-agnostic datagram transport interface (docs/WASM_CLIENT_PLAN.md
 * §6.5). `WasmClientEventLoop` (H3) and its raw-QUIC counterpart are
 * written only against this interface — never against `node:dgram`
 * directly — so a future workerd adapter (behind `cloudflare/workerd#4463`)
 * is a drop-in second implementation, not a rewrite.
 *
 * This file itself imports nothing host-specific. The sole Node
 * implementation lives in `lib/wasm/node-udp-adapter.ts`.
 */

export interface DatagramTransportAddress {
  address: string;
  family: string;
  port: number;
}

export interface DatagramTransport {
  /** Send one datagram to the transport's single connected peer. */
  send(datagram: Uint8Array): void;
  /** Register the (single) receive callback for inbound datagrams from the connected peer. */
  onMessage(cb: (datagram: Uint8Array) => void): void;
  /** The transport's local bound address. */
  localAddress(): DatagramTransportAddress;
  /** Release the underlying socket/resource. Idempotent. */
  close(): Promise<void>;
}
