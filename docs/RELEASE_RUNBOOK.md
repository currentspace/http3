# Release Runbook (canary -> rc -> stable)

## Dist Tags

- `canary`: fast iterative validation
- `rc`: pre-production candidate
- `latest`: stable

## Pre-Release Checklist

1. `npm run release:check`
2. `npm run smoke:install`
3. Verify docs updates for any API surface change
4. Validate ECS staging deploy and protocol smoke checks (`bash scripts/verify-aws-http3.sh <host>`)
5. For `rc` and `latest`, execute Safari runbook: [`SAFARI_VALIDATION_RUNBOOK.md`](./SAFARI_VALIDATION_RUNBOOK.md)

## Publish Commands

- Canary:
  - `npm publish --tag canary --provenance`
- RC:
  - `npm publish --tag rc --provenance`
- Stable:
  - `npm publish --tag latest --provenance`

## Post-Release Validation

- Install package in clean project and run import smoke.
- Run H3 and H2 requests against staged deployment.
- Confirm SSE stream and EventSource reconnect behavior.
- Confirm multi-platform prebuild binaries are present in publish artifact.

## Rollback

- If bad release:
  - Publish fixed patch quickly with same dist-tag
  - Move consumers to known-good version via package manager pin
  - Record incident and remediation in release notes
