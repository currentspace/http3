# API Contract (v1)

This document defines the public API surface that is semver-protected for the
first production-ready release line.

## Stable Entry Points

- Root package: `@currentspace/http3`
- Subpath exports:
  - `@currentspace/http3/parity`
  - `@currentspace/http3/h3`
  - `@currentspace/http3/fetch`
  - `@currentspace/http3/sse`
  - `@currentspace/http3/eventsource`
  - `@currentspace/http3/ops`
  - `@currentspace/http3/aws-cert`
  - `@currentspace/http3/express`

## Stable Server API

- `createSecureServer(options, onStream?)`
- parity alias: `createServer(options, onStream?)`
- `Http3SecureServer#listen()`
- `Http3SecureServer#close()`
- `Http3SecureServer#address()`
- session methods:
  - `session.getMetrics()`
  - `session.ping()`
  - `session.getRemoteSettings()`
  - `session.exportQlog()`
- `stream` handler signature:
  - `(stream, headers, flags)`
  - identical call pattern for H3 and H2 traffic
- stable server options additions:
  - `quicLb?: boolean`
  - `serverId?: Buffer | string` (8-byte ID, hex accepted)
  - `sessionTicketKeys?: Buffer` (shared ticket key material for resumption / 0-RTT across instances)

## Stable Fetch/SSE API

- `createFetchHandler()`
- `serveFetch()`
- `createSseStream()`
- `createSseFetchResponse()`
- `Http3EventSource` / `createEventSource()`
- grouped extension namespace: `h3.*`

## Stable Client Connect API

- `connect(authority, options?)` (event-driven)
- `connectAsync(authority, options?)` (awaitable handshake helper)

## Stable QUIC Server API

- `createQuicServer(options)` → `QuicServer`
- `QuicServer#listen(port, host?)` → `Promise<{ address, family, port }>`
- `QuicServer#close()` → `Promise<void>`
- `QuicServer#address()` → `{ address, family, port } | null`
- `QuicServer` events: `'session'`, `'listening'`, `'close'`
- `QuicServerSession#openStream()` → `QuicStream`
- `QuicServerSession#close(errorCode?, reason?)`
- `QuicServerSession#sendDatagram(data)`
- `QuicServerSession#getMetrics()` → `{ packetsIn, packetsOut, bytesIn, bytesOut, handshakeTimeMs, rttMs, cwnd } | null`
- `QuicServerSession#ping()`
- `QuicServerSession#peerCertificatePresented` (read-only boolean)
- `QuicServerSession#getPeerCertificate()` → `X509Certificate | null`
- `QuicServerSession#getPeerCertificateChain()` → `readonly X509Certificate[]`
- `QuicServerSession` events: `'stream'`, `'datagram'`, `'close'`, `'error'`

## Stable QUIC Client API

- `connectQuic(authority, options?)` → `QuicClientSession` (event-driven)
- `connectQuicAsync(authority, options?)` → `Promise<QuicClientSession>` (awaitable handshake)
- `QuicClientSession#ready()` → `Promise<void>`
- `QuicClientSession#openStream()` → `QuicStream`
- `QuicClientSession#close()` → `Promise<void>`
- `QuicClientSession#sendDatagram(data)`
- `QuicClientSession#getMetrics()` → `{ packetsIn, packetsOut, bytesIn, bytesOut, handshakeTimeMs, rttMs, cwnd } | null`
- `QuicClientSession#ping()`
- `QuicClientSession#handshakeComplete` (read-only boolean)
- `QuicClientSession` events: `'connect'`, `'stream'`, `'datagram'`, `'sessionTicket'`, `'close'`, `'error'`

## Stable QUIC Stream

- `QuicStream` (extends `Duplex`)
- `.id` — stream ID (read-only)
- `.close(code?)` — reset the stream with error code (default 0)
- Standard Duplex methods: `.write()`, `.end()`, `.pipe()`, `.destroy()`

## Stable QUIC Options

- `QuicServerOptions`: `key`, `cert`, `ca?`, `clientAuth?`, `alpn?`, `maxIdleTimeoutMs?`, `maxUdpPayloadSize?`, `initialMaxData?`, `initialMaxStreamDataBidiLocal?`, `initialMaxStreamsBidi?`, `disableActiveMigration?`, `enableDatagrams?`, `maxConnections?`, `disableRetry?`, `qlogDir?`, `qlogLevel?`, `keylog?`
- `QuicConnectOptions`: `ca?`, `cert?`, `key?`, `rejectUnauthorized?`, `alpn?`, `servername?`, `maxIdleTimeoutMs?`, `maxUdpPayloadSize?`, `initialMaxData?`, `initialMaxStreamDataBidiLocal?`, `initialMaxStreamsBidi?`, `sessionTicket?`, `allow0RTT?`, `enableDatagrams?`, `keylog?`, `qlogDir?`, `qlogLevel?`

When `QuicServerOptions.ca` is provided and `clientAuth` is omitted, raw QUIC
servers default to `clientAuth: 'require'`. Use `clientAuth: 'request'` only
when you intentionally want to allow anonymous clients while still capturing a
validated client certificate when one is presented.

When a raw QUIC peer certificate is available through `QuicServerSession`, it is
exposed as Node.js `X509Certificate` objects so applications can inspect
fingerprints, subject/issuer fields, validity windows, and raw DER bytes using
the standard `node:crypto` API.

## QUIC Error Constants

- `QUIC_NO_ERROR` (0x00) through `QUIC_CRYPTO_ERROR` (0x0100)
- See [QUIC_GUIDE.md](./QUIC_GUIDE.md) for the full list and usage.

## Stable Runtime Ops API

- `createHealthController()`
- `startHealthServer()`
- `installGracefulShutdown()`
- `loadTlsOptionsFromAwsEnv()`

## Semver Policy

- Additive API changes are `minor`.
- Behavior changes that alter wire compatibility, default security posture, or
  public TypeScript signatures are `major`.
- Bug fixes with unchanged API and intended behavior are `patch`.

## Backward Compatibility Rules

- `createSecureServer()` remains the canonical entrypoint for unified H3/H2.
- Existing H3-only code must continue to run unchanged.
- All public exports listed above require deprecation in one minor release
  before removal in the next major release.
