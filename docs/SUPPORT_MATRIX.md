# Support Matrix

## Runtime

- Node.js: `>=24.0.0`
- Module target: Node ESM/CJS-compatible package output
- Browser validation: Chromium + Firefox automated in CI, Safari validated via release runbook

## Protocol Support

- HTTP/3: supported over QUIC/UDP via Rust worker transport
- HTTP/2: supported over TLS/TCP via Node `node:http2`
- Unified listener: both protocols on the same host/port
- QUIC-LB plaintext CID mode: supported via `quicLb + serverId`
- Raw QUIC: bidirectional streams, datagrams, session resumption, custom ALPN

## Transport Layer

- macOS: kqueue (platform-native I/O)
- Linux: io_uring (platform-native I/O)
- QUIC engine: quiche 0.26.1

## Platform Targets (N-API prebuild intent)

- Linux x64 (gnu)
- Linux arm64 (gnu)
- macOS x64
- macOS arm64
- Windows x64 (msvc)

## Production Environments

- ECS Fargate behind NLB with TCP/UDP 443 listeners
- EC2 behind NLB `QUIC` / `TCP_QUIC` listeners with `QuicServerId`
- Optional Cloudflare-only origin mode via mTLS + allowlisting

## Non-Goals For v1

- HTTP/1 as default protocol path (disabled unless explicitly enabled)
- WebTransport API parity
- Reverse-proxy/load-balancer features inside this package
