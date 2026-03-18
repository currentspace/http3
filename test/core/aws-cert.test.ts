import { describe, it } from 'node:test';
import assert from 'node:assert';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { loadTlsOptionsFromAwsEnv } from '../../lib/aws-cert.js';

describe('AWS TLS env loader', () => {
  it('loads key/cert from inline PEM env', () => {
    const tls = loadTlsOptionsFromAwsEnv({
      HTTP3_TLS_KEY_PEM: 'KEY-PEM',
      HTTP3_TLS_CERT_PEM: 'CERT-PEM',
      HTTP3_TLS_CA_PEM: 'CA-PEM',
    });
    assert.strictEqual(Buffer.from(tls.key).toString(), 'KEY-PEM');
    assert.strictEqual(Buffer.from(tls.cert).toString(), 'CERT-PEM');
    const ca = tls.ca;
    assert.ok(ca && !Array.isArray(ca), 'ca should be a single buffer');
    assert.strictEqual(Buffer.from(ca).toString(), 'CA-PEM');
  });

  it('loads key/cert from secret json', () => {
    const tls = loadTlsOptionsFromAwsEnv({
      HTTP3_TLS_SECRET_JSON: JSON.stringify({
        key: 'SECRET-KEY',
        cert: 'SECRET-CERT',
      }),
    });
    assert.strictEqual(Buffer.from(tls.key).toString(), 'SECRET-KEY');
    assert.strictEqual(Buffer.from(tls.cert).toString(), 'SECRET-CERT');
  });

  it('loads key/cert from filesystem paths', () => {
    const dir = mkdtempSync(join(tmpdir(), 'http3-aws-cert-'));
    const keyPath = join(dir, 'key.pem');
    const certPath = join(dir, 'cert.pem');
    writeFileSync(keyPath, 'FILE-KEY');
    writeFileSync(certPath, 'FILE-CERT');
    try {
      const tls = loadTlsOptionsFromAwsEnv({
        HTTP3_TLS_KEY_PATH: keyPath,
        HTTP3_TLS_CERT_PATH: certPath,
      });
      assert.strictEqual(Buffer.from(tls.key).toString(), 'FILE-KEY');
      assert.strictEqual(Buffer.from(tls.cert).toString(), 'FILE-CERT');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('throws when cert material is missing', () => {
    assert.throws(() => {
      loadTlsOptionsFromAwsEnv({});
    }, /TLS key\/cert not resolved/);
  });
});
