import { readFileSync } from 'node:fs';
import type { TlsOptions } from './server.js';

export interface AwsTlsSecretShape {
  key?: string;
  cert?: string;
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

export interface AwsTlsEnv {
  HTTP3_TLS_CERT_PEM?: string;
  HTTP3_TLS_KEY_PEM?: string;
  HTTP3_TLS_CA_PEM?: string;
  HTTP3_TLS_CERT_PATH?: string;
  HTTP3_TLS_KEY_PATH?: string;
  HTTP3_TLS_CA_PATH?: string;
  HTTP3_TLS_SECRET_JSON?: string;
}

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
