Below is a **detailed build spec** for a Node.js **HTTP/3 server + client** library that:

- uses **Cloudflare quiche** for **QUIC + HTTP/3**
- keeps UDP/timer/polling loops in Rust worker threads for **high performance**
- exposes an API that feels as close as possible to **`node:http2`**
- can be used by **Hono** and other “fetch-style” frameworks with **little/no modification**

I’m going to describe this as a package called `@yourorg/http3` with an optional alias `node:http3`-style surface.

---

## Current implementation status (Mar 2026)

- `createSecureServer()` is a unified dual-stack entrypoint:
  - HTTP/3 runs on Rust worker threads over UDP.
  - HTTP/2 runs on Node `node:http2` over TLS/TCP.
  - both bind to the same host+port.
- `allowHTTP1` defaults to `false` and is only enabled when explicitly requested.
- server `session.alpnProtocol` is protocol-aware (`'h3'` or `'h2'`).
- SSE is implemented with standards-compliant framing:
  - server helper: `createSseStream(...)`
  - fetch helper: `createSseFetchResponse(...)`
  - client helper: `createEventSource(...)` with reconnect + `Last-Event-ID`.

---

# 1) Goals and non-goals

## Goals

1. **HTTP/3 server and client** with TLS (QUIC mandates TLS 1.3).
2. **Idiomatic Node API** mirroring `http2`:
   - `createSecureServer()`, `connect()`
   - `'session'`, `'stream'` events
   - `.request()`, `.respond()`, `.close()`, `.destroy()`
   - stream objects behave like Node streams (backpressure, `'data'`, `'end'`)

3. **High performance runtime integration**
   - drive quiche with minimal overhead
   - avoid per-packet JS crossings
   - batching for UDP send/receive

4. **Framework compatibility**
   - Provide `fetch`-compatible adapter so frameworks like **Hono** work with minimal code changes.

5. **Cross-platform major Node runtimes**
   - Node 24+ across Linux/macOS/Windows; x64 + arm64 where possible.

## Non-goals (v1)

- Full WebTransport API (can be v2)
- HTTP/3 proxy (reverse proxy) features
- Perfect parity with every `http2` internal nuance (but close)

---

# 2) High-level architecture

```
┌───────────────────────────────────────────────────────────────┐
│ JS/TS Layer                                                   │
│  - http3 API (http2-like)                                     │
│  - streams: Http3ServerRequest/Response + ClientRequest       │
│  - fetch adapter (Request/Response)                           │
└───────────────┬───────────────────────────────────────────────┘
                │ (few crossings, event-batched)
┌───────────────▼───────────────────────────────────────────────┐
│ Native Addon (N-API + Rust workers)                            │
│  - UDP sockets owned in Rust                                   │
│  - worker-thread poll loops (mio + timers)                     │
│  - connection table + routing via Connection IDs               │
│  - per-connection send buffers + pooled retry/temp buffers      │
│  - event batching to JS via ThreadSafeFunction                  │
└───────────────┬───────────────────────────────────────────────┘
                │
┌───────────────▼───────────────────────────────────────────────┐
│ quiche (Rust)                                                  │
│  - QUIC transport                                              │
│  - HTTP/3 (qpack + control streams)                            │
└───────────────────────────────────────────────────────────────┘
```

### Core principle: keep the hot path native

- UDP receive → quiche `recv()` → quiche `timeout()` → quiche `send()` loops happen **in native**.
- JS sees **high-level events**: _new session_, _new stream_, _headers_, _body data_, _stream end_.

---

# 3) Package layout

## 3.1 Modules

- `@yourorg/http3`
  - `createSecureServer(options[, onStream])`
  - `connect(authority[, options])`
  - `constants`, `errors`

- `@yourorg/http3/fetch`
  - `createFetchHandler(appOrFetch)` for Hono + other frameworks
  - `serveFetch({ port, ...tls, fetch })`

- `@yourorg/http3/internal` (not public)
  - connection registry, stream wrappers, buffer pools, diagnostics

## 3.2 Native build

Recommended: **Rust + napi-rs** (since quiche is Rust, this removes a lot of FFI pain).

