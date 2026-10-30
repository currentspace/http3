// Pack the freshly built host binary so candidate installs cannot use an older
// same-version registry sidecar. Run after the native build, from the repo root.
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { nativePackages, selectNativePackage } from './native-packages.mjs';

const destination = resolve(process.argv[2] ?? '.');
const root = process.cwd();
const packages = nativePackages(root);
const targetFlag = process.argv.indexOf('--target');
const native = (targetFlag >= 0
  ? packages.find(({ dir }) => dir === process.argv[targetFlag + 1])
  : selectNativePackage(packages))?.pkg;
if (!native) throw new Error(`No published native target for ${process.platform}-${process.arch}; build from source instead.`);
const stage = mkdtempSync(join(tmpdir(), 'http3-native-pack-'));
try {
  mkdirSync(destination, { recursive: true });
  writeFileSync(join(stage, 'package.json'), JSON.stringify(native));
  copyFileSync(join(root, native.main), join(stage, native.main));
  execFileSync('pnpm', ['pack', '--config.ignore-scripts=true', '--pack-destination', destination], { cwd: stage, stdio: 'inherit' });
} finally { rmSync(stage, { recursive: true, force: true }); }
