// Exercise the packed or registry public API in an isolated installed consumer. No source
// aliases, native overrides, or inherited NODE_PATH may rescue the install.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const native = args.includes('--native');
const valueOptions = new Set(['--sidecar', '--tarball', '--registry-version', '--registry', '--receipt']);
const seen = new Set();
for (let index = 0; index < args.length; index++) {
  const name = args[index];
  assert.ok(valueOptions.has(name) || name === '--native' || name === '--include-optional', `unknown option: ${name}`);
  assert.ok(!seen.has(name), `duplicate option: ${name}`);
  seen.add(name);
  if (valueOptions.has(name)) {
    assert.ok(args[index + 1] && !args[index + 1].startsWith('--'), `${name} needs a value`);
    index++;
  }
}
function option(name) {
  const index = args.indexOf(name);
  if (index < 0) return undefined;
  return args[index + 1];
}
const sidecar = option('--sidecar') && resolve(option('--sidecar'));
const candidate = option('--tarball') && resolve(option('--tarball'));
const version = option('--registry-version');
const receiptPath = option('--receipt') && resolve(option('--receipt'));
const includeOptional = native || args.includes('--include-optional');
const number = '(?:0|[1-9][0-9]*)';
const prerelease = '(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)';
const exactVersion = new RegExp(`^${number}\\.${number}\\.${number}(?:-${prerelease}(?:\\.${prerelease})*)?(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$`);
if (version) {
  assert.equal(version, version.trim(), '--registry-version must not contain surrounding whitespace');
  assert.match(version, exactVersion, '--registry-version requires an exact semver, not a tag, range, URL or path');
  assert.ok(!candidate && !sidecar, '--registry-version cannot be combined with --tarball or --sidecar');
} else {
  assert.ok(!option('--registry') && !args.includes('--include-optional'), '--registry and --include-optional require --registry-version');
  assert.equal(Boolean(sidecar), native, '--native requires --sidecar <candidate.tgz>; omit both to test WASM fallback');
}
const registry = new URL(option('--registry') ?? 'https://registry.npmjs.org/');
assert.ok(!registry.username && !registry.password && !registry.search && !registry.hash, 'registry URL must not contain credentials, query or fragment');
assert.ok(registry.protocol === 'https:' || (registry.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(registry.hostname)), 'registry must use HTTPS (HTTP is allowed only for loopback tests)');
if (!registry.pathname.endsWith('/')) registry.pathname += '/';

