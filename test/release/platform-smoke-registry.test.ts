import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { it, type TestContext } from 'node:test';
import { gzipSync } from 'node:zlib';

const script = resolve(__dirname, '../../../scripts/platform-smoke.mjs');
const version = '1.2.3-rc.4';
const sidecar = '@currentspace/http3-test-platform';

// A tiny real npm tarball avoids package-manager mocks or a tar executable.
// These packages test installation/provenance only, not HTTP/3 behavior.
function tarball(files: Record<string, string>): Buffer {
  const blocks: Buffer[] = [];
  for (const [name, contents] of Object.entries(files)) {
    const bytes = Buffer.from(contents);
    const header = Buffer.alloc(512);
    header.write(`package/${name}`);
    const octal = (value: number, offset: number, length: number): void => {
      header.write(value.toString(8).padStart(length - 1, '0') + '\0', offset, length);
    };
    octal(0o644, 100, 8);
    octal(0, 108, 8);
    octal(0, 116, 8);
    octal(bytes.length, 124, 12);
    octal(0, 136, 12);
    header.fill(32, 148, 156);
    header.write('0', 156);
    header.write('ustar\0', 257);
    header.write('00', 263);
    octal(header.reduce((sum, byte) => sum + byte, 0), 148, 8);
    blocks.push(header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...blocks, Buffer.alloc(1024)]));
}

interface FixtureOptions {
  native?: boolean;
  includeOptional?: boolean;
  unavailablePlatform?: boolean;
  missingSidecar?: boolean;
  wrongRootVersion?: boolean;
  wrongNativeVersion?: boolean;
  failConsumer?: boolean;
}

async function registryFixture(t: TestContext, options: FixtureOptions = {}) {
  const directory = mkdtempSync(join(tmpdir(), 'http3-registry-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  mkdirSync(join(directory, 'scripts/fixtures'), { recursive: true });
  copyFileSync(script, join(directory, 'scripts/platform-smoke.mjs'));
  const receiptPath = join(directory, 'receipts/installed.json');
  const rootManifest = {
    name: '@currentspace/http3', version: options.wrongRootVersion ? '1.2.4' : version,
    main: 'index.js', optionalDependencies: { [sidecar]: version },
    scripts: { postinstall: 'node -e "require(\'node:fs\').writeFileSync(\'postinstall-ran\', \'bad\')"' },
  };
  const nativeManifest = {
    name: sidecar, version, os: [options.unavailablePlatform ? `!${process.platform}` : process.platform],
    cpu: [process.arch], main: 'fixture.node',
  };
  const rootTarball = tarball({ 'package.json': JSON.stringify(rootManifest), 'index.js': 'module.exports = {};', 'dist/wasm/http3_client.wasm': 'wasm fixture' });
  const nativeTarball = tarball({ 'package.json': JSON.stringify({ ...nativeManifest, version: options.wrongNativeVersion ? '1.2.4' : version }), 'fixture.node': 'native fixture' });
  writeFileSync(join(directory, 'scripts/fixtures/platform-consumer.cjs'), `
    const assert = require('node:assert/strict');
    const fs = require('node:fs');
    assert.equal(require('@currentspace/http3/package.json').version, ${JSON.stringify(version)});
    assert.equal(fs.existsSync('node_modules/@currentspace/http3/postinstall-ran'), false);
    assert.equal(fs.existsSync('node_modules/${sidecar}/fixture.node'), ${Boolean(options.native)});
    for (const name of ['NODE_PATH', 'NAPI_RS_NATIVE_LIBRARY_PATH', 'NAPI_RS_FORCE_WASI']) assert.equal(process.env[name], undefined);
    ${options.failConsumer ? 'throw new Error("consumer intentionally failed");' : 'console.log("registry selection fixture passed");'}
  `);
  const requests: string[] = [];
  let origin = '';
  const server = createServer((request, response) => {
    const path = decodeURIComponent(request.url ?? '/');
    requests.push(path);
    if (path === '/root.tgz' || path === '/native.tgz') {
      response.end(path === '/root.tgz' ? rootTarball : nativeTarball);
      return;
    }
    const nativePackage = path === `/${sidecar}`;
    if (path !== '/@currentspace/http3' && !nativePackage || nativePackage && options.missingSidecar) {
      response.writeHead(404).end('{}');
      return;
    }
    const manifest = nativePackage ? nativeManifest : { ...rootManifest, version };
    const bytes = nativePackage ? nativeTarball : rootTarball;
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify({
      name: manifest.name,
      // Exact-version selection must ignore this unrelated latest tag.
      'dist-tags': { latest: '9.9.9' },
      versions: { [version]: { ...manifest, dist: {
        tarball: origin + (nativePackage ? '/native.tgz' : '/root.tgz'),
        integrity: 'sha512-' + createHash('sha512').update(bytes).digest('base64'),
      } } },
    }));
  });
  t.after(() => { server.closeAllConnections(); server.close(); });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  origin = `http://127.0.0.1:${address.port}`;
  const child = spawn(process.execPath, [join(directory, 'scripts/platform-smoke.mjs'),
    '--registry-version', version, '--registry', origin, '--receipt', receiptPath,
    ...(options.native ? ['--native'] : options.includeOptional ? ['--include-optional'] : []),
  ], { cwd: directory, signal: t.signal, timeout: 12_000, env: {
    ...process.env, NODE_OPTIONS: '', NODE_PATH: '/must-not-load-local-modules',
    NAPI_RS_NATIVE_LIBRARY_PATH: '/must-not-load-local-addon', NAPI_RS_FORCE_WASI: '1',
    npm_config_registry: 'http://127.0.0.1:1/', npm_config_cache: join(directory, 'must-not-use-cache'),
    'npm_config_@currentspace:registry': 'http://127.0.0.1:1/',
  } });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', chunk => { stdout += String(chunk); });
  child.stderr.on('data', chunk => { stderr += String(chunk); });
  const [code] = await once(child, 'close');
  const receipt = JSON.parse(readFileSync(receiptPath, 'utf8'));
  assert.equal(existsSync(join(directory, 'must-not-use-cache')), false);
  return { code, stdout, stderr, receipt, requests, rootTarball, origin };
}

it('registry smoke rejects ranges, paths, mixed installation sources and ambiguous CLI arguments', () => {
  for (const args of [
    ['--registry-version', 'latest'], ['--registry-version', '^1.0.0'], ['--registry-version', './release.tgz'],
    ['--registry-version', '01.0.0'], ['--registry-version', '1.0.0-01'],
    ['--registry-version', '1.0.0\n'],
    ['--registry-version', '1.0.0', '--tarball', 'root.tgz'],
    ['--registry-version', '1.0.0', '--sidecar', 'native.tgz'],
    ['--native'], ['--include-optional'], ['--unknown'],
    ['--registry-version', '1.0.0', '--registry-version', '2.0.0'],
  ]) {
    const result = spawnSync(process.execPath, [script, ...args], { encoding: 'utf8', timeout: 5000 });
    assert.equal(result.status, 1, `${args.join(' ')}: ${result.stderr}`);
    assert.match(result.stderr, /AssertionError/);
  }
});

it('registry smoke installs the exact version without optional packages using isolated npm config and cache', async t => {
  const result = await registryFixture(t);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /registry selection fixture passed/);
  assert.equal(result.receipt.status, 'passed');
  assert.match(result.receipt.consumerOutput, /registry selection fixture passed/);
  assert.equal(result.receipt.optionalDependencies, 'omitted');
  assert.equal(result.receipt.packages.length, 1);
  assert.equal(result.receipt.packages[0].resolved, result.origin + '/root.tgz');
  assert.equal(result.receipt.packages[0].integrity, 'sha512-' + createHash('sha512').update(result.rootTarball).digest('base64'));
  const wasm = result.receipt.packages[0].files.find((file: { path: string }) => file.path === 'dist/wasm/http3_client.wasm');
  assert.equal(wasm.sha256, createHash('sha256').update('wasm fixture').digest('hex'));
  assert.ok(!result.requests.includes('/native.tgz'));
});

