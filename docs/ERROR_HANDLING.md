# Error Handling Guide

`@currentspace/http3` exposes a small, stable set of error primitives for
transport, stream, and configuration failures.

The main public type is `Http3Error`, plus a set of exported `ERR_HTTP3_*`
string constants. Native worker failures are normalized into `Http3Error`
instances before they reach JS session and stream objects.

## `Http3Error`

`Http3Error` extends `Error` and adds protocol-specific metadata:

| Property | Type | Meaning |
| --- | --- | --- |
| `message` | `string` | Human-readable failure summary. |
| `code` | `string` | Stable `ERR_HTTP3_*` category. |
| `quicCode` | `number \| undefined` | QUIC transport close code, when the failure is session-scoped. |
| `h3Code` | `number \| undefined` | HTTP/3 application error code, when the failure is stream-scoped. |
| `requestedMode` | `RuntimeMode \| undefined` | Requested runtime mode for runtime-selection failures, including `'wasm'`. |
| `selectedMode` | `SelectedRuntimeMode \| undefined` | Selected runtime mode when known. |
| `driver` | `RuntimeDriver \| undefined` | Driver associated with the failure, including `'wasm'`. |
| `reasonCode` | `string \| undefined` | Structured reason identifier for endpoint/runtime failures. |
| `errno` | `number \| undefined` | Native errno when available. |
| `syscall` | `string \| undefined` | Native syscall/operation name when available. |
| `endpoint` | `string \| undefined` | Endpoint string involved in parse/resolve failures. |
| `runtimeInfo` | `RuntimeInfo \| undefined` | Runtime snapshot attached to selection failures. |

## Allocation and body-limit errors

`WasmMemoryError` is available from both `@currentspace/http3` and
`@currentspace/http3/wasm`, including the `worker` and `workerd` export
conditions. It extends `Error` directly, not `Http3Error`. Its readonly `code`
has the literal type `'ERR_HTTP3_WASM_MEMORY_LIMIT'`, and its readonly
`requestedBytes` is the number of payload bytes whose allocation was rejected.

```ts
import { WasmMemoryError } from '@currentspace/http3';

function handleStreamError(error: Error): void {
  if (error instanceof WasmMemoryError) {
    console.error(`WASM rejected ${error.requestedBytes} payload bytes`);
    return;
  }
  console.error(error);
}

// Attach before writing; a rejected write also reports through its callback.
// stream.on('error', handleStreamError);
```

Stream error listeners and write callbacks remain typed as `Error`, because
transport failures and other errors can also occur. Narrow with `instanceof`
before accessing subclass fields. For code-based matching across duplicate
package instances, check that the error is an object with a `code` property
equal to the exported constant; code matching alone does not narrow the other
fields to the class type.

`RequestBodyTooLargeError` is exported from the root, `/fetch`, and `/h3`.
Fetch handlers can catch it when consuming a request body that exceeds
`maxBodyBytes`, including through `request.text()` or `request.json()`. If it
escapes the handler before response headers are sent, the Fetch adapter produces
a 413 response. A response already in progress cannot have its status replaced.
The error has no custom `code` or byte-count property.

The receive-buffer safety limit is a separate error: it is a plain `Error`
with `code: ERR_HTTP3_RECEIVE_BUFFER_LIMIT`, exported from the root. It does
not imply that the configured total Fetch body limit was exceeded.

## Exported Error Codes