- Addon built as `.node`
- Prebuilds via GitHub Actions matrix:
  - Linux x64/arm64 (glibc + optionally musl)
  - macOS x64/arm64
  - Windows x64

Fallback: build-from-source supported.

---

# 4) Public API spec (http2-like)

## 4.1 Server API

### `createSecureServer(options[, onStream]) -> Http3SecureServer`

**Options (superset of tls + quiche tuning, worker-only transport):**

- TLS:
  - `key`, `cert` (PEM) OR `pfx`, `passphrase`
  - `ca`, `alpnProtocols` (for H2 TLS ALPN; H3 transport remains QUIC-native)
  - `sessionTicketKeys` (for resumption/0-RTT)

- QUIC tuning:
  - `maxIdleTimeoutMs` (default e.g. 30_000)
  - `maxUdpPayloadSize` (default 1350-ish)
  - `initialMaxData`, `initialMaxStreamDataBidiLocal`, `initialMaxStreamsBidi`
  - `disableActiveMigration` (default true for server simplicity)
  - `enableDatagrams` (default false)

- HTTP/3 tuning:
  - `qpackMaxTableCapacity`
  - `qpackBlockedStreams`

- Performance:
  - `recvBatchSize` (default 32/64)
  - `sendBatchSize` (default 32/64)
  - `reusePort` (default false; enables `SO_REUSEPORT` for multi-process UDP bind)
  - worker-thread transport is always enabled

- Node-ish:
  - `allowHTTP1` (default `false`; applies to TCP/H2 listener)
  - `settings` (applies to H2 server settings)

**Events:**

- `'session' (session: Http3ServerSession)`
- `'stream' (stream: ServerHttp3Stream, headers: IncomingHeaders, flags: StreamFlags)`
- `'error' (err)`
- `'close'`
- `'listening'`

**Methods:**

- `.listen(port[, host])`
- `.close([cb])` (stop accepting, close sockets, drain)
- `.address()` (like net.Server)

**Optional callback `onStream(stream, headers)`**

- sugar equivalent to `server.on('stream', ...)`

### Server session: `Http3ServerSession`

Mirrors `Http2Session` style.

- Properties:
  - `alpnProtocol` = `'h3'` or `'h2'`
  - `remoteAddress`, `remotePort`
  - `serverName` (SNI)
  - `handshakeComplete` boolean

- Events:
  - `'close'`, `'error'`
  - `'goaway'` (if implemented)
  - `'keylog'` (optional debug)

- Methods:
  - `.close([code])`
  - `.destroy([err])`
  - `.ping()` (optional)
  - `.getRemoteSettings()` (when available)

### Incoming stream: `ServerHttp3Stream`

Acts like `ServerHttp2Stream` but with H3 semantics.

- Events:
  - `'headers' (headers, flags)`
  - `'data'(chunk)`
  - `'end'`
  - `'trailers'(trailers)`
  - `'aborted'`
  - `'error'`
  - `'close'`

- Methods:
  - `.respond(headers[, options])` (sends response headers)
  - `.write(chunk)` / `.end([chunk])`
  - `.trailers(trailers)`
  - `.close([code])` (maps to STOP_SENDING/RESET_STREAM)
  - `.setTimeout(ms, cb)` (optional)

- Backpressure:
  - `.write()` returns boolean
  - `'drain'` event

- Headers:
  - must support pseudo headers: `:method`, `:path`, `:authority`, `:scheme`

---

## 4.2 Client API

### `connect(authority[, options]) -> Http3ClientSession`

- `authority`: `'https://example.com:443'` or `{ host, port, servername }`
- Options:
  - TLS: `ca`, `rejectUnauthorized`, `servername`
  - QUIC: similar tuning as server
  - session resumption / 0-RTT controls:
    - `sessionTicket` (optional)
    - `allow0RTT` (default false)

- Methods:
  - `.request(headers[, options]) -> ClientHttp3Stream`
  - `.close()`, `.destroy()`

- Events:
  - `'connect'`
  - `'close'`, `'error'`
  - `'keylog'` (optional)

### Client stream: `ClientHttp3Stream`

Similar to `ClientHttp2Stream`.

- Events:
  - `'response'(headers, flags)` (first headers)
  - `'data'`, `'end'`, `'trailers'`, `'error'`, `'close'`

