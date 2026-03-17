import { copyFileSync, existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SLEEP_BUFFER = new Int32Array(new SharedArrayBuffer(4));

function parseArgs(argv) {
  const options = new Map();
  const flags = new Set();

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg.startsWith('--') && arg.includes('=')) {
      const [key, value] = arg.split(/=(.*)/su, 2);
      options.set(key, value);
      continue;
    }
    if (arg.startsWith('--')) {
      const next = argv[i + 1];
      if (next && !next.startsWith('--')) {
        options.set(arg, next);
        i += 1;
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
    env = {},
    allowFailure = false,
  } = options;

  const result = spawnSync(command, args, {
    cwd,
    env: {
      ...process.env,
      ...env,
    },
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

function sleep(ms) {
  Atomics.wait(SLEEP_BUFFER, 0, 0, ms);
}

function discoverNativePackages() {
  const npmDir = join(ROOT_DIR, 'npm');
  if (!existsSync(npmDir)) {
    return [];
  }

  return readdirSync(npmDir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(npmDir, entry.name, 'package.json')))
    .map((entry) => {
      const directory = join(npmDir, entry.name);
      const manifestPath = join(directory, 'package.json');
      const manifest = readJson(manifestPath);
      if (typeof manifest.name !== 'string' || typeof manifest.version !== 'string') {
        throw new Error(`Invalid package manifest: ${manifestPath}`);
      }
      if (typeof manifest.main !== 'string' || !manifest.main.endsWith('.node')) {
        throw new Error(`Native package ${manifest.name} is missing a .node main entry`);
      }

      return {
        directory,
        manifestPath,
        name: manifest.name,
        version: manifest.version,
        binaryName: manifest.main,
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
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

  throw new Error(
    `Failed to query npm for ${spec} ${field}:\n${result.stderr || result.stdout}`,
  );
}

function assertVersionNotPublished(name, version) {
  const published = npmViewJson(`${name}@${version}`, 'version');
  if (published !== null) {
    throw new Error(`Refusing to publish ${name}@${version}; that version already exists on npm.`);
  }
}

function verifyDistTag(name, version, distTag) {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    const tags = npmViewJson(name, 'dist-tags');
    if (tags && tags[distTag] === version) {
      return;
    }
    if (attempt < 11) {
      sleep(5000);
    } else {
      throw new Error(
        `npm dist-tag verification failed for ${name}: expected ${distTag} -> ${version}, got ${JSON.stringify(tags)}`,
      );
    }
  }
}

function publishPackage(directory, distTag, dryRun, provenance) {
  const args = ['publish', '--tag', distTag, '--access', 'public'];
  if (provenance) {
    args.push('--provenance');
  }
  if (dryRun) {
    args.push('--dry-run');
  }

  run('npm', args, { cwd: directory });
}

const { options, flags } = parseArgs(process.argv.slice(2));
const distTag = options.get('--dist-tag') ?? process.env.HTTP3_DIST_TAG ?? 'latest';
const dryRun = flags.has('--dry-run') || process.env.HTTP3_DRY_RUN === '1';
const provenance = process.env.GITHUB_ACTIONS === 'true';

const rootManifestPath = join(ROOT_DIR, 'package.json');
const rootManifest = readJson(rootManifestPath);
if (typeof rootManifest.name !== 'string' || typeof rootManifest.version !== 'string') {
  throw new Error(`Invalid root package manifest: ${rootManifestPath}`);
}

console.log(`Publishing release packages for ${rootManifest.name}@${rootManifest.version}`);
console.log(`Mode: ${dryRun ? 'dry-run' : 'publish'}; npm dist-tag: ${distTag}`);

run(process.execPath, [join(ROOT_DIR, 'scripts', 'verify-prebuilds.mjs')]);

const nativePackages = discoverNativePackages();
for (const pkg of nativePackages) {
  if (pkg.version !== rootManifest.version) {
    throw new Error(
      `Version mismatch: ${pkg.name} is ${pkg.version} but root package is ${rootManifest.version}`,
    );
  }

  const sourceBinary = join(ROOT_DIR, pkg.binaryName);
  if (!existsSync(sourceBinary)) {
    throw new Error(`Missing prebuild artifact required for ${pkg.name}: ${pkg.binaryName}`);
  }
  copyFileSync(sourceBinary, join(pkg.directory, pkg.binaryName));
}

const packagesToPublish = [
  ...nativePackages,
  {
    directory: ROOT_DIR,
    name: rootManifest.name,
    version: rootManifest.version,
  },
];

for (const pkg of packagesToPublish) {
  assertVersionNotPublished(pkg.name, pkg.version);
}

for (const pkg of nativePackages) {
  console.log(`${dryRun ? 'Dry-running' : 'Publishing'} native package ${pkg.name}@${pkg.version}`);
  publishPackage(pkg.directory, distTag, dryRun, provenance);
}

console.log(`${dryRun ? 'Dry-running' : 'Publishing'} root package ${rootManifest.name}@${rootManifest.version}`);
publishPackage(ROOT_DIR, distTag, dryRun, provenance);

if (!dryRun) {
  for (const pkg of packagesToPublish) {
    verifyDistTag(pkg.name, pkg.version, distTag);
  }
}

console.log(
  `${dryRun ? 'Dry-run completed' : 'Publish completed'} for ${packagesToPublish.length} package(s).`,
);
