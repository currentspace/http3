/**
 * Generate self-signed test certificates using openssl.
 * Returns { key, cert } as Buffers.
 */

import { execSync } from 'node:child_process';
import { readFileSync, unlinkSync, mkdtempSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

export function generateTestCerts(): { key: Buffer; cert: Buffer } {
  const dir = mkdtempSync(join(tmpdir(), 'h3-test-'));
  const keyPath = join(dir, 'key.pem');
  const certPath = join(dir, 'cert.pem');

  execSync(
    `openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 ` +
    `-keyout ${keyPath} -out ${certPath} -days 1 -nodes ` +
    `-subj "/CN=localhost" 2>/dev/null`
  );

  const key = readFileSync(keyPath);
  const cert = readFileSync(certPath);

  try { unlinkSync(keyPath); } catch { /* cleanup is best-effort */ }
  try { unlinkSync(certPath); } catch { /* cleanup is best-effort */ }

  return { key, cert };
}
