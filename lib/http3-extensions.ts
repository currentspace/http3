/**
 * HTTP/3-specific extensions beyond the `node:http2` parity surface.
 *
 * Includes Server-Sent Events, EventSource, Fetch adapter, health checks,
 * AWS TLS credential loading, and operational helpers.
 * @module
 */
export { createSseStream, ServerSentEventStream, createSseReadableStream, encodeSseEvent, encodeSseComment, sseHeaders } from './sse.js';
export type { SseEvent, SseStreamOptions } from './sse.js';

export { createEventSource, Http3EventSource } from './eventsource.js';
export type { EventSourceInit, EventSourceMessage } from './eventsource.js';

export { createFetchHandler, createSseFetchResponse, serveFetch } from './fetch-adapter.js';
export type { FetchApp, FetchHandler, ServeFetchOptions } from './fetch-adapter.js';

export { createHealthController, startHealthServer, installGracefulShutdown, HealthController } from './ops.js';
export type { HealthSnapshot, HealthServerOptions, HealthServerHandle, GracefulShutdownOptions, GracefulShutdownHandle } from './ops.js';

export { loadTlsOptionsFromAwsEnv } from './aws-cert.js';
export type { AwsTlsEnv, AwsTlsSecretShape } from './aws-cert.js';

export * as constants from './constants.js';

