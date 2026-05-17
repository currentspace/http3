import { after, before, describe, it } from 'node:test';
import assert from 'node:assert';
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { BrowserContext, BrowserType, LaunchOptions } from 'playwright';
import { chromium, firefox, webkit } from 'playwright';
import { serveFetch, createSseFetchResponse } from '../../lib/fetch-adapter.js';
import type { Http3SecureServer } from '../../lib/server.js';
import { generateMutualTlsTestCerts } from '../support/generate-certs.js';

const ENABLED = process.env.HTTP3_BROWSER_E2E === '1';
const BROWSER_TIMEOUT_MS = 45_000;
const BROWSER_TEST_HOST = '127.0.0.1';
const SYSTEM_KEYCHAIN_PATH = '/Library/Keychains/System.keychain';
const LINUX_CA_CERTIFICATES_PATH = '/usr/local/share/ca-certificates';

interface BrowserRunResult {
  readonly token: string;
  readonly navProtocol: string;
  readonly fetchProtocols: readonly string[];
  readonly sseProtocols: readonly string[];
  readonly fetchServerProtocols: readonly string[];
  readonly sseServerProtocols: readonly string[];
  readonly fetchStatus: string;
  readonly sseStatus: string;
}

interface BrowserCase {
  readonly name: 'chromium' | 'firefox' | 'webkit';
  readonly launcher: BrowserType;
  readonly primeAltSvcBeforeLaunch?: boolean;
  launchOptions(port: number): LaunchOptions;
  prepareProfile?(profileDir: string, caPath: string, port: number): void;
}

interface ObservedRequest {
  readonly path: string;
  readonly protocol: 'h2' | 'h3';
}

interface UserKeychainTrust {
  readonly mode: 'user-keychain';
  readonly keychainPath: string;
  readonly originalSearchList: readonly string[];
}

interface SystemKeychainTrust {
  readonly mode: 'system-keychain';
  readonly caPath: string;
  readonly sha1Fingerprint: string;
}

interface LinuxCaCertificatesTrust {
  readonly mode: 'linux-ca-certificates';
  readonly installedPath: string;
}

type BrowserCertificateTrust = UserKeychainTrust | SystemKeychainTrust | LinuxCaCertificatesTrust;

let nonInteractiveSudoAvailable: boolean | null = null;

function run(command: string, args: readonly string[]): string {
  return execFileSync(command, [...args], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    timeout: 15_000,
  });
}

function canUseNonInteractiveSudo(): boolean {
  if (nonInteractiveSudoAvailable !== null) return nonInteractiveSudoAvailable;
  try {
    execFileSync('sudo', ['-n', 'true'], {
      stdio: 'ignore',
      timeout: 5_000,
    });
    nonInteractiveSudoAvailable = true;
  } catch {
    nonInteractiveSudoAvailable = false;
  }
  return nonInteractiveSudoAvailable;
}

function shouldUseSudoSecurity(): boolean {
  const mode = (process.env.HTTP3_BROWSER_SECURITY_SUDO ?? 'auto').toLowerCase();
  if (mode === '0' || mode === 'false' || mode === 'no' || mode === 'off') return false;
  if (mode === '1' || mode === 'true' || mode === 'yes' || mode === 'on') {
    assert.ok(
      canUseNonInteractiveSudo(),
      'HTTP3_BROWSER_SECURITY_SUDO=1 requires cached sudo credentials; run sudo -v before test:browser:e2e',
    );
    return true;
  }
  assert.ok(
    canUseNonInteractiveSudo(),
    'browser H3 certificate trust setup requires cached sudo credentials on macOS; run sudo -v before test:browser:e2e',
  );
  return true;
}

function runSudoSecurity(args: readonly string[]): string {
  return run('sudo', ['-n', 'security', ...args]);
}

function parseKeychainSearchList(output: string): string[] {
  return output
    .split('\n')
    .map(line => line.trim().replace(/^"|"$/g, ''))
    .filter(Boolean);
}

function certificateSha1Fingerprint(caPath: string): string {
  const output = run('openssl', ['x509', '-in', caPath, '-noout', '-fingerprint', '-sha1']);
  const match = output.match(/Fingerprint=([A-Fa-f0-9:]+)/);
  assert.ok(match, `failed to read SHA-1 fingerprint from ${output}`);
  return match[1].replaceAll(':', '').toUpperCase();
}

function installSystemMacTrust(caPath: string): SystemKeychainTrust {
  const sha1Fingerprint = certificateSha1Fingerprint(caPath);
  runSudoSecurity(['add-trusted-cert', '-d', '-r', 'trustRoot', '-k', SYSTEM_KEYCHAIN_PATH, caPath]);
  return { mode: 'system-keychain', caPath, sha1Fingerprint };
}

