import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { basename, extname, join, relative, resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const scanEntries = ['src', 'build.rs'];
const sourceExtensions = new Set([
  '.c',
  '.cc',
  '.cpp',
  '.cxx',
  '.h',
  '.hh',
  '.hpp',
  '.m',
  '.mm',
  '.rs',
]);

const forbiddenPatterns = [
  {
    label: 'Node C++ headers',
    pattern: /#\s*include\s*[<"](?:node|node_buffer|node_object_wrap)\.h[>"]/u,
  },
  {
    label: 'V8 headers',
    pattern: /#\s*include\s*[<"]v8(?:-[^>"]*)?\.h[>"]/u,
  },
  {
    label: 'libuv headers',
    pattern: /#\s*include\s*[<"]uv\.h[>"]/u,
  },
  {
    label: 'NAN API',
    pattern: /#\s*include\s*[<"]nan\.h[>"]|\bNan::|\bNAN_/u,
  },
  {
    label: 'Node C++ namespace',
    pattern: /\bnode::/u,
  },
  {
    label: 'V8 C++ namespace',
    pattern: /\bv8::/u,
  },
  {
    label: 'direct libuv API',
    pattern: /\buv_[A-Za-z0-9_]+\b/u,
  },
];

function shouldScan(path) {
  const name = basename(path);
  if (name === 'build.rs') {
    return true;
  }
  return sourceExtensions.has(extname(path));
}

function collectFiles(entry, files = []) {
  const path = resolve(root, entry);
  if (!existsSync(path)) {
    return files;
  }

  const stat = statSync(path);
  if (stat.isDirectory()) {
    for (const child of readdirSync(path)) {
      collectFiles(join(entry, child), files);
    }
    return files;
  }

  if (stat.isFile() && shouldScan(path)) {
    files.push(path);
  }
  return files;
}

const violations = [];

for (const file of scanEntries.flatMap((entry) => collectFiles(entry))) {
  const source = readFileSync(file, 'utf8');
  const lines = source.split(/\r?\n/u);
  for (const [index, line] of lines.entries()) {
    for (const { label, pattern } of forbiddenPatterns) {
      if (pattern.test(line)) {
        violations.push({
          file: relative(root, file),
          line: index + 1,
          label,
          text: line.trim(),
        });
      }
    }
  }
}

if (violations.length > 0) {
  console.error('Node-API boundary check failed.');
  console.error('The release prebuilds are shared across Node 24/25/26, so native code must not use Node/V8/libuv ABI-bound addon APIs.');
  for (const violation of violations) {
    console.error(`- ${violation.file}:${violation.line} ${violation.label}: ${violation.text}`);
  }
  process.exit(1);
}

console.log('Node-API boundary check passed: no direct Node/V8/libuv addon APIs found.');
