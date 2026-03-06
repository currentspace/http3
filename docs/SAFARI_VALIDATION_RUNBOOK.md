# Safari Validation Runbook

Safari validation is a release-blocking manual gate for `rc` and `latest`
publishes.

## Prerequisites

- macOS host with Safari latest stable.
- Staging deployment reachable over HTTPS.
- Valid TLS chain matching staging hostname.

## Validation checklist

1. Run `npm run safari:checklist` and use it as the execution worksheet.
2. Open staging endpoint in Safari and verify the page loads cleanly.
3. Confirm fetch API endpoint behavior.
4. Confirm SSE/EventSource receives events and reconnects after reload.
5. Capture screenshot evidence for both fetch and SSE success state.
6. Record Safari version and macOS version.
7. Run curl protocol validation against same endpoint:
   - `curl --http3-only -k -s -D /dev/stderr https://<host>/`
   - `curl --http2 -k -s -D /dev/stderr https://<host>/`
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