function installTemporaryMacUserTrust(caPath: string, tempDir: string): UserKeychainTrust {
  const keychainPath = join(tempDir, 'http3-browser-test.keychain-db');
  const originalSearchList = parseKeychainSearchList(run('security', ['list-keychains', '-d', 'user']));
  try {
    run('security', ['create-keychain', '-p', '', keychainPath]);
    run('security', ['unlock-keychain', '-p', '', keychainPath]);
    run('security', ['set-keychain-settings', keychainPath]);
    run('security', ['list-keychains', '-d', 'user', '-s', keychainPath, ...originalSearchList]);
    run('security', ['add-trusted-cert', '-r', 'trustRoot', '-k', keychainPath, caPath]);
    return { mode: 'user-keychain', keychainPath, originalSearchList };
  } catch (err: unknown) {
    try {
      run('security', ['list-keychains', '-d', 'user', '-s', ...originalSearchList]);
    } catch {
      // Preserve the original failure.
    }
    try {
      run('security', ['delete-keychain', keychainPath]);
    } catch {
      // Preserve the original failure.
    }
    throw err;
  }
}

function installMacTrust(caPath: string, tempDir: string): BrowserCertificateTrust | null {
  if (process.platform !== 'darwin') return null;
  if (shouldUseSudoSecurity()) return installSystemMacTrust(caPath);
  return installTemporaryMacUserTrust(caPath, tempDir);
}

function canInstallLinuxSystemTrust(): boolean {
  if (process.platform !== 'linux') return false;
  if (typeof process.getuid === 'function' && process.getuid() !== 0) return false;
  try {
    run('which', ['update-ca-certificates']);
    return true;
  } catch {
    return false;
  }
}

function installLinuxTrust(caPath: string): BrowserCertificateTrust | null {
  if (!canInstallLinuxSystemTrust()) return null;
  const installedPath = join(LINUX_CA_CERTIFICATES_PATH, `http3-browser-e2e-${process.pid}.crt`);
  copyFileSync(caPath, installedPath);
  run('update-ca-certificates', []);
  return { mode: 'linux-ca-certificates', installedPath };
}

function installBrowserTrust(caPath: string, tempDir: string): BrowserCertificateTrust | null {
  if (process.platform === 'darwin') return installMacTrust(caPath, tempDir);
  if (process.platform === 'linux') return installLinuxTrust(caPath);
  return null;
}

function isLinuxContainer(): boolean {
  return process.platform === 'linux' && (existsSync('/.dockerenv') || existsSync('/run/.containerenv'));
}

function restoreBrowserTrust(trust: BrowserCertificateTrust | null): void {
  if (!trust) return;
  if (trust.mode === 'system-keychain') {
    runSudoSecurity(['delete-certificate', '-Z', trust.sha1Fingerprint, SYSTEM_KEYCHAIN_PATH]);
    return;
  }
  if (trust.mode === 'linux-ca-certificates') {
    try {
      rmSync(trust.installedPath, { force: true });
    } finally {
      run('update-ca-certificates', []);
    }
    return;
  }

  try {
    run('security', ['list-keychains', '-d', 'user', '-s', ...trust.originalSearchList]);
  } finally {
    try {
      run('security', ['delete-keychain', trust.keychainPath]);
    } catch {
      // Best-effort cleanup; the keychain is in the test temp directory.
    }
  }
}

function importCaIntoNssProfile(profileDir: string, caPath: string): void {
  mkdirSync(profileDir, { recursive: true });
  run('certutil', ['-N', '-d', `sql:${profileDir}`, '--empty-password']);
  run('certutil', ['-A', '-d', `sql:${profileDir}`, '-n', 'http3-browser-test-ca', '-t', 'C,,', '-i', caPath]);
}

function altSvcHeader(port: number): string {
  return `h3=":${port}"; ma=3600`;
}

function pageHtml(): string {
  return `<!doctype html>
<html>
  <head><meta charset="utf-8"><title>HTTP/3 browser smoke</title></head>
  <body>
    <div id="fetch-status">pending</div>
    <div id="sse-status">pending</div>
    <script>
      const token = crypto.randomUUID();
      document.body.dataset.token = token;
      fetch('/api/ping?token=' + token)
        .then((response) => response.json())
        .then((payload) => {
          document.getElementById('fetch-status').textContent = payload.ok ? 'fetch-ok' : 'fetch-failed';
        })
        .catch((error) => {
          document.body.dataset.fetchError = String(error);
          document.getElementById('fetch-status').textContent = 'fetch-failed';
        });

      const events = new EventSource('/events?token=' + token);
      events.addEventListener('ready', (event) => {
        document.getElementById('sse-status').textContent = event.data;
        events.close();
      });
      events.onerror = () => {
        document.body.dataset.sseError = 'eventsource-error';
      };
    </script>
  </body>
</html>`;
}

