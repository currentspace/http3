# Support Matrix

## Runtime

- Node.js support floor: `>=24.0.0`
- CI-tested Node.js majors: `24`, `25`, and `26`
- Node.js 26 status: Current as of 2026-05-20; scheduled to enter LTS in
  October 2026 according to the Node.js release schedule.
- Module target: Node ESM/CJS-compatible package output
- Browser compatibility smoke: Chromium, Firefox, and WebKit automated over the HTTPS entrypoint with H3 protocol assertions

## Protocol Support

- HTTP/3: supported over QUIC/UDP via Rust worker transport
- HTTP/2: supported over TLS/TCP via Node `node:http2`
- Unified listener: both protocols on the same host/port
- QUIC-LB plaintext CID mode: supported via `quicLb + serverId`
- Raw QUIC: bidirectional streams, datagrams, session resumption, custom ALPN
- Feature-level support, experimental status, and deferred protocol work are
  tracked in [PROTOCOL_FEATURE_STATUS.md](./PROTOCOL_FEATURE_STATUS.md).

## Transport Layer

- macOS: kqueue (`fast` and `portable`)
- Linux `fast`: io_uring
- Linux `portable`: readiness-based `poll(2)` + `eventfd`
- QUIC engine: quiche 0.29

## Platform Targets (N-API prebuild intent)

- Prebuilds are shared across Node 24/25/26 through Node-API and are split by
  supported OS/arch/libc, not by `NODE_MODULE_VERSION`.
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

## WASM Client Runtime

- `runtimeMode: 'wasm'` runs the same HTTP/3 + raw QUIC **client** protocol
  core (quiche + BoringSSL) compiled to `wasm32-wasip1`, driven from Node.js
  over a `node:dgram` adapter — not a reimplementation, not a native-driver
  substitute.
- Protocol support: HTTP/3 client (no client mTLS, matching an existing
  native asymmetry) and raw QUIC client (mTLS supported) — no server
  support on either protocol.
- `runtimeMode: 'auto'` never selects `'wasm'`; it must be requested
  explicitly, and `fallbackPolicy` does not apply to it.
- See [WASM_RUNTIME.md](./WASM_RUNTIME.md) for build steps, usage examples,
  and limitations.

| Platform | Status |
| --- | --- |
| Node.js (same `>=24.0.0` floor as native) | Supported today via `runtimeMode: 'wasm'` |
| Cloudflare Workers / `workerd` | Designed for, not yet deployable — blocked on outbound UDP client sockets ([cloudflare/workerd discussion #4463](https://github.com/cloudflare/workerd/discussions/4463)) |

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
