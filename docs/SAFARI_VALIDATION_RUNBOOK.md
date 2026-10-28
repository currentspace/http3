# Safari Validation Runbook

Safari validation is a release-blocking manual gate for `rc` and `latest`
publishes.

## Prerequisites

- macOS host with Safari latest stable.
- Staging deployment reachable over HTTPS.
- Valid TLS chain matching staging hostname.

An explicitly selected loopback HTTPS server is suitable for this gate. Trust
only its temporary CA for the test, record the source revision and certificate
fingerprint, and restore the original trust settings afterward. Do not treat
Playwright WebKit results as evidence of real Safari behavior.

## Validation checklist

1. Run `pnpm run safari:checklist` and use it as the execution worksheet.
2. Open staging endpoint in Safari and verify the page loads cleanly.
3. Confirm fetch API endpoint behavior.
4. Confirm SSE/EventSource receives events and reconnects after reload.
5. Capture screenshot evidence for both fetch and SSE success state.
6. Record Safari version and macOS version.
7. Run curl protocol validation against same endpoint:
   - `curl --http3-only -sS -D /dev/stderr https://<host>/`
   - `curl --http2 -sS -D /dev/stderr https://<host>/`
   - For a local test CA, add `--cacert <ca.pem>` to both commands.
8. Attach artifacts to release notes / CI summary.

## Required evidence

- Screenshot: page success state.
- Screenshot: network entries showing successful requests.
- Text log: Safari + macOS versions.
- Curl outputs for H3 and H2 checks.

## Pass criteria

- All checklist steps complete with evidence attached.
- No functional regressions in fetch/SSE/EventSource behavior.
- Both curl protocol checks pass against the same deployment.

