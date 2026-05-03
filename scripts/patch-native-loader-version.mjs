import { readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const loaderPath = resolve(root, 'index.js');

let source = readFileSync(loaderPath, 'utf8');

if (!source.includes('const EXPECTED_NATIVE_PACKAGE_VERSION = require(\'./package.json\').version')) {
  source = source.replace(
    'const loadErrors = []\n',
    'const loadErrors = []\nconst EXPECTED_NATIVE_PACKAGE_VERSION = require(\'./package.json\').version\n',
  );
}

source = source.replaceAll(
  /bindingPackageVersion !== '[^']+'/g,
  'bindingPackageVersion !== EXPECTED_NATIVE_PACKAGE_VERSION',
);

source = source.replaceAll(
  /expected [^`]+ but got \$\{bindingPackageVersion\}/g,
  'expected ${EXPECTED_NATIVE_PACKAGE_VERSION} but got ${bindingPackageVersion}',
);

writeFileSync(loaderPath, source);
