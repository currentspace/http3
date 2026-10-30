import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync, spawnSync, execFile } from 'node:child_process';
import { X509Certificate } from 'node:crypto';
import { generateDevCertificate, devTrustCommands } from '../../lib/dev-cert.js';

test('dev-cert issues constrained localhost certificates, preserves keys, and rejects public names', t => {
  const dir = mkdtempSync(join(tmpdir(), 'http3-dev-cert-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  generateDevCertificate(dir);
  const cert = new X509Certificate(readFileSync(join(dir, 'cert.pem')));
  const ca = new X509Certificate(readFileSync(join(dir, 'ca.pem')));
  assert.equal(ca.ca, true);
  assert.equal(cert.ca, false);
  assert.ok(cert.checkHost('localhost'));
  assert.equal(cert.checkHost('app.localhost'), undefined);
  assert.equal(cert.checkIP('127.0.0.1'), undefined);
  assert.equal(cert.checkIP('::1'), undefined);
  assert.equal(cert.checkHost('example.com'), undefined);
  const key = readFileSync(join(dir, 'key.pem'));
  generateDevCertificate(dir);
  assert.deepEqual(readFileSync(join(dir, 'key.pem')), key);
  if (process.platform !== 'win32') {
    assert.equal(statSync(dir).mode & 0o777, 0o700);
    for (const file of ['ca-key.pem', 'key.pem']) assert.equal(statSync(join(dir, file)).mode & 0o777, 0o600);
  }
  const run = (args: string[]) => execFileSync('openssl', args, { stdio: 'pipe' });
  run(['verify', '-CAfile', join(dir, 'ca.pem'), '-verify_hostname', 'localhost', join(dir, 'cert.pem')]);
  assert.notEqual(spawnSync('openssl', ['verify', '-CAfile', join(dir, 'ca.pem'), '-verify_ip', '::1', join(dir, 'cert.pem')]).status, 0);
  // Even a leaf signed with the generated CA key must fail outside its scope.
  writeFileSync(join(dir, 'outside.cnf'), 'subjectAltName=DNS:example.com,IP:192.0.2.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n');
  run(['req', '-new', '-key', join(dir, 'key.pem'), '-subj', '/CN=example.com', '-out', join(dir, 'outside.csr')]);
  run(['x509', '-req', '-in', join(dir, 'outside.csr'), '-CA', join(dir, 'ca.pem'), '-CAkey', join(dir, 'ca-key.pem'),
    '-set_serial', '2', '-days', '1', '-extfile', join(dir, 'outside.cnf'), '-out', join(dir, 'outside.pem')]);
  const outside = spawnSync('openssl', ['verify', '-CAfile', join(dir, 'ca.pem'), join(dir, 'outside.pem')], { encoding: 'utf8' });
  assert.notEqual(outside.status, 0);
  assert.match(outside.stdout + outside.stderr, /permitted subtree violation/i);

  const mac = devTrustCommands(dir, ca, false, 'darwin');
  assert.deepEqual(mac[0]!.args.slice(0, 8), ['security', 'add-trusted-cert', '-d', '-r', 'trustRoot', '-p', 'ssl', '-k']);
  const remove = devTrustCommands(dir, ca, true, 'darwin');
  assert.ok(remove[0]!.args.includes('deny'));
  assert.equal(remove[0]!.args.at(-1), join(dir, 'ca.pem'));
  const linux = devTrustCommands(dir, ca, false, 'linux', { linuxTool: '/usr/sbin/update-ca-certificates', nssDirectories: ['/tmp/test-nss'] });
  assert.equal(linux.at(-1)!.file, 'certutil');
  assert.ok(linux.at(-1)!.args.includes('C,,'));
  assert.ok(linux[0]!.args.at(-1)!.endsWith('.crt'));
  const windows = devTrustCommands(dir, ca, true, 'win32');
  assert.deepEqual(windows[0]!.args.slice(0, 3), ['-user', '-delstore', 'Root']);

  writeFileSync(join(dir, 'ca.json'), '{}');
  assert.throws(() => generateDevCertificate(dir), /does not match/);
});

test('dev-cert CLI documents trust explicitly and rejects unknown options', () => {
  // dist-test module lets the CLI test run with build:test, before build:dist.
  const entry = resolve(__dirname, '../../lib/dev-cert.js');
  const run = (args: string[]) => spawnSync(process.execPath,
    ['-e', `require(${JSON.stringify(entry)}).runDevCertCli(${JSON.stringify(args)})`], { encoding: 'utf8' });
  const help = run(['--help']);
  assert.equal(help.status, 0);
  assert.match(help.stdout, /does not change trust/);
  assert.notEqual(run(['dev-cert', '--out']).status, 0);
  assert.notEqual(run(['dev-cert', '--unknown']).status, 0);
});

test('project certificates share a constrained CA and renew without replacing keys', t => {
  const base = mkdtempSync(join(tmpdir(), 'http3-shared-ca-'));
  t.after(() => rmSync(base, { recursive: true, force: true }));
  const shared = join(base, 'authority');
  const project = join(base, 'project');
  generateDevCertificate(project, shared);
  assert.deepEqual(readFileSync(join(project, 'ca.pem')), readFileSync(join(shared, 'ca.pem')));
  assert.throws(() => readFileSync(join(project, 'ca-key.pem')), /ENOENT/);
  const key = readFileSync(join(project, 'key.pem'));
  execFileSync('openssl', ['x509', '-req', '-in', join(project, 'leaf.csr'), '-CA', join(shared, 'ca.pem'),
    '-CAkey', join(shared, 'ca-key.pem'), '-set_serial', '42', '-days', '1', '-extfile', join(project, 'leaf.cnf'),
    '-extensions', 'leaf', '-out', join(project, 'cert.pem')], { stdio: 'pipe' });
  const old = new X509Certificate(readFileSync(join(project, 'cert.pem')));
  generateDevCertificate(project, shared);
  const renewed = new X509Certificate(readFileSync(join(project, 'cert.pem')));
  assert.ok(Date.parse(renewed.validTo) > Date.parse(old.validTo));
  assert.notEqual(old.serialNumber, renewed.serialNumber);
  assert.deepEqual(readFileSync(join(project, 'key.pem')), key);
});


test('stock macOS LibreSSL issues and reuses a verified certificate', { skip: process.platform !== 'darwin' }, t => {
  const dir = mkdtempSync(join(tmpdir(), 'http3-libressl-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const entry = resolve(__dirname, '../../lib/dev-cert.js');
  execFileSync(process.execPath, ['-e', `const {generateDevCertificate}=require(process.argv[1]); generateDevCertificate(process.argv[2]); generateDevCertificate(process.argv[2]);`, entry, dir],
    { env: { ...process.env, PATH: '/usr/bin:/bin' }, stdio: 'pipe' });
  assert.ok(new X509Certificate(readFileSync(join(dir, 'cert.pem'))).checkHost('localhost'));
});

test('concurrent projects safely share first-run CA generation', async t => {
  const dir = mkdtempSync(join(tmpdir(), 'http3-cert-race-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const entry = resolve(__dirname, '../../lib/dev-cert.js');
  const shared = join(dir, 'authority');
  await Promise.all(Array.from({ length: 6 }, (_, i) => new Promise<void>((resolve, reject) => {
    execFile(process.execPath, ['-e', 'require(process.argv[1]).generateDevCertificate(process.argv[2], process.argv[3])', entry, join(dir, 'project-' + i % 2), shared],
      { timeout: 10000 }, error => error ? reject(error) : resolve());
  })));
  const ca = new X509Certificate(readFileSync(join(shared, 'ca.pem')));
  for (const name of ['project-0', 'project-1']) {
    const cert = new X509Certificate(readFileSync(join(dir, name, 'cert.pem')));
    assert.ok(cert.verify(ca.publicKey));
  }
});
