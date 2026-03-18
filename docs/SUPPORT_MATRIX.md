# Support Matrix

## Runtime

- Node.js: `>=24.0.0`
- Module target: Node ESM/CJS-compatible package output
- Browser compatibility smoke: Chromium + Firefox automated in CI over the HTTPS entrypoint, Safari validated manually via the release runbook

## Protocol Support

- HTTP/3: supported over QUIC/UDP via Rust worker transport
- HTTP/2: supported over TLS/TCP via Node `node:http2`
- Unified listener: both protocols on the same host/port
- QUIC-LB plaintext CID mode: supported via `quicLb + serverId`
- Raw QUIC: bidirectional streams, datagrams, session resumption, custom ALPN

## Transport Layer

- macOS: kqueue (`fast` and `portable`)
- Linux `fast`: io_uring
- Linux `portable`: readiness-based `poll(2)` + `eventfd`
- QUIC engine: quiche 0.26.1

## Platform Targets (N-API prebuild intent)

- Linux x64 (gnu)
- Linux arm64 (gnu)
- macOS arm64
- Other platforms may require local native compilation and are not part of the
  current prebuild release set.

## Runtime Modes By Environment

| Environment | `fast` | `portable` | `auto` |
| --- | --- | --- | --- |
| macOS | supported (`kqueue`) | supported (`kqueue`) | prefers `fast` |
| Native Linux with `io_uring` allowed | supported (`io_uring`) | supported (`poll`) | prefers `fast` |
| Ordinary Docker/Kubernetes on Linux | usually blocked by seccomp | supported | falls back to `portable` if allowed |
| Docker/Kubernetes with `seccomp=unconfined` or equivalent custom seccomp allowing `io_uring_*` | supported | supported | prefers `fast` |
| `privileged: true` container | supported | supported | prefers `fast` |

## Container / Deployment Notes

- Linux arm64 Docker Desktop is validated through the repository runtime matrix.
- `cap_add` alone did not restore `io_uring` in the tested default Docker seccomp profile.
- `seccomp=unconfined` restored the Linux fast path without requiring `privileged: true`.
- `privileged: true` remains a broad fallback, not the recommended default deployment mode.

## Production Environments

- ECS Fargate behind NLB with TCP/UDP 443 listeners
- EC2 behind NLB `QUIC` / `TCP_QUIC` listeners with `QuicServerId`
- Optional Cloudflare-only origin mode via mTLS + allowlisting

## Non-Goals For v1

- HTTP/1 as default protocol path (disabled unless explicitly enabled)
- WebTransport API parity
- Reverse-proxy/load-balancer features inside this package
