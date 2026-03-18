/**
 * `@currentspace/http3` -- HTTP/3 and raw QUIC for Node.js.
 *
 * Provides a native QUIC transport (via quiche) with an API modelled
 * after `node:http2`, plus higher-level adapters for Fetch, Express,
 * Server-Sent Events, and operational tooling.
 * @module
 */

export { createSecureServer, Http3SecureServer } from './server.js';
export type { ServerOptions, TlsOptions, StreamListener, AddressInfo } from './server.js';

export { connect, connectAsync, Http3ClientSession } from './client.js';
export type { ConnectOptions, RequestOptions } from './client.js';

export { Http3Session, Http3ServerSession } from './session.js';
export type { SessionOptions, SessionMetrics } from './session.js';

export { ServerHttp3Stream, ClientHttp3Stream } from './stream.js';
export type { IncomingHeaders, StreamFlags, RespondOptions } from './stream.js';

export {
  Http3Error,
  ERR_HTTP3_STREAM_ERROR,
  ERR_HTTP3_SESSION_ERROR,
  ERR_HTTP3_HEADERS_SENT,
  ERR_HTTP3_INVALID_STATE,
  ERR_HTTP3_GOAWAY,
  ERR_HTTP3_TLS_CONFIG_ERROR,
  ERR_HTTP3_KEYLOG_ERROR,
  ERR_HTTP3_ENDPOINT_INVALID,
  ERR_HTTP3_ENDPOINT_RESOLUTION,
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  ERR_HTTP3_RUNTIME_FALLBACK,
  WARN_HTTP3_RUNTIME_FALLBACK,
} from './errors.js';

export { createSseStream, ServerSentEventStream, encodeSseEvent, encodeSseComment, sseHeaders } from './sse.js';
export type { SseEvent, SseStreamOptions } from './sse.js';

export { createEventSource, Http3EventSource } from './eventsource.js';
export type { EventSourceInit, EventSourceMessage } from './eventsource.js';

export { createHealthController, startHealthServer, installGracefulShutdown, HealthController } from './ops.js';
export type { HealthSnapshot, HealthServerOptions, HealthServerHandle, GracefulShutdownOptions, GracefulShutdownHandle } from './ops.js';

export { loadTlsOptionsFromAwsEnv } from './aws-cert.js';
export type { AwsTlsEnv, AwsTlsSecretShape } from './aws-cert.js';

export { createExpressAdapter } from './express-adapter.js';
export type { ExpressLikeHandler, ExpressLikeRequest, ExpressLikeResponse } from './express-adapter.js';

export { createQuicServer, QuicServer, QuicServerSession } from './quic-server.js';
export type { QuicServerOptions } from './quic-server.js';

export { connectQuic, connectQuicAsync, QuicClientSession } from './quic-client.js';
export type { QuicConnectOptions } from './quic-client.js';

export { QuicStream } from './quic-stream.js';

export type {
  RuntimeMode,
  SelectedRuntimeMode,
  FallbackPolicy,
  RuntimeDriver,
  RuntimeReasonCode,
  RuntimeInfo,
  RuntimeOptions,
} from './runtime.js';
export type { ConnectionEndpoint, HostEndpoint, AddressEndpoint } from './endpoint.js';

export * as constants from './constants.js';
export * as parity from './http2-parity.js';
export * as h3 from './http3-extensions.js';