| Code | Current meaning | Typical source |
| --- | --- | --- |
| `ERR_HTTP3_STREAM_ERROR` | A request/response stream was reset or failed. | Native stream reset/error mapped through `toStreamError()`. |
| `ERR_HTTP3_STREAM_BLOCKED` | Request creation is temporarily blocked by stream flow control. | `request()`; use `requestAsync()` to wait for capacity. |
| `ERR_HTTP3_SESSION_ERROR` | A connection-level failure occurred. | Native session close/error mapped through `toSessionError()`. |
| `ERR_HTTP3_HEADERS_SENT` | Headers were already committed. | Reserved public code for response-state errors in compatibility layers. |
| `ERR_HTTP3_INVALID_STATE` | API call was made in the wrong lifecycle state. | Requesting before connect, re-listening, invalid 0-RTT use. |
| `ERR_HTTP3_GOAWAY` | Peer is draining and should not receive new requests. | New request attempts and locally opened streams rejected by GOAWAY. |
| `ERR_HTTP3_TLS_CONFIG_ERROR` | TLS inputs are invalid or missing. | Missing `key`/`cert`, bad `pfx`, bad passphrase. |
| `ERR_HTTP3_KEYLOG_ERROR` | TLS key logging setup failed. | Exported public constant for keylog setup failures. |
| `ERR_HTTP3_ENDPOINT_INVALID` | Endpoint string/object is malformed. | Invalid URL, missing host, invalid port, malformed endpoint object. |
| `ERR_HTTP3_ENDPOINT_RESOLUTION` | Hostname lookup failed. | Docker service names, DNS lookups, bad hostnames. |
| `ERR_HTTP3_FAST_PATH_UNAVAILABLE` | Requested Linux fast path could not be used. | `io_uring_setup` blocked by seccomp/policy, unsupported fast runtime. |
| `ERR_HTTP3_RUNTIME_UNSUPPORTED` | Runtime driver failed for an environmental reason. | Post-start driver failure or unsupported runtime environment. |
| `ERR_HTTP3_RUNTIME_FALLBACK` | Runtime fallback category for consumers classifying warnings/events. | `runtimeInfo` / `'runtime'` events / `onRuntimeEvent`. |
| `ERR_HTTP3_WASM_MEMORY_LIMIT` | Payload allocation was rejected by the WASM memory limit. | `WasmMemoryError` through stream errors/write callbacks or direct WASM helpers. |
| `ERR_HTTP3_RECEIVE_BUFFER_LIMIT` | The bounded JavaScript receive buffer was exceeded. | Readable backpressure safety limit. |

## How Native Errors Are Mapped

The native worker reports `errorReason` plus a numeric `errorCode`. Runtime
selection failures and post-start transport driver failures also preserve
driver-specific metadata when available.

- Stream failures are converted with `toStreamError()`, which sets `code` to `ERR_HTTP3_STREAM_ERROR` and copies the numeric native code into `h3Code`.
- Session failures are converted with `toSessionError()`, which sets `code` to `ERR_HTTP3_SESSION_ERROR` and copies the numeric native code into `quicCode`.
- Runtime driver failures are converted into `ERR_HTTP3_FAST_PATH_UNAVAILABLE`
  or `ERR_HTTP3_RUNTIME_UNSUPPORTED` and preserve `driver`, `errno`, `syscall`,
  and `reasonCode`.

This keeps application code stable even if the lower-level native event shape
changes.

## Where Errors Surface

### Server startup

- `createSecureServer()` / `server.listen()` can throw `Http3Error` with `ERR_HTTP3_INVALID_STATE` if the server is already listening.
- TLS resolution can throw `Http3Error` with `ERR_HTTP3_TLS_CONFIG_ERROR` when credentials are missing or a `pfx` bundle cannot be decoded.
- `fast` runtime selection failures throw `ERR_HTTP3_FAST_PATH_UNAVAILABLE`.
- `auto` runtime fallback surfaces through `runtimeInfo`, the `'runtime'` event,
  `onRuntimeEvent`, and a `WARN_HTTP3_RUNTIME_FALLBACK` process warning.

### HTTP/3 server sessions and streams

- `Http3SecureServer` emits `'error'` for server-level failures.
- `Http3ServerSession` emits `'error'` when the connection closes with a native session error.
- `ServerHttp3Stream` instances are destroyed with `Http3Error` when a stream reset or stream-scoped error occurs.
- Stream resets also emit `'aborted'` before teardown on request/response streams where applicable.

### HTTP/3 client sessions and request streams

