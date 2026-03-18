import { cpSync, copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg.startsWith('--') && arg.includes('=')) {
      const [key, value] = arg.split(/=(.*)/su, 2);
      options.set(key, value);
      continue;
    }
    if (arg.startsWith('--')) {
      const next = argv[index + 1];
      if (next && !next.startsWith('--')) {
        options.set(arg, next);
        index += 1;
      } else {
        flags.add(arg);
      }
    }
  }

  return { options, flags };
}

function run(command, args, options = {}) {
  const {
    cwd = ROOT_DIR,
    capture = false,
    allowFailure = false,
  } = options;

  const result = spawnSync(command, args, {
    cwd,
    env: process.env,
    encoding: 'utf8',
    stdio: capture ? 'pipe' : 'inherit',
  });

  if (result.error) {
    throw result.error;
  }

  if (!allowFailure && result.status !== 0) {
    const details = capture
      ? `\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`
      : '';
    throw new Error(`Command failed: ${command} ${args.join(' ')}${details}`);
  }

  return result;
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

function npmViewJson(spec, field) {
  const result = run('npm', ['view', spec, field, '--json'], {
    capture: true,
    allowFailure: true,
  });

  if (result.status === 0) {
    const text = result.stdout.trim();
    return text.length > 0 ? JSON.parse(text) : null;
  }

  if (/E404|404 Not Found/u.test(result.stderr)) {
    return null;
  }

  throw new Error(`Failed to query npm for ${spec} ${field}:\n${result.stderr || result.stdout}`);
}

function assertVersionNotPublished(name, version) {
  if (npmViewJson(`${name}@${version}`, 'version') !== null) {
    throw new Error(`Refusing to publish ${name}@${version}; that version already exists on npm.`);
  }
}

function packedFiles(directory) {
  const result = run('npm', ['pack', '--dry-run', '--json'], {
    cwd: directory,
    capture: true,
  });
  const parsed = JSON.parse(result.stdout.trim());
  const metadata = Array.isArray(parsed) ? parsed[0] : parsed;
  return new Set(
    (metadata.files ?? [])
      .map((entry) => entry?.path)
      .filter((value) => typeof value === 'string'),
  );
}

const { options, flags } = parseArgs(process.argv.slice(2));
const packageDir = resolve(
  ROOT_DIR,
  options.get('--package-dir') ?? join('npm', 'linux-x64-gnu'),
);
const binaryPath = options.get('--binary');
const version = options.get('--version');
const distTag = options.get('--dist-tag') ?? 'bootstrap';
const publish = flags.has('--publish');

if (!binaryPath || !version) {
  throw new Error(
    'Usage: node scripts/bootstrap-native-package.mjs '
    + '--binary /path/to/http3.linux-x64-gnu.node --version <version>-bootstrap.0 [--dist-tag bootstrap] [--publish]',
  );
}

if (!existsSync(packageDir)) {
  throw new Error(`Package directory does not exist: ${packageDir}`);
}

const manifestPath = join(packageDir, 'package.json');
const manifest = readJson(manifestPath);
if (typeof manifest.name !== 'string' || typeof manifest.main !== 'string') {
  throw new Error(`Invalid package manifest: ${manifestPath}`);
}

const resolvedBinaryPath = resolve(ROOT_DIR, binaryPath);
if (!existsSync(resolvedBinaryPath)) {
  throw new Error(`Binary does not exist: ${resolvedBinaryPath}`);
}

assertVersionNotPublished(manifest.name, version);

const stagingDir = mkdtempSync(join(tmpdir(), 'http3-native-bootstrap-'));
try {
  cpSync(packageDir, stagingDir, { recursive: true });
  const stagedManifestPath = join(stagingDir, 'package.json');
  const stagedManifest = readJson(stagedManifestPath);
  stagedManifest.version = version;
  writeFileSync(stagedManifestPath, `${JSON.stringify(stagedManifest, null, 2)}\n`);

  copyFileSync(resolvedBinaryPath, join(stagingDir, manifest.main));

  const files = packedFiles(stagingDir);
  if (!files.has(manifest.main)) {
    throw new Error(`Bootstrap tarball is missing ${manifest.main}.`);
  }

  console.log(`${publish ? 'Publishing' : 'Dry-running'} ${manifest.name}@${version} from ${stagingDir}`);
  run('npm', [
    'publish',
    '--access',
    'public',
    '--tag',
    distTag,
    ...(publish ? [] : ['--dry-run']),
  ], { cwd: stagingDir });
} finally {
  rmSync(stagingDir, { recursive: true, force: true });
}
