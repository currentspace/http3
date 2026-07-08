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
  const bytes = new Uint8Array(20);
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

/** Options common to both `h3c_new` and `qc_new` beyond the connect params (§5.3). */
export interface CommonWasmClientOptions {
  ca?: Buffer;
  rejectUnauthorized?: boolean;
  maxIdleTimeoutMs?: number;
  maxUdpPayloadSize?: number;
  initialMaxData?: number;
  initialMaxStreamDataBidiLocal?: number;
  initialMaxStreamsBidi?: number;
  sessionTicket?: Buffer;
  allow0RTT?: boolean;
  enableDatagrams?: boolean;
  keylog?: boolean;
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
  if (opts.ca) json.ca = opts.ca.toString('utf8');
  if (opts.rejectUnauthorized !== undefined) json.rejectUnauthorized = opts.rejectUnauthorized;
  if (opts.maxIdleTimeoutMs !== undefined) json.maxIdleTimeoutMs = opts.maxIdleTimeoutMs;
  if (opts.maxUdpPayloadSize !== undefined) json.maxUdpPayloadSize = opts.maxUdpPayloadSize;
  if (opts.initialMaxData !== undefined) json.initialMaxData = opts.initialMaxData;
  if (opts.initialMaxStreamDataBidiLocal !== undefined) json.initialMaxStreamDataBidiLocal = opts.initialMaxStreamDataBidiLocal;
  if (opts.initialMaxStreamsBidi !== undefined) json.initialMaxStreamsBidi = opts.initialMaxStreamsBidi;
  if (opts.sessionTicket) json.sessionTicket = opts.sessionTicket.toString('base64');
  if (opts.allow0RTT !== undefined) json.allow0rtt = opts.allow0RTT;
  if (opts.enableDatagrams !== undefined) json.enableDatagrams = opts.enableDatagrams;
  if (opts.keylog !== undefined) json.keylog = opts.keylog;
  return json;
}
