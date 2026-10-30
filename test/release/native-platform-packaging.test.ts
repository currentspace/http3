import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { describe, it } from 'node:test';
import { pathToFileURL } from 'node:url';

const root = resolve(__dirname, '../../..');
interface Sidecar { dir: string; pkg: { name: string; version: string; main: string; os: string[]; cpu: string[]; libc?: string[] } }
interface Inventory {
  nativePackages: () => Sidecar[];
  selectNativePackage: (packages: Sidecar[], host: { platform: string; arch: string; libc?: string }) => Sidecar | undefined;
}
async function inventory(): Promise<Inventory> {
  return await import(pathToFileURL(join(root, 'scripts/native-packages.mjs')).href) as Inventory;
}

describe('native platform package inventory', () => {
  it('selects each published OS/architecture/libc independently', async () => {
    const { nativePackages, selectNativePackage } = await inventory();
    const packages = nativePackages();
    for (const platform of ['darwin', 'linux']) {
      for (const arch of ['arm64', 'x64']) {
        for (const libc of platform === 'linux' ? ['glibc', 'musl'] : [undefined]) {
          const suffix = platform === 'linux' ? `-${libc === 'glibc' ? 'gnu' : 'musl'}` : '';
          const target = `${platform}-${arch}${suffix}`;
          assert.equal(selectNativePackage(packages, { platform, arch, libc })?.dir, target);
        }
      }
    }
    assert.equal(selectNativePackage(packages, { platform: 'win32', arch: 'x64' }), undefined);
    assert.equal(selectNativePackage(packages, { platform: 'linux', arch: 'x64' }), undefined);
    assert.throws(() => selectNativePackage([...packages, ...packages], { platform: 'darwin', arch: 'x64' }), /Ambiguous/u);
  });

  it('matches every N-API target to a sidecar and vice versa', async () => {
    const { nativePackages } = await inventory();
    const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as { napi: { targets: string[] } };
    const targets = nativePackages().map(({ pkg }) => {
      const arch = pkg.cpu[0] === 'x64' ? 'x86_64' : 'aarch64';
      return pkg.os[0] === 'darwin' ? `${arch}-apple-darwin` : `${arch}-unknown-linux-${pkg.libc?.[0] === 'musl' ? 'musl' : 'gnu'}`;
    });
    assert.deepEqual(targets.sort(), [...manifest.napi.targets].sort());
  });

  it('version bump updates all sidecars and root optional versions', () => {
    const stage = mkdtempSync(join(tmpdir(), 'http3-version-test-'));
    try {
      mkdirSync(join(stage, 'scripts'));
      for (const name of ['package.json', 'Cargo.toml', 'Cargo.lock', 'npm', 'scripts/bump-version.mjs', 'scripts/native-packages.mjs']) {
        cpSync(join(root, name), join(stage, name), { recursive: true });
      }
      const result = spawnSync(process.execPath, [join(stage, 'scripts/bump-version.mjs'), '1.2.3-test.1'], { encoding: 'utf8' });
      assert.equal(result.status, 0, result.stderr);
      const manifest = JSON.parse(readFileSync(join(stage, 'package.json'), 'utf8')) as { version: string; optionalDependencies: Record<string, string> };
      assert.equal(manifest.version, '1.2.3-test.1');
      for (const [name, version] of Object.entries(manifest.optionalDependencies)) {
        assert.equal(version, manifest.version);
        const sidecar = JSON.parse(readFileSync(join(stage, 'npm', name.replace('@currentspace/http3-', ''), 'package.json'), 'utf8')) as { version: string };
        assert.equal(sidecar.version, manifest.version);
      }
      assert.match(readFileSync(join(stage, 'Cargo.toml'), 'utf8'), /version = "1\.2\.3-test\.1"/u);
      assert.match(readFileSync(join(stage, 'Cargo.lock'), 'utf8'), /name = "http3"\nversion = "1\.2\.3-test\.1"/u);
    } finally {
      rmSync(stage, { recursive: true, force: true });
    }
  });
});
