// Exercise the packed public API in an isolated installed consumer. No source
// aliases, native overrides, or inherited NODE_PATH may rescue the install.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const native = args.includes('--native');
function option(name) {
  const index = args.indexOf(name);
  if (index < 0) return undefined;
  assert.ok(args[index + 1] && !args[index + 1].startsWith('--'), `${name} needs a path`);
  return resolve(args[index + 1]);
}
const sidecar = option('--sidecar');
const candidate = option('--tarball');
assert.equal(Boolean(sidecar), native, '--native requires --sidecar <candidate.tgz>; omit both to test WASM fallback');

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
const env = { ...process.env, NODE_PATH: '', NAPI_RS_NATIVE_LIBRARY_PATH: '', NAPI_RS_FORCE_WASI: '', NAPI_RS_WASI_FLAVOR: '' };
delete env.NODE_OPTIONS;
const temp = mkdtempSync(join(tmpdir(), 'http3-platform-smoke-'));
function run(commandArgs, cwd = temp, timeout = 120000) {
  const result = spawnSync(process.execPath, commandArgs, { cwd, env, encoding: 'utf8', timeout });
  assert.equal(result.status, 0, `${commandArgs.join(' ')} failed: ${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}
try {
  let tarball = candidate;
  if (!tarball) {
    const report = JSON.parse(run([npm, 'pack', '--ignore-scripts', '--json', '--pack-destination', temp], root));
    // npm 11 reports an array; npm 12 keys pack results by package name.
    const packed = Array.isArray(report) ? report[0] : report['@currentspace/http3'];
    assert.ok(packed?.filename && Array.isArray(packed.files), 'unexpected npm pack JSON result');
    assert.ok(packed.files.some(file => file.path === 'dist/wasm/http3_client.wasm'), 'WASM artifact is required in the root tarball');
    assert.ok(packed.files.every(file => !file.path.endsWith('.node')), 'root package must not bundle native addons');
    tarball = join(temp, packed.filename);
  }
  writeFileSync(join(temp, 'package.json'), JSON.stringify({ private: true }));
  run([npm, 'install', native ? '--include=optional' : '--omit=optional', '--ignore-scripts', '--no-audit', '--no-fund', '--package-lock=false', tarball, ...(sidecar ? [sidecar] : [])]);
  const installed = join(temp, 'node_modules/@currentspace/http3');
  assert.ok(existsSync(join(installed, 'dist/wasm/http3_client.wasm')), 'installed WASM artifact is missing');
  if (native) {
    const manifest = JSON.parse(readFileSync(join(installed, 'package.json'), 'utf8'));
    // The fixture verifies actual native selection; version equality prevents
    // an older registry sidecar accidentally qualifying the current candidate.
    const packages = Object.keys(manifest.optionalDependencies);
    for (const name of packages) {
      const path = join(temp, 'node_modules', name, 'package.json');
      if (existsSync(path)) assert.equal(JSON.parse(readFileSync(path, 'utf8')).version, manifest.version);
    }
  }
  copyFileSync(join(root, 'scripts/fixtures/platform-consumer.cjs'), join(temp, 'consumer.cjs'));
  process.stdout.write(run(['consumer.cjs', native ? 'native' : 'wasm'], temp, 60000));
} finally {
  rmSync(temp, { recursive: true, force: true });
}
