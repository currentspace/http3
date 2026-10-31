import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { describe, it } from 'node:test';
import { pathToFileURL } from 'node:url';

interface PackMetadata {
  name: string;
  files: { path: string }[];
}
interface PackParser {
  parseNpmPackMetadata: (output: string, packageName: string) => PackMetadata;
}
async function parser(): Promise<PackParser> {
  return await import(pathToFileURL(resolve(__dirname, '../../../scripts/npm-pack-metadata.mjs')).href) as PackParser;
}

describe('npm release pack metadata', () => {
  const name = '@currentspace/http3-darwin-x64';
  const metadata = {
    name,
    version: '1.0.0-bootstrap.0',
    files: [{ path: 'package.json' }, { path: 'http3.darwin-x64.node' }],
  };

  for (const [format, report] of [
    ['npm 11 array', [metadata]],
    ['npm 12 package map', { [name]: metadata }],
    ['direct metadata', metadata],
  ] as const) {
    it(`reads the actual file list from ${format}`, async () => {
      const { parseNpmPackMetadata } = await parser();
      assert.deepEqual(parseNpmPackMetadata(JSON.stringify(report), name), metadata);
    });
  }

  it('rejects empty, incomplete, or ambiguous results before publication', async () => {
    const { parseNpmPackMetadata } = await parser();
    for (const report of [
      null,
      {},
      [],
      [metadata, metadata],
      { name, files: [] },
      { name, files: [{}] },
      { name, files: [{ path: '' }] },
      { name, files: [{ path: 42 }] },
      { name },
      { name: '@currentspace/other', files: metadata.files },
      { '@currentspace/other': metadata },
    ]) {
      assert.throws(() => parseNpmPackMetadata(JSON.stringify(report), name), /Invalid npm pack metadata/u);
    }
    assert.throws(() => parseNpmPackMetadata('not json', name), SyntaxError);
  });
});
