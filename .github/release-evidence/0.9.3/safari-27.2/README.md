# Safari 27.2 release qualification for 0.9.3

**Passed, including direct visual review of the page and actual Web Inspector Network tab.**

- Release/PR source: `19dafa4001851b73a268078b377cd551d78200c7` ([PR #14](https://github.com/currentspace/http3/pull/14)). Its 28 checks passed; the scheduled-only deep-safety job is skipped on PR events.
- Safari qualification source: `a099c14eafc4d95dabfbd8d964780838378b9467`. Only the isolated test harness and qualification workflow differ from the release source; runtime/build inputs are unchanged.
- [Successful qualification run 40769595741](https://github.com/currentspace/http3/actions/runs/40769595741).
- Safari 27.2 (`20625.2.8.18.4`) on hosted macOS 15.7.9 (`24G830`), Apple Silicon. [Apple lists Safari 27.2 as released on October 26, 2026 for Sequoia and Tahoe](https://support.apple.com/en-us/100100).
- Uses the exact macOS ARM64 release prebuild from [final-head dry-run 40758754002](https://github.com/currentspace/http3/actions/runs/40758754002), SHA-256 `20868682543a214aa7f933ea9341d15445cbd48a0efb0b2d4660fa507d6434bd`. That release dry-run passed all 11 jobs.

## Evidence reviewed

- [Page success screenshot](safari-manual-page.png): fetch returned HTTP 200 and expected JSON; SSE delivered two messages across automatic reconnection with Last-Event-ID; secure context is true.
- [Actual Web Inspector Network screenshot after reload](safari-inspector-network.png): document, hello fetch, both EventSource requests, and observation fetch all returned HTTP 200. The second EventSource row is red because the fixture deliberately calls `EventSource.close()` after its second message; server receipts record successful resumption from ID 1 to ID 2.
- `safari-initial.json` and `safari-reload.json`: real Safari WebDriver assertions passed for initial load and reload (session load counts 1 and 2), fetch, both SSE message IDs, secure context, and zero fixture errors. Browser capabilities explicitly report `acceptInsecureCerts: false`.
- `sse-resume.jsonl`: successful ID 1 -> ID 2 reconnection in both automated phases and the normal Safari window before/after Inspector reload.
- `curl-http3-only.txt` and `curl-http2.txt`: HTTP 200 and protocol versions 3 and 2, respectively, against the same `https://127.0.0.1:60908` fixture with `--cacert` verification.

Safari's fetch/SSE traffic in this qualification used HTTP/2. The curl checks separately establish HTTP/3 and HTTP/2 behavior; they are not a claim that Safari fetch/SSE used HTTP/3. WebDriver screenshots on the hosted VM were unsuitable for visual evidence, so the two retained PNGs are unmodified screenshots of a normal Safari window, reviewed directly. `inspector-capture-status.json` is the harness's pre-review receipt; `manifest.json` records the subsequent visual review.

## Isolation and cleanup

The fixture listened only on loopback. Its temporary CA was installed only in the disposable hosted VM. The exact certificate was deleted and independently verified absent from the system keychain by fingerprint before job completion; the remaining VM-local admin trust record is confined to the disposable runner lifecycle. The earlier local Safari 27.0.1 qualification separately restored Brian's original trust settings. No new trust or OS/browser update was installed on Brian's Mac by this hosted qualification.

This evidence lives on an **isolated qualification branch, which must not be merged**: its existing release workflow is replaced with a Safari-only, read-permission test job and contains no publication job. The actual release PR remains on the qualified source above. Publication and tagging have not occurred. npm preflight at the timestamp in `npm-availability.json` found all four 0.9.3 versions unused; publishing still requires the runbook's immediate preflights.

The final GitHub readback is retained in `pr-readback.json`; `codeql-open-alerts.json` confirms zero open CodeQL alerts for the PR merge ref after the 17 explicitly approved false-positive dismissals.
