const { mkdirSync, existsSync } = require('node:fs');
const { execFileSync } = require('node:child_process');
const path = require('node:path');
const directory = path.join(__dirname, '../.certs');
mkdirSync(directory, { recursive: true });
const key = path.join(directory, 'key.pem');
const cert = path.join(directory, 'cert.pem');
if (existsSync(key) || existsSync(cert)) {
  throw new Error('Certificate files already exist; remove .certs explicitly to regenerate them.');
}
execFileSync('openssl', [
  'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
  '-nodes', '-days', '30', '-subj', '/CN=localhost',
  '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1',
  '-keyout', key, '-out', cert,
], { stdio: 'inherit' });
console.log('Created a self-signed localhost certificate. No system trust settings were changed.');
