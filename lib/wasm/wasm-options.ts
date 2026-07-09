/**
 * Shared helpers for building the `h3c_new`/`qc_new` options JSON object
 * (docs/WASM_CLIENT_PLAN.md §5.3 "h3c_new options (normative)") and for
 * the small amount of socket-address string munging the wasm client event
 * loops need. Factored out of `h3-client-event-loop.ts` /
 * `quic-client-event-loop.ts` so the two files can't independently drift
 * on the ABI's exact field-name contract — notably `allow0rtt` (see below).
 */

/** Generate a 20-byte SCID and return it as 40 lowercase hex chars (the `scidHex` ABI field). */
export function randomScidHex(): string {
  return randomHex(20);
}

/**
 * Generate a 32-byte retry-token/SCID-derivation key and return it as 64
 * lowercase hex chars (the `retryTokenKeyHex` ABI field `hs_new`/`qs_new`
 * expect — mirrors `randomScidHex`'s exact convention, generated once per
 * server instance at construction time, per `crates/http3-wasm/src/h3_server.rs`'s
 * `hs_new` doc comment).
 */
export function randomRetryTokenKeyHex(): string {
  return randomHex(32);
}

function randomHex(byteLength: number): string {
  const bytes = new Uint8Array(byteLength);
  globalThis.crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Split a `"host:port"` or `"[v6]:port"` socket address string (the format `lib/endpoint.ts` produces) into its parts. */
export function parseSocketAddress(addr: string): { host: string; port: number } {
  if (addr.startsWith('[')) {
    const closeIdx = addr.indexOf(']');
    const host = addr.slice(1, closeIdx);
    const port = Number.parseInt(addr.slice(closeIdx + 2), 10);
    return { host, port };
  }
  const idx = addr.lastIndexOf(':');
  return { host: addr.slice(0, idx), port: Number.parseInt(addr.slice(idx + 1), 10) };
}

/** Format a local address the way Rust's `SocketAddr::from_str` requires — IPv6 must be bracketed. */
export function formatLocalAddr(address: string, family: string, port: number): string {
  return family === 'IPv6' ? `[${address}]:${String(port)}` : `${address}:${String(port)}`;
}

/**
 * Options common to both `h3c_new` and `qc_new` beyond the connect params
 * (§5.3). Binary fields are `Uint8Array`, not Node's `Buffer` — this file
 * must stay compilable with no `@types/node` in scope (Phase 5,
 * docs/WASM_CLIENT_PLAN.md §9; a future workerd host has none). Node's
 * `Buffer` instances satisfy `Uint8Array` fine, so every existing
 * Node-facing call site (`lib/client.ts`/`lib/quic-client.ts` passing
 * `options?.ca`/`options?.sessionTicket`, both publicly typed `Buffer`)
 * keeps working unchanged.
 */
export interface CommonWasmClientOptions {
  ca?: Uint8Array;
  rejectUnauthorized?: boolean;
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Uint8Array;
  allow0RTT?: boolean;
  enableDatagrams?: boolean;
  keylog?: boolean;
}

/** UTF-8 decode, portable across Node and workerd (`TextDecoder` is a Web-standard global present in both, unlike Buffer's `.toString('utf8')`). */
function utf8Decode(bytes: Uint8Array): string {
  return new TextDecoder('utf-8').decode(bytes);
}

/**
 * Base64-encode, portable across Node and workerd: `btoa` is a Web-standard
 * global present in both (unlike Buffer's `.toString('base64')`) but
 * operates on a "binary string" (one UTF-16 code unit per byte), not a
 * byte array directly — hence the `String.fromCharCode` spread. Fine for
 * this module's inputs (PEM certs, session tickets: at most a few KB),
 * well under engines' safe argument-spread limits.
 */
function base64Encode(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes));
}

/**
 * Build the common (protocol-agnostic) portion of the options JSON object.
 * Merge the result with connect params (`serverAddr`/`serverName`/`localAddr`/
 * `scidHex`) and any protocol-specific fields (QUIC's `cert`/`key`/`alpn`).
 *
 * **`allow0rtt` is lowercase-`rtt`** in the wasm ABI
 * (`crates/http3-wasm/src/json_opts.rs`: `get_bool(v, "allow0rtt")`) —
 * unlike the native binding's `allow0Rtt` field
 * (`lib/event-loop.ts`'s `NativeClientOptions`/`NativeQuicClientOptions`).
 * Getting this wrong silently drops the option (the Rust side just reads
 * `None` and defaults `false`) rather than failing loudly, so it is
 * deliberately centralized here instead of duplicated in both event loop
 * files.
 */
