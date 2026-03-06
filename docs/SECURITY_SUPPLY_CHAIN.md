# Security And Supply-Chain Controls

## CI Controls

- `npm audit` with high-severity threshold
- Dependency review on pull requests
- CodeQL static analysis
- SBOM generation (`npm sbom`)
- Lint + typecheck gate
- Browser interop gate (Chromium + Firefox)
- Rust clippy gate

## Release Controls

- Required checks before publish:
  - full tests
  - curl interop tests
  - concurrency gate
  - install smoke from packed tarball
- npm provenance enabled during publish

## Runtime Controls

- Default `allowHTTP1: false`
- TLS key/cert from secret management flow only
- No plaintext fallback listeners in production
- Principle of least privilege IAM for task role

## Secrets Handling

- Never commit PEM material or secret JSON
- Use scoped IAM permissions (`secretsmanager:GetSecretValue` to specific ARNs)
- Rotate credentials and certs on a fixed schedule
