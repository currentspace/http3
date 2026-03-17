import { readFileSync } from 'node:fs';
import type { TlsOptions } from './server.js';

/** JSON shape expected when loading TLS credentials from AWS Secrets Manager. */
export interface AwsTlsSecretShape {
  /** PEM-encoded private key. */
  key?: string;
  /** PEM-encoded certificate chain. */
  cert?: string;
  /** PEM-encoded CA certificate. */
  ca?: string;
}

function resolvePemValue(
  inlinePem?: string,
  path?: string,
): Buffer | undefined {
  if (inlinePem && inlinePem.trim().length > 0) {
    return Buffer.from(inlinePem);
  }
  if (path && path.trim().length > 0) {
    return readFileSync(path);
  }
  return undefined;
}

function parseSecretJson(raw?: string): AwsTlsSecretShape {
  if (!raw || raw.trim().length === 0) return {};
  const parsed = JSON.parse(raw) as AwsTlsSecretShape;
  return parsed;
}

/**
 * Environment variables used to resolve TLS credentials in AWS deployments.
 * Supports inline PEM, file paths, and Secrets Manager JSON.
 */
export interface AwsTlsEnv {
  /** Inline PEM certificate. */
  HTTP3_TLS_CERT_PEM?: string;
  /** Inline PEM private key. */
  HTTP3_TLS_KEY_PEM?: string;
  /** Inline PEM CA certificate. */
  HTTP3_TLS_CA_PEM?: string;
  /** Filesystem path to the certificate PEM file. */
  HTTP3_TLS_CERT_PATH?: string;
  /** Filesystem path to the private key PEM file. */
  HTTP3_TLS_KEY_PATH?: string;
  /** Filesystem path to the CA PEM file. */
  HTTP3_TLS_CA_PATH?: string;
  /** Raw JSON string matching {@link AwsTlsSecretShape}. */
  HTTP3_TLS_SECRET_JSON?: string;
}

/**
 * Load TLS key/cert/ca from environment variables or AWS Secrets Manager JSON.
 * Throws if neither key nor cert can be resolved.
 */
export function loadTlsOptionsFromAwsEnv(
  env: AwsTlsEnv = process.env,
): TlsOptions & { key: Buffer; cert: Buffer } {
  const fromSecret = parseSecretJson(env.HTTP3_TLS_SECRET_JSON);
  const key = resolvePemValue(env.HTTP3_TLS_KEY_PEM ?? fromSecret.key, env.HTTP3_TLS_KEY_PATH);
  const cert = resolvePemValue(env.HTTP3_TLS_CERT_PEM ?? fromSecret.cert, env.HTTP3_TLS_CERT_PATH);
  const ca = resolvePemValue(env.HTTP3_TLS_CA_PEM ?? fromSecret.ca, env.HTTP3_TLS_CA_PATH);

  if (!key || !cert) {
    throw new Error(
      'TLS key/cert not resolved. Provide HTTP3_TLS_KEY_PEM + HTTP3_TLS_CERT_PEM, ' +
      'or *_PATH variables, or HTTP3_TLS_SECRET_JSON with {key,cert}.',
    );
  }

  return {
    key,
    cert,
    ca,
  };
}
