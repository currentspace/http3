# Local Docker + Load Test Guide

This runbook starts the unified H3/H2 server in Docker and runs load tests
using either curl (`--http3-only` / `--http2`) or a Rust QUIC client.

For the Linux runtime-mode matrix (`portable`, `auto`, `fast`, seccomp checks),
use [`RUNTIME_MODES.md`](./RUNTIME_MODES.md) plus:

```bash
npm run test:docker:runtime
```

For raw QUIC runtime/driver benchmarking inside Linux containers, use:

```bash
npm run bench:quic:docker
```

For the matching HTTP/3 matrix, use:

```bash
npm run bench:h3:docker
```

Both Docker runners execute the same runtime lanes and can persist a matrix
artifact with embedded per-lane benchmark summaries:

```bash
npm run bench:quic:docker -- --results-dir perf-results --label quic-docker
npm run bench:h3:docker -- --results-dir perf-results --label h3-docker
```

These run the benchmark in Docker across:

- ordinary container `portable`
- ordinary container `auto` fallback
- ordinary container `fast` failure diagnostic
- `cap_add: SYS_ADMIN` fast failure diagnostic
- `seccomp=unconfined` `portable`
- `seccomp=unconfined` `fast`
- mixed `portable`/`fast` client-server lanes

To also compare the broader `privileged: true` fast lane:

```bash
npm run bench:quic:docker:privileged
npm run bench:h3:docker:privileged
```

Forward additional benchmark knobs after `--`, for example:

```bash
npm run bench:quic:docker -- --profile balanced --rounds 3
```

For host-side shared-reactor benchmarks with the same internal telemetry surface,
use:

```bash
npm run bench:quic -- --profile smoke --results-dir perf-results --label quic-host
npm run bench:h3 -- --profile smoke --results-dir perf-results --label h3-host
```

Those summaries now include:

- selected runtime/driver
- driver setup attempts/successes
- worker spawn counts
- shared-worker creation/reuse counts
- session open/close counts
- backend-specific queue/backlog counters for `io_uring` and `kqueue`
- TX buffer recycle counts

For the profiler workflow, comparison matrix, and baseline criteria, see
[`PERF_PROFILING.md`](./PERF_PROFILING.md).

## 1) Generate local TLS certs

```bash
./examples/certs/generate.sh
```

This creates:

- `examples/certs/server.crt`
- `examples/certs/server.key`

## 2) Build and run container

```bash
npm run docker:build
npm run docker:up
```

Default local ports:

- `8443/tcp` -> container `443/tcp` (H2)
- `8443/udp` -> container `443/udp` (H3)
- `8080/tcp` -> health endpoints

## 3) Smoke checks

Readiness:

```bash
curl -sSf http://127.0.0.1:8080/readyz
```

HTTP/3:

```bash
curl --http3-only -k https://127.0.0.1:8443/
```

HTTP/2:

```bash
curl --http2 -k https://127.0.0.1:8443/
```

## 4) Curl-based load tests

HTTP/3 load:

```bash
npm run load:curl:h3
```

HTTP/2 load:

```bash
npm run load:curl:h2
```

Tune parameters:

```bash
PROTO=h3 TOTAL=2000 CONCURRENCY=100 MAX_TIME=10 bash scripts/load-curl-http.sh https://127.0.0.1:8443/
```

The script prints:

- success/failures
- elapsed time and RPS
- avg/p50/p95/p99 latency (ms)

## 5) Rust QUIC client load

```bash
npm run load:rust:h3
```

Custom run:

```bash
cargo run --release --manifest-path tools/rust-h3-load/Cargo.toml -- \
  --addr 127.0.0.1:8443 \
  --host localhost \
  --path / \
  --clients 20 \
  --requests-per-client 50 \
  --timeout-ms 5000 \
  --verify-peer false
```

## 6) Inspect logs and stop

```bash
npm run docker:logs
npm run docker:down
```

## Notes

- If your local curl lacks HTTP/3 support, H3 load tests will fail quickly.
- Use the Rust load client path as a fallback for H3 benchmarking.
- For privileged host port 443 mapping, adjust `docker-compose.yml` published
  ports from `8443` to `443` for both TCP and UDP.
- Ordinary Linux containers usually need `runtimeMode: 'portable'`; Linux
  `fast` mode typically requires a seccomp policy that allows `io_uring_*`.
- On macOS, both `fast` and `portable` use `kqueue`; the benchmarks still show
  shared client-worker reuse so you can separate backend choice from topology.
