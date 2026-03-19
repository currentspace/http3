# Configuration Options Reference

This reference covers the public configuration types exposed by
`@currentspace/http3`.

Defaults below reflect the current runtime behavior in the JS wrappers and
native config builders. Where a field is optional and there is no internal
fallback, the default is listed as `none`.

For deployment/runtime guidance, see [`RUNTIME_MODES.md`](./RUNTIME_MODES.md).

## Relevant RFCs

- QUIC transport: [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000)
- QUIC TLS mapping: [RFC 9001](https://www.rfc-editor.org/rfc/rfc9001)
- HTTP/3: [RFC 9114](https://www.rfc-editor.org/rfc/rfc9114)
- QUIC DATAGRAM: [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221)
- Server-Sent Events format used by `EventSource`: [HTML Living Standard SSE](https://html.spec.whatwg.org/multipage/server-sent-events.html)

## `TlsOptions`

Used by `createSecureServer()` for server-side TLS material and related helpers.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `key` | `string \| Buffer` | `none` | PEM-encoded private key. Required unless `pfx` is provided. |
| `cert` | `string \| Buffer` | `none` | PEM-encoded certificate chain. Required unless `pfx` is provided. |
| `ca` | `string \| Buffer \| Array<string \| Buffer>` | `none` | PEM-encoded CA bundle used for peer verification on fallback TLS surfaces. |
| `pfx` | `Buffer` | `none` | PKCS#12 archive used instead of `key` + `cert`. |
| `passphrase` | `string` | `none` | Passphrase for `pfx`. |
| `alpnProtocols` | `string[]` | auto | Overrides ALPN on the fallback TLS server. If omitted, fallback ALPN is `['h2']` or `['h2', 'http/1.1']` when `allowHTTP1` is enabled. Native HTTP/3 ALPN remains fixed to `h3`. |
| `sessionTicketKeys` | `Buffer` | `none` | Shared TLS 1.3 session ticket keys for multi-instance resumption / 0-RTT. |
| `keylog` | `boolean \| string` | `false` | `true` creates an auto-managed keylog file; a string uses that exact path. |

## `ServerOptions`

Used by `createSecureServer()` and `serveFetch()`.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `runtimeMode` | `'auto' \| 'fast' \| 'portable'` | `'auto'` | Runtime selection policy for the QUIC/H3 worker. |
| `fallbackPolicy` | `'error' \| 'warn-and-fallback'` | `'warn-and-fallback'` | Only used when `runtimeMode` is `'auto'`. |
| `onRuntimeEvent` | `(info) => void` | `none` | Called when runtime selection resolves or falls back. |
| `maxIdleTimeoutMs` | `number` | `30000` | QUIC idle timeout in milliseconds. |
| `maxUdpPayloadSize` | `number` | `1350` | Max UDP payload size for send/receive paths. |
| `initialMaxData` | `number` | `100000000` | Connection-level flow-control window in bytes. |
| `initialMaxStreamDataBidiLocal` | `number` | `2000000` | Per-stream bidi flow-control window in bytes. |
| `initialMaxStreamsBidi` | `number` | `10000` | Max peer-initiated bidirectional streams. |
| `disableActiveMigration` | `boolean` | `true` | Disables QUIC active connection migration. Set `false` to enable migration. |
| `enableDatagrams` | `boolean` | `false` | Enables QUIC DATAGRAM support (RFC 9221). |
| `qpackMaxTableCapacity` | `number` | `none` | Passed to the HTTP/3 QPACK decoder/encoder config. |
| `qpackBlockedStreams` | `number` | `none` | Limits the number of blocked streams due to QPACK dynamic references. |
| `recvBatchSize` | `number` | `none` | Declared public option, but not currently wired into the native transport loop. |
| `sendBatchSize` | `number` | `none` | Declared public option, but not currently wired into the native transport loop. |
| `qlogDir` | `string` | `none` | Directory where per-connection qlog files are written. |
| `qlogLevel` | `string` | `none` | qlog verbosity string passed through to the native layer. |
| `metricsIntervalMs` | `number` | `1000` | Interval for emitting session `'metrics'` events. |
| `allowHTTP1` | `boolean` | `false` | Enables HTTP/1.1 fallback on the TLS listener alongside HTTP/2. |
| `settings` | `Record<string, number>` | Node defaults | Additional HTTP/2 settings for the fallback TLS server. |
| `disableRetry` | `boolean` | `false` | Disables QUIC Retry token validation for HTTP/3 server mode. |
| `maxConnections` | `number` | `10000` | Max concurrent QUIC connections accepted by the native worker. |
| `reusePort` | `boolean` | `false` | Enables `SO_REUSEPORT` on the UDP listener. |
| `quicLb` | `boolean` | `false` | Enables QUIC-LB plaintext connection ID encoding. |
| `serverId` | `Buffer \| string` | `none` | Required when `quicLb` is `true`; must be an 8-byte server ID (buffer or 16-digit hex string). |

## `ConnectOptions`

Used by `connect()` and `connectAsync()`.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `runtimeMode` | `'auto' \| 'fast' \| 'portable'` | `'auto'` | Runtime selection policy for the QUIC/H3 client worker. |
| `fallbackPolicy` | `'error' \| 'warn-and-fallback'` | `'warn-and-fallback'` | Only used when `runtimeMode` is `'auto'`. |
| `onRuntimeEvent` | `(info) => void` | `none` | Called when runtime selection resolves or falls back. |
| `ca` | `string \| Buffer \| Array<string \| Buffer>` | `none` | PEM-encoded CA bundle used to verify the remote certificate. |
| `rejectUnauthorized` | `boolean` | `true` | Set `false` to accept self-signed or otherwise untrusted certs. |
| `servername` | `string` | authority host | Overrides the SNI hostname sent during TLS handshake. |
| `maxIdleTimeoutMs` | `number` | `30000` | QUIC idle timeout in milliseconds. |
| `maxUdpPayloadSize` | `number` | `1350` | Max UDP payload size for send/receive paths. |
| `initialMaxData` | `number` | `100000000` | Connection-level flow-control window in bytes. |
| `initialMaxStreamDataBidiLocal` | `number` | `2000000` | Per-stream bidi flow-control window in bytes. |
| `initialMaxStreamsBidi` | `number` | `10000` | Max bidirectional streams advertised to the peer. |
| `sessionTicket` | `Buffer` | `none` | TLS session ticket for 0-RTT resumption attempts. |
| `allow0RTT` | `boolean` | `false` | Enables early data before handshake completion. |
| `keylog` | `boolean \| string` | `false` | `true` creates an auto-managed keylog file; a string uses that exact path. |
| `enableDatagrams` | `boolean` | `false` | Enables QUIC DATAGRAM support. |
| `allowUnsafe0RTTMethods` | `boolean` | `false` | Allows non-safe methods such as `POST` during 0-RTT. |
| `onEarlyData` | `(headers) => boolean` | `none` | Per-request override hook for unsafe 0-RTT requests. |
| `metricsIntervalMs` | `number` | `1000` | Interval for emitting session `'metrics'` events. |
| `qlogDir` | `string` | `none` | Directory where per-connection qlog files are written. |
| `qlogLevel` | `string` | `none` | qlog verbosity string passed through to the native layer. |

### Endpoint parameter

`connect()` / `connectAsync()` accept:

- authority strings such as `https://api.example.com:443`, `api.example.com:443`, or `127.0.0.1:443`
- `{ host, port, servername? }`
- `{ address, port, servername }`

Use `{ address, port, servername }` when the transport address differs from the
certificate identity.

## `RequestOptions`

Used by `Http3ClientSession#request()`.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `endStream` | `boolean` | `false` | Set `true` for body-less requests that should send FIN immediately. |

## `QuicServerOptions`

Used by `createQuicServer()`.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `key` | `Buffer \| string` | required | PEM-encoded private key. |
| `cert` | `Buffer \| string` | required | PEM-encoded certificate chain. |
| `ca` | `Buffer \| string` | `none` | PEM-encoded CA bundle used to verify client certificates. |
| `clientAuth` | `'none' \| 'request' \| 'require'` | `'require'` when `ca` is set, otherwise `'none'` | `'require'` closes clients that do not present a verifiable certificate; `'request'` accepts anonymous clients but records a verified client cert when one is presented. |
| `alpn` | `string[]` | `['quic']` | ALPN protocols advertised by the raw QUIC server. |
| `runtimeMode` | `'auto' \| 'fast' \| 'portable'` | `'auto'` | Runtime selection policy for the raw QUIC server worker. |
| `fallbackPolicy` | `'error' \| 'warn-and-fallback'` | `'warn-and-fallback'` | Only used when `runtimeMode` is `'auto'`. |
| `onRuntimeEvent` | `(info) => void` | `none` | Called when runtime selection resolves or falls back. |
| `maxIdleTimeoutMs` | `number` | `30000` | QUIC idle timeout in milliseconds. |
| `maxUdpPayloadSize` | `number` | `1350` | Max UDP payload size for send/receive paths. |
| `initialMaxData` | `number` | `100000000` | Connection-level flow-control window in bytes. |
| `initialMaxStreamDataBidiLocal` | `number` | `2000000` | Per-stream bidi flow-control window in bytes. |
| `initialMaxStreamsBidi` | `number` | `10000` | Max peer-initiated bidirectional streams. |
| `disableActiveMigration` | `boolean` | `true` | Disables QUIC active connection migration. |
| `enableDatagrams` | `boolean` | `false` | Enables QUIC DATAGRAM support. |
| `maxConnections` | `number` | `10000` | Max concurrent QUIC connections accepted by the server. |
| `disableRetry` | `boolean` | `true` | Disables QUIC Retry token validation in raw QUIC server mode. |
| `qlogDir` | `string` | `none` | Directory where per-connection qlog files are written. |
| `qlogLevel` | `string` | `none` | qlog verbosity string passed through to the native layer. |
| `keylog` | `boolean` | `false` | Enables TLS key logging for raw QUIC. |

`clientAuth: 'request'` and `clientAuth: 'require'` both require `ca`.
`clientAuth: 'none'` cannot be combined with `ca`.

## `QuicConnectOptions`

Used by `connectQuic()` and `connectQuicAsync()`.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `runtimeMode` | `'auto' \| 'fast' \| 'portable'` | `'auto'` | Runtime selection policy for the raw QUIC client worker. |
| `fallbackPolicy` | `'error' \| 'warn-and-fallback'` | `'warn-and-fallback'` | Only used when `runtimeMode` is `'auto'`. |
| `onRuntimeEvent` | `(info) => void` | `none` | Called when runtime selection resolves or falls back. |
| `ca` | `Buffer \| string` | `none` | PEM-encoded CA bundle used to verify the remote certificate. |
| `cert` | `Buffer \| string` | `none` | PEM-encoded client certificate chain for mutual TLS. Requires `key`. |
| `key` | `Buffer \| string` | `none` | PEM-encoded client private key for mutual TLS. Requires `cert`. |
| `rejectUnauthorized` | `boolean` | `true` | Set `false` to accept self-signed or otherwise untrusted certs. |
| `alpn` | `string[]` | `['quic']` | ALPN protocols offered by the raw QUIC client. |
| `servername` | `string` | authority host | Overrides the SNI hostname sent during TLS handshake. |
| `maxIdleTimeoutMs` | `number` | `30000` | QUIC idle timeout in milliseconds. |
| `maxUdpPayloadSize` | `number` | `1350` | Max UDP payload size for send/receive paths. |
| `initialMaxData` | `number` | `100000000` | Connection-level flow-control window in bytes. |
| `initialMaxStreamDataBidiLocal` | `number` | `2000000` | Per-stream bidi flow-control window in bytes. |
| `initialMaxStreamsBidi` | `number` | `10000` | Max bidirectional streams advertised to the peer. |
| `sessionTicket` | `Buffer` | `none` | TLS session ticket for 0-RTT resumption attempts. |
| `allow0RTT` | `boolean` | `false` | Enables early data before handshake completion. |
| `enableDatagrams` | `boolean` | `false` | Enables QUIC DATAGRAM support. |
| `keylog` | `boolean` | `false` | Enables TLS key logging for raw QUIC. |
| `qlogDir` | `string` | `none` | Directory where per-connection qlog files are written. |
| `qlogLevel` | `string` | `none` | qlog verbosity string passed through to the native layer. |

When either `cert` or `key` is provided, the other must also be provided.
Invalid client mTLS input combinations or malformed PEM material fail through
the public API with `ERR_HTTP3_TLS_CONFIG_ERROR`.

### Raw QUIC endpoint parameter

`connectQuic()` / `connectQuicAsync()` accept the same endpoint shapes as
`connect()` / `connectAsync()`, including Docker service-name URLs such as
`https://sfu:9080`.
