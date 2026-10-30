/** Local development certificates. This module never loads the native addon. */
import { createHash, createPrivateKey, createPublicKey, X509Certificate } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';

const HELP = `Usage: http3 dev-cert [generate|trust|untrust] [--out DIRECTORY] [--trust]

Generate a localhost CA and certificate (OpenSSL required). Default directory:
  ~/.http3/dev-cert

Generation does not change trust settings. --trust explicitly installs this CA
in the OS/browser trust store; "untrust" removes it. Private keys stay local.
The CA is restricted to localhost, its subdomains and loopback IP addresses.
The issued leaf covers the hostname localhost over IPv4 or IPv6, not IP URLs.
Linux trust requires sudo, update-ca-certificates (or update-ca-trust), and the
NSS certutil tool for Chrome. macOS trust requires sudo for the System keychain.

Examples:
  http3 dev-cert --out .certs
  http3 dev-cert trust --out .certs
  http3 dev-cert untrust --out .certs
`;

function command(file: string, args: string[]): string {
  return execFileSync(file, args, { encoding: 'utf8', stdio: ['inherit', 'pipe', 'pipe'] }).trim();
}
function openssl(args: string[]): string { return command('openssl', args); }
function fingerprint(cert: X509Certificate): string { return cert.fingerprint256.replaceAll(':', '').toLowerCase(); }
function privateFile(path: string, contents: string): void { writeFileSync(path, contents, { mode: 0o600, flag: 'wx' }); }

function loadCa(directory: string, forRemoval = false): X509Certificate {
  const ca = new X509Certificate(readFileSync(join(directory, 'ca.pem')));
  const state: unknown = JSON.parse(readFileSync(join(directory, 'ca.json'), 'utf8'));
  if (!state || typeof state !== 'object' || !('sha256' in state) || state.sha256 !== fingerprint(ca) || !ca.ca) {
    throw new Error('CA does not match the generated certificate record; refusing to trust or overwrite it.');
  }
  if (forRemoval) return ca;
  if (Date.parse(ca.validTo) <= Date.now()) throw new Error('Development CA has expired. Untrust it before creating a new CA in another directory.');
  const publicKey = createPublicKey(createPrivateKey(readFileSync(join(directory, 'ca-key.pem'))));
  if (!publicKey.equals(ca.publicKey)) throw new Error('CA private key does not match ca.pem');
  return ca;
}

export function generateDevCertificate(directory: string): void {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  chmodSync(directory, 0o700);
  const caFile = join(directory, 'ca.pem');
  if (!existsSync(caFile)) {
    if (['ca-key.pem', 'ca.json', 'key.pem', 'cert.pem'].some(file => existsSync(join(directory, file)))) {
      throw new Error('Certificate directory contains existing keys or certificates. Use a new directory; files will not be overwritten.');
    }
    // An IPv4 /8 and IPv6 /128 permit loopback only. DNS constraints permit
    // localhost and its subdomains, not arbitrary development or public names.
    const config = join(directory, 'ca.cnf');
    privateFile(config, `[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\nCN=http3 localhost development CA\n[ca]\nbasicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\nnameConstraints=critical,permitted;DNS:localhost,permitted;DNS:.localhost,permitted;IP:127.0.0.0/255.0.0.0,permitted;IP:0:0:0:0:0:0:0:1/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff\n`);
    openssl(['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '365', '-config', config,
      '-keyout', join(directory, 'ca-key.pem'), '-out', caFile]);
    chmodSync(join(directory, 'ca-key.pem'), 0o600);
    privateFile(join(directory, 'ca.json'), JSON.stringify({ sha256: fingerprint(new X509Certificate(readFileSync(caFile))) }) + '\n');
  }
  const ca = loadCa(directory);
  if (existsSync(join(directory, 'cert.pem')) || existsSync(join(directory, 'key.pem'))) {
    openssl(['verify', '-CAfile', caFile, '-verify_hostname', 'localhost', join(directory, 'cert.pem')]);
    const cert = new X509Certificate(readFileSync(join(directory, 'cert.pem')));
    if (!createPublicKey(createPrivateKey(readFileSync(join(directory, 'key.pem')))).equals(cert.publicKey)) throw new Error('Leaf key does not match cert.pem');
    if (Date.parse(cert.validTo) <= Date.now() + 86400000) throw new Error('Leaf certificate expires within a day. Remove only key.pem and cert.pem, then regenerate using this CA.');
    return;
  }
  const days = Math.min(30, Math.floor((Date.parse(ca.validTo) - Date.now()) / 86400000));
  if (days < 1) throw new Error('CA expires too soon to issue a leaf. Untrust it and create a new CA in another directory.');
  // BoringSSL's legacy verifier rejects IP SANs under IP name constraints.
  // Keep the CA fully constrained and issue a DNS-only localhost leaf; both
  // network families work with localhost as the verified server name.
  const config = join(directory, 'leaf.cnf');
  writeFileSync(config, `[req]\nprompt=no\ndistinguished_name=dn\n[dn]\nCN=localhost\n[leaf]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n`, { mode: 0o600 });
  openssl(['req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-config', config,
    '-keyout', join(directory, 'key.pem'), '-out', join(directory, 'leaf.csr')]);
  chmodSync(join(directory, 'key.pem'), 0o600);
  openssl(['x509', '-req', '-in', join(directory, 'leaf.csr'), '-CA', caFile, '-CAkey', join(directory, 'ca-key.pem'),
    '-set_serial', '0x' + createHash('sha256').update(readFileSync(join(directory, 'leaf.csr'))).digest('hex').slice(0, 32),
    '-days', String(days), '-extfile', config, '-extensions', 'leaf', '-out', join(directory, 'cert.pem')]);
  openssl(['verify', '-CAfile', caFile, '-verify_hostname', 'localhost', join(directory, 'cert.pem')]);
}

