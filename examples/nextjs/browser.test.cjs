const assert = require('node:assert/strict');
const { spawn, execFileSync } = require('node:child_process');
const { once } = require('node:events');
const { createHash } = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('../../node_modules/playwright');
const port = Number(process.env.PORT || 3443);
const origin = `https://localhost:${port}`;
const protocol = process.env.HTTP3_TEST_PROTOCOL || 'h3';
assert.ok(['h3', 'h2'].includes(protocol));
assert.ok(process.env.TLS_CERT_FILE && process.env.TLS_KEY_FILE, 'Set TLS_CERT_FILE and TLS_KEY_FILE');
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
  const server = spawn(process.execPath, [path.join(__dirname, 'server.cjs')], {
    cwd: __dirname, env: { ...process.env, PORT: String(port) }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '', exited = false;
  server.stdout.on('data', data => { logs += data; });
  server.stderr.on('data', data => { logs += data; });
  const exit = once(server, 'exit').then(([code, signal]) => { exited = true; return { code, signal }; });
  let browser;
  const counter = path.join(__dirname, 'app/counter.jsx');
  const original = fs.readFileSync(counter, 'utf8');
  const report = { protocol, responses: [], failures: [], errors: [], websocketStatuses: [] };
  try {
    await until(() => exited || logs.includes('Ready: https://'), 60000, 'Next startup');
    assert.equal(exited, false, logs);
    browser = await chromium.launch({ args: [
      ...(protocol === 'h3' ? ['--enable-quic', `--origin-to-force-quic-on=localhost:${port}`] : ['--disable-quic']),
      '--host-resolver-rules=MAP localhost 127.0.0.1',
      `--ignore-certificate-errors-spki-list=${pin}`,
    ] });
    const page = await browser.newPage();
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('Network.enable');
    cdp.on('Network.responseReceived', ({ response: r }) => {
      if (r.url.startsWith(origin)) report.responses.push({ url: r.url, status: r.status, protocol: r.protocol, headers: r.headers });
    });
    cdp.on('Network.webSocketHandshakeResponseReceived', e => report.websocketStatuses.push(e.response.status));
    report.console = [];
    page.on('console', message => { if (message.type() === 'error') report.console.push(message.text()); });
    page.on('pageerror', error => report.errors.push(String(error)));
    page.on('requestfailed', request => report.failures.push({ url: request.url(), failure: request.failure() }));
    await page.goto(origin, { waitUntil: 'networkidle' });
    await page.getByRole('button', { name: 'Count 0' }).click();
    await page.getByRole('button', { name: 'Count 1', exact: true }).waitFor();
    report.hydrated = true;
    fs.writeFileSync(counter, original.replace('Count {n}', 'Updated {n}'));
    await page.getByRole('button', { name: 'Updated 1', exact: true }).waitFor({ timeout: 20000 });
    report.refreshPreservedState = true;
    await page.waitForLoadState('networkidle');
    assert.deepEqual(await page.evaluate(() => fetch('/api/echo', { method: 'POST', body: 'hello HTTP/3' }).then(r => r.json())),
      { body: 'hello HTTP/3' });
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
    assert.ok(report.responses.some(r => Object.entries(r.headers).some(([name, value]) =>
      name.toLowerCase() === 'content-encoding' && value === 'gzip')), 'must exercise compressed responses');
    assert.ok(report.responses.length > 5);
    assert.ok(report.responses.every(r => r.protocol === protocol), 'must verify negotiated protocol, not just page success');
    assert.ok(report.websocketStatuses.includes(101), 'Fast Refresh must use the public HTTP/1 upgrade bridge');
    assert.deepEqual(report.failures, []);
    assert.deepEqual(report.console, []);
    assert.deepEqual(report.errors, []);
    // Close with the development WebSocket still connected.
    const start = Date.now();
    server.kill('SIGTERM');
    await until(() => exited, 5000, 'server shutdown with active HMR');
    assert.deepEqual(await exit, { code: 0, signal: null }, logs);
    report.shutdownMs = Date.now() - start;
    console.log(JSON.stringify({ protocol, responses: report.responses.length, hydrated: true,
      refreshPreservedState: true, navigation: true, shutdownMs: report.shutdownMs }));
  } finally {
    fs.writeFileSync(counter, original);
    if (browser) await browser.close();
    if (!exited) { server.kill('SIGKILL'); await exit; }
    if (process.env.HTTP3_TEST_REPORT) fs.writeFileSync(process.env.HTTP3_TEST_REPORT, JSON.stringify({ ...report, logs }, null, 2));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
