import {
  createSecureServer, createNodeServerFactory as rootFactory,
  createExpressHandler as rootHandler, Http3Error, WasmMemoryError,
  RequestBodyTooLargeError, ERR_HTTP3_WASM_MEMORY_LIMIT, ERR_HTTP3_RECEIVE_BUFFER_LIMIT,
  type RuntimeInfo, type RuntimeAttemptFailure, type RuntimeOptions,
  type ServerOptions, type ConnectOptions, type QuicServerOptions, type QuicConnectOptions,
  type IncomingHeaders, type StreamFlags, type RespondOptions, type RequestAsyncOptions,
  type SessionMetrics, type SessionCloseInfo, type GoawayInfo, type PingCallback,
  type ConnectionEndpoint, type DnsLookupFn, type ListenOptions,
  type ShutdownOptions, type ShutdownController,
} from '@currentspace/http3';
import { createNodeServerFactory, type NodeServerOptions } from '@currentspace/http3/node';
import { createExpressHandler, type ExpressLikeHandler } from '@currentspace/http3/express';
import { listenNest, type ListenNestOptions, type NestHttp3Listener } from '@currentspace/http3/nest';
import { proxy, type ProxyOptions } from '@currentspace/http3/proxy';
import { createDevTls, type DevTlsOptions, type TrustCommand } from '@currentspace/http3/dev';
import { createFetchHandler, RequestBodyTooLargeError as FetchBodyError, type FetchHandlerOptions, type FetchApp, type ServeFetchOptions } from '@currentspace/http3/fetch';
import { createFetchHandler as h3FetchHandler, type FetchHandlerOptions as H3FetchOptions } from '@currentspace/http3/h3';
import { type SseEvent, type SseStreamOptions } from '@currentspace/http3/sse';
import { type EventSourceInit, type EventSourceMessage } from '@currentspace/http3/eventsource';
import { type HealthSnapshot, type HealthServerOptions, type GracefulShutdownOptions } from '@currentspace/http3/ops';
import { type AwsTlsEnv, type AwsTlsSecretShape } from '@currentspace/http3/aws-cert';
import { type SecureServerOptions, type SecureClientConnectOptions } from '@currentspace/http3/parity';
import {
  WasmMemoryError as WasmEntryError, ERR_HTTP3_WASM_MEMORY_LIMIT as wasmCode,
  type CommonWasmClientOptions, type CommonWasmServerOptions,
  type WasmH3ServerEventLoopOptions, type WasmQuicServerEventLoopOptions,
  type WasmH3ClientEventLoopOptions, type WasmQuicClientEventLoopOptions,
  type DatagramTransport, type DatagramServerTransport, type WasmEvent,
  type Http3WasmCore, type ShimOptions,
} from '@currentspace/http3/wasm';

void listenNest; void proxy; void createDevTls;
const app = createExpressHandler((_req, res) => res.end('ok'));
const options: NodeServerOptions = {};
const server = createNodeServerFactory(options)(app);
const selectedRuntime: RuntimeInfo | null = server.runtimeInfo;
void selectedRuntime;
// @ts-expect-error Runtime selection is exposed as a read-only snapshot getter.
server.runtimeInfo = null;
const listen: ListenOptions = { port: 0, host: '::' };
server.listen(listen);
server.listen(0, '::', () => {});
rootFactory({})(rootHandler(app));
createSecureServer({}).listen({ port: 0 });
// @ts-expect-error Ports must be numeric.
server.listen({ port: '443' });

const runtime: RuntimeOptions = {
  onRuntimeEvent(info: RuntimeInfo) {
    const failure: RuntimeAttemptFailure | null | undefined = info.fastAttempt;
    void failure;
  },
};
void runtime;
const fetchOptions: FetchHandlerOptions & H3FetchOptions = { maxBodyBytes: 1024 };
const fetchApp: FetchApp = { fetch: async request => {
  try { return new Response(await request.text()); }
  catch (error) {
    if (error instanceof FetchBodyError) return new Response('too large', { status: 413 });
    throw error;
  }
} };
createFetchHandler(fetchApp, fetchOptions);
h3FetchHandler(fetchApp, fetchOptions);

function inspect(error: unknown): number | undefined {
  if (error instanceof WasmMemoryError) {
    const code: typeof ERR_HTTP3_WASM_MEMORY_LIMIT = error.code;
    const bytes: number = error.requestedBytes;
    // @ts-expect-error The requested byte count is readonly.
    error.requestedBytes = 0;
    void code;
    return bytes;
  }
  if (error instanceof WasmEntryError) {
    const code: typeof wasmCode = error.code;
    void code;
    return error.requestedBytes;
  }
  if (error instanceof RequestBodyTooLargeError) return undefined;
  if (error instanceof Http3Error) { const wireCode: number | undefined = error.h3Code; return wireCode; }
  if (error instanceof Error && 'code' in error && error.code === ERR_HTTP3_RECEIVE_BUFFER_LIMIT) return undefined;
  return undefined;
}
createSecureServer({}, stream => {
  stream.on('error', error => {
    // @ts-expect-error Stream errors are not all WASM memory errors.
    error.requestedBytes;
    inspect(error);
  });
  stream.write('ok', error => { inspect(error); });
});

// Each configuration/result family must be reachable through a public entry.
export type ConsumerTypes = [
  ServerOptions, ConnectOptions, QuicServerOptions, QuicConnectOptions,
  IncomingHeaders, StreamFlags, RespondOptions, RequestAsyncOptions,
  SessionMetrics, SessionCloseInfo, GoawayInfo, PingCallback, ConnectionEndpoint,
  DnsLookupFn, ShutdownOptions, ShutdownController, ExpressLikeHandler,
  ListenNestOptions, NestHttp3Listener, ProxyOptions, DevTlsOptions, TrustCommand,
  ServeFetchOptions, SseEvent, SseStreamOptions, EventSourceInit, EventSourceMessage,
  HealthSnapshot, HealthServerOptions, GracefulShutdownOptions, AwsTlsEnv,
  AwsTlsSecretShape, SecureServerOptions, SecureClientConnectOptions,
  CommonWasmClientOptions, CommonWasmServerOptions, WasmH3ServerEventLoopOptions,
  WasmQuicServerEventLoopOptions, WasmH3ClientEventLoopOptions, WasmQuicClientEventLoopOptions,
  DatagramTransport, DatagramServerTransport, WasmEvent, Http3WasmCore, ShimOptions,
];
