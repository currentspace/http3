import assert from 'node:assert';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { describe, it } from 'node:test';

const SCRIPT_PATH = resolve(__dirname, '../../scripts/verify-prebuilds.mjs');

const EXPECTED_FILES = [
  'http3.linux-x64-gnu.node',
  'http3.linux-arm64-gnu.node',
  'http3.darwin-arm64.node',
];

const REQUIRED_EXPORTS = [
  'NativeWorkerServer',
  'NativeWorkerClient',
  'NativeQuicServer',
  'NativeQuicClient',
];

function makeFixtureDir(overrides: Partial<Record<string, string>> = {}): string {
  const dir = mkdtempSync(join(tmpdir(), 'verify-prebuilds-'));
  const contents = REQUIRED_EXPORTS.join('\0');
  for (const name of EXPECTED_FILES) {
    writeFileSync(join(dir, name), overrides[name] ?? contents);
  }
  return dir;
}

function runVerifyPrebuilds(dir: string) {
  return spawnSync(process.execPath, [SCRIPT_PATH], {
    encoding: 'utf8',
    env: {
      ...process.env,
      VERIFY_PREBUILDS_DIR: dir,
    },
  });
}

describe('verify-prebuilds script', () => {
  it('accepts prebuilds that expose the full raw QUIC surface', () => {
    const dir = makeFixtureDir();
    try {
      const result = runVerifyPrebuilds(dir);
      assert.strictEqual(result.status, 0, result.stderr);
      assert.match(result.stdout, /expected native exports/u);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('fails when a binary is missing NativeQuicClient or NativeQuicServer', () => {
    const dir = makeFixtureDir({
      'http3.linux-arm64-gnu.node': [
        'NativeWorkerServer',
        'NativeWorkerClient',
        'NativeQuicServer',
      ].join('\0'),
    });
    try {
      const result = runVerifyPrebuilds(dir);
      assert.strictEqual(result.status, 1);
      assert.match(result.stderr, /http3\.linux-arm64-gnu\.node: NativeQuicClient/u);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
