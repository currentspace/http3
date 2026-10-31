# Hosted native platform qualification

[Release run 41151580339](https://github.com/currentspace/http3/actions/runs/41151580339) passed at `74a7c977db89b79c0fb71630e55b13cbdfd7e261`: 29 successful jobs; publication was skipped. [summary.json](summary.json) indexes 18 native job receipts with their original SHA256 hashes. Paths in JSON are relative to the repository root. Receipts preserve the captured CI text, including temporary paths quoted by build tools.

All six native prebuilds passed. Both musl builds ran 54 transport tests. All twelve installed native consumer combinations passed public H2/H3 and raw QUIC checks and completed 1,000 portable GETs at concurrency 50. Rates in logs are informational.

| Hosted runtime | Observed native auto selection |
| --- | --- |
| macOS arm64 and x64, Node 24/26 | fast / kqueue |
| GNU Linux arm64, Node 24/26 | fast / io_uring |
| GNU Linux x64, Node 24/26 | portable / poll; `EINVAL` at `io_uring_register_buf_ring`, kernel `6.8.0-1070-azure` |
| Alpine arm64 and x64, Node 24/26 | portable / poll; `EPERM` at `io_uring_setup` under Docker |

The six combinations with unavailable fast paths also passed eight strict-policy rejections and warning deduplication. The conditional strict-rejection branch does not apply when fast startup succeeds. Alpine additionally passed the no-sidecar WASM fallback consumer on both architectures and Node versions.

[pbuf-fault/summary.json](pbuf-fault/summary.json) records a separate, local deterministic proof against source `1c1c2215d0da043fac6ab77dabaf01098d23c6eb`. Its profile rejects only PBUF_RING registration with `EINVAL`. Exact package/addon hashes, the original fixture, the additional warning-count assertion, syscall excerpts, and Node 24/26 acceptance receipts are distinguished explicitly. It is not a claim that those local tarballs were the hosted run's artifacts.

The supplied spec request 2 literally requires no fallback warning for Alpine auto. Hosted default Docker does not meet that literal clause: the correct musl addon stays native but emits one `fast-path-unavailable` warning when io_uring setup is denied. Explicit portable mode emits no warning, and installed-native auto never selects WASM. Warning-free Alpine auto requires an environment permitting its fast driver; these Docker receipts do not establish that case. This is an acceptance distinction, not a reason to suppress the diagnostic or call the observed warning-free.
