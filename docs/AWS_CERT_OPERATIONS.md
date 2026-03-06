# AWS TLS Certificate Operations (App-Terminated TLS)

## Important Constraint

When TLS terminates in the application container, the process must have access
to the private key. Public ACM certificates that are only terminable at ALB/NLB
cannot be directly consumed by Node in-container.

## Supported Material Sources

Use `loadTlsOptionsFromAwsEnv()` from `@currentspace/http3/aws-cert`:

- Inline PEM env vars:
  - `HTTP3_TLS_KEY_PEM`
  - `HTTP3_TLS_CERT_PEM`
  - `HTTP3_TLS_CA_PEM` (optional)
- File paths:
  - `HTTP3_TLS_KEY_PATH`
  - `HTTP3_TLS_CERT_PATH`
  - `HTTP3_TLS_CA_PATH`
- JSON secret blob:
  - `HTTP3_TLS_SECRET_JSON` with `{ key, cert, ca? }`

## Recommended AWS Secret Flow

- Store key/cert material in AWS Secrets Manager.
- Sync secret into task environment or mounted file via sidecar/init process.
- Avoid writing key material to persistent disk.

## Rotation Strategy

- Deploy new tasks with new secret version.
- Mark old tasks not-ready and drain.
- Validate H3 and H2 handshake health after rotation.

## Rotation Validation Checklist

- New certificate chain accepted by major clients.
- H3 (`curl --http3-only`) and H2 (`curl --http2`) pass.
- SSE streams reconnect cleanly during rolling deploy.