// Invoke npm's JS entry point directly, including on Windows, so no shell is
// needed to execute npm.cmd and paths containing spaces remain ordinary argv.
function npmCli() {
  const dirs = [dirname(realpathSync(process.execPath)), ...(process.env.PATH ?? '').split(delimiter)];
  for (const dir of dirs) {
    for (const path of [join(dir, 'node_modules/npm/bin/npm-cli.js'), join(dir, '../lib/node_modules/npm/bin/npm-cli.js')]) {
      if (existsSync(path)) return realpathSync(path);
    }
    const executable = join(dir, 'npm');
    if (existsSync(executable)) {
      const path = realpathSync(executable);
      if (path.endsWith('npm-cli.js')) return path;
    }
  }
  throw new Error('Cannot find npm/bin/npm-cli.js; install the npm bundled with Node.js');
}
const npm = npmCli();
const env = { ...process.env };
for (const name of Object.keys(env)) {
  if (/^(NODE_PATH|NODE_OPTIONS|NAPI_RS_NATIVE_LIBRARY_PATH|NAPI_RS_FORCE_WASI|NAPI_RS_WASI_FLAVOR)$/iu.test(name)
      || (version && /^npm_config_/iu.test(name))) delete env[name];
}
const temp = mkdtempSync(join(tmpdir(), 'http3-platform-smoke-'));
const receipt = {
  mode: version ? 'registry' : 'tarball', version: version ?? null,
  registry: version ? registry.href : null, optionalDependencies: includeOptional ? 'included' : 'omitted',
  expectedRuntime: native ? 'native' : 'wasm', platform: process.platform, arch: process.arch,
  node: process.version, sourceRevision: process.env.GITHUB_SHA ?? null,
  startedAt: new Date().toISOString(), status: 'failed', packages: [],
};
function run(commandArgs, cwd = temp, timeout = 120000) {
  const result = spawnSync(process.execPath, commandArgs, { cwd, env, encoding: 'utf8', timeout });
  assert.equal(result.status, 0, `${commandArgs.join(' ')} failed: ${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}
function files(directory, prefix = '') {
  return readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)).flatMap(entry => {
    const path = prefix + entry.name;
    assert.ok(!entry.isSymbolicLink(), `installed package contains a symbolic link: ${path}`);
    if (entry.isDirectory()) return files(join(directory, entry.name), path + '/');
    const bytes = readFileSync(join(directory, entry.name));
    return [{ path, bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') }];
  });
}
try {
  let tarball = candidate;
  if (!version && !tarball) {
    const report = JSON.parse(run([npm, 'pack', '--ignore-scripts', '--json', '--pack-destination', temp], root));
    // npm 11 reports an array; npm 12 keys pack results by package name.
    const packed = Array.isArray(report) ? report[0] : report['@currentspace/http3'];
    assert.ok(packed?.filename && Array.isArray(packed.files), 'unexpected npm pack JSON result');
    assert.ok(packed.files.some(file => file.path === 'dist/wasm/http3_client.wasm'), 'WASM artifact is required in the root tarball');
    assert.ok(packed.files.every(file => !file.path.endsWith('.node')), 'root package must not bundle native addons');
    tarball = join(temp, packed.filename);
  }
  writeFileSync(join(temp, 'package.json'), JSON.stringify({ private: true }));
  const registryArgs = [];
  if (version) {
    // Scope configuration and cache to this consumer: user scope registries,
    // old cached packages and environment config must not rescue the release.
    for (const name of ['user.npmrc', 'global.npmrc']) writeFileSync(join(temp, name), '');
    registryArgs.push('--registry', registry.href, '--cache', join(temp, 'cache'),
      '--userconfig', join(temp, 'user.npmrc'), '--globalconfig', join(temp, 'global.npmrc'), '--save-exact');
  }
  run([npm, 'install', includeOptional ? '--include=optional' : '--omit=optional', '--ignore-scripts', '--no-audit', '--no-fund',
    `--package-lock=${Boolean(version)}`, ...registryArgs, version ? `@currentspace/http3@${version}` : tarball, ...(sidecar ? [sidecar] : [])]);
  const installed = join(temp, 'node_modules/@currentspace/http3');
  assert.ok(existsSync(join(installed, 'dist/wasm/http3_client.wasm')), 'installed WASM artifact is missing');
  const manifest = JSON.parse(readFileSync(join(installed, 'package.json'), 'utf8'));
  const packages = Object.keys(manifest.optionalDependencies ?? {});
  if (version) {
    assert.equal(manifest.name, '@currentspace/http3');
    assert.equal(manifest.version, version, 'registry installed the wrong root version');
    const lock = JSON.parse(readFileSync(join(temp, 'package-lock.json'), 'utf8'));
    const installedPackages = ['@currentspace/http3', ...packages.filter(name => existsSync(join(temp, 'node_modules', name, 'package.json')))];
    assert.equal(installedPackages.length - 1, native ? 1 : 0, 'unexpected installed native sidecar count');
    for (const name of packages) assert.equal(manifest.optionalDependencies[name], version, `root optional dependency ${name} must pin the release version`);
    for (const name of installedPackages) {
      const directory = join(temp, 'node_modules', name);
      const packageManifest = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'));
      const entry = lock.packages[`node_modules/${name}`];
      assert.equal(packageManifest.version, version, `wrong installed version for ${name}`);
      assert.equal(entry?.version, version, `wrong lockfile version for ${name}`);
      const resolved = new URL(entry.resolved);
      assert.equal(resolved.origin, registry.origin, `package ${name} did not resolve from the selected registry`);
      assert.ok(resolved.pathname.startsWith(registry.pathname), `package ${name} resolved outside the registry path`);
      assert.match(entry.integrity, /^sha512-[A-Za-z0-9+/]+={0,2}$/u, `missing SHA-512 registry integrity for ${name}`);
      const inventory = files(directory);
      if (name === manifest.name) assert.ok(inventory.every(file => !file.path.endsWith('.node') && !/^(docs|examples)\//u.test(file.path)), 'root contains native binaries, docs or examples');
      else assert.ok(inventory.some(file => file.path.endsWith('.node')), `sidecar ${name} contains no native binary`);
      receipt.packages.push({ name, version, resolved: resolved.href, integrity: entry.integrity, files: inventory });
    }
  }
  if (native) {
    // The fixture verifies actual native selection; version equality prevents
    // an older registry sidecar accidentally qualifying the current candidate.
    for (const name of packages) {
      const path = join(temp, 'node_modules', name, 'package.json');
      if (existsSync(path)) assert.equal(JSON.parse(readFileSync(path, 'utf8')).version, manifest.version);
    }
  }
  copyFileSync(join(root, 'scripts/fixtures/platform-consumer.cjs'), join(temp, 'consumer.cjs'));
  receipt.consumerOutput = run(['consumer.cjs', native ? 'native' : 'wasm'], temp, 60000);
  process.stdout.write(receipt.consumerOutput);
  receipt.status = 'passed';
} catch (error) {
  receipt.error = error.message;
  throw error;
} finally {
  receipt.completedAt = new Date().toISOString();
  try {
    if (receiptPath) {
      mkdirSync(dirname(receiptPath), { recursive: true });
      writeFileSync(receiptPath, JSON.stringify(receipt, null, 2) + '\n');
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}