- `Http3ClientSession#request()` throws `Http3Error` with `ERR_HTTP3_INVALID_STATE` if the session is not ready or if 0-RTT restrictions are violated. This is an intentional HTTP/2 parity difference: HTTP/3 requests are not queued before the QUIC/TLS handshake unless the caller explicitly opts into the 0-RTT path.
- `Http3ClientSession#request()` throws `ERR_HTTP3_STREAM_BLOCKED` when quiche reports transient `StreamBlocked` while creating the request stream. Use `requestAsync()` for high-concurrency clients that should wait for request-stream capacity instead of failing synchronously.
- `Http3ClientSession` emits `'error'` for session-level transport failures.
- `ClientHttp3Stream` instances are destroyed with `Http3Error` on stream resets and stream-scoped native failures.
- GOAWAY is surfaced via the session `'goaway'` event, followed by normal close/drain behavior.
- Endpoint parsing/resolution failures reject `connectAsync()` cleanly with
  `ERR_HTTP3_ENDPOINT_INVALID` or `ERR_HTTP3_ENDPOINT_RESOLUTION`.

### Fetch/SSE/EventSource adapters

- `serveFetch()` converts handler throws into `500` responses on fallback HTTP/1.1/HTTP/2 paths, except `RequestBodyTooLargeError`, which produces 413.
- `ServerSentEventStream` closes itself if heartbeat writes fail.
- `Http3EventSource` emits plain `Error` objects for bad status/content-type responses and reconnects unless `reconnect: false` is set.

## Recovery Patterns

### Invalid state

Use when:

- The client session is not connected yet.
- The caller tries to use unsafe 0-RTT without explicitly enabling it.
- The server is asked to `listen()` twice.

Recovery:

- Wait for the client `'connect'` event or use `connectAsync()`.
- Retry the action on a fresh session after the previous one closes.
- Treat repeated `listen()` or post-close use as application bugs.

### Stream error

Use when:

- A single request stream is reset.
- The peer rejects the stream or violates stream-level protocol constraints.

Recovery:

- Retry only idempotent work (`GET`, safe replays, resumable operations).
- Recreate the request stream on the existing session if the session is still healthy.
- Inspect `h3Code` when you need transport-level diagnostics.

### Session error

Use when:

- The whole QUIC connection closes.
- Handshake/setup fails after the session object exists.

Recovery:

- Reconnect with a new session.
- Reuse cached tickets only if the previous failure was not caused by bad credentials or incompatible server config.
- Inspect `quicCode` when you need transport-level diagnostics.

### GOAWAY

Use when:

- The server is draining and does not want new requests.

Recovery:

- Stop creating new streams on the existing session.
- Allow in-flight work below the server's GOAWAY stream ID to finish.
- Retry or fail locally opened request streams with IDs greater than or equal to the GOAWAY stream ID.
- Open a new session for subsequent requests.

### TLS/configuration error

Use when:

- `key` / `cert` are missing.
- `pfx` or `passphrase` cannot be decoded.
- Trust roots or local cert material are invalid.

Recovery:

- Fix the certificate inputs before retrying.
- Validate PKCS#12 archives with OpenSSL when debugging packaging issues.
- Prefer explicit `ca` material in local/dev environments rather than disabling verification everywhere.

### Endpoint parse / resolution error

Use when:

- The authority string is malformed.
- The hostname cannot be resolved.
- A caller should pass `{ address, port, servername }` instead of a hostname.

Recovery:

- Fix the endpoint syntax.
- Verify container DNS / host resolver behavior.
- Use the explicit endpoint object form when the transport address and TLS
  identity differ.

### Fast path unavailable

Use when:

- `runtimeMode: 'fast'` was requested and Linux `io_uring` could not be enabled.
- `runtimeMode: 'auto'` was requested with `fallbackPolicy: 'error'`.

Recovery:

- Switch to `runtimeMode: 'portable'` for ordinary containers.
- Keep `runtimeMode: 'fast'` and allow `io_uring_*` syscalls, typically via
  `seccomp=unconfined` or an equivalent custom seccomp policy.
- Inspect `driver`, `errno`, and `syscall` in logs.

## Practical Recommendations

- Listen for `'error'`, `'close'`, and `'goaway'` on sessions, not just streams.
- Treat stream-level and session-level failures differently: stream errors can often be retried in place, session errors usually require reconnect.
- Keep retries idempotent and bounded.
- Log `code`, `quicCode`, `h3Code`, `driver`, `errno`, and `syscall` together so transport and application failures are easy to correlate.
- Log or forward `runtimeInfo` whenever you allow `auto` fallback.
