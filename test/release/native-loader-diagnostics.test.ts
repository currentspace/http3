import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { describe, it } from 'node:test';
import { runInNewContext } from 'node:vm';
import { generateTestCerts } from '../support/generate-certs.js';

const root = resolve(__dirname, '../../..');
const source = readFileSync(join(root, 'index.js'), 'utf8');
const diagnostics = readFileSync(join(root, 'bin/native-binding-error.cjs'), 'utf8');
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));

function missing(id: string): Error {
  return Object.assign(new Error(`Cannot find module '${id}'\nRequire stack:\n- fixture/index.js`), { code: 'MODULE_NOT_FOUND' });
}

// Run the actual generated loader, with platform and module availability supplied
// by the fixture. No foreign binary is executed on the test host.
function load(platform: string, arch: string, options: {
  musl?: boolean;
  modules?: Record<string, unknown>;
  failures?: Record<string, Error>;
  env?: Record<string, string>;
} = {}): any {
  const modules = options.modules ?? {};
  const requireFixture: any = (id: string): any => {
    if (id === './package.json' || id === '../package.json') return manifest;
    if (id === 'fs') return { readFileSync: () => options.musl ? 'musl' : 'glibc' };
    if (id === './bin/native-binding-error.cjs') {
      const module = { exports: {} };
      runInNewContext(diagnostics, { module, require: requireFixture, Error });
      return module.exports;
    }
    if (options.failures?.[id]) throw options.failures[id];
    if (id in modules) return modules[id];
    throw missing(id);
  };
  requireFixture.resolve = (id: string): string => {
    if (id in modules || options.failures?.[id]) return id;
    throw missing(id);
  };
  const module = { exports: {} };
  runInNewContext(source, {
    require: requireFixture, module, Error,
    process: { platform, arch, env: options.env ?? {} },
  });
  return module.exports;
}

function failure(platform: string, arch: string, options?: Parameters<typeof load>[2]): any {
  try { load(platform, arch, options); }
  catch (error) { return error; }
  assert.fail('expected loader failure');
}

