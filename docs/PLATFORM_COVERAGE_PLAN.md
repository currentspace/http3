# Platform coverage and automatic WASM fallback

Date: 2026-10-30. Status: implementation in a stacked PR series.
Qualification results and remaining release steps are tracked in
[PLATFORM_COVERAGE_EVIDENCE.md](./PLATFORM_COVERAGE_EVIDENCE.md).

Source request: `/tmp/http3-platforms/README.md`, with consumer repros in
`/tmp/http3-platforms/repro/`. Baseline: `main` at `966c611`, package version 0.9.9.

## Outcome and scope

Make the default server/client APIs usable without a native addon by selecting
the existing WASM runtime, while keeping native performance where an addon
loads. Preserve explicit native modes and strict fallback policy. Add native
musl x64/arm64 and Intel macOS packages, qualify Windows through WASM, and make
large Node stream writes work after fallback.

Native Windows drivers, other new native platforms, a universal macOS package,
and a second napi-rs WASI runtime are outside this implementation. Prefer a
separate `darwin-x64` sidecar over changing the existing arm64 distribution.
Other Node platforms may use WASM, but list only tested platforms as qualified.

## What is already present

- `966c611` already replaces the misleading npm error with platform/libc-aware
  diagnostics in `bin/native-binding-error.cjs`, preserves causes, and patches
  regenerated loaders through `scripts/patch-native-loader-version.mjs`.
  Extend that work instead of replacing the generated loader.
- `lib/runtime.ts` supports explicit WASM, but `auto` only falls back from native
  `fast` to native `portable`.
- H3 server startup uses a synchronous native selector and a separate async
  `_listenWasm` bootstrap. H3 clients and raw QUIC clients/servers already use
  the async selector and lazy event-loop factories.
- WASM writes stage an entire payload in linear memory, then Rust copies it
  into owned storage. A 32 MiB write cannot fit both copies plus protocol state
  into the current 64 MiB heap.
- `Http3NodeServer.transport` is private; expose runtime metadata directly on
  the facade rather than requiring consumers to access that implementation detail.

Local baseline reproduction used isolated copies of the existing compiled
artifacts on macOS arm64 / Node 26.13.0, without installing packages:

| Case | Observed result |
| --- | --- |
| Native absent, explicit WASM | H2 and H3 responses succeed |
| Native absent, auto or portable | Platform-specific missing-package error |
| Native present, auto | `fast` / `kqueue`, no fallback; H2/H3 succeed |
| WASM, single 32 MiB response | H3 reset 268, `ERR_HTTP3_WASM_MEMORY_LIMIT` |
| WASM, streamed 32 MiB response | Completes, approximately 92 MiB/s |
| Native portable, single 32 MiB response | Completes, approximately 753 MiB/s |

These are exploratory runs, not a clean-build, cross-platform, or performance
qualification. The supplied repros exit zero even on errors/timeouts and do not
assert exact response length; they cannot serve as CI gates unchanged.

## 1. Binding availability, selection, and facade metadata

Primary files: `bin/native-binding-error.cjs`, `lib/event-loop.ts`,
`lib/runtime.ts`, `lib/errors.ts`, `lib/server.ts`, `lib/client.ts`,
`lib/quic-server.ts`, `lib/quic-client.ts`, `lib/client-event-loop-factory.ts`,
`lib/node-server.ts`.

1. Give binding-load failures a structured `native-binding-unavailable` reason,
   with OS, architecture, Linux libc, expected package, and failure category
   (unpublished, missing, or found-but-unloadable). Preserve the original cause
   chain through `getBinding()` and `Http3Error`. Use package metadata as the
   published-target inventory. Do not classify errors by matching prose or by
   treating every `ERR_HTTP3_RUNTIME_UNSUPPORTED` as a missing binding.
2. Introduce a shared lazy availability/selection step that resolves the addon
   before any native constructor or bind/connect call. Explicit WASM skips it.
   Reuse successful binding caching; if failed resolution is cached, scope it
   to the binding identity and provide a test reset separate from the existing
   fast-driver cache. Importing the library must remain lazy.
3. Apply the following policy consistently across H3 and raw QUIC:

   | Request | Binding loads | Binding cannot load |
   | --- | --- | --- |
   | `auto`, warn-and-fallback | Existing fast/portable selection | WASM |
   | `auto`, error | Existing strict native selection | `ERR_HTTP3_RUNTIME_UNSUPPORTED` |
   | explicit fast or portable | Requested native mode | `ERR_HTTP3_RUNTIME_UNSUPPORTED` |
   | explicit WASM, either policy | WASM, no native probe | WASM, no native probe |

   Missing packages, unsupported targets, incompatible addon binaries, and
   invalid native overrides all retain their distinct diagnostic details.
   Do not retry TLS, DNS, bind, handshake, configuration, or application errors
   through WASM. Keep fast-driver unavailability and its existing portable
   fallback separate from binding availability.
4. For H3 servers, inspect the selection decision before entering
   `runWithRuntimeSelectionSync`. Route a WASM decision through deferred async
   bootstrap, preserving `listen()` returning `this` and the healthy native
   startup path. Refactor `_listenWasm` to consume the original decision rather
   than relabeling the request as explicit WASM or rerunning a native attempt.
   Keep argument validation and public error timing compatible. Cover close
   during startup, bind retries, and partial startup cleanup.
