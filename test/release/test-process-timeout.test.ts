import { it } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

it('the Node test runner reports failures before terminating leaked handles', () => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-test-deadline-'));
  try {
    const fixture = join(directory, 'leaked.test.cjs');
    writeFileSync(fixture, `
      const { test } = require('node:test');
      test('deliberate failure with a leaked handle', () => {
        setInterval(() => {}, 1000);
        throw new Error('fixture assertion failure');
      });
    `);
    const result = spawnSync(process.execPath, [resolve(__dirname, '../../../scripts/test-node.mjs'), fixture], {
      encoding: 'utf8', timeout: 5000,
      env: { ...process.env, HTTP3_NODE_TEST_TIMEOUT_MS: '1000' },
    });
    assert.equal(result.status, 124, result.stderr);
    assert.match(result.stdout, /fixture assertion failure/);
    assert.match(result.stderr, /Node test process exceeded 1000ms/);
  } finally { rmSync(directory, { recursive: true, force: true }); }
});
