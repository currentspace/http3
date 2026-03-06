import { existsSync } from 'node:fs';

const expected = [
  'http3.linux-x64-gnu.node',
  'http3.linux-arm64-gnu.node',
  'http3.darwin-x64.node',
  'http3.darwin-arm64.node',
  'http3.win32-x64-msvc.node',
];

const missing = expected.filter((name) => !existsSync(name));
if (missing.length > 0) {
  console.error('Missing required prebuild binaries:');
  for (const name of missing) {
    console.error(`- ${name}`);
  }
  process.exit(1);
}

console.log('All required prebuild binaries are present.');

