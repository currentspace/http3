/**
 * Generate self-signed and CA-signed test certificates using openssl.
 */

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export interface MutualTlsTestCerts {
  ca: { key: Buffer; cert: Buffer };
  server: { key: Buffer; cert: Buffer };
  client: { key: Buffer; cert: Buffer };
  alternateClient: { key: Buffer; cert: Buffer };
}

function runOpenSsl(args: string[]): void {
  execFileSync('openssl', args, { stdio: ['ignore', 'ignore', 'ignore'] });
}

export function generateTestCerts(): { key: Buffer; cert: Buffer } {
  const dir = mkdtempSync(join(tmpdir(), 'h3-test-'));
  const keyPath = join(dir, 'key.pem');
  const certPath = join(dir, 'cert.pem');

  try {
    runOpenSsl([
      'req',
      '-x509',
      '-newkey',
      'ec',
      '-pkeyopt',
      'ec_paramgen_curve:prime256v1',
      '-keyout',
      keyPath,
      '-out',
      certPath,
      '-days',
      '1',
      '-nodes',
      '-subj',
      '/CN=localhost',
    ]);

    return {
      key: readFileSync(keyPath),
      cert: readFileSync(certPath),
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

export function generateMutualTlsTestCerts(): MutualTlsTestCerts {
  const dir = mkdtempSync(join(tmpdir(), 'h3-mtls-test-'));
  const caKeyPath = join(dir, 'ca-key.pem');
  const caCertPath = join(dir, 'ca-cert.pem');
  const caConfigPath = join(dir, 'ca.cnf');
  const serverKeyPath = join(dir, 'server-key.pem');
  const serverCsrPath = join(dir, 'server.csr');
  const serverCertPath = join(dir, 'server-cert.pem');
  const serverExtPath = join(dir, 'server-ext.cnf');
  const clientKeyPath = join(dir, 'client-key.pem');
  const clientCsrPath = join(dir, 'client.csr');
  const clientCertPath = join(dir, 'client-cert.pem');
  const clientExtPath = join(dir, 'client-ext.cnf');
  const alternateClientKeyPath = join(dir, 'alternate-client-key.pem');
  const alternateClientCsrPath = join(dir, 'alternate-client.csr');
  const alternateClientCertPath = join(dir, 'alternate-client-cert.pem');
  const alternateClientExtPath = join(dir, 'alternate-client-ext.cnf');

  try {
    writeFileSync(caConfigPath, [
      '[req]',
      'prompt = no',
      'distinguished_name = dn',
      'x509_extensions = v3_ca',
      '',
      '[dn]',
      'CN = http3-test-ca',
      '',
      '[v3_ca]',
      'basicConstraints = critical, CA:TRUE',
      'keyUsage = critical, keyCertSign, cRLSign',
      'subjectKeyIdentifier = hash',
      '',
    ].join('\n'));
    runOpenSsl([
      'req',
      '-x509',
      '-newkey',
      'ec',
      '-pkeyopt',
      'ec_paramgen_curve:prime256v1',
      '-keyout',
      caKeyPath,
      '-out',
      caCertPath,
      '-days',
      '1',
      '-nodes',
      '-config',
      caConfigPath,
    ]);

    runOpenSsl([
      'req',
      '-new',
      '-newkey',
      'ec',
      '-pkeyopt',
      'ec_paramgen_curve:prime256v1',
      '-keyout',
      serverKeyPath,
      '-out',
      serverCsrPath,
      '-nodes',
      '-subj',
      '/CN=localhost',
    ]);
    writeFileSync(serverExtPath, [
      'basicConstraints = CA:FALSE',
      'extendedKeyUsage = serverAuth',
      'keyUsage = digitalSignature,keyAgreement',
      'subjectAltName = @alt_names',
      '',
      '[alt_names]',
      'DNS.1 = localhost',
      'IP.1 = 127.0.0.1',
      '',
    ].join('\n'));
    runOpenSsl([
      'x509',
      '-req',
      '-in',
      serverCsrPath,
      '-CA',
      caCertPath,
      '-CAkey',
      caKeyPath,
      '-CAcreateserial',
      '-out',
      serverCertPath,
      '-days',
      '1',
      '-sha256',
      '-extfile',
      serverExtPath,
    ]);

    runOpenSsl([
      'req',
      '-new',
      '-newkey',
      'ec',
      '-pkeyopt',
      'ec_paramgen_curve:prime256v1',
      '-keyout',
      clientKeyPath,
      '-out',
      clientCsrPath,
      '-nodes',
      '-subj',
      '/CN=quic-client',
    ]);
    writeFileSync(clientExtPath, [
      'basicConstraints = CA:FALSE',
      'extendedKeyUsage = clientAuth',
      'keyUsage = digitalSignature,keyAgreement',
      '',
    ].join('\n'));
    runOpenSsl([
      'x509',
      '-req',
      '-in',
      clientCsrPath,
      '-CA',
      caCertPath,
      '-CAkey',
      caKeyPath,
      '-CAcreateserial',
      '-out',
      clientCertPath,
      '-days',
      '1',
      '-sha256',
      '-extfile',
      clientExtPath,
    ]);

    runOpenSsl([
      'req',
      '-new',
      '-newkey',
      'ec',
      '-pkeyopt',
      'ec_paramgen_curve:prime256v1',
      '-keyout',
      alternateClientKeyPath,
      '-out',
      alternateClientCsrPath,
      '-nodes',
      '-subj',
      '/CN=quic-client-alt',
    ]);
    writeFileSync(alternateClientExtPath, [
      'basicConstraints = CA:FALSE',
      'extendedKeyUsage = clientAuth',
      'keyUsage = digitalSignature,keyAgreement',
      '',
    ].join('\n'));
    runOpenSsl([
      'x509',
      '-req',
      '-in',
      alternateClientCsrPath,
      '-CA',
      caCertPath,
      '-CAkey',
      caKeyPath,
      '-CAcreateserial',
      '-out',
      alternateClientCertPath,
      '-days',
      '1',
      '-sha256',
      '-extfile',
      alternateClientExtPath,
    ]);

    return {
      ca: {
        key: readFileSync(caKeyPath),
        cert: readFileSync(caCertPath),
      },
      server: {
        key: readFileSync(serverKeyPath),
        cert: readFileSync(serverCertPath),
      },
      client: {
        key: readFileSync(clientKeyPath),
        cert: readFileSync(clientCertPath),
      },
      alternateClient: {
        key: readFileSync(alternateClientKeyPath),
        cert: readFileSync(alternateClientCertPath),
      },
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
