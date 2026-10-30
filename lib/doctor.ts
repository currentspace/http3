/** Read-only local transport diagnostics. Never changes OS trust or browser profiles. */
import { connect as connectTls } from 'node:tls';
import { connect as connectH2 } from 'node:http2';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { connectAsync } from './client.js';

const exec = promisify(execFile);
export interface DoctorOptions { ca?: Buffer; chromeProfile?: string; timeoutMs?: number }
export interface DoctorCheck { name: string; status: 'pass' | 'fail' | 'unavailable'; detail: string }
export interface DoctorReport { origin: string; checks: DoctorCheck[] }
const message = (error: unknown): string => error instanceof Error ? error.message : String(error);

/** Report only matching origin entries; never print the rest of a browser's network history. */
export function chromeAlternativeServiceState(profile: string, host: string, port: number): DoctorCheck {
  const file = [join(profile, 'Network', 'Network Persistent State'), join(profile, 'Network Persistent State')].find(existsSync);
  if (!file) return { name: 'Chrome alternative-service cache', status: 'unavailable', detail: 'No Network Persistent State file found in the supplied profile' };
  const state = JSON.parse(readFileSync(file, 'utf8')) as { net?: { http_server_properties?: { broken_alternative_services?: { host?: string; port?: number | string; protocol?: string }[] } } };
  const entries = state.net?.http_server_properties?.broken_alternative_services;
  if (!Array.isArray(entries)) return { name: 'Chrome alternative-service cache', status: 'unavailable', detail: 'No recognized broken_alternative_services list; schema varies by Chromium version' };
  const found = entries.filter(entry => entry.host === host && Number(entry.port) === port && entry.protocol === 'h3');
  return { name: 'Chrome alternative-service cache', status: found.length ? 'fail' : 'pass', detail: found.length
    ? `${found.length} stored broken H3 alternative(s) for this origin. Close Chrome and use a fresh development profile to test without that cache. This command does not modify profiles.`
    : 'No stored broken H3 alternative for this origin. A running browser may have newer in-memory state.' };
}

