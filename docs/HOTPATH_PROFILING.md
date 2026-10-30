# Profiling the current build

`scripts/profile-h3-hotpaths.mjs` exercises the current native and WASM builds in
separate server and load-generator processes. It does not check out another
revision or apply throughput thresholds.

Build the artifacts from the sources you want to inspect:

```sh
npm run build:test
npx napi build --platform --release
npm run patch:native-loader
# Requires the WASI SDK and wasm-opt used by the repository build script.
WASI_SDK_PATH=/path/to/wasi-sdk npm run build:wasm
```

Run the harness with a new output directory:

```sh
node scripts/profile-h3-hotpaths.mjs \
  --duration-ms 10000 --warmup-ms 2000 \
  --out perf-results/hotpaths-current --sample
node scripts/summarize-h3-hotpaths.mjs perf-results/hotpaths-current
```

`--sample` adds macOS all-thread stack sampling of the actual server process.
Omit it on other platforms. Use `--cases` with a comma-separated selection:

| Case | Connections | Workload |
| --- | ---: | --- |
| `wasm-fanout` | 64 | WASM server, 256 KiB responses |
| `native-fanout` | 64 | Native server, 256 KiB responses |
| `native-bulk` | 8 | 1 MiB uploads and 4 MiB responses; half the readers pause after each chunk |
| `wasm-bulk` | 8 | Same upload/response pattern through the WASM server |
| `native-upload-fanout` | 64 | 256 KiB uploads and 1 KiB responses to exercise request event batches |
| `express-small` | 16 | Express 5 cookies and approximately 1 KiB JSON responses |
| `mixed-sse` | 32 | One 100 ms SSE stream per connection alongside 1 KiB requests |

Clients use the native portable runtime. Each connection has one ordinary
request outstanding at a time. Sessions are established before warmup; first
request timings are recorded separately from the steady-state histogram.
The certificate is generated for the run without installing trust. All traffic
uses loopback, with client certificate verification disabled for this fixture.

Each case writes:

- `server.cpuprofile` and `client.cpuprofile`: V8 CPU samples, loadable in Chrome
  DevTools. These cover each main isolate, not the native Rust worker threads.
- `server.heapprofile` and `client.heapprofile`: sampled JavaScript allocations,
  including objects collected during measurement. These are allocation estimates,
  not retained-heap snapshots, and exclude Rust/C allocations.
- `server.json` and `client.json`: memory, GC, CPU usage, event-loop delay,
  request latency, per-connection progress, SSE chunk-arrival gaps, errors, and
  native telemetry. Server tracking counts and memory after GC are diagnostic
  observations of private implementation fields.
- `server.sample.txt` on macOS when requested: all-thread stacks, including
  native Rust and blocked threads. Sample counts are not CPU utilization.
- Logs and the exact case configuration. The top-level manifest records the
  source revision, dirty status, runtime, host, and native/WASM binary hashes.
  `source.diff` captures tracked working-tree changes against HEAD, and
  `harness.mjs.txt` saves the harness used for the run; both hashes are recorded.
  Untracked source files are listed in the dirty status but are not in that diff.

The summarizer writes `summary.json` with weighted CPU self/inclusive totals,
allocation estimates, and connection progress. Inclusive percentages overlap;
do not add a caller's percentage to its callee's. The bulk cases deliberately
have fast and slow readers, so their aggregate fairness index is not a
starvation test. SSE gaps measure received chunks, not exact application-event
latency. Throughput includes requests started during the measurement interval
and completed during the recorded drain period.

For named WASM frames, generate a function map from the same unstripped build,
using exactly the production optimization flags. Verify that the output binary
hash matches `dist/wasm/http3_client.wasm` before supplying the map as the
summarizer's second argument. A map from a differently optimized build is invalid.

```sh
wasm-opt -Oz --print-function-map --strip-debug \
  target/wasm32-wasip1/release/http3_wasm.wasm \
  -o /tmp/http3-symbol-check.wasm > /tmp/http3-functions.txt
shasum -a 256 /tmp/http3-symbol-check.wasm dist/wasm/http3_client.wasm
# Optional: llvm-cxxfilt can demangle the names in the function map.
node scripts/summarize-h3-hotpaths.mjs perf-results/hotpaths-current \
  /tmp/http3-functions.txt
```

