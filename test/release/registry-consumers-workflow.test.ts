import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { it } from 'node:test';

const workflow = readFileSync(resolve(__dirname, '../../../.github/workflows/registry-consumers.yml'), 'utf8');
const validator = workflow.match(/node --input-type=module <<'NODE'\n([\s\S]*?)\n\s+NODE/u)?.[1];
assert.ok(validator, 'the workflow must validate the input before starting consumer jobs');

function validate(version: string): { status: number | null; output: string } {
  const directory = mkdtempSync(join(tmpdir(), 'http3-registry-version-'));
  const output = join(directory, 'output');
  try {
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', validator!], {
      env: { ...process.env, HTTP3_REQUESTED_VERSION: version, GITHUB_OUTPUT: output },
      encoding: 'utf8',
    });
    return { status: result.status, output: existsSync(output) ? readFileSync(output, 'utf8') : '' };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

it('registry workflow accepts exact releases and prereleases', () => {
  for (const version of ['0.9.10', '1.0.0-rc.1', '1.0.0-0', '2.0.0-alpha-beta.01a']) {
    assert.deepEqual(validate(version), { status: 0, output: `version=${version}\n` });
  }
});

it('registry workflow rejects tags, ranges, paths and shell input before producing job output', () => {
  for (const version of [
    '', 'latest', '^1.2.3', 'v1.2.3', '01.2.3', '1.2.3-01', '1.2.3+build',
    '1.2.3\n', 'file:../candidate', 'https://example.com/package.tgz', '1.2.3\nnext=evil',
    '1.2.3; echo unsafe', '1.2.3$(echo unsafe)', '1.2.3`echo unsafe`',
  ]) {
    const result = validate(version);
    assert.notEqual(result.status, 0, version);
    assert.equal(result.output, '', version);
  }
});
