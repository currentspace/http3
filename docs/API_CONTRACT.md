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