/** Probes reachability, not OS socket ownership. A TLS failure alone cannot prove a UDP bind failure. */
export async function doctor(input: string | URL, options: DoctorOptions = {}): Promise<DoctorReport> {
  const url = new URL(input);
  if (url.protocol !== 'https:' || url.username || url.password) throw new TypeError('doctor requires an https URL without credentials');
  const timeoutMs = options.timeoutMs ?? 5000;
  if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60000) throw new RangeError('doctor timeoutMs must be between 1 and 60000');
  const port = Number(url.port || 443);
  const hostname = url.hostname.replace(/^\[|\]$/gu, '');
  const checks: DoctorCheck[] = [];
  const check = async (name: string, run: () => Promise<string>): Promise<void> => {
    try { checks.push({ name, status: 'pass', detail: await run() }); }
    catch (error) { checks.push({ name, status: 'fail', detail: message(error) }); }
  };
  const local = hostname === 'localhost' || hostname === '127.0.0.1' || hostname === '::1';
  for (const host of local ? ['127.0.0.1', '::1'] : [hostname]) {
    await check(`TCP/TLS reachability ${host}`, async () => new Promise<string>((resolve, reject) => {
      const socket = connectTls({ host, port, servername: hostname, rejectUnauthorized: false });
      const timer = setTimeout(() => socket.destroy(new Error('TCP/TLS probe timed out')), timeoutMs);
      socket.once('error', reject);
      socket.once('secureConnect', () => { resolve('TLS handshake reached the listener; certificate trust checked separately'); socket.destroy(); });
      socket.once('close', () => clearTimeout(timer));
    }));
    await check(`UDP/H3 reachability ${host}`, async () => {
      const session = await connectAsync(`https://${host.includes(':') ? '[' + host + ']' : host}:${port}`, {
        servername: hostname, rejectUnauthorized: false, connectTimeoutMs: timeoutMs, runtimeMode: 'portable',
      });
      await session.close();
      return 'QUIC/H3 handshake reached the listener; this transport probe does not verify certificate trust';
    });
  }
  // Separate process loads system roots at startup, without NODE_EXTRA_CA_CERTS.
  await check('System certificate trust (Node TLS)', async () => {
    const env = { ...process.env };
    delete env.NODE_EXTRA_CA_CERTS; delete env.NODE_TLS_REJECT_UNAUTHORIZED; delete env.NODE_OPTIONS;
    await exec(process.execPath, ['--use-system-ca', '-e', `
      const s = require('node:tls').connect({host: process.argv[1], port: Number(process.argv[2]), servername: process.argv[1]});
      s.setTimeout(Number(process.argv[3]), () => s.destroy(new Error('TLS timeout')));
      s.on('error', e => { console.error(e.message); process.exitCode = 1; });
      s.on('secureConnect', () => s.destroy());
    `, hostname, String(port), String(timeoutMs)], { env, timeout: timeoutMs + 1000 });
    return 'Node accepted the server chain using system roots; Chromium has additional local-CA rules for QUIC';
  });
  await check('Verified H3 request', async () => {
    const session = await connectAsync(url.origin, { ca: options.ca, connectTimeoutMs: timeoutMs, runtimeMode: 'portable' });
    try {
      const stream = session.request({ ':method': 'HEAD', ':path': url.pathname + url.search, ':authority': url.host }, { endStream: true });
      const timer = setTimeout(() => stream.destroy(new Error('H3 response timed out')), timeoutMs);
      try { for await (const _chunk of stream) { /* drain */ } } finally { clearTimeout(timer); }
      return 'H3 request completed with certificate verification enabled';
    } finally { await session.close(); }
  });
  await check('Alt-Svc over H2', async () => new Promise<string>((resolve, reject) => {
    const session = connectH2(url.origin, { ca: options.ca, rejectUnauthorized: true });
    const timer = setTimeout(() => session.destroy(new Error('H2 probe timed out')), timeoutMs);
    session.once('close', () => clearTimeout(timer)); session.once('error', reject);
    const stream = session.request({ ':method': 'HEAD', ':path': url.pathname + url.search });
    stream.once('error', reject);
    stream.once('response', headers => {
      const altSvc = headers['alt-svc'];
      if (typeof altSvc !== 'string' || !/(?:^|,)\s*h3="/u.test(altSvc)) reject(new Error('No H3 Alt-Svc advertisement in the HEAD response'));
      else resolve(altSvc);
    });
    stream.resume(); stream.once('end', () => session.close()); stream.end();
  }));
  try {
    const version = await exec('curl', ['--version'], { timeout: timeoutMs });
    if (!/\bHTTP3\b/u.test(version.stdout)) checks.push({ name: 'curl --http3-only', status: 'unavailable', detail: 'Installed curl was built without HTTP/3' });
    else await check('curl --http3-only', async () => {
      // --ca is consumed by the native verified H3 check; curl uses its own trust store.
      const result = await exec('curl', ['--http3-only', '--silent', '--show-error', '--head', '--max-time', String(timeoutMs / 1000), '--write-out', '\nhttp_version=%{http_version}\n', url.href], { timeout: timeoutMs + 1000, maxBuffer: 65536 });
      if (!/http_version=3/u.test(result.stdout)) throw new Error('curl did not report HTTP/3');
      return 'curl verified the certificate and completed an HTTP/3 HEAD request using its own trust store';
    });
  } catch (error) { checks.push({ name: 'curl --http3-only', status: 'unavailable', detail: message(error) }); }
  if (options.chromeProfile) {
    try { checks.push(chromeAlternativeServiceState(options.chromeProfile, hostname, port)); }
    catch (error) { checks.push({ name: 'Chrome alternative-service cache', status: 'unavailable', detail: message(error) }); }
  } else checks.push({ name: 'Chrome alternative-service cache', status: 'unavailable', detail: 'Pass --chrome-profile DIRECTORY to inspect one profile; profiles are never modified' });
  return { origin: url.origin, checks };
}