export interface TrustCommand { file: string; args: string[] }
/** Build exact trust operations, also used by tests without changing host trust. */
export function devTrustCommands(
  directory: string, ca: X509Certificate, remove: boolean,
  platform: string, options: { keychain?: string; linuxTool?: string; nssDirectories?: string[] } = {},
): TrustCommand[] {
  const caFile = join(directory, 'ca.pem');
  const id = 'http3-localhost-' + fingerprint(ca).slice(0, 24);
  if (platform === 'darwin') {
    const keychain = options.keychain ?? '/Library/Keychains/System.keychain';
    return remove ? [
      { file: 'sudo', args: ['security', 'remove-trusted-cert', '-d', caFile] },
      { file: 'sudo', args: ['security', 'delete-certificate', '-Z', ca.fingerprint.replaceAll(':', ''), keychain] },
    ] : [{ file: 'sudo', args: ['security', 'add-trusted-cert', '-d', '-r', 'trustRoot', '-k', keychain, caFile] }];
  }
  if (platform === 'linux') {
    if (!options.linuxTool || (!remove && !options.nssDirectories?.length)) throw new Error('Linux trust needs update-ca-certificates/update-ca-trust and an NSS database');
    const debian = options.linuxTool.endsWith('update-ca-certificates');
    const target = join(debian ? '/usr/local/share/ca-certificates' : '/etc/pki/ca-trust/source/anchors', id + '.crt');
    return [
      { file: 'sudo', args: remove ? ['rm', '-f', target] : ['install', '-m', '644', caFile, target] },
      { file: 'sudo', args: [options.linuxTool] },
      ...(options.nssDirectories ?? []).map(db => ({ file: 'certutil', args: remove
        ? ['-D', '-d', 'sql:' + db, '-n', id]
        : ['-A', '-d', 'sql:' + db, '-n', id, '-t', 'C,,', '-i', caFile] })),
    ];
  }
  if (platform === 'win32') return [{ file: 'certutil.exe', args: remove
    ? ['-user', '-delstore', 'Root', ca.fingerprint.replaceAll(':', '')]
    : ['-user', '-addstore', 'Root', caFile] }];
  throw new Error(`Automatic trust is not supported on ${platform}. Import ca.pem manually.`);
}

