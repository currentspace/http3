# quiche 0.30.0 source patch

Source: https://static.crates.io/crates/quiche/quiche-0.30.0.crate
Archive SHA-256: `52b35a721a2a56797347929745fa783fc99e680d9484b80324ce40106b75493a`
Upstream commit: `be47c5011215b9f13bad06bd7627d3ae49888a19`.
The upstream BSD-2-Clause license is preserved in COPYING.

TLS compatibility change in `src/tls/mod.rs`: call `boring_sys::ERR_error_string_n`
and remove quiche's handwritten declaration. BoringSSL 5 returns `char *`,
whereas that declaration returns void. The mismatch produces a WASM linker
warning and a trapping call on the TLS error-reporting path. Using the generated
binding follows the actual linked BoringSSL version, including boring 4.

PMTUD changes in `src/lib.rs` and `src/pmtud.rs` bound probes and application
packets by both the configured send limit and the peer's advertised receive
limit. The negotiated cap persists across discovery restarts and revalidation;
late failures from larger probes cannot invalidate the new ceiling. Root tests
`pmtud_peer_limit` and `pmtud_state` cover packet sizes and discovery state.

The workspace uses this source for native, WASM, and the spike. No registry
cache is modified and no build changes Cargo.lock. Remove this vendored copy
and the root patch entry when a stable quiche release includes these corrections;
rerun native/WASM error-path tests and require a warning-free WASM link.
No upstream issue or PR has been submitted by this task.

STOP_SENDING in `src/lib.rs` now re-arms the writable notification even when
an idle stream still has capacity. `stream_writable_next()` consumes the previous
notification; the old capacity check could suppress cancellation until another
write. `insert_writable` already deduplicates linked entries. Root test
`idle_unblocked_stream_reports_peer_stop_sending` consumes the initial writable
notification and checks one reset; the Node proxy test cancels an idle H3 SSE
response before body data arrives. The application transport maps StreamStopped
to a stream reset independently of its blocked-write queue.

RESET_STREAM in `src/stream/recv_buf.rs` preserves its readable error marker
when FIN arrived before the application consumed the body. The normal write
path deduplicates an empty FIN at a known final offset; using that path alone
silently discarded the reset notification after clearing the unread bytes.
Root test `paused_body_reports_peer_reset_without_more_read_credit` reproduces
the failure with exhausted receive credit and verifies exactly one reset.
