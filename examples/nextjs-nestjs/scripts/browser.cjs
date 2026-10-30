const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const { chromium } = require('playwright');

(async () => {
  const { startServer, until, directory, origin, port, certFile } = await import('./harness.mjs');
  const protocol = process.env.HTTP3_TEST_PROTOCOL ?? 'h3';
  assert.ok(['h3', 'h2'].includes(protocol));
  const certificate = execFileSync('openssl', ['x509', '-in', certFile, '-pubkey', '-noout']);
  const publicKey = execFileSync('openssl', ['pkey', '-pubin', '-outform', 'DER'], { input: certificate });
  const pin = createHash('sha256').update(publicKey).digest('base64');
  const server = await startServer();
  const report = { node: process.version, mode: process.env.NODE_ENV ?? 'development', protocol,
    pid: server.pid, responses: [], failures: [], pageErrors: [], websocketStatuses: [],
    targetOriginForced: false, localCertificateException: true };
  const controls = path.join(directory, 'web/app/controls.jsx');
  const original = fs.readFileSync(controls, 'utf8');
  let browser;
  try {
    browser = await chromium.launch({ args: [
      ...(protocol === 'h2' ? ['--disable-quic'] : ['--origin-to-force-quic-on=localhost:1']),
      // The unused port 1 only enables Chromium's localhost root exception.
      // The app port is learned through Alt-Svc, never forced to QUIC.
      '--host-resolver-rules=MAP localhost 127.0.0.1',
      `--ignore-certificate-errors-spki-list=${pin}`,
    ] });
    report.browser = browser.version();
    const page = await browser.newPage();
    page.setDefaultTimeout(20000);
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('Network.enable');
    await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    cdp.on('Network.responseReceived', ({ response }) => {
      if (response.url.startsWith(origin)) report.responses.push({
        url: response.url, status: response.status, protocol: response.protocol, headers: response.headers,
      });
    });
    cdp.on('Network.webSocketHandshakeResponseReceived', e => report.websocketStatuses.push(e.response.status));
    page.on('pageerror', error => report.pageErrors.push(String(error)));
    page.on('requestfailed', req => report.failures.push({ url: req.url(), failure: req.failure() }));
    await page.goto(origin, { waitUntil: 'networkidle' });
    const initial = report.responses.find(r => r.url === origin + '/');
    assert.equal(initial.protocol, 'h2');
    assert.equal(initial.headers['alt-svc'], `h3=":${port}"; ma=60`);
    for (let n = 0; n < 12; n++) {
      await page.waitForTimeout(250);
      await page.reload({ waitUntil: 'networkidle' });
      if (report.responses.filter(r => r.url === origin + '/').at(-1).protocol === protocol) break;
    }
    assert.equal(report.responses.filter(r => r.url === origin + '/').at(-1).protocol, protocol);
    const learned = report.responses.length;
    assert.equal(Number(await page.locator('#server-pid').textContent()), server.pid);
    assert.equal(Number(await page.locator('#server-bff-pid').textContent()), server.pid);
    assert.equal(await page.locator('#server-bff-version').textContent(), '3.0');
    await page.getByRole('button', { name: 'Count 0' }).click();
    await page.getByRole('button', { name: 'Count 1', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Call BFF' }).click();
    await page.waitForFunction(() => document.querySelector('#bff-result').textContent.includes('nestjs-express'));
    const bff = JSON.parse(await page.locator('#bff-result').textContent());
    assert.equal(bff.pid, server.pid);
    assert.equal(bff.httpVersion, protocol === 'h3' ? '3.0' : '2.0');
    assert.equal(bff.secure, true);
    const plain = await page.evaluate(() => fetch('/express/info').then(r => r.json()));
    assert.equal(plain.framework, 'express');
    assert.equal(plain.pid, server.pid);
    assert.equal(plain.httpVersion, bff.httpVersion);

    const echo = await page.evaluate(async () => {
      const body = JSON.stringify({ greeting: 'hello Nest' });
      const response = await fetch('/bff/echo', { method: 'POST', headers: { 'content-type': 'application/json' }, body });
      return { status: response.status, result: await response.json(), body };
    });
    assert.equal(echo.status, 201);
    assert.deepEqual(echo.result.body, { greeting: 'hello Nest' });
    assert.equal(echo.result.rawBodySha256, createHash('sha256').update(echo.body).digest('hex'));
    const nextEcho = await page.evaluate(() => fetch('/api/echo', { method: 'POST', body: 'Next owns this body' }).then(r => r.json()));
    assert.deepEqual(nextEcho, { body: 'Next owns this body', pid: server.pid });
    await page.getByLabel('Action value').fill('action body intact');
    await page.getByRole('button', { name: 'Submit action' }).click();
    await page.waitForFunction(() => document.querySelector('#action-result').textContent.includes('action body intact'));
    assert.deepEqual(JSON.parse(await page.locator('#action-result').textContent()), { value: 'action body intact', pid: server.pid });

    const session = await page.evaluate(() => fetch('/bff/session', { method: 'POST' }).then(r => r.json()));
    report.nextCookies = await page.evaluate(() => fetch('/api/session').then(r => r.json()));
    report.nestCookies = await page.evaluate(() => fetch('/bff/identity').then(r => r.json()));
    assert.equal(report.nextCookies.session, session.session);
    assert.equal(report.nextCookies.preference, 'compact');
    assert.equal(report.nestCookies.session, session.session);
    assert.equal(report.nestCookies.preference, 'compact');
    assert.equal(typeof report.nestCookies.cookieHeader, 'string');
    if (protocol === 'h3') assert.ok(report.nestCookies.rawCookieFields >= 2, 'Chromium must exercise split Cookie fields');
    report.proxiedCookies = await page.evaluate(() => fetch('/api/bff').then(r => r.json()));
    assert.equal(report.proxiedCookies.session, session.session);
    assert.equal(report.proxiedCookies.preference, 'compact');
    assert.equal(report.proxiedCookies.httpVersion, '3.0');
    assert.equal(report.proxiedCookies.pid, server.pid);
    const cookie = (await page.context().cookies()).find(c => c.name === 'demo-session');
    assert.ok(cookie.httpOnly && cookie.secure && cookie.path === '/');
    await page.reload({ waitUntil: 'networkidle' });
    assert.equal(await page.locator('#server-session').textContent(), session.session);
    assert.equal(await page.locator('#server-bff-session').textContent(), session.session);

    await page.exposeFunction('waitForUploadPrefix', id => server.prefix(id));
    report.upload = await page.evaluate(async () => {
      const bytes = new Uint8Array(8 * 1024 * 1024);
      for (let i = 0; i < bytes.length; i++) bytes[i] = i % 251;
      const id = crypto.randomUUID();
      const body = new ReadableStream({ async start(controller) {
        controller.enqueue(bytes.subarray(0, 65536));
        const received = await window.waitForUploadPrefix(id);
        if (!(received > 0 && received <= 65536)) throw new Error('Invalid prefix progress');
        for (let offset = 65536; offset < bytes.length; offset += 65536) controller.enqueue(bytes.subarray(offset, offset + 65536));
        controller.close();
      } });
      const response = await fetch('/bff/upload', { method: 'POST', body, duplex: 'half',
        headers: { 'content-type': 'application/octet-stream', 'x-upload-id': id } });
      const expected = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)), b => b.toString(16).padStart(2, '0')).join('');
      return { status: response.status, result: await response.json(), expected, bytes: bytes.length, prefixBeforeEof: true };
    });
    assert.equal(report.upload.status, 201);
    assert.equal(report.upload.result.bytes, report.upload.bytes);
    assert.equal(report.upload.result.sha256, report.upload.expected);
    assert.equal(report.upload.result.pid, server.pid);

    report.events = await page.evaluate(() => new Promise((resolve, reject) => {
      const events = [];
      const source = new EventSource('/bff/events');
      const deadline = setTimeout(() => { source.close(); reject(new Error('SSE timed out')); }, 5000);
      source.onmessage = event => {
        events.push(JSON.parse(event.data));
        if (events.length === 3) { clearTimeout(deadline); source.close(); resolve(events); }
      };
      source.onerror = () => { clearTimeout(deadline); source.close(); reject(new Error('SSE failed')); };
    }));
    assert.deepEqual(report.events, [0, 1, 2].map(id => ({ id, pid: server.pid })));

    if (process.env.NODE_ENV !== 'production') {
      await page.getByRole('button', { name: 'Count 0' }).click();
      fs.writeFileSync(controls, original.replace('Count {count}', 'Updated {count}'));
      await page.getByRole('button', { name: 'Updated 1', exact: true }).waitFor({ timeout: 20000 });
      report.fastRefreshPreservedState = true;
      assert.ok(report.websocketStatuses.includes(101));
    }
    for (let round = 0; round < 5; round++) {
      await page.evaluate(() => { window.navigationSentinel = 'same-document'; });
      await page.getByRole('link', { name: 'About', exact: true }).click();
      await page.getByRole('heading', { name: 'Client navigation works' }).waitFor();
      assert.equal(await page.evaluate(() => window.navigationSentinel), 'same-document');
      assert.equal(Number(await page.locator('#server-pid').textContent()), server.pid);
      await page.getByRole('link', { name: 'Home', exact: true }).click();
      await page.getByRole('heading', { name: 'Next.js and NestJS with Express over HTTP/3' }).waitFor();
      assert.equal(await page.evaluate(() => window.navigationSentinel), 'same-document');
    }
    await page.waitForLoadState('networkidle');
    assert.ok(report.responses.some(r => r.url.includes('_rsc=')));
    assert.ok(report.responses.slice(learned).every(r => r.protocol === protocol));
    assert.ok(report.responses.slice(learned).every(r => String(r.headers['x-app-pid']) === String(server.pid)));
    assert.deepEqual(report.pageErrors, []);
    // Closing EventSource after the third message may cancel the still-open
    // response. Only that deliberate cancellation is expected.
    report.expectedCancellations = report.failures.filter(r =>
      r.url === origin + '/bff/events' && r.failure?.errorText === 'net::ERR_ABORTED');
    assert.ok(report.expectedCancellations.length <= 1);
    assert.deepEqual(report.failures.filter(r => !report.expectedCancellations.includes(r)), []);
    report.shutdownMs = await server.stop();
    report.passed = true;
    console.log(JSON.stringify({ node: report.node, mode: report.mode, protocol, pid: server.pid,
      responses: report.responses.length, streamedUploadBytes: report.upload.bytes, shutdownMs: report.shutdownMs }));
  } catch (error) { console.error(server.logs()); throw error; }
  finally {
    fs.writeFileSync(controls, original);
    await browser?.close();
    await server.kill();
    if (process.env.HTTP3_TEST_REPORT) fs.writeFileSync(process.env.HTTP3_TEST_REPORT, JSON.stringify({ ...report, logs: server.logs() }, null, 2));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