function changeTrust(directory: string, remove: boolean): void {
  // Removal must also work after expiry; never require the CA private key to untrust.
  const ca = loadCa(directory, remove);
  const options: Parameters<typeof devTrustCommands>[4] = {};
  if (process.platform === 'linux') {
    options.linuxTool = ['/usr/sbin/update-ca-certificates', '/usr/bin/update-ca-trust'].find(existsSync);
    if (!options.linuxTool) throw new Error('Install update-ca-certificates or update-ca-trust before using automatic trust.');
    try { command('certutil', ['-H']); } catch (error) {
      // NSS certutil -H may return 1 even when installed. ENOENT is the missing executable case.
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') throw new Error('Install the NSS certutil tool (libnss3-tools/nss-tools) before using automatic trust.', { cause: error });
    }
    const candidates = [join(homedir(), '.pki/nssdb'), join(homedir(), '.local/share/pki/nssdb')];
    options.nssDirectories = candidates.filter(db => existsSync(join(db, 'cert9.db')));
    if (!remove && options.nssDirectories.length === 0) {
      const db = candidates[0];
      mkdirSync(db, { recursive: true, mode: 0o700 });
      command('certutil', ['-N', '-d', 'sql:' + db, '--empty-password']);
      options.nssDirectories.push(db);
    }
  }
  const commands = devTrustCommands(directory, ca, remove, process.platform, options);
  console.log(`${remove ? 'Removing' : 'Trusting'} localhost CA SHA-256 ${ca.fingerprint256}`);
  for (const item of commands) execFileSync(item.file, item.args, { stdio: 'inherit' });
  console.log(remove ? 'Removed local CA trust. Certificate files were retained.' : 'Installed local CA trust. Restart the browser to reload trust settings.');
}

function printUsage(directory: string): void {
  const cert = new X509Certificate(readFileSync(join(directory, 'cert.pem')));
  const pin = createHash('sha256').update(cert.publicKey.export({ type: 'spki', format: 'der' })).digest('base64');
  const quote = (value: string): string => "'" + value.replaceAll("'", "'\\''") + "'";
  console.log('For POSIX shells:');
  console.log(`TLS_KEY_FILE=${quote(join(directory, 'key.pem'))}\nTLS_CERT_FILE=${quote(join(directory, 'cert.pem'))}\nTLS_CA_FILE=${quote(join(directory, 'ca.pem'))}\nNODE_EXTRA_CA_CERTS=${quote(join(directory, 'ca.pem'))}`);
  console.log('Set NODE_EXTRA_CA_CERTS before starting Node clients. For this library, pass ca: readFileSync(ca.pem) explicitly; its native TLS engine does not use Node\'s CA store.');
  console.log('Chrome local QUIC: launch a separate development browser profile with --origin-to-force-quic-on=localhost:1');
  console.log('Port 1 is unused; the application port is learned through Alt-Svc. Check the Network Protocol column for h3.');
  console.log(`For an isolated browser test without OS trust, also pin only this leaf with --ignore-certificate-errors-spki-list=${pin}`);
  console.log('A 127.0.0.1-only bind cannot receive localhost QUIC attempts to ::1. Use a dual-stack :: listener (all interfaces), or an explicit matching IP URL.');
}

export function runDevCertCli(args: string[]): void {
  try {
    if (args.includes('--help') || args.includes('-h')) { console.log(HELP); return; }
    if (args.shift() !== 'dev-cert') throw new Error(HELP);
    let action = 'generate';
    if (args[0] && !args[0].startsWith('--')) action = args.shift() ?? '';
    if (!['generate', 'trust', 'untrust'].includes(action)) throw new Error('Expected generate, trust, or untrust');
    let directory = join(homedir(), '.http3/dev-cert');
    let trust = false;
    while (args.length) {
      const arg = args.shift();
      if (arg === '--out' && args[0] && !args[0].startsWith('--')) directory = resolve(args.shift() ?? '');
      else if (arg === '--trust' && action === 'generate') trust = true;
      else throw new Error(`Unknown or incomplete option: ${String(arg)}`);
    }
    if (action === 'generate') {
      generateDevCertificate(directory);
      if (trust) changeTrust(directory, false);
      else console.log('Generated/reused local certificates. Trust settings were not changed. Run "http3 dev-cert trust --out DIRECTORY" to install trust explicitly.');
      printUsage(directory);
    } else {
      changeTrust(directory, action === 'untrust');
      if (action === 'trust') printUsage(directory);
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
