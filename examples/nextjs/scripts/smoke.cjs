const assert = require('node:assert/strict');
const { spawn, execFileSync } = require('node:child_process');
const { once } = require('node:events');
const { createHash } = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const appDir = path.resolve(__dirname, '..');
const port = Number(process.env.PORT || 3443);
const origin = `https://localhost:${port}`;
const protocol = process.env.HTTP3_TEST_PROTOCOL || 'h3';
assert.ok(['h3', 'h2'].includes(protocol));
assert.ok(Number.isInteger(port) && port > 1 && port <= 65535, 'Use an app port other than the diagnostic port 1');
const rounds = Number(process.env.HTTP3_TEST_NAVIGATION_ROUNDS || 10);
assert.ok(Number.isInteger(rounds) && rounds > 0, 'Navigation rounds must be a positive integer');
process.env.TLS_CERT_FILE ||= path.join(appDir, '.certs/cert.pem');
process.env.TLS_KEY_FILE ||= path.join(appDir, '.certs/key.pem');
const forceH2 = protocol === 'h2';
const certificate = execFileSync('openssl', ['x509', '-in', process.env.TLS_CERT_FILE, '-pubkey', '-noout']);
const publicKey = execFileSync('openssl', ['pkey', '-pubin', '-outform', 'DER'], { input: certificate });
const pin = createHash('sha256').update(publicKey).digest('base64');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, timeout, description) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, `Timed out: ${description}`);
    await delay(50);
  }
}
(async () => {
  const server = spawn(process.execPath, [path.join(appDir, 'server.cjs')], {
    cwd: appDir,
    env: { ...process.env, PORT: String(port), HTTP3_SMOKE_UPLOAD_PROGRESS: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '', exited = false;
  server.stdout.on('data', data => { logs += data; });
  server.stderr.on('data', data => { logs += data; });
  const exit = once(server, 'exit').then(([code, signal]) => { exited = true; return { code, signal }; });
  let browser;
  const counter = path.join(appDir, 'app/counter.jsx');
  const original = fs.readFileSync(counter, 'utf8');
  const report = { automaticAltSvc: !forceH2, targetOriginForced: false, localRootException: !forceH2, node: process.version, mode: process.env.NODE_ENV || 'development', packagePath: require.resolve('@currentspace/http3/node'), protocol, responses: [], failures: [], errors: [], websocketStatuses: [] };
  server.on('error', error => { logs += String(error); });
  try {
    await until(() => exited || logs.includes('Ready: https://'), 60000, 'Next startup');
    assert.equal(exited, false, logs);
    browser = await chromium.launch({ ...(process.env.CHROMIUM_EXECUTABLE ? {executablePath:process.env.CHROMIUM_EXECUTABLE} : {}), args: [
      ...(forceH2 ? ['--disable-quic'] : []),
      // Local certificate-policy diagnostic: force an UNUSED port only.
      // Chromium permits unknown roots by hostname, but forced transport matches host AND port.
      ...(!forceH2 ? ['--origin-to-force-quic-on=localhost:1'] : []),
      ...(process.env.HTTP3_NETLOG ? [`--log-net-log=${process.env.HTTP3_NETLOG}`] : []),
      `--ignore-certificate-errors-spki-list=${pin}`,
    ] });
    report.browser = browser.version();
    const page = await browser.newPage();
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('Network.enable');
    await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    cdp.on('Network.responseReceived', ({ response: r }) => {
      if (r.url.startsWith(origin)) report.responses.push({ url: r.url, status: r.status, protocol: r.protocol, headers: r.headers });
    });
    cdp.on('Network.webSocketHandshakeResponseReceived', e => report.websocketStatuses.push(e.response.status));
    report.console = [];
    page.on('console', message => { if (message.type() === 'error') report.console.push(message.text()); });
    page.on('pageerror', error => report.errors.push(String(error)));
    page.on('requestfailed', request => report.failures.push({ url: request.url(), failure: request.failure() }));
    await page.goto(origin, { waitUntil: 'networkidle' });
    report.initialDocument = report.responses.find(r => r.url === origin + '/');
    assert.equal(report.initialDocument.protocol, 'h2');
    assert.equal(report.initialDocument.headers['alt-svc'], `h3=":${port}"; ma=60`);
    report.discoveryReloads = [];
    for (let attempt = 0; attempt < (forceH2 ? 1 : 12); attempt++) {
      await delay(250);
      const before = report.responses.length;
      await page.reload({ waitUntil: 'networkidle' });
      const document = report.responses.slice(before).find(r => r.url === origin + '/');
      report.discoveryReloads.push(document.protocol);
      if (document.protocol === 'h3') break;
    }
    assert.equal(report.discoveryReloads.at(-1), protocol);
    const learnedAt = report.responses.length;
    await page.getByRole('button', { name: 'Count 0' }).click();
    await page.getByRole('button', { name: 'Count 1', exact: true }).waitFor();
    report.hydrated = true;
    if (process.env.NODE_ENV !== 'production') {
      fs.writeFileSync(counter, original.replace('Count {n}', 'Updated {n}'));
      await page.getByRole('button', { name: 'Updated 1', exact: true }).waitFor({ timeout: 20000 });
      report.refreshPreservedState = true;
      await page.waitForLoadState('networkidle');
    }
    assert.deepEqual(await page.evaluate(() => fetch('/api/echo', { method: 'POST', body: 'hello HTTP/3' }).then(r => r.json())),
      { body: 'hello HTTP/3' });
    report.uploads = await require('./upload-smoke.cjs')(page, async id => {
      const prefix = 'HTTP3_UPLOAD_PREFIX ';
      const observed = () => logs.slice(0, logs.lastIndexOf('\n') + 1).split('\n')
        .filter(line => line.startsWith(prefix))
        .map(line => JSON.parse(line.slice(prefix.length)))
        .find(entry => entry.id === id);
      await until(() => observed(), 15000, 'route consumed upload prefix before EOF');
      return observed().received;
    });
    const fullRsc = await page.evaluate(() => fetch('/about', { headers: { rsc: '1' } }).then(r => r.text()));
    assert.ok(fullRsc.includes('Client navigation works'));
    report.rscBytes = Buffer.byteLength(fullRsc);
    await page.evaluate(() => { window.__http3NavigationSentinel = 'same-document'; });
    await page.waitForLoadState('networkidle');
    await page.getByRole('link', { name: 'About' }).click();
    await page.getByRole('heading', { name: 'Client navigation works' }).waitFor();
    await page.waitForLoadState('networkidle');
    assert.equal(await page.evaluate(() => window.__http3NavigationSentinel), 'same-document');
    report.navigation = true;
    assert.ok(report.responses.some(r => r.url.includes('_rsc=')), 'client navigation fetched an RSC response');
    report.navigationRounds = 1;
    for (let round = 1; round < rounds; round++) {
      await page.goto(origin, { waitUntil: 'networkidle' });
      await page.getByRole('button', { name: /^(Count|Updated) 0$/ }).click();
      await page.getByRole('button', { name: /^(Count|Updated) 1$/ }).waitFor();
      await page.evaluate(() => { window.__http3NavigationSentinel = 'same-document'; });
      await page.getByRole('link', { name: 'About' }).click();
      await page.getByRole('heading', { name: 'Client navigation works' }).waitFor();
      await page.waitForLoadState('networkidle');
      assert.equal(await page.evaluate(() => window.__http3NavigationSentinel), 'same-document');
      report.navigationRounds++;
    }

    assert.ok(report.responses.some(r => Object.entries(r.headers).some(([name, value]) =>
      name.toLowerCase() === 'content-encoding' && value === 'gzip')), 'must exercise compressed responses');
    assert.ok(report.responses.length > 5);
    assert.ok(report.responses.slice(learnedAt).every(r => r.protocol === protocol), 'all requests after discovery must use the requested protocol');
    if (process.env.NODE_ENV !== 'production') assert.ok(report.websocketStatuses.includes(101), 'Fast Refresh must use the public HTTP/1 upgrade bridge');
    assert.deepEqual(report.failures, []);
    assert.deepEqual(report.console, []);
    assert.deepEqual(report.errors, []);
    // Close with the development WebSocket still connected.
    const start = Date.now();
    server.kill('SIGTERM');
    await until(() => exited, 5000, 'server shutdown with active HMR');
    assert.deepEqual(await exit, { code: 0, signal: null }, logs);
    report.shutdownMs = Date.now() - start;
    report.passed = true;
    console.log(JSON.stringify({ protocol, mode: report.mode, responses: report.responses.length, hydrated: true,
      refreshPreservedState: report.refreshPreservedState, navigation: true, shutdownMs: report.shutdownMs }));
  } catch (error) {
    console.error(logs);
    throw error;
  } finally {
    fs.writeFileSync(counter, original);
    if (browser) await browser.close();
    if (!exited) { server.kill('SIGKILL'); await exit; }
    if (process.env.HTTP3_TEST_REPORT) fs.writeFileSync(process.env.HTTP3_TEST_REPORT, JSON.stringify({ ...report, logs }, null, 2));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