export function buildCommonOptionsJson(opts: CommonWasmClientOptions): Record<string, unknown> {
  const json: Record<string, unknown> = {};
  if (opts.ca) json.ca = utf8Decode(opts.ca);
  if (opts.rejectUnauthorized !== undefined) json.rejectUnauthorized = opts.rejectUnauthorized;
  if (opts.maxIdleTimeoutMs !== undefined) json.maxIdleTimeoutMs = opts.maxIdleTimeoutMs;
  if (opts.maxUdpPayloadSize !== undefined) json.maxUdpPayloadSize = opts.maxUdpPayloadSize;
  if (opts.initialMaxData !== undefined) json.initialMaxData = opts.initialMaxData;
  if (opts.initialMaxStreamDataBidiLocal !== undefined) json.initialMaxStreamDataBidiLocal = opts.initialMaxStreamDataBidiLocal;
  if (opts.initialMaxStreamsBidi !== undefined) json.initialMaxStreamsBidi = opts.initialMaxStreamsBidi;
  if (opts.sessionTicket) json.sessionTicket = base64Encode(opts.sessionTicket);
  if (opts.allow0RTT !== undefined) json.allow0rtt = opts.allow0RTT;
  if (opts.enableDatagrams !== undefined) json.enableDatagrams = opts.enableDatagrams;
  if (opts.keylog !== undefined) json.keylog = opts.keylog;
  return json;
}

/**
 * Options common to both `hs_new` and `qs_new` beyond the ABI-only setup
 * fields (`localAddr`/`retryTokenKeyHex`, built by the caller — see
 * `crates/http3-wasm/src/json_opts.rs`'s `ServerParams`/`parse_server_params`).
 * `key`/`cert` are mandatory (a server cannot start without them); binary
 * fields are `Uint8Array` for the same host-agnostic reasons as
 * {@link CommonWasmClientOptions}.
 */
export interface CommonWasmServerOptions {
  key: Uint8Array;
  cert: Uint8Array;
  ca?: Uint8Array;
  clientAuth?: 'none' | 'request' | 'require';
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  disableActiveMigration?: boolean;
  enableDatagrams?: boolean;
  disableRetry?: boolean;
  maxConnections?: number;
  keylog?: boolean;
}

/**
 * Build the common (protocol-agnostic) portion of the server options JSON
 * object. Merge the result with the ABI-only setup fields (`localAddr`,
 * `retryTokenKeyHex`) and any protocol-specific fields (H3's
 * `qpackMaxTableCapacity`/`qpackBlockedStreams`/`quicLb`/`serverId`, QUIC's
 * `alpn`).
 */
export function buildCommonServerOptionsJson(opts: CommonWasmServerOptions): Record<string, unknown> {
  const json: Record<string, unknown> = {
    key: utf8Decode(opts.key),
    cert: utf8Decode(opts.cert),
  };
  if (opts.ca) json.ca = utf8Decode(opts.ca);
  if (opts.clientAuth !== undefined) json.clientAuth = opts.clientAuth;
  if (opts.maxIdleTimeoutMs !== undefined) json.maxIdleTimeoutMs = opts.maxIdleTimeoutMs;
  if (opts.maxUdpPayloadSize !== undefined) json.maxUdpPayloadSize = opts.maxUdpPayloadSize;
  if (opts.initialMaxData !== undefined) json.initialMaxData = opts.initialMaxData;
  if (opts.initialMaxStreamDataBidiLocal !== undefined) json.initialMaxStreamDataBidiLocal = opts.initialMaxStreamDataBidiLocal;
  if (opts.initialMaxStreamsBidi !== undefined) json.initialMaxStreamsBidi = opts.initialMaxStreamsBidi;
  if (opts.disableActiveMigration !== undefined) json.disableActiveMigration = opts.disableActiveMigration;
  if (opts.enableDatagrams !== undefined) json.enableDatagrams = opts.enableDatagrams;
  if (opts.disableRetry !== undefined) json.disableRetry = opts.disableRetry;
  if (opts.maxConnections !== undefined) json.maxConnections = opts.maxConnections;
  if (opts.keylog !== undefined) json.keylog = opts.keylog;
  return json;
}
