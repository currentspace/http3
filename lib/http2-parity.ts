/**
 * Migration surface for users coming from `node:http2`.
 *
 * Re-exports core APIs under http2-style names. Behavioral differences:
 * - Transport is QUIC/UDP instead of TCP/TLS.
 * - Streams use HTTP/3 framing (QPACK headers, per-stream flow control).
 * - `createServer` is an alias for `createSecureServer` (QUIC always requires TLS).
 * - No push-promise support (HTTP/3 removed server push).
 * @module
 */
import { createSecureServer, type AddressInfo, type ServerOptions, type StreamListener, type TlsOptions } from './server.js';
import { connect, connectAsync, type ConnectOptions, type RequestOptions } from './client.js';
import type { SessionMetrics } from './session.js';
import type { IncomingHeaders, RespondOptions, StreamFlags } from './stream.js';

/**
 * Dedicated surface for users migrating from node:http2.
 * The API intentionally preserves the existing runtime behavior while exposing
 * aliases with http2-style naming where possible.
 */
export const createServer = createSecureServer;
export { createSecureServer, connect, connectAsync };

export type SecureServerOptions = ServerOptions;
export type SecureServerTlsOptions = TlsOptions;
export type SecureClientConnectOptions = ConnectOptions;
export type RequestStreamOptions = RequestOptions;
export type IncomingRequestHeaders = IncomingHeaders;
export type ResponseOptions = RespondOptions;
export type IncomingStreamFlags = StreamFlags;
export type SessionSnapshotMetrics = SessionMetrics;
export type ServerStreamHandler = StreamListener;
export type ServerAddressInfo = AddressInfo;

