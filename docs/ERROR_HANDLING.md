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

## Exported Error Codes

| Code | Current meaning | Typical source |
| --- | --- | --- |
| `ERR_HTTP3_STREAM_ERROR` | A request/response stream was reset or failed. | Native stream reset/error mapped through `toStreamError()`. |
| `ERR_HTTP3_SESSION_ERROR` | A connection-level failure occurred. | Native session close/error mapped through `toSessionError()`. |
| `ERR_HTTP3_HEADERS_SENT` | Headers were already committed. | Reserved public code for response-state errors in compatibility layers. |
| `ERR_HTTP3_INVALID_STATE` | API call was made in the wrong lifecycle state. | Requesting before connect, re-listening, invalid 0-RTT use. |
| `ERR_HTTP3_GOAWAY` | Peer is draining and should not receive new requests. | Exported public constant; current session handling primarily surfaces GOAWAY as an event. |
| `ERR_HTTP3_TLS_CONFIG_ERROR` | TLS inputs are invalid or missing. | Missing `key`/`cert`, bad `pfx`, bad passphrase. |
| `ERR_HTTP3_KEYLOG_ERROR` | TLS key logging setup failed. | Exported public constant for keylog setup failures. |

## How Native Errors Are Mapped

The native worker reports `errorReason` plus a numeric `errorCode`.

- Stream failures are converted with `toStreamError()`, which sets `code` to `ERR_HTTP3_STREAM_ERROR` and copies the numeric native code into `h3Code`.
- Session failures are converted with `toSessionError()`, which sets `code` to `ERR_HTTP3_SESSION_ERROR` and copies the numeric native code into `quicCode`.

This keeps application code stable even if the lower-level native event shape
changes.

## Where Errors Surface

### Server startup

- `createSecureServer()` / `server.listen()` can throw `Http3Error` with `ERR_HTTP3_INVALID_STATE` if the server is already listening.
- TLS resolution can throw `Http3Error` with `ERR_HTTP3_TLS_CONFIG_ERROR` when credentials are missing or a `pfx` bundle cannot be decoded.

### HTTP/3 server sessions and streams

- `Http3SecureServer` emits `'error'` for server-level failures.
- `Http3ServerSession` emits `'error'` when the connection closes with a native session error.
- `ServerHttp3Stream` instances are destroyed with `Http3Error` when a stream reset or stream-scoped error occurs.
- Stream resets also emit `'aborted'` before teardown on request/response streams where applicable.

### HTTP/3 client sessions and request streams

- `Http3ClientSession#request()` throws `Http3Error` with `ERR_HTTP3_INVALID_STATE` if the session is not ready or if 0-RTT restrictions are violated.
- `Http3ClientSession` emits `'error'` for session-level transport failures.
- `ClientHttp3Stream` instances are destroyed with `Http3Error` on stream resets and stream-scoped native failures.
- GOAWAY is surfaced via the session `'goaway'` event, followed by normal close/drain behavior.

### Fetch/SSE/EventSource adapters

- `serveFetch()` converts handler throws into `500` responses on fallback HTTP/1.1/HTTP/2 paths.
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
- Allow in-flight work to finish if possible.
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

## Practical Recommendations

- Listen for `'error'`, `'close'`, and `'goaway'` on sessions, not just streams.
- Treat stream-level and session-level failures differently: stream errors can often be retried in place, session errors usually require reconnect.
- Keep retries idempotent and bounded.
- Log `code`, `quicCode`, and `h3Code` together so transport and application failures are easy to correlate.