async function waitForServer(server: Http3SecureServer): Promise<number> {
  return await new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
  });
}

function protocolsForToken(observedRequests: readonly ObservedRequest[], pathPrefix: string, token: string): string[] {
  return observedRequests
    .filter(request => request.path.startsWith(pathPrefix) && request.path.includes(`token=${token}`))
    .map(request => request.protocol);
}

async function primeAltSvc(browserCase: BrowserCase, port: number, profileDir: string): Promise<void> {
  const context = await browserCase.launcher.launchPersistentContext(`${profileDir}-altsvc-prime`, {
    headless: true,
    ignoreHTTPSErrors: false,
  });
  try {
    const page = await context.newPage();
    await page.goto(`https://${BROWSER_TEST_HOST}:${port}/?altsvc-prime=1`, {
      waitUntil: 'load',
      timeout: BROWSER_TIMEOUT_MS,
    });
  } finally {
    await context.close();
  }
}

async function runBrowserCase(
  browserCase: BrowserCase,
  port: number,
  tempDir: string,
  caPath: string,
  observedRequests: readonly ObservedRequest[],
): Promise<BrowserRunResult> {
  const profileDir = join(tempDir, browserCase.name);
  browserCase.prepareProfile?.(profileDir, caPath, port);

  if (browserCase.primeAltSvcBeforeLaunch) {
    await primeAltSvc(browserCase, port, profileDir);
  }

  const context: BrowserContext = await browserCase.launcher.launchPersistentContext(profileDir, {
    headless: true,
    ignoreHTTPSErrors: false,
    ...browserCase.launchOptions(port),
  });

  try {
    const page = await context.newPage();
    const failedRequests: string[] = [];
    page.on('requestfailed', (request) => {
      failedRequests.push(`${request.url()} ${request.failure()?.errorText ?? '<unknown>'}`);
    });

    let result: BrowserRunResult | null = null;
    for (let attempt = 0; attempt < 4; attempt += 1) {
      await page.goto(`https://${BROWSER_TEST_HOST}:${port}/?attempt=${attempt}`, {
        waitUntil: 'load',
        timeout: BROWSER_TIMEOUT_MS,
      });
      await page.waitForFunction(() => {
        return document.getElementById('fetch-status')?.textContent === 'fetch-ok' &&
          document.getElementById('sse-status')?.textContent === 'sse-ok';
      }, null, { timeout: BROWSER_TIMEOUT_MS });
      await page.waitForTimeout(500);

      const pageResult = await page.evaluate(() => {
        const nav = performance.getEntriesByType('navigation').at(-1) as PerformanceNavigationTiming | undefined;
        const resources = performance.getEntriesByType('resource') as PerformanceResourceTiming[];
        return {
          token: document.body.dataset.token ?? '',
          navProtocol: nav?.nextHopProtocol ?? '',
          fetchProtocols: resources
            .filter(entry => entry.name.includes('/api/ping'))
            .map(entry => entry.nextHopProtocol),
          sseProtocols: resources
            .filter(entry => entry.name.includes('/events'))
            .map(entry => entry.nextHopProtocol),
          fetchStatus: document.getElementById('fetch-status')?.textContent ?? '',
          sseStatus: document.getElementById('sse-status')?.textContent ?? '',
        };
      });
      result = {
        ...pageResult,
        fetchServerProtocols: protocolsForToken(observedRequests, '/api/ping', pageResult.token),
        sseServerProtocols: protocolsForToken(observedRequests, '/events', pageResult.token),
      };

      if (
        result.navProtocol.startsWith('h3') &&
        (
          result.fetchProtocols.some(protocol => protocol.startsWith('h3')) ||
          result.fetchServerProtocols.includes('h3')
        ) &&
        (
          result.sseProtocols.some(protocol => protocol.startsWith('h3')) ||
          result.sseServerProtocols.includes('h3')
        )
      ) {
        return result;
      }
    }

    assert.fail(`${browserCase.name} did not move all page, fetch, and SSE traffic to h3; result=${JSON.stringify(result)} failed=${JSON.stringify(failedRequests)}`);
  } finally {
    await context.close();
  }
}

