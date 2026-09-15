import { it } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { spawnSync } from 'node:child_process';

function verify(failLint: boolean): ReturnType<typeof spawnSync> {
  const root = mkdtempSync(join(tmpdir(), 'http3-verify-'));
  try {
    mkdirSync(join(root, 'scripts'));
    mkdirSync(join(root, 'bin'));
    writeFileSync(join(root, 'scripts/verify.sh'), readFileSync(resolve(__dirname, '../../../scripts/verify.sh')));
    for (const name of ['node', 'pnpm', 'rustc', 'cargo']) {
      writeFileSync(join(root, 'bin', name), '#!/bin/sh\n' +
        (failLint && name === 'pnpm' ? '[ "$*" = "run lint" ] && exit 7\n' : '') + 'exit 0\n', { mode: 0o755 });
    }
    return spawnSync('/bin/bash', [join(root, 'scripts/verify.sh'), '--fast', '--no-build'], {
      encoding: 'utf8',
      env: { PATH: `${root}/bin:/usr/bin:/bin`, VERIFY_SKIP_WASM: '1' },
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
}

it('verification reports skips separately from passes', () => {
  const result = verify(false);
  assert.equal(result.status, 0, String(result.stderr));
  assert.match(String(result.stdout), /0 failed, 5 skipped/);
  const passed = String(result.stdout).split('  passed: ')[1].split('\n')[0];
  assert.doesNotMatch(passed, /wasm build|browser|smoke|native \+ dist/);
});

it('verification preserves failure status and identifies the failed step', () => {
  const result = verify(true);
  assert.equal(result.status, 7);
  assert.match(String(result.stderr), /FAILED: lint/);
  assert.match(String(result.stdout), /2 passed, 1 failed, 0 skipped/);
});
