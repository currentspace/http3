import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';
import { doctor, chromeAlternativeServiceState } from '../../lib/doctor.js';
import { createDevTls } from '../../lib/dev-cert.js';
import { Http3NodeServer } from '../../lib/node-server.js';
import { debugHandshakeClose } from '../../lib/debug.js';

test('doctor probes dual-stack transport and explicit CA without changing trust', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-doctor-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const { key, cert, ca } = createDevTls({ directory });
  const server = new Http3NodeServer({ key, cert, allowHTTP1: true, runtimeMode: 'portable' }, (_req, res) => res.end('ok'));
  server.listen(0); await once(server, 'listening');
  t.after(async () => new Promise<void>(resolve => server.close(() => resolve())));
  const result = await doctor(`https://localhost:${server.address()!.port}`, { ca, timeoutMs: 2000 });
  for (const name of ['TCP reachability 127.0.0.1', 'TCP reachability ::1', 'UDP/H3 reachability 127.0.0.1', 'UDP/H3 reachability ::1', 'Verified H3 request', 'Alt-Svc over H2']) {
    const check = result.checks.find(item => item.name === name);
    assert.equal(check?.status, 'pass', JSON.stringify(check));
  }
  assert.equal(result.checks.find(item => item.name === 'System certificate trust (Node TLS)')?.status, 'fail');
});

test('Chrome diagnosis reports only matching cached H3 failures and leaves profile untouched', t => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-chrome-state-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const file = join(directory, 'Network Persistent State');
  const contents = JSON.stringify({ net: { http_server_properties: { broken_alternative_services: [
    { host: 'localhost', port: 3443, protocol: 'h3' }, { host: 'private.example', port: 443, protocol: 'h3' },
  ] } } });
  writeFileSync(file, contents);
  const check = chromeAlternativeServiceState(directory, 'localhost', 3443);
  assert.equal(check.status, 'fail');
  assert.doesNotMatch(check.detail, /private.example/);
  assert.equal(chromeAlternativeServiceState(directory, 'localhost', 4443).status, 'pass');
  assert.equal(readFileSync(file, 'utf8'), contents);
});

test('debug logs distinguish observed close codes from possible browser causes', t => {
  const original = process.env.HTTP3_DEBUG;
  t.after(() => { if (original === undefined) delete process.env.HTTP3_DEBUG; else process.env.HTTP3_DEBUG = original; });
  const lines: string[] = [];
  t.mock.method(console, 'error', (line: string) => lines.push(line));
  delete process.env.HTTP3_DEBUG;
  debugHandshakeClose(1, 0x130);
  assert.equal(lines.length, 0);
  process.env.HTTP3_DEBUG = '1';
  debugHandshakeClose(1, 0x130);
  assert.match(lines[0]!, /"tlsAlert":48/);
  assert.match(lines[0]!, /cannot determine/);
});