- Methods:
  - `.write()`, `.end()`
  - `.close([code])`
  - `.setTimeout()`

---

# 5) Fetch / Hono compatibility layer (critical)

Frameworks like **Hono** typically want an entrypoint like:

```ts
fetch(req: Request): Response | Promise<Response>
```

So you provide a server helper that converts an HTTP/3 stream to a Fetch `Request`, calls the handler, then writes a `Response` back.

## 5.1 `createFetchHandler(appOrFetch)`

Accepts:

- a function `(req: Request) => Response | Promise<Response>`
- OR a Hono app instance with `.fetch`

Returns:

- `(stream, headers) => void` suitable for `server.on('stream', ...)`

### Mapping rules

- Incoming H3 headers → Fetch Request:
  - `:method` → `req.method`
  - `:path` + `:authority` + `:scheme` → `req.url`
  - other headers copied, lowercased

- Body handling:
  - If no body (GET/HEAD), `req.body = null`
  - Otherwise create a **ReadableStream** backed by the Node stream (or a web-stream shim)

- Response handling:
  - `Response.status`, `Response.headers`
  - Body is streamed back with backpressure

## 5.2 `serveFetch({ port, tls, fetch, ... })`

One-liner:

- creates server
- attaches fetch adapter
- listens

This lets Hono users do:

```ts
import { serveFetch } from "@yourorg/http3/fetch";
import { Hono } from "hono";

const app = new Hono();
app.get("/", (c) => c.text("hi"));

serveFetch({ port: 443, key, cert, fetch: app.fetch });
```

**No framework changes required.**

---

# 6) Transport loop integration (current implementation)

This is the heart of it.

## 6.1 Worker-owned UDP socket model

- Server: one Rust worker thread owns the listener UDP socket.
- Client: one Rust worker thread owns the client UDP socket.
- Both loops use mio polling + timeout scheduling in Rust.

## 6.2 Native hot loop design

1. Poll UDP socket readiness and command queue.
2. Receive packets, route by connection ID, and call `quiche_conn_recv()`.
3. Poll HTTP/3 events, drain flow-control writes, and flush `quiche_conn_send()` packets.
4. Handle timeouts (`quiche_conn_on_timeout()`) and closed-session cleanup.
5. Deliver event batches to JS through a TSFN callback.

## 6.3 JS event delivery strategy (avoid per-frame crossings)

- Native maintains an **event ring buffer** per tick:
  - `NEW_SESSION`, `NEW_STREAM`, `HEADERS`, `DATA`, `FIN`, `RESET`, `CLOSE`

- After processing packets, native issues one TSFN callback with a compact event batch.
- JS demuxes events and emits Node events on the right objects.

This prevents “one JS callback per packet” (which kills performance).

---

# 7) Threading/runtime model

Current implementation is worker-only:

- Server transport loop runs in a dedicated Rust worker thread.
- Client transport loop runs in a dedicated Rust worker thread.
- Node/TS layer performs API object orchestration and event emission only.
- No JS-owned UDP/dgram transport loop remains in runtime paths.

---

# 8) Streams and backpressure semantics

## 8.1 Node stream abstraction

Each HTTP/3 stream should present:

- readable side (request body / response body)
- writable side (response body / request body)

Implementation options:

- Use actual Node `Duplex` in JS backed by native methods:
  - JS `.write()` calls into native `streamSendData()`
  - native returns “blocked” status
  - JS emits `'drain'` when native indicates writable again

## 8.2 Flow control mapping

- QUIC stream flow control window changes come from quiche.
- When quiche indicates stream is blocked:
  - `.write()` returns false

- When window opens:
  - emit `'drain'`

## 8.3 Data chunking

- Native should coalesce small writes into larger QUIC frames when possible.
- Avoid copying:
  - accept Node `Buffer` in napi and pass pointer+len to quiche as long as lifetime is safe
  - if not safe, copy into pooled native buffers

---

# 9) HTTP/3 semantics mapping to http2-like API

## 9.1 Header representation

- Use Node’s common lowercased header map:
  - `IncomingHeaders` as `{ [k: string]: string | string[] }`