The Express workload can expose retained completed streams. This bounded
reproducer runs without the profiling harness or a per-stream tracking Set,
forces GC between phases, and reports the server's own tracking map:

```sh
node --expose-gc scripts/probe-h3-express-retention.mjs /tmp/retention.json
```

It sends 1,000 raw GETs, 1,000 GETs through the Node facade, and two groups of
1,000 Express GETs. A nonzero accumulated stream count after the responses finish
is a finding to investigate; the script reports observations rather than
asserting that the current implementation is correct.

CPU/allocation sampling adds overhead. These are diagnostic workloads, not
production capacity measurements. In particular, short loopback runs do not
qualify Linux io_uring, packet loss, cancellation storms, HTTP/2, browser behavior,
long-term memory stability, or pure cold-start allocation latency. Native clients
share a UDP port, so their traffic can concentrate on one native server worker;
this harness does not measure scaling across independent remote clients.

## Measured run: 2026-10-30

The seven workloads completed 172,794 measured requests with zero recorded
transfer errors on macOS ARM64 with Node 26.13.0. Every ordinary workload
drained to zero tracked streams; the mixed workload retained its 32 live SSE
streams. A separate Node 24.22.1 probe, without CPU/heap sampling or a harness
stream Set, also left zero tracked streams after each 1,000-request phase.

These measurements cover the fixes to facade EOF cleanup, cached Express
descriptor enumeration, memory-growth-aware WASI views, receive-credit
coalescing, and lazy native server packet pools. The earlier observations came
from the same branch before these fixes, not a historical release comparison.

| Measurement | Before fixes | After fixes |
| --- | ---: | ---: |
| Facade streams retained per 1,000 completed GETs | 1,000 | 0 |
| Express server observed peak RSS | 1,365 MiB | 263 MiB |
| Express descriptor preservation, sampled CPU per request | 86.2 µs | 57.5 µs |
| WASM fan-out estimated JS allocation per request | 1.46 MB | 0.78 MB |
| Native bulk client receive-credit subtree, sampled CPU per request | 587 µs | 98 µs |
| Native H3 idle-server RSS after listen | 399 MiB | 78 MiB |

Express post-GC heap was 9.4 MiB after the load run. WASM fan-out connections
each completed 26–27 requests. SSE received-chunk gaps had a p99 of 103.9 ms
and a maximum of 105.2 ms for the 100 ms send interval.

The profiles also show costs that remain:

- Express descriptor installation still accounts for 26.6% of sampled wall
  time; the prototype-swap compatibility protection remains in place.
- WASM fan-out send processing (`hs_next_send`) accounts for 44.3% of inclusive
  wall samples, clock handling 13.5%, and timer deadline updates 5.0%. Clock
  sampling frequency and values were preserved.
- Credit coalescing adds scheduling allocations: native bulk client estimated
  allocation rose from 92 to 120 KB/request. Native bulk completed 908 requests
  versus 929 earlier, with p99 latency of 250 versus 253 ms. These runs do not
  establish a bulk-throughput improvement. Deferred pumping also moves CPU
  attribution between callbacks, so a smaller credit subtree does not imply
  an equivalent reduction in total CPU.
- Lazy packet pools move allocation to first traffic. Production cold-start
  latency under concurrency was not separately qualified.

Validation passed 450 Node tests, 114 WASM tests, 67 FFI tests, and 228 Rust
unit tests, plus 27 focused Node 24 tests. The focused run includes two credit
retry tests added after the full Node run. TypeScript checks, including workerd,
and repository lint passed. Earlier attempts had one queued-response drain
timeout and one shutdown timeout during concurrent suite execution. Both passed
in isolation and on full reruns; the drain test additionally passed 20 repeats.
The causes of the initial timeouts remain unproven.

Each profile used 2 s warmup and 10 s load followed by in-flight drain, with CPU
and allocation sampling enabled together. Values are diagnostic observations
from short runs, not statistically established performance guarantees. The
optimized WASM artifact hash was
`b65f616407da3d46d0ad0bf1fd990d33a75153b9ec57fc6888038c7c85e73424`.
Raw profiles and validation logs remain local under the ignored
`perf-results/hotpaths-fixed-20261030/` directory; the commands above reproduce
the workloads and generate new artifacts.