5. Report successful fallback as `requestedMode: 'auto'`, `selectedMode: 'wasm'`,
   `driver: 'wasm'`, `fallbackOccurred: true`, and
   `reasonCode: 'native-binding-unavailable'`. Keep `fastAttempt` for actual
   fast-driver failures. Preserve native-load context if WASM startup itself
   fails, without reporting successful selection or emitting a success warning.
6. Emit `WARN_HTTP3_RUNTIME_FALLBACK` once per distinct fallback cause per
   process, using the existing deduplication machinery. Name platform, arch,
   Linux libc, cause, and selected runtime. Audit WASM option limitations
   (including qlog, server keylog, session ticket keys, reuse-port) and document
   them; reject an unsupported option if ignoring it would change required
   security or application semantics.
7. Add a typed read-only `Http3NodeServer.runtimeInfo` getter delegating to the
   transport and forward its `runtime` event once. Keep `onRuntimeEvent` single
   delivery. Define preselection, success, and failure snapshots consistently
   with the underlying server and generated declarations.

Tests belong with `test/runtime/runtime-selection.test.ts`,
`test/release/native-loader-diagnostics.test.ts`, and the existing Node facade
and WASM startup suites. Update the current test that deliberately expects
`auto` to fail without sidecars. Include strict modes, malformed binaries,
explicit overrides, both clients, both servers, warning deduplication,
successful-native lazy WASM loading, and unrelated failures that must not retry.

## 2. Bounded large writes under WASM

Primary files: `lib/stream.ts`, `lib/quic-stream.ts`,
`lib/stream-backpressure.ts`, event-loop interfaces, and
`lib/wasm/{h3,quic}-{client,server}-event-loop.ts`.

1. Add an internal optional maximum stream-send size capability, initially
   64 KiB for WASM and absent for native. Retain the original buffer and offset
   in JS and pass bounded `subarray` views to the existing WASM ABI.
2. Advance only by admitted bytes. Yield to an event-loop turn (for example,
   `setImmediate`, not recursive microtasks) between accepted slices so UDP I/O
   and deferred BLOCKED/DRAIN events can run; wait for real drain only when actually blocked.
   A size-limited slice needs its own scheduled continuation, since accepting
   that slice does not guarantee a future drain event. Avoid recursive or tight
   loops that queue the entire body inside WASM.
3. Send FIN only with the final slice, preserving the empty-FIN acknowledgment
   convention. Complete each caller callback once; cancel continuations and
   release retained buffers on reset, destroy, or connection close. Preserve
   trailer ordering and Node writable backpressure.
4. Route large `respondWithBody()` calls through headers plus the bounded stream
   path before setting shortcut completion flags. Keep native and small-body
   shortcuts. Apply the same behavior to client uploads and both raw QUIC stream
   directions; do not split datagram messages.
5. Keep the heap cap and recoverable allocation errors. Document that this
   removes the public Node stream single-write limit, not aggregate WASM memory
   limits or direct low-level ABI allocation limits. Direct exported event-loop
   calls need a documented per-call bound unless separately given a bounded
   admission contract.

Replace Node-write failure expectations in `test/wasm/memory-pressure.test.ts`
with byte-count/hash assertions for 32 MiB, plus 64/128 MiB cases with suitable
timeouts. Exercise `write`, `end`, `respondWithBody`, facade `res.end`, uploads,
raw QUIC, small flow-control windows, blocked response headers with a large
`respondWithBody`, paused readers, concurrent transfers,
reset during continuation, empty FIN, and a subsequent healthy request.
Retain direct allocation-exhaustion tests. Measure heap high-water marks under
slow receivers to verify that slicing actually bounds WASM admission.

## 3. Native musl and Intel macOS packages

