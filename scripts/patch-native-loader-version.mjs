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

const diagnostic = `if (!nativeBinding) {
  throw require('./bin/native-binding-error.cjs')({
    platform: process.platform,
    arch: process.arch,
    musl: process.platform === 'linux' && isMusl(),
    override: process.env.NAPI_RS_NATIVE_LIBRARY_PATH,
    loadErrors,
    cause: createLoadErrorChain(loadErrors),
  })
}`;
const failureBlock = /if \(!nativeBinding\) \{\n  if \(loadErrors\.length > 0\) \{[\s\S]*?\n  throw new Error\(`Failed to load native binding`\)\n\}/u;
if (failureBlock.test(source)) {
  source = source.replace(failureBlock, diagnostic);
} else if (!source.includes(diagnostic)) {
  throw new Error('Native loader failure block changed; update the diagnostics patch before building.');
}

writeFileSync(loaderPath, source);
