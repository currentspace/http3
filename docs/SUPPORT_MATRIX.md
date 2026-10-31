# Support Matrix

## Runtime

- Node.js support floor: `>=24.0.0`
- Release qualification Node.js majors: `24` and `26` (exact versions pinned in workflows).
- Module target: Node ESM/CJS-compatible package output
- Browser compatibility smoke: Chromium, Firefox, and WebKit automated over the HTTPS entrypoint with H3 protocol assertions

## Protocol Support

- HTTP/3: supported over QUIC/UDP via native Rust workers or the WASM core
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
- WASM: quiche/BoringSSL over Node UDP sockets; no native driver needed
- QUIC engine: quiche 0.30

## Native package targets

Node-API prebuilds are split by OS/architecture/libc, not Node module version.
The following are the release build and installed-consumer targets. A configured
workflow is not proof that an unpublished sidecar has been qualified or released.

| Target | Sidecar suffix | Publication baseline |
| --- | --- | --- |
| Linux glibc x64 | linux-x64-gnu | Included in 1.0.0; previously available in 0.9.9 |
| Linux glibc arm64 | linux-arm64-gnu | Included in 1.0.0; previously available in 0.9.9 |
| macOS arm64 | darwin-arm64 | Included in 1.0.0; previously available in 0.9.9 |
| Linux musl x64 | linux-x64-musl | Added in 1.0.0 |
| Linux musl arm64 | linux-arm64-musl | Added in 1.0.0 |
| macOS Intel | darwin-x64 | Added in 1.0.0 |
| Windows x64 | none | WASM fallback in 1.0.0; no native driver |

The platform-coverage candidate passed native builds and installed consumers on
Node 24/26, including the three new targets and Windows WASM fallback. See
[source-bound qualification evidence](./PLATFORM_COVERAGE_EVIDENCE.md). These
results do not change the package contents already published as 0.9.9.

Other Node platforms may use the WASM fallback but are not qualified merely
because the generated native loader recognizes their names. Native Windows,
Android, FreeBSD, OpenHarmony, and other architectures are not release targets.

## Package layout and source builds

The root tarball no longer duplicates native binaries. Keep npm optional
dependencies enabled so the matching platform package is installed. Docs and
examples are available in the GitHub repository. WASM remains in the root.

Native startup failures name the OS/architecture and Linux libc, distinguish
unpublished targets from missing optional packages, and retain the original
loader errors as causes. See [startup error handling](./ERROR_HANDLING.md#native-binding-startup-failures).

There is no automatic native compilation during npm installation. Default
`auto` uses the bundled WASM runtime when native loading fails. Explicit native
modes require a matching sidecar or an absolute `NAPI_RS_NATIVE_LIBRARY_PATH`
pointing to a compatible source-built `.node` file. Source builds can load even
on an unpublished target. The packed-install test compiles installed declarations
with legacy `node`, `node16`, `nodenext`, and `bundler` resolution.

## Runtime Modes By Environment

| Environment | `fast` | `portable` | `auto` |
| --- | --- | --- | --- |
| macOS with an addon | supported (`kqueue`) | supported (`kqueue`) | prefers `fast` |
| Native Linux with `io_uring` allowed | supported (`io_uring`) | supported (`poll`) | prefers `fast` |
| Ordinary Docker/Kubernetes on Linux | usually blocked by seccomp | supported | falls back to `portable` if allowed |
| Docker/Kubernetes with `seccomp=unconfined` or equivalent custom seccomp allowing `io_uring_*` | supported | supported | prefers `fast` |
| `privileged: true` container | supported | supported | prefers `fast` |
| Native addon absent/unloadable | unsupported | unsupported | WASM if policy permits |

Linux native rows assume a matching libc sidecar. `portable` is a native poll
driver, not a synonym for WASM.

## WASM Runtime

- The same H3 and raw QUIC protocol core (quiche + BoringSSL) is compiled to
  `wasm32-wasip1` and driven from Node.js through `node:dgram` adapters.
- H3 and raw QUIC clients and servers are supported. Raw QUIC supports mTLS;
  H3 clients have no mTLS option in either runtime.
- Default `auto` selects WASM when the native addon cannot load, with a warning
  and structured metadata. Strict policy and explicit native modes reject it.
- Explicit WASM never probes the native loader.
- Windows x64, macOS arm64/Intel, and GNU Linux x64/arm64 installed fallback
  checks passed in `platform-fallback.yml`; Alpine x64/arm64 checks passed in the
  musl consumer lanes. The evidence document identifies the qualified source.
- See [WASM_RUNTIME.md](./WASM_RUNTIME.md) for options, memory limits, and the
  separate workerd client status. Node server support does not establish
  support for server sockets in Workers.

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
