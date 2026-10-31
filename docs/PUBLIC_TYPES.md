# Consumer types and error exports

Use the package root or documented subpaths for imports. Internal files under
`dist/` are not supported entry points, even when a declaration file is present
in the tarball. Callback argument types and return types are also inferred from
the public declarations; consumers need not import internal transport classes.

## Audit: 2026-10-30

The audit checked the package export map, legacy `typesVersions`, generated
declarations, public error subclasses/codes, and configuration and result types.
It found and corrected these omissions:

| Consumer need | Public entry point |
| --- | --- |
| Catch WASM allocation failures and read `requestedBytes` | `WasmMemoryError` from the root and `/wasm`, including Worker conditions |
| Match the WASM allocation error code without importing Node APIs | `ERR_HTTP3_WASM_MEMORY_LIMIT` from `/wasm` as well as the root |
| Catch an oversized Fetch request body | `RequestBodyTooLargeError` from the root, `/fetch`, and `/h3` |
| Match receive-buffer overflow separately from total body limits | `ERR_HTTP3_RECEIVE_BUFFER_LIMIT` from the root |
| Annotate runtime fallback details | `RuntimeAttemptFailure` from the root; used by `RuntimeInfo.fastAttempt` |
| Inspect native binding fallback details (1.0.0+) | `NativeBindingFailure` from the root; used by `RuntimeInfo.nativeBinding` and `Http3Error.nativeBinding` |
| Log Node facade runtime selection (1.0.0+) | Read-only `Http3NodeServer.runtimeInfo`, typed as `RuntimeInfo \| null` |
| Configure `createFetchHandler` through `/h3` | `FetchHandlerOptions` from `/h3`, matching the existing `/fetch` export |
| Share options between WASM H3 and raw QUIC servers | `CommonWasmServerOptions` from the Node `/wasm` entry |

The existing public types for server/client options, headers, request flags,
session metrics and close events, endpoints/DNS, shutdown, Node/Express/Nest
integration, proxy, certificates, Fetch, SSE/EventSource, health checks, AWS TLS,
and WASM transports remain accessible. `/parity` retains its HTTP/2-style type
aliases. Anonymous option/result shapes can be named by consumers with
`Parameters`, `ConstructorParameters`, or `ReturnType`; they do not require
deep imports or new runtime exports.

Native bindings, event-loop adapters used internally by Node streams,
backpressure state, runtime-selection internals, and CLI-only doctor types
remain internal. The Worker entry remains client-only and does not expose Node
UDP adapters or server types. Error callbacks remain `Error`; the library does
not claim that every failure is `Http3Error` or `WasmMemoryError`.

## Package-level verification

`scripts/smoke-install.mjs` packs and installs the candidate, then compiles the
fixtures in `scripts/fixtures/` against the installed package. No source aliases
are used, and declaration checking is enabled (`skipLibCheck: false`).

- The Node fixture imports all 14 public entry points and compiles with
  `node`, `node16`, `nodenext`, and `bundler` resolution.
- The Worker fixture compiles with both `worker` and `workerd` conditions under
  `nodenext` and `bundler`, using WebWorker libraries and no Node types. It also
  verifies that the Node UDP adapter is unavailable.
- Runtime checks verify that root/subpath exports use the same error
  constructors and that conditional WASM imports expose the error without
  loading Node UDP or filesystem-loader adapters.
- WASM memory-pressure integration tests verify that real errors reach callers
  as the public class; Fetch body tests verify public-class narrowing on H1,
  H2, and H3. The receive-overflow test checks the exported code.

These checks run in the existing Distribution workflow. Error handling examples
and limitations are in [ERROR_HANDLING.md](ERROR_HANDLING.md).
