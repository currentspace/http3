# HTTP/2 Parity Matrix (v1)

This matrix defines compatibility expectations for users migrating from
`node:http2` to `@currentspace/http3`.

## Import surfaces

- Canonical API: `@currentspace/http3`
- Parity-focused API aliases: `@currentspace/http3/parity`
- HTTP/3-only extensions: `@currentspace/http3/h3`

## Server surface

| Area | `node:http2` | `@currentspace/http3` | Notes |
|---|---|---|---|
| Server constructor | `createSecureServer()` | `createSecureServer()` / parity `createServer()` | Dual-stack H3 + H2 on same port. |
| Stream callback | `on('stream', ...)` | `on('stream', ...)` | Same high-level pattern for request/response. |
| Session event | `on('session', ...)` | `on('session', ...)` | Includes H3 and H2 session adapters. |
| Addressing | `server.address()` | `server.address()` | Same shape `{ address, family, port }`. |
| Close | `server.close()` | `server.close()` | Async close with drain best effort. |
| HTTP/1 fallback | `allowHTTP1` option | `allowHTTP1` option | Disabled by default. |

## Client surface

| Area | `node:http2` | `@currentspace/http3` | Notes |
|---|---|---|---|
| Connect | `connect(authority, options)` | `connect(authority, options)` | Returns session immediately, emits `connect` on handshake completion. |
| Request | `session.request(headers)` | `session.request(headers)` | Same call pattern and stream-based lifecycle. |
| Session close | `session.close()` | `session.close()` | Supported for graceful shutdown. |
| Session destroy | `session.destroy()` | `session.destroy()` | Supported for immediate teardown. |
| Settings access | `remoteSettings` | `getRemoteSettings()` | Returned as normalized key/value map. |
| Ping | `session.ping()` | `session.ping()` | Returns most recent RTT snapshot. |

## Stream semantics

| Area | `node:http2` | `@currentspace/http3` | Notes |
|---|---|---|---|
| Duplex stream | yes | yes | Both request and response streams are duplex. |
| Response headers | `stream.respond()` | `stream.respond()` | Supported for server streams. |
| Trailers | `stream.sendTrailers()` | `stream.sendTrailers()` | Supported for H3 and adapted for H2. |
| Timeouts | `stream.setTimeout()` | `stream.setTimeout()` | Supported on client and server streams. |
| Flow-control drain | stream `drain` | stream `drain` | Backpressure supported through native worker events. |

## Explicit differences

- HTTP/3 transport details are exposed as extensions:
  - datagrams,
  - qlog/session ticket events,
  - QUIC-LB settings (`quicLb`, `serverId`).
- Remote settings are available through normalized APIs rather than raw
  `http2` session properties.
- Certain HTTP/2-specific internals have no direct QUIC equivalent and are not
  exposed as stable parity guarantees.

## Stability rules

- Behaviors in this matrix are semver-protected for v1.
- Additive parity improvements are minor releases.
- Breaking compatibility behavior changes require a major release.

