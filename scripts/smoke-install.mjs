import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';

const repoRoot = process.cwd();
const tarballs = readdirSync(repoRoot)
  .filter((name) => name.endsWith('.tgz'))
  .sort();
if (tarballs.length === 0) {
  throw new Error('no package tarball found. Run `npm pack` first.');
}
const tarball = resolve(repoRoot, tarballs[tarballs.length - 1]);

const tempDir = mkdtempSync(join(tmpdir(), 'http3-smoke-install-'));
const run = (command, args) => {
  const result = spawnSync(command, args, { cwd: tempDir, stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed`);
  }
};

try {
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
}
