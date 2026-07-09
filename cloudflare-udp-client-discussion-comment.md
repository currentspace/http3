I would like to add a concrete request for outbound UDP client support in Workers, scoped narrowly enough to be useful for QUIC/HTTP/3 clients without requiring a full general-purpose UDP server surface.

Current state, as I understand it:

- `cloudflare:sockets` gives Workers a TCP client surface, but I do not see an equivalent datagram/UDP client API.
- The Workers Node.js compatibility docs list `node:dgram` as a non-functional stub enabled with `nodejs_compat` on or after `2026-01-29`.
- The `workerd` implementation appears intentionally import-compatible rather than transport-capable; it lets packages load, but it is not suitable for application UDP traffic.

The minimum useful API for client-side QUIC/HTTP/3 would be much smaller than full Node `dgram` support:

- outbound-only UDP client sockets
- connect/send/receive/close
- destination host/port
- error/close signaling
- datagram payloads of at least QUIC's 1200-byte minimum initial packet size, preferably enough for ordinary path MTU-sized packets
- no multicast, broadcast, UDP server listen socket, or arbitrary privileged binding needed for this use case

Either of these shapes would unblock real client work:

- a Workers-native `cloudflare:sockets` datagram API, for example `connectDatagram({ hostname, port })` with readable/writable datagram streams, or
- a functional outbound-client subset of `node:dgram`, such as `createSocket("udp4").connect(port, host)`, `send()`, message events, close, and error events.

This would unlock Workers-hosted clients for protocols that are currently unreachable from Workers despite fitting the rest of the isolate model well: HTTP/3/QUIC, DNS transports that are not using fetch/DoH, syslog/metrics sinks, game/server status protocols such as Minecraft Bedrock, and other UDP-based client protocols.

For my immediate case, I am working on an HTTP/3 client library that can be structured as a sans-IO QUIC/HTTP/3 core plus a small host datagram adapter. Workers already have enough WebAssembly/JS capability for the protocol logic; the missing primitive is just outbound datagram I/O.