Add `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, and
`x86_64-apple-darwin` to the N-API/release target inventory, with sidecars
`http3-linux-x64-musl`, `http3-linux-arm64-musl`, and `http3-darwin-x64`.

- Build and load-test musl on each real architecture in Alpine, using a pinned
  toolchain/image and matching C/C++/libclang/CMake dependencies for BoringSSL.
  Verify the Rust cdylib CRT settings and linked runtime dependencies rather
  than assuming a successful Rust cross-compile produces a loadable Node addon.
  Avoid a driver redesign: Linux already has poll and io_uring implementations.
- Extend `.github/workflows/release.yml`, `.github/workflows/distribution.yml`,
  `scripts/build-prebuilds.sh`, `package.json`, `npm/*/package.json`,
  `pnpm-workspace.yaml`, and the lockfile. Musl packages need `libc: ["musl"]`;
  retain GNU libc constraints. Keep versions and licenses consistent.
- Make `scripts/smoke-install.mjs` and `scripts/pack-host-native.mjs` match libc
  as well as OS/CPU; otherwise a musl/gnu pair makes host-package selection
  ambiguous. Update `scripts/bump-version.sh` and prebuild verification/tests,
  preferably deriving inventories from package metadata rather than adding
  more independent three/six-target lists.
- Retire or synchronize legacy `scripts/publish-canary.sh` and
  `scripts/publish-latest.sh`, which hard-code only a subset of sidecars.
- Run the existing GLIBC symbol-floor check only for GNU binaries. Add musl
  architecture/linkage checks and execute installed-package smoke tests in a
  clean Alpine runtime without build dependencies.
- Promote the existing Intel source-build qualification in
  `.github/workflows/verify.yml` to published-sidecar
  qualification on `macos-26-intel`, subject to runner availability at rollout.
  Do not use the spec's obsolete `macos-13` label.
- Follow `docs/RELEASE_RUNBOOK.md` for first-publication bootstrap and Trusted
  Publisher setup for the three new npm packages. Existing publication tooling
  discovers `npm/*` and publishes sidecars before the root. Gate the root release
  on complete matching sidecars and clean installed-package verification.

The loader can retain generated branches for other targets: route final failure
through the structured diagnostics/selection boundary. Do not build or publish
the generated loader's separate napi-rs WASI path.

## 4. Acceptance and release gates

Convert the supplied repro scenarios into assertion-based subprocess tests
using a packed candidate installed outside the checkout. Prevent local `.node`
files, `NODE_PATH`, overrides, or workspace links from rescuing the missing-addon
case. Repository work uses pnpm; preserve a separate npm consumer-install check
because `npm install --omit=optional` is an explicit reported scenario.

| Environment / install | Required evidence |
| --- | --- |
| macOS arm64, no sidecars | Auto H2/H3 success, one runtime fallback warning, facade metadata; strict auto/fast/portable errors |
| Alpine arm64 and x64, no sidecars | Same auto/strict checks; H3 and raw QUIC auto clients and servers |
| Windows x64, no sidecars | Same H2/H3 auto success on `windows-latest`; raw QUIC smoke; test certificates/OpenSSL resolved explicitly |
| Existing native macOS arm64 and GNU Linux x64/arm64 | Native selection unchanged; native throughput/request-rate baseline retained |
| Alpine arm64 and x64, normal install | Correct musl sidecar installed; native portable request-rate repro and auto runtime checks |
| Intel macOS, normal install | Correct sidecar installed; auto selects native; H2/H3 and raw QUIC smoke |
| WASM large writes | `throughput.cjs wasm 32 buffer` succeeds, plus assertion-based integrity/backpressure cases |
| All packed candidates | `.wasm` artifact present and loadable, public declaration compilation, correct sidecar versions |

Use the repository's supported Node-major matrix for qualification. Require
native architecture execution for release evidence; emulated amd64 is useful
for reproduction but not a replacement for native x64 qualification.

There is one acceptance clarification: normal Docker seccomp can prevent
io_uring even with the correct musl binary installed. In that environment,
`auto` legitimately selects native `portable` with the existing
`fast-path-unavailable` warning. Require **no WASM/binding-unavailable fallback**
there; require no warning for explicit portable, and test warning-free auto on
an io_uring-capable host. Do not suppress an existing meaningful warning to
satisfy an ambiguous assertion.

Run lint, typecheck (including workerd boundaries), runtime/release/core/WASM
tests, native H3/QUIC interop, and packed-install checks. Rebuild WASM/native
artifacts and run relevant Rust gates if shared Rust changes become necessary.
Benchmark small GETs and buffer/stream downloads repeatedly against the same
pre-change native setup; keep performance evidence separate from correctness.

## 5. Documentation and delivery sequence

Update `README.md`, `docs/RUNTIME_MODES.md`, `docs/SUPPORT_MATRIX.md`,
`docs/WASM_RUNTIME.md`, `docs/NODE_SERVER.md`, `docs/ERROR_HANDLING.md`, and
`docs/RELEASE_RUNBOOK.md`. Several guides still claim WASM is client-only or
always opt-in despite existing server support. Include all four modes, the
selection table, tested platform coverage, strict-policy examples, facade
logging, and remaining WASM capability/memory limits.

Attribute the spec's roughly half request rate and one-tenth bulk throughput
to its 0.9.9 macOS-arm64 measurements, including workload and date. They are
context for choosing a runtime, not portable performance guarantees. Add fresh
post-change measurements before release.

Suggested review units, each including its focused tests and documentation:

1. Structured binding selection, async H3 fallback, and facade observability.
2. Bounded WASM writes and completion/backpressure regressions; can develop in
   parallel with runtime selection, but gate automatic-fallback release on it.
3. Native sidecars and build/package qualification, independently of fallback.
4. Cross-platform installed-consumer qualification, final documentation matrix,
   and release evidence. Windows WASM is a required fallback qualification lane.

Runtime fallback can ship before the new sidecars once units 1/2 and their
platform gates pass; musl and Intel packages can follow. Publishing remains a
separate release action. After publication, repeat clean registry installs with
and without optional dependencies before declaring the consumer's launcher
workaround removable. Publication is separate from the implementation PR series.

Runner reference: [GitHub-hosted runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Musl toolchain reference: [napi-rs cross-building and native dependencies](https://napi.rs/docs/cross-build).