it('registry smoke lets npm select the matching native sidecar without explicit sidecar installation', async t => {
  const result = await registryFixture(t, { native: true });
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.receipt.expectedRuntime, 'native');
  assert.equal(result.receipt.optionalDependencies, 'included');
  assert.equal(result.receipt.packages.length, 2);
  assert.equal(result.receipt.packages[1].version, version);
  assert.ok(result.requests.includes('/native.tgz'));
});

it('registry smoke supports normal optional installation with WASM expected on an unsupported native platform', async t => {
  const result = await registryFixture(t, { includeOptional: true, unavailablePlatform: true });
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.receipt.optionalDependencies, 'included');
  assert.equal(result.receipt.expectedRuntime, 'wasm');
  assert.equal(result.receipt.packages.length, 1);
});

it('registry smoke rejects npm silently omitting a missing native sidecar', async t => {
  const result = await registryFixture(t, { native: true, missingSidecar: true });
  assert.equal(result.code, 1);
  assert.equal(result.receipt.status, 'failed');
  assert.match(result.receipt.error, /unexpected installed native sidecar count/);
});

it('registry smoke rejects a registry tarball with a mismatched root version', async t => {
  const result = await registryFixture(t, { wrongRootVersion: true });
  assert.equal(result.code, 1);
  assert.match(result.receipt.error, /wrong root version/);
});

it('registry smoke rejects a registry sidecar with a mismatched version', async t => {
  const result = await registryFixture(t, { native: true, wrongNativeVersion: true });
  assert.equal(result.code, 1);
  assert.match(result.receipt.error, /wrong installed version for @currentspace\/http3-test-platform/);
});

it('registry smoke retains provenance but reports failure when the installed consumer fails', async t => {
  const result = await registryFixture(t, { failConsumer: true });
  assert.equal(result.code, 1);
  assert.equal(result.receipt.status, 'failed');
  assert.equal(result.receipt.packages.length, 1);
  assert.match(result.receipt.error, /consumer intentionally failed/);
});