describe('native loader diagnostics', () => {
  for (const [platform, arch, musl] of [
    ['darwin', 'x64', false], ['linux', 'x64', true], ['linux', 'arm64', true], ['win32', 'x64', false],
    ['freebsd', 'x64', false], ['linux', 's390x', true],
  ] as const) {
    if (`${manifest.name}-${platform}-${arch}${platform === 'linux' ? (musl ? '-musl' : '-gnu') : ''}` in manifest.optionalDependencies) continue;
    it(`names unpublished target ${platform}-${arch}${musl ? '-musl' : ''}`, () => {
      const error = failure(platform, arch, { musl });
      assert.equal(error.code, 'ERR_HTTP3_RUNTIME_UNSUPPORTED');
      assert.equal(error.reasonCode, 'native-binding-unavailable');
      assert.equal(error.nativeBinding.platform, platform);
      assert.equal(error.nativeBinding.arch, arch);
      assert.ok(error.message.includes(`${platform} ${arch}`));
      if (platform === 'linux') assert.match(error.message, /\(musl\)/u);
      assert.match(error.message, /no prebuilt native package is published/u);
      for (const name of Object.keys(manifest.optionalDependencies)) {
        assert.ok(error.message.includes(name.slice(manifest.name.length + 1)));
      }
      assert.doesNotMatch(error.message, /npm install|npm i`|package-lock|npm has a bug/u);
      assert.ok(error.cause);
    });
  }

  for (const name of Object.keys(manifest.optionalDependencies)) {
    const [platform, arch, libc] = name.slice(manifest.name.length + 1).split('-');
    const suffix = libc ? `-${libc}` : '';
    it(`names the missing package for ${platform}-${arch}`, () => {
      const error = failure(platform, arch, { musl: libc === 'musl' });
      assert.equal(error.code, 'ERR_HTTP3_RUNTIME_UNSUPPORTED');
      assert.ok(error.message.includes(`@currentspace/http3-${platform}-${arch}${suffix}@${manifest.version} is missing`));
      assert.equal(error.nativeBinding.category, 'missing');
      if (platform === 'linux') assert.equal(error.nativeBinding.libc, libc === 'musl' ? 'musl' : 'glibc');
      assert.match(error.message, /npm install --include=optional/u);
      assert.doesNotMatch(error.message, /no prebuilt|package-lock|npm has a bug/u);
    });
  }

  for (const [id, cause] of [
    ['./http3.darwin-arm64.node', Object.assign(new Error('incompatible architecture'), { code: 'ERR_DLOPEN_FAILED' })],
    ['@currentspace/http3-darwin-arm64', missing('dependency-inside-binding')],
    ['@currentspace/http3-darwin-arm64', new Error('version mismatch')],
  ] as const) {
    it(`preserves load failure: ${cause.message}`, () => {
      const error = failure('darwin', 'arm64', { failures: { [id]: cause } });
      assert.match(error.message, /failed to load or resolve/u);
      assert.doesNotMatch(error.message, /npm install|is missing/u);
      let current = error.cause;
      while (current && current.message !== cause.message) current = current.cause;
      assert.equal(current?.message, cause.message);
    });
  }

  it('reports an invalid explicit override without suggesting a sidecar install', () => {
    const error = failure('darwin', 'arm64', { env: { NAPI_RS_NATIVE_LIBRARY_PATH: '/missing/custom.node' } });
    assert.match(error.message, /NAPI_RS_NATIVE_LIBRARY_PATH=\/missing\/custom.node/u);
    assert.doesNotMatch(error.message, /npm install|optional platform package/u);
  });

  it('accepts a local binary on an unpublished target', () => {
    const binding = {};
    assert.equal(load('freebsd', 'x64', { modules: { './http3.freebsd-x64.node': binding } }), binding);
  });

  it('accepts a valid explicit native override', () => {
    const binding = {};
    assert.equal(load('darwin', 'x64', {
      modules: { '/custom/binding.node': binding }, env: { NAPI_RS_NATIVE_LIBRARY_PATH: '/custom/binding.node' },
    }), binding);
  });

  it('accepts the published sidecar', () => {
    const binding = {};
    assert.equal(load('darwin', 'arm64', { modules: {
      '@currentspace/http3-darwin-arm64': binding,
      '@currentspace/http3-darwin-arm64/package.json': manifest,
    } }), binding);
  });

  it('patches a regenerated failure block and remains idempotent', () => {
    const dir = mkdtempSync(join(tmpdir(), 'http3-loader-patch-'));
    try {
      mkdirSync(join(dir, 'scripts'));
      cpSync(join(root, 'scripts/patch-native-loader-version.mjs'), join(dir, 'scripts/patch-native-loader-version.mjs'));
      const original = `if (!nativeBinding) {\n  if (loadErrors.length > 0) {\n    throw new Error('npm has a bug')\n  }\n  throw new Error(\`Failed to load native binding\`)\n}`;
      writeFileSync(join(dir, 'index.js'), source.replace(/if \(!nativeBinding\) \{\n {2}throw require[\s\S]*?\n\}/u, original));
      for (let i = 0; i < 2; i++) {
        const result = spawnSync(process.execPath, [join(dir, 'scripts/patch-native-loader-version.mjs')], { encoding: 'utf8' });
        assert.equal(result.status, 0, result.stderr);
        assert.equal(readFileSync(join(dir, 'index.js'), 'utf8'), source);
      }
      writeFileSync(join(dir, 'index.js'), 'unexpected generated loader');
      const result = spawnSync(process.execPath, [join(dir, 'scripts/patch-native-loader-version.mjs')], { encoding: 'utf8' });
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /failure block changed/u);
    } finally { rmSync(dir, { recursive: true, force: true }); }
  });

  for (const requested of ['auto', 'wasm', 'auto:error', 'portable', 'fast']) {
    const [runtimeMode, fallbackPolicy = 'warn-and-fallback'] = requested.split(':');
    const succeeds = runtimeMode === 'wasm' || requested === 'auto';
    it(`${requested} startup from an isolated package without sidecars`, {
      skip: succeeds && !existsSync(join(root, 'dist/wasm/http3_client.wasm'))
        ? 'build the WASM artifact to exercise explicit WASM startup' : false,
    }, () => {
      const dir = mkdtempSync(join(tmpdir(), 'http3-missing-sidecar-'));
      try {
        cpSync(resolve(__dirname, '../../lib'), join(dir, 'dist'), { recursive: true });
        cpSync(join(root, 'bin'), join(dir, 'bin'), { recursive: true });
        cpSync(join(root, 'index.js'), join(dir, 'index.js'));
        cpSync(join(root, 'package.json'), join(dir, 'package.json'));
        if (succeeds) {
          cpSync(join(root, 'dist/wasm/http3_client.wasm'), join(dir, 'dist/wasm/http3_client.wasm'));
        }
        const certs = generateTestCerts();
        writeFileSync(join(dir, 'key.pem'), certs.key);
        writeFileSync(join(dir, 'cert.pem'), certs.cert);
        const result = spawnSync(process.execPath, ['-e', `
          const assert = require('node:assert/strict');
          const { readFileSync } = require('node:fs');
          const { Http3Error, ERR_HTTP3_RUNTIME_UNSUPPORTED } = require('./dist/index.js');
          const { createNodeServerFactory } = require('./dist/node-server.js');
          assert.equal(require.cache[require.resolve('./index.js')], undefined, 'import stays lazy');
          const runtimeMode = ${JSON.stringify(runtimeMode)};
          const server = createNodeServerFactory({ runtimeMode, fallbackPolicy: ${JSON.stringify(fallbackPolicy)}, key: readFileSync('key.pem'), cert: readFileSync('cert.pem') })(() => {});
          const timer = setTimeout(() => { throw new Error('startup error not emitted'); }, 5000);
          let returned = false;
          server.once('error', error => {
            clearTimeout(timer);
            assert.equal(${succeeds}, false, error.message);
            assert.ok(returned, 'listen error stays asynchronous');
            assert.ok(error instanceof Http3Error);
            assert.equal(error.code, ERR_HTTP3_RUNTIME_UNSUPPORTED);
            assert.ok(error.message.includes(process.platform + ' ' + process.arch));
            assert.match(error.message, /is missing|no prebuilt native package/);
            assert.ok(error.cause);
            server.close();
          });
          server.once('listening', () => {
            clearTimeout(timer);
            assert.equal(${succeeds}, true, 'strict native startup should fail without a binding');
            assert.equal(server.runtimeInfo.selectedMode, 'wasm');
            assert.equal(server.runtimeInfo.fallbackOccurred, runtimeMode === 'auto');
            if (runtimeMode === 'auto') assert.equal(server.runtimeInfo.reasonCode, 'native-binding-unavailable');
            else assert.equal(require.cache[require.resolve('./index.js')], undefined, 'explicit WASM bypasses the native loader');
            server.close();
          });
          server.listen(0, '127.0.0.1');
          returned = true;
        `], { cwd: dir, encoding: 'utf8', timeout: 10000, env: {
          ...process.env, NODE_PATH: '', NAPI_RS_NATIVE_LIBRARY_PATH: '', NAPI_RS_FORCE_WASI: '', NAPI_RS_WASI_FLAVOR: '',
        } });
        assert.equal(result.status, 0, result.stderr);
      } finally { rmSync(dir, { recursive: true, force: true }); }
    });
  }
});
