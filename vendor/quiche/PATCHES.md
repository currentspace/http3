# quiche 0.30.0 source patch

Source: https://static.crates.io/crates/quiche/quiche-0.30.0.crate
Archive SHA-256: `52b35a721a2a56797347929745fa783fc99e680d9484b80324ce40106b75493a`
Upstream commit: `be47c5011215b9f13bad06bd7627d3ae49888a19`.
The upstream BSD-2-Clause license is preserved in COPYING.

One change in `src/tls/mod.rs`: call `boring_sys::ERR_error_string_n`
and remove quiche's handwritten declaration. BoringSSL 5 returns `char *`,
whereas that declaration returns void. The mismatch produces a WASM linker
warning and a trapping call on the TLS error-reporting path. Using the generated
binding follows the actual linked BoringSSL version, including boring 4.

The workspace uses this source for native, WASM, and the spike. No registry
cache is modified and no build changes Cargo.lock. Remove this vendored copy
and the root patch entry when a stable quiche release includes the correction;
rerun native/WASM error-path tests and require a warning-free WASM link.
No upstream issue or PR has been submitted by this task.
