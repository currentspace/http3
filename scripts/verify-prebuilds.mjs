import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const prebuildDir = resolve(process.env.VERIFY_PREBUILDS_DIR ?? '.');

const expected = [
  'http3.linux-x64-gnu.node',
  'http3.linux-arm64-gnu.node',
  'http3.darwin-x64.node',
  'http3.darwin-arm64.node',
  'http3.win32-x64-msvc.node',
];

const requiredExports = [
  'NativeWorkerServer',
  'NativeWorkerClient',
  'NativeQuicServer',
  'NativeQuicClient',
];

const hasAsciiMarker = (buffer, marker) => buffer.indexOf(Buffer.from(marker, 'utf8')) !== -1;

const missing = expected.filter((name) => !existsSync(resolve(prebuildDir, name)));
if (missing.length > 0) {
  console.error('Missing required prebuild binaries:');
  for (const name of missing) {
    console.error(`- ${name}`);
  }
  process.exit(1);
}

const broken = [];

for (const name of expected) {
  const binary = readFileSync(resolve(prebuildDir, name));
  const missingExports = requiredExports.filter((exportName) => !hasAsciiMarker(binary, exportName));
  if (missingExports.length > 0) {
    broken.push({ name, missingExports });
  }
}

if (broken.length > 0) {
  console.error('Broken prebuild binaries are missing required native exports:');
  for (const entry of broken) {
    console.error(`- ${entry.name}: ${entry.missingExports.join(', ')}`);
  }
  process.exit(1);
}

console.log(
  `All required prebuild binaries are present in ${prebuildDir} and expose the expected native exports.`,
);