- Preserve pseudo headers:
  - `:method`, `:path`, `:authority`, `:scheme`

- Provide helper:
  - `http3.headersToHttp2Style()` (optional)

## 9.2 `respond()`

- Sends HEADERS frame.
- If `endStream` option set, immediately FIN.

## 9.3 Trailers

- `.trailers(obj)` sends trailing HEADERS

## 9.4 Errors and codes

Expose:

- QUIC transport error codes
- HTTP/3 error codes (H3_NO_ERROR, H3_GENERAL_PROTOCOL_ERROR, etc.)
  Map into Node-ish errors:
- `ERR_HTTP3_STREAM_ERROR`
- `ERR_HTTP3_SESSION_ERROR`
  Include `.code`, `.quicCode`, `.h3Code`

---

# 10) TLS, ALPN, session resumption, 0-RTT

## 10.1 TLS configuration

- quiche uses BoringSSL in many builds; ensure your build story is consistent.
- Support:
  - key/cert
  - SNI selection callback (optional v2)
  - keylog for debugging (opt-in)

## 10.2 Session tickets

- Provide hooks to store/reuse tickets on client:
  - `'sessionTicket'` event on client session

- Server uses `sessionTicketKeys` for stateless resumption.

## 10.3 0-RTT policy

- Default: **off**
- Option: `allow0RTT: true`
- Enforce safe methods only by default (GET/HEAD)
- Provide `onEarlyData(req)` hook if you want app-level gatekeeping

---

# 11) Diagnostics and observability (must-have for shipping)

## 11.1 qlog support

- quiche can produce qlog events; expose:
  - `session.exportQlog()` or stream to file

- Provide config:
  - `qlogDir`, `qlogLevel`

## 11.2 Metrics hooks

Expose counters:

- packets in/out
- bytes in/out
- handshake time
- RTT estimate
- cwnd/pacing if available

Provide event:

- `'metrics'(snapshot)` per interval

---

# 12) Build + distribution requirements

1. Prebuild `.node` binaries for major platforms.
2. `npm install` should “just work” with prebuild; fallback build from source.
3. Pin quiche version and track its releases.
4. Provide reproducible builds (CI).
5. Provide `--features` toggles:
   - enable datagrams
   - enable qlog
   - enable keylog

---

# 13) Compatibility with “app servers” (Hono, etc.)

## 13.1 Primary integration: fetch adapter

This is the least disruptive approach and works across:

- Hono
- itty-router
- many “web standard” servers

## 13.2 Secondary integration: Node http2-style interface

For Node-native apps that already use `http2`:

- Offer an adapter that provides `req`/`res`-like objects:
  - `req.headers`, `req.method`, `req.url`, readable body
  - `res.writeHead(status, headers)`, `res.write`, `res.end`
    This lets Express-like middleware be adapted (not always perfect, but workable).

---

# 14) Minimal v1 milestone plan (deliverable slices)

## Milestone 1: Transport + basic server

- UDP listener
- quiche handshake
- create sessions
- accept request streams
- respond with headers + body
- simple example server

## Milestone 2: Backpressure + batching

- stream write returns boolean + drain
- batch event delivery JS
- send/recv batch support

## Milestone 3: Client + request API

- connect + request + streaming response
- basic redirects left to user

## Milestone 4: Fetch adapter + Hono example

- `serveFetch`
- streaming request/response bodies

## Milestone 5: Resumption, qlog, metrics

- session tickets
- qlog export
- observability hooks

---

# 15) “As close as http2” feature parity checklist

Implement (v1):

- ✅ `createSecureServer`, `connect`
- ✅ `'stream'` event
- ✅ `.request()` / `.respond()`
- ✅ streaming bodies + backpressure
- ✅ trailers
- ✅ session close/destroy
- ✅ common error shapes

Nice-to-have (v2):

- server push (HTTP/3 has it conceptually removed/changed; most clients don’t want it)
- priority signals (HTTP/3 has different prioritization model)

---

## If you want, I can turn this into:

1. a **TypeScript `.d.ts` API contract** (public types, events, interfaces), and
2. a **native module contract** (exact event batch layout, structs, and napi entrypoints),
   so it becomes a build-ready engineering document rather than just a design spec.
