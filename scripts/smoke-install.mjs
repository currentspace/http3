import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

const repoRoot = process.cwd();
const tempDir = mkdtempSync(join(tmpdir(), 'http3-smoke-install-'));
const manifest = JSON.parse(readFileSync(join(repoRoot, 'package.json'), 'utf8'));
function run(command, args, cwd = tempDir, env = process.env) {
  const result = spawnSync(command, args, { cwd, env, encoding: 'utf8' });
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}
function pack(directory, name) {
  const output = JSON.parse(run('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', tempDir], directory));
  const metadata = Array.isArray(output) ? output[0] : output[name] ?? output;
  return { ...metadata, tarball: join(tempDir, metadata.filename) };
}
try {
  const root = pack(repoRoot, manifest.name);
  assert.ok(root.files.every(f => !f.path.endsWith('.node')), 'root tarball must not duplicate native binaries');
  assert.ok(root.files.every(f => !/^(docs|examples)\//u.test(f.path)), 'docs and examples belong in the repository');
  assert.ok(root.files.some(f => f.path === 'bin/http3.cjs'), 'dev-cert CLI missing');
  const native = readdirSync(join(repoRoot, 'npm')).map(dir => ({
    dir, pkg: JSON.parse(readFileSync(join(repoRoot, 'npm', dir, 'package.json'), 'utf8')),
  })).find(({ pkg }) => pkg.os.includes(process.platform) && pkg.cpu.includes(process.arch));
  const artifacts = [root.tarball];
  let env = { ...process.env, NAPI_RS_ENFORCE_VERSION_CHECK: '1' };
  if (native) {
    const stage = mkdtempSync(join(tempDir, 'native-'));
    writeFileSync(join(stage, 'package.json'), JSON.stringify(native.pkg));
    copyFileSync(join(repoRoot, native.pkg.main), join(stage, native.pkg.main));
    artifacts.push(pack(stage, native.pkg.name).tarball);
  } else {
    // Intel macOS is a source-build CI target, not a published native package.
    // Test the documented explicit override instead of claiming registry support.
    const binary = join(repoRoot, `http3.${process.platform}-${process.arch}.node`);
    assert.ok(existsSync(binary), `No published sidecar or source-built override for ${process.platform}-${process.arch}`);
    env.NAPI_RS_NATIVE_LIBRARY_PATH = binary;
    console.log(`Testing source-built native override: ${binary}`);
  }
  writeFileSync(join(tempDir, 'package.json'), JSON.stringify({ private: true }));
  run('npm', ['install', '--include=optional', '--ignore-scripts', '--no-audit', '--no-fund', '--package-lock=false', ...artifacts]);
  const installedRoot = join(tempDir, 'node_modules/@currentspace/http3');
  const output = run(process.execPath, ['-e', `
    const assert = require('node:assert/strict');
    const {createRequire} = require('node:module');
    const {join} = require('node:path');
    const {readdirSync} = require('node:fs');
    const pkg = require('@currentspace/http3');
    for (const key of ['createSecureServer','createQuicServer','connectQuic','createSseStream','loadTlsOptionsFromAwsEnv','createNodeServerFactory','createExpressHandler','listenNest','proxy','createDevTls']) assert.equal(typeof pkg[key], 'function', key);
    assert.equal(typeof require('@currentspace/http3/node').createNodeServerFactory, 'function');
    assert.equal(typeof require('@currentspace/http3/express').createExpressHandler, 'function');
    const root = ${JSON.stringify(installedRoot)};
    assert.ok(readdirSync(root).every(f => !f.endsWith('.node')));
    const binding = require(join(root, 'index.js'));
    assert.equal(typeof binding.NativeWorkerServer, 'function');
    ${native ? `const resolved = createRequire(join(root, 'index.js')).resolve(${JSON.stringify(native.pkg.name)}); assert.ok(resolved.includes(${JSON.stringify(native.pkg.name)}));` : ''}
    console.log('Packed install loaded the native binding and Node/Express exports on ' + process.version);
  `], tempDir, env);
  process.stdout.write(output);
  // Compile a consumer against the installed tarball, not source aliases.
  const require = createRequire(import.meta.url);
  const compilerManifest = require.resolve('typescript/package.json');
  const compiler = JSON.parse(readFileSync(compilerManifest, 'utf8'));
  const compilerBin = resolve(compilerManifest, '..', compiler.bin.tsc ?? compiler.bin.tsc6);
  writeFileSync(join(tempDir, 'consumer.ts'), `
    import {createNodeServerFactory} from '@currentspace/http3/node';
    import {createExpressHandler} from '@currentspace/http3/express';
    import {createNodeServerFactory as rootFactory, createExpressHandler as rootHandler, createSecureServer} from '@currentspace/http3';
    import {listenNest} from '@currentspace/http3/nest';
    import {proxy} from '@currentspace/http3/proxy';
    import {createDevTls} from '@currentspace/http3/dev';
    void listenNest; void proxy; void createDevTls;
    const app = createExpressHandler((_req, res) => res.end('ok'));
    const server = createNodeServerFactory({})(app);
    server.listen({port: 0, host: '::'});
    server.listen(0, '::', () => {});
    rootFactory({})(rootHandler(app));
    createSecureServer({}).listen({port: 0});
    // @ts-expect-error Ports must be numeric.
    server.listen({port: '443'});
  `);
  for (const [moduleResolution, module] of [['node', 'CommonJS'], ['node16', 'Node16'], ['nodenext', 'NodeNext'], ['bundler', 'ESNext']]) {
    writeFileSync(join(tempDir, 'tsconfig.json'), JSON.stringify({ compilerOptions: {
      moduleResolution, module, target: 'ES2022', strict: true, noEmit: true,
      skipLibCheck: true, ignoreDeprecations: '6.0',
      typeRoots: [resolve(repoRoot, 'node_modules/@types')], types: ['node'],
    }, files: ['consumer.ts'] }));
    run(process.execPath, [compilerBin, '-p', 'tsconfig.json']);
    console.log(`Installed type declarations resolve with moduleResolution=${moduleResolution}`);
  }
  console.log(`Root package: ${root.size} packed bytes, ${root.unpackedSize} unpacked bytes; no bundled native binaries.`);
  process.stdout.write(run(process.execPath, [join(installedRoot, 'bin/http3.cjs'), '--help']));
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
