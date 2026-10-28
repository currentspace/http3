# quiche WASI Initial-packet spike

This workspace member now shares the main lockfile and quiche 0.30 dependency.
It creates a client connection, generates a QUIC Initial packet, and reports the
PTO. Build and run with the same ABI-matched BoringSSL and SDK as the main client:

```bash
export WASI_SDK_PATH=/path/to/wasi-sdk-33
node scripts/build-wasm.mjs --spike
```

The runner uses Node's experimental `node:wasi`; the production client uses its
own host shim. See [runtime build documentation](../../docs/WASM_RUNTIME.md).

The original July 2026 feasibility run used quiche 0.29.2, boring-sys 4.22.0,
and SDK 33. quiche 0.30 includes its two FFI fixes upstream. boring 5 supports
socket exclusion and WASI entropy without the old source edits or C shim.
The obsolete AES/ChaCha patch, shim, and machine-specific Cargo configuration
were removed. One new BoringSSL 5 error-string signature fix is tracked in
[vendor/quiche/PATCHES.md](../../vendor/quiche/PATCHES.md).

`mini-shim.mjs`, `inspect-imports.mjs`, and the other diagnostic scripts remain
historical development utilities; the current artifact contract is enforced by
`test/wasm/artifact-shape.test.ts`.