describe('browser HTTP/3 compatibility', { skip: !ENABLED && 'set HTTP3_BROWSER_E2E=1 to enable browser tests' }, () => {
  let tempDir = '';
  let trust: BrowserCertificateTrust | null = null;
  let server: Http3SecureServer | null = null;
  let port = 0;
  let caPath = '';
  const observedRequests: ObservedRequest[] = [];

  before(async () => {
    tempDir = mkdtempSync(join(tmpdir(), 'http3-browser-e2e-'));
    const certs = generateMutualTlsTestCerts();
    caPath = join(tempDir, 'ca.pem');
    writeFileSync(caPath, certs.ca.cert);
    trust = installBrowserTrust(caPath, tempDir);

    const certChain = Buffer.concat([certs.server.cert, certs.ca.cert]);
    server = serveFetch({
      port: 0,
      host: BROWSER_TEST_HOST,
      key: certs.server.key,
      cert: certChain,
      disableRetry: true,
      allowHTTP1: false,
      fetch: (request: Request): Response => {
        const url = new URL(request.url);
        const headers = { 'alt-svc': altSvcHeader(port) };
        if (url.pathname === '/api/ping') {
          return new Response(JSON.stringify({ ok: true }), {
            headers: { ...headers, 'content-type': 'application/json' },
          });
        }
        if (url.pathname === '/events') {
          async function* events(): AsyncGenerator<{ event: string; data: string }> {
            yield { event: 'ready', data: 'sse-ok' };
          }
          const response = createSseFetchResponse(events());
          response.headers.set('alt-svc', headers['alt-svc']);
          return response;
        }
        if (url.pathname === '/favicon.ico') {
          return new Response(null, { status: 204, headers });
        }
        return new Response(pageHtml(), {
          headers: { ...headers, 'content-type': 'text/html; charset=utf-8' },
        });
      },
    });
    server.on('stream', (stream, headers) => {
      const path = String(headers[':path'] ?? '');
      const protocol = stream.constructor.name === 'ServerHttp2StreamAdapter' ? 'h2' : 'h3';
      observedRequests.push({ path, protocol });
    });
    port = await waitForServer(server);
  });

  after(async () => {
    try {
      if (server) await server.close();
    } finally {
      try {
        restoreBrowserTrust(trust);
      } finally {
        if (tempDir) rmSync(tempDir, { recursive: true, force: true });
      }
    }
  });

  const cases: BrowserCase[] = [
    {
      name: 'chromium',
      launcher: chromium,
      prepareProfile(profileDir, ca, _port) {
        if (process.platform !== 'darwin') {
          importCaIntoNssProfile(profileDir, ca);
        }
      },
      launchOptions(testPort) {
        return {
          args: [
            '--no-sandbox',
            '--enable-quic',
            '--ignore-certificate-errors',
            `--origin-to-force-quic-on=${BROWSER_TEST_HOST}:${testPort}`,
          ],
        };
      },
    },
    {
      name: 'firefox',
      launcher: firefox,
      prepareProfile(profileDir, ca, _port) {
        if (process.platform !== 'darwin') {
          importCaIntoNssProfile(profileDir, ca);
        }
      },
      launchOptions(testPort) {
        return {
          firefoxUserPrefs: {
            'network.http.http3.enabled': true,
            'network.http.http3.enable': true,
            'security.enterprise_roots.enabled': true,
            'network.http.http3.disable_when_third_party_roots_found': false,
            'network.http.http3.version_negotiation.enabled': true,
            'network.http.http3.alt-svc-mapping-for-testing': `${BROWSER_TEST_HOST};h3=":${testPort}"`,
          },
        };
      },
    },
    {
      name: 'webkit',
      launcher: webkit,
      primeAltSvcBeforeLaunch: true,
      launchOptions() {
        return {
          args: process.platform === 'darwin' ? ['--enable-http3'] : [],
        };
      },
    },
  ];

  for (const browserCase of cases) {
    it(`${browserCase.name} loads page, fetch, and SSE over h3`, async (t) => {
      if (browserCase.name === 'chromium' && isLinuxContainer()) {
        t.skip('Playwright Chromium forced-QUIC validation is unreliable in Linux containers; Firefox covers Linux browser h3');
        return;
      }
      if (
        browserCase.name === 'webkit' &&
        process.platform !== 'darwin'
      ) {
        t.skip('Playwright WebKit HTTP/3 validation currently requires macOS Safari/WebKit');
        return;
      }
      const result = await runBrowserCase(browserCase, port, tempDir, caPath, observedRequests);
      assert.ok(result.navProtocol.startsWith('h3'), `${browserCase.name} navigation protocol ${result.navProtocol}`);
      assert.ok(
        result.fetchProtocols.some(protocol => protocol.startsWith('h3')) || result.fetchServerProtocols.includes('h3'),
        `${browserCase.name} fetch protocols ${JSON.stringify(result.fetchProtocols)} server=${JSON.stringify(result.fetchServerProtocols)}`,
      );
      assert.ok(
        result.sseProtocols.some(protocol => protocol.startsWith('h3')) || result.sseServerProtocols.includes('h3'),
        `${browserCase.name} SSE protocols ${JSON.stringify(result.sseProtocols)} server=${JSON.stringify(result.sseServerProtocols)}`,
      );
      assert.strictEqual(result.fetchStatus, 'fetch-ok');
      assert.strictEqual(result.sseStatus, 'sse-ok');
    });
  }
});
