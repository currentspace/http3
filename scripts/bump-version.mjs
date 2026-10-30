import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { nativePackages, rootDirectory } from './native-packages.mjs';

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u.test(version ?? '')) {
  throw new Error('Usage: node scripts/bump-version.mjs <new-semver>');
}
const rootPath = join(rootDirectory, 'package.json');
const manifest = JSON.parse(readFileSync(rootPath, 'utf8'));
const packages = nativePackages();
const cargoPath = join(rootDirectory, 'Cargo.toml');
const cargo = readFileSync(cargoPath, 'utf8');
if (!cargo.includes(`version = "${manifest.version}"`)) throw new Error('Cargo version differs from package.json');
const previous = manifest.version;
const lockPath = join(rootDirectory, 'Cargo.lock');
const lock = existsSync(lockPath) ? readFileSync(lockPath, 'utf8') : undefined;
const crateEntry = `[[package]]\nname = "http3"\nversion = "${previous}"`;
if (lock !== undefined && !lock.includes(crateEntry)) throw new Error('Cargo.lock root version differs from package.json');
manifest.version = version;
for (const { dir, pkg } of packages) {
  manifest.optionalDependencies[pkg.name] = version;
  pkg.version = version;
  writeFileSync(join(rootDirectory, 'npm', dir, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`);
}
writeFileSync(rootPath, `${JSON.stringify(manifest, null, 2)}\n`);
writeFileSync(cargoPath, cargo.replace(`version = "${previous}"`, `version = "${version}"`));
if (lock !== undefined) writeFileSync(lockPath, lock.replace(crateEntry, `[[package]]\nname = "http3"\nversion = "${version}"`));
console.log(`Updated root, Cargo and ${packages.length} native packages: ${previous} -> ${version}`);
console.log('Add CHANGELOG.md release notes and complete release qualification before publishing.');
