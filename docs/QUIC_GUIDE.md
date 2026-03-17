# Raw QUIC Guide

This guide covers the raw QUIC API in `@currentspace/http3`. Raw QUIC gives you
bidirectional streams and datagrams without HTTP/3 framing — useful for custom
protocols, game networking, tunneling, and any use case where HTTP semantics are
unnecessary overhead.

## When to use raw QUIC vs HTTP/3

| | Raw QUIC | HTTP/3 |
|---|---|---|
| Framing | None — raw bytes on streams | HTTP request/response framing |
| Use case | Custom protocols, games, tunnels | Web APIs, browsers, standard HTTP |
| Streams | Arbitrary bidirectional streams | Request/response pairs |
| Datagrams | Supported | Not exposed |
| Entry point | `createQuicServer` / `connectQuic` | `createSecureServer` / `connect` |

## Echo server

```ts
import { createQuicServer } from '@currentspace/http3';

const server = createQuicServer({
  key: process.env.TLS_KEY_PEM,
  cert: process.env.TLS_CERT_PEM,
});

server.on('session', (session) => {
  session.on('stream', (stream) => {
    stream.pipe(stream); // echo everything back
  });
});

await server.listen(4433, '0.0.0.0');
console.log('QUIC echo server listening on :4433');
```

## Client connect (async)

`connectQuicAsync` waits for the QUIC handshake to complete before resolving:

```ts
import { connectQuicAsync } from '@currentspace/http3';

const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false, // for self-signed certs
});

const stream = session.openStream();
stream.end(Buffer.from('hello QUIC'));

const chunks: Buffer[] = [];
stream.on('data', (chunk) => chunks.push(chunk));
stream.on('end', () => {
  console.log('received:', Buffer.concat(chunks).toString());
});

// Clean up
stream.on('end', async () => {
  await session.close();
});
```

## Event-driven client

`connectQuic` returns a session immediately. Use `session.ready()` to wait for
the handshake, or attach event listeners:

```ts
import { connectQuic } from '@currentspace/http3';

const session = connectQuic('127.0.0.1:4433', {
  rejectUnauthorized: false,
});

await session.ready();

const stream = session.openStream();
stream.end(Buffer.from('hello'));

// ... read response, then close
await session.close();
```

## Multiple streams

QUIC multiplexes streams over a single connection with no head-of-line blocking:

```ts
const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
});

const results = await Promise.all(
  [0, 1, 2, 3, 4].map(async (i) => {
    const stream = session.openStream();
    stream.end(Buffer.from(`stream-${i}`));
    const chunks: Buffer[] = [];
    return new Promise<string>((resolve) => {
      stream.on('data', (c) => chunks.push(c));
      stream.on('end', () => resolve(Buffer.concat(chunks).toString()));
    });
  }),
);

console.log(results); // ['stream-0', 'stream-1', ...]
await session.close();
```

Client-initiated bidi stream IDs: 0, 4, 8, 12, ... (per QUIC spec).

## Server-initiated streams

The server can open streams toward the client:

```ts
// Server side
server.on('session', (session) => {
  const pushStream = session.openStream();
  pushStream.end(Buffer.from('server push'));
});

// Client side
const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
});

session.on('stream', (stream) => {
  const chunks: Buffer[] = [];
  stream.on('data', (c) => chunks.push(c));
  stream.on('end', () => {
    console.log('server sent:', Buffer.concat(chunks).toString());
  });
});
```

Server-initiated bidi stream IDs: 1, 5, 9, 13, ... (per QUIC spec).

## Custom ALPN

Both server and client accept an `alpn` option to negotiate application-layer
protocols:

```ts
const server = createQuicServer({
  key, cert,
  alpn: ['my-custom-proto'],
});

const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
  alpn: ['my-custom-proto'],
});
```

The default ALPN is `"quic"`. Both sides must agree on at least one protocol or
the handshake will fail.

## Datagrams

QUIC datagrams are unreliable, unordered messages sent outside of streams.
Enable on both server and client:

```ts
// Server
const server = createQuicServer({
  key, cert,
  enableDatagrams: true,
});

server.on('session', (session) => {
  session.on('datagram', (data) => {
    console.log('received datagram:', data.toString());
    session.sendDatagram(Buffer.concat([Buffer.from('echo:'), data]));
  });
});

// Client
const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
  enableDatagrams: true,
});

session.on('datagram', (data) => {
  console.log('server replied:', data.toString());
});

session.sendDatagram(Buffer.from('ping'));
```

Datagrams are best-effort — they may be dropped under congestion.

## Session resumption

After the first handshake, the server sends a session ticket. Store it and
pass it on subsequent connections for faster reconnection:

```ts
// First connection — capture the ticket
const session1 = connectQuic('127.0.0.1:4433', {
  rejectUnauthorized: false,
});

let ticket: Buffer | null = null;
session1.on('sessionTicket', (t) => {
  ticket = t;
});

await session1.ready();
// ... use session1 ...
await session1.close();

// Second connection — resume with stored ticket
const session2 = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
  sessionTicket: ticket!,
});

// Handshake completes faster with resumed session
```

If the ticket is invalid or expired, the connection falls back to a full
handshake automatically.

## Flow control tuning

Adjust QUIC flow control windows for your workload:

```ts
const server = createQuicServer({
  key, cert,
  initialMaxData: 10 * 1024 * 1024,          // 10 MB connection window
  initialMaxStreamDataBidiLocal: 1024 * 1024, // 1 MB per-stream window
  initialMaxStreamsBidi: 100,                 // max concurrent bidi streams
});

const session = await connectQuicAsync('127.0.0.1:4433', {
  rejectUnauthorized: false,
  initialMaxData: 10 * 1024 * 1024,
  initialMaxStreamDataBidiLocal: 1024 * 1024,
  initialMaxStreamsBidi: 100,
});
```

When a stream's write exceeds the flow control window, it transparently pauses
and resumes via QUIC's drain cycle — no application-level buffering needed.

## Session metrics

Inspect connection statistics at any time:

```ts
const metrics = session.getMetrics();
if (metrics) {
  console.log('packets in/out:', metrics.packetsIn, metrics.packetsOut);
  console.log('bytes in/out:', metrics.bytesIn, metrics.bytesOut);
  console.log('handshake time:', metrics.handshakeTimeMs, 'ms');
  console.log('RTT:', metrics.rttMs, 'ms');
  console.log('congestion window:', metrics.cwnd);
}
```

Returns `null` if the session has already closed.

## Connection limits

Limit the number of concurrent connections a server will accept:

```ts
const server = createQuicServer({
  key, cert,
  maxConnections: 1000,
  maxIdleTimeoutMs: 30000, // close idle connections after 30s
});
```

Connections beyond `maxConnections` are silently dropped. The idle timeout
applies to both server and client — set it on both sides for consistent
behavior.

## Graceful shutdown

Close individual sessions or the entire server:

```ts
// Close a single session (with optional error code and reason)
session.close(0, 'done');

// Close the entire server (closes all sessions, then the listener)
await server.close();
```

On the client side:

```ts
await session.close();
```

`server.close()` and `session.close()` both emit a `'close'` event when
complete.

## Error constants

Import QUIC transport error codes from the constants module:

```ts
import {
  QUIC_NO_ERROR,              // 0x00
  QUIC_INTERNAL_ERROR,        // 0x01
  QUIC_CONNECTION_REFUSED,    // 0x02
  QUIC_FLOW_CONTROL_ERROR,    // 0x03
  QUIC_STREAM_LIMIT_ERROR,    // 0x04
  QUIC_STREAM_STATE_ERROR,    // 0x05
  QUIC_FINAL_SIZE_ERROR,      // 0x06
  QUIC_FRAME_ENCODING_ERROR,  // 0x07
  QUIC_TRANSPORT_PARAMETER_ERROR, // 0x08
  QUIC_CONNECTION_ID_LIMIT_ERROR, // 0x09
  QUIC_PROTOCOL_VIOLATION,    // 0x0a
  QUIC_INVALID_TOKEN,         // 0x0b
  QUIC_APPLICATION_ERROR,     // 0x0c
  QUIC_CRYPTO_BUFFER_EXCEEDED, // 0x0d
  QUIC_KEY_UPDATE_ERROR,      // 0x0e
  QUIC_AEAD_LIMIT_REACHED,    // 0x0f
  QUIC_NO_VIABLE_PATH,        // 0x10
  QUIC_CRYPTO_ERROR,          // 0x0100 (range: 0x0100–0x01ff)
} from '@currentspace/http3';
```

Use these with `stream.close(code)` or `session.close(code, reason)`.

## QuicStream API

`QuicStream` extends Node.js `Duplex`. It works with `pipe()`, async
iteration, and all standard stream patterns.

| Property/Method | Description |
|---|---|
| `.id` | Stream ID (read-only) |
| `.write(data)` | Write data to the stream |
| `.end(data?)` | Write optional final data and send FIN |
| `.close(code?)` | Reset the stream with an error code (default 0) |
| `.pipe(dest)` | Pipe to another writable (e.g., echo with `stream.pipe(stream)`) |
| `.destroyed` | `true` after `close()` or `destroy()` |
| `'data'` event | Emitted for each chunk received |
| `'end'` event | Emitted when the remote side sends FIN |
| `'error'` event | Emitted on stream reset or transport error |
