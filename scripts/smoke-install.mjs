import { mkdtempSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';

const repoRoot = process.cwd();
const tempDir = mkdtempSync(join(tmpdir(), 'http3-smoke-install-'));
const run = (command, args) => {
  const result = spawnSync(command, args, { cwd: tempDir, stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed`);
  }
};

let tarball;
try {
  const pack = spawnSync('npm', ['pack', '--silent'], {
    cwd: repoRoot,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  if (pack.status !== 0) {
    throw new Error('npm pack --silent failed');
  }

  const tarballName = pack.stdout
    .split(/\r?\n/u)
    .map((line) => line.trim())
    .filter(Boolean)
    .at(-1);
  if (!tarballName) {
    throw new Error('npm pack --silent did not produce a tarball name');
  }

  tarball = resolve(repoRoot, tarballName);
  run('npm', ['init', '-y']);
  run('npm', ['install', tarball]);
  const check = spawnSync(
    process.execPath,
    [
      '-e',
      [
        "const pkg = require('@currentspace/http3');",
        "if (typeof pkg.createSecureServer !== 'function') throw new Error('createSecureServer missing');",
        "if (typeof pkg.createSseStream !== 'function') throw new Error('createSseStream missing');",
        "if (typeof pkg.loadTlsOptionsFromAwsEnv !== 'function') throw new Error('loadTlsOptionsFromAwsEnv missing');",
        "console.log('smoke install passed');",
      ].join(''),
    ],
    { cwd: tempDir, stdio: 'inherit' },
  );
  if (check.status !== 0) {
    throw new Error('package smoke runtime import failed');
  }
} finally {
  rmSync(tempDir, { recursive: true, force: true });
  if (tarball) {
    rmSync(tarball, { force: true });
  }
}
