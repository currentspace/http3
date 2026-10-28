# 0.9.3 release preparation receipts

Evidence captured on 2026-10-28. Qualified release source:
`84335c235df9e4ffa1fc518cfde46d87517d487d`.

The [abi-corrected receipts](abi-corrected/) identify the qualified four-artifact
set from [run 40755081567](https://github.com/currentspace/http3/actions/runs/40755081567),
which passed all 11 jobs. Both Linux binaries require at most GLIBC_2.34, and clean
root/native-sidecar installs passed on Node 24 and 26. `comparison.json` proves
that only the Linux x64 binary changed from the first run.

Files in this directory's root retain the first functional qualification at
`c97b3d17e53dbe68a884d0fdc4d8d9b8e1901979`. Its x64 binary raised the glibc floor
to 2.38 and is not approved for publication. The corrected x64 build uses Ubuntu
22.04 and an enforced ELF symbol-version gate; the earlier receipts remain for
the audit trail.

## Hosted artifacts and package qualification

[Release dry-run 40750317271](https://github.com/currentspace/http3/actions/runs/40750317271)
passed all 11 jobs. `hosted-artifacts.json` records GitHub artifact IDs/digests;
`hosted-file-checksums.json` records the extracted binary/WASM hashes.
`local-package-checksums.json` records locally packed 0.9.3 tarballs and proves
their embedded binaries match the hosted downloads. These are preparation packs,
not npm-published tarballs or signed npm provenance.

The two `packed-consumer-node*.log` files cover clean installed-package imports
and WASM instantiation. `hosted-wasm-tests.log` covers 39 tests against the hosted
WASM. `local-release-gate-summary.json` records broader versioned local testing
and the final storage-guard failure after successful test completion.

## Real Safari

- `safari-manual-window.png`: manually inspected page, successful fetch and SSE.
- `safari-web-inspector-network.png`: actual Safari Network tab after reload;
  document, fetch, initial SSE, resumed SSE, and observation requests returned 200.
  The resumed SSE is red because the page closes EventSource after the second
  message. Resource Timing does not expose responseStatus in this Safari; the
  page's `undefined` values are not HTTP failures. Inspector supplies the status.
- `curl-manual-http3-only.txt` / `curl-manual-http2.txt`: same manual endpoint
  (`https://127.0.0.1:62802`) passed both protocols with certificate verification.
- `manual-ca-certificate.txt`: temporary CA subject, validity, and SHA-256.
- `safari-initial.json` / `safari-reload.json`: earlier WebDriver functional run
  on the same implementation at `https://127.0.0.1:58753`; successful fetch,
  two SSE messages with IDs 1/2, repeat after reload, no application errors.
- `safari-session.json`: real Safari capabilities, including
  `acceptInsecureCerts: false`; version text files identify browser and OS.
- `trust-cleanup.json`: original keychain search list restored and temporary
  keychain removed. The existing developer-features preference remained enabled.

Safari negotiated HTTP/2; these receipts do not claim Safari negotiated HTTP/3.
The matching curl HTTP/3 check and Chromium/Firefox/WebKit HTTP/3 test lanes
provide the separate HTTP/3 evidence.

The staging fixture uses `serveFetch`, a CA-signed localhost/127.0.0.1 certificate,
JSON fetch, and an SSE response that closes and resumes using Last-Event-ID.
Its temporary sources and full logs are retained locally under
`artifacts/release-0.9.3/`; private certificate keys are not included here.

## Release status

See [the release ledger](../../../docs/RELEASE_EVIDENCE.md) and
[individual CodeQL triage](../../../docs/release-evidence/0.9.3/CODEQL_TRIAGE.md).
Brian approved the 17 false-positive dismissals; `codeql-triage-readback.json`
records their application and independent GitHub readback.
No publication, tag creation, main merge, or production deployment occurred.
