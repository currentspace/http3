import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';

export const rootDirectory = resolve(import.meta.dirname, '..');

// The sidecar manifests are the inventory. Keep host selection shared between
// packing and installed-consumer tests so GNU and musl never select each other.
export function nativePackages(root = rootDirectory) {
  const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
  const packages = readdirSync(join(root, 'npm')).sort().map(dir => ({
    dir,
    pkg: JSON.parse(readFileSync(join(root, 'npm', dir, 'package.json'), 'utf8')),
  }));
  for (const { dir, pkg } of packages) {
    if (pkg.name !== `${manifest.name}-${dir}` || pkg.main !== `http3.${dir}.node`
      || pkg.version !== manifest.version || manifest.optionalDependencies?.[pkg.name] !== manifest.version) {
      throw new Error(`Native package metadata is inconsistent: npm/${dir}/package.json`);
    }
  }
  const names = new Set(packages.map(({ pkg }) => pkg.name));
  for (const name of Object.keys(manifest.optionalDependencies ?? {})) {
    if (name.startsWith(`${manifest.name}-`) && !names.has(name)) {
      throw new Error(`Missing native package manifest: ${name}`);
    }
  }
  return packages;
}

export function hostPlatform() {
  const { platform, arch } = process;
  if (platform !== 'linux') return { platform, arch };
  const report = process.report.getReport();
  return { platform, arch, libc: report.header.glibcVersionRuntime ? 'glibc' : 'musl' };
}

export function selectNativePackage(packages, host = hostPlatform()) {
  const matches = packages.filter(({ pkg }) => pkg.os.includes(host.platform)
    && pkg.cpu.includes(host.arch)
    && (host.platform !== 'linux' || pkg.libc?.includes(host.libc)));
  if (matches.length > 1) throw new Error(`Ambiguous native package for ${JSON.stringify(host)}`);
  return matches[0];
}
