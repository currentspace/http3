// Runs only on disposable CI runners: installs and removes a generated localhost CA.
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { chromium } from 'playwright';
import { createNodeServerFactory, connectAsync } from '../dist/index.js';

assert.equal(process.env.CI, 'true', 'Trust integration runs only on disposable CI runners');
assert.equal(process.env.HTTP3_TEST_INSTALL_DEV_CA, '1', 'Explicit temporary CA installation must be enabled');
const directory = mkdtempSync(join(tmpdir(), 'http3-dev-trust-'));
const cli = resolve('bin/http3.cjs');
function cert(action) {
  console.log(`dev-cert ${action}`);
  const result = spawnSync(process.execPath, [cli, 'dev-cert', action, '--out', directory], {
    stdio: 'inherit', timeout: 120_000, killSignal: 'SIGKILL',
  });
  assert.equal(result.status, 0, `dev-cert ${action}: ${result.error ?? result.signal ?? result.status}`);
}
let server;
let browser;
let trustAttempted = false;
let qualified = false;
try {
  cert('generate');
  trustAttempted = true;
  cert('trust');
  server = createNodeServerFactory({
    key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')),
    runtimeMode: 'portable', allowHTTP1: true,
  })((_req, res) => res.end('trusted localhost'));
  server.listen({ port: 0 });
  await once(server, 'listening');
  const origin = `https://localhost:${server.address().port}`;
  const client = spawn(process.execPath, ['-e', `
    require('node:https').get(${JSON.stringify(origin)}, { agent: false }, res => {
      if (!res.socket.authorized) process.exitCode = 1;
      res.resume();
    }).on('error', e => { console.error(e); process.exitCode = 1; });
  `], { env: { ...process.env, NODE_EXTRA_CA_CERTS: join(directory, 'ca.pem') }, stdio: 'inherit' });
  assert.equal((await once(client, 'exit'))[0], 0, 'Node HTTPS client must trust NODE_EXTRA_CA_CERTS');
  for (const host of ['127.0.0.1', '[::1]']) {
    console.log(`Checking native H3 on ${host}`);
    const session = await connectAsync(`https://${host}:${server.address().port}`, {
      ca: readFileSync(join(directory, 'ca.pem')), servername: 'localhost', runtimeMode: 'portable',
    });
    try {
      const stream = session.request({ ':method': 'GET', ':path': '/' }, { endStream: true });
      const chunks = [];
      for await (const chunk of stream) chunks.push(chunk);
      assert.equal(Buffer.concat(chunks).toString(), 'trusted localhost');
    } finally { await session.close(); }
  }
  // No SPKI bypass, ignoreHTTPSErrors, or DNS override: test real local CA trust.
  // Port 1 enables Chromium's localhost QUIC CA exception without forcing our port.
  browser = await chromium.launch({ args: ['--origin-to-force-quic-on=localhost:1'] });
  const page = await browser.newPage();
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Network.enable');
  const responses = [];
  cdp.on('Network.responseReceived', ({ response }) => {
    if (response.url === origin + '/') responses.push({ protocol: response.protocol, remoteIPAddress: response.remoteIPAddress });
  });
  await page.goto(origin);
  assert.equal(responses[0].protocol, 'h2');
  for (let n = 0; n < 15 && responses.at(-1).protocol !== 'h3'; n++) {
    await page.waitForTimeout(300);
    await page.reload();
  }
  assert.equal(responses.at(-1).protocol, 'h3');
  assert.equal(await page.textContent('body'), 'trusted localhost');
  console.log(JSON.stringify({ passed: true, browser: browser.version(), responses }));
  qualified = true;
} finally {
  try {
    await browser?.close();
    if (trustAttempted) cert('untrust');
    if (qualified) {
      // A fresh profile/process must reject the same leaf after CA removal.
      browser = await chromium.launch();
      const page = await browser.newPage();
      await assert.rejects(page.goto(`https://localhost:${server.address().port}`), /ERR_CERT_AUTHORITY_INVALID/);
      console.log('Fresh Chromium rejects the leaf after untrust');
    }
  } finally {
    await browser?.close();
    if (server) await new Promise((resolve, reject) => server.close(e => e ? reject(e) : resolve()));
    rmSync(directory, { recursive: true, force: true });
  }
}
