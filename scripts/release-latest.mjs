import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const WORKFLOW_FILE = 'release.yml';

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

function output(command, args, options = {}) {
  return run(command, args, { ...options, capture: true }).stdout.trim();
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

function getRepoSlug(repositoryUrl) {
  const match = repositoryUrl.match(/github\.com[/:]([^/]+\/[^/.]+)(?:\.git)?$/u);
  if (!match) {
    throw new Error(`Could not determine GitHub repo from ${repositoryUrl}`);
  }
  return match[1];
}

function extractChangelogNotes(version) {
  const changelogPath = join(ROOT_DIR, 'CHANGELOG.md');
  const contents = readFileSync(changelogPath, 'utf8');
  const lines = contents.split(/\r?\n/u);
  const heading = `## ${version}`;
  const start = lines.findIndex((line) => line.trim() === heading);
  if (start === -1) {
    return `Release ${version}`;
  }

  let end = start + 1;
  while (end < lines.length && !lines[end].startsWith('## ')) {
    end += 1;
  }

  const notes = lines.slice(start + 1, end).join('\n').trim();
  return notes.length > 0 ? notes : `Release ${version}`;
}

function npmDistTags(name) {
  const result = run('npm', ['view', name, 'dist-tags', '--json'], {
    capture: true,
  });
  return JSON.parse(result.stdout.trim());
}

function discoverNativePackages() {
  const npmDir = join(ROOT_DIR, 'npm');
  if (!existsSync(npmDir)) {
    return [];
  }

  return readdirSync(npmDir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(npmDir, entry.name, 'package.json')))
    .map((entry) => {
      const manifest = readJson(join(npmDir, entry.name, 'package.json'));
      return {
        name: manifest.name,
        version: manifest.version,
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
}

async function sleep(ms) {
  await new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

async function waitForWorkflowRun(repoSlug, branch, headSha, createdAfterMs) {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    const response = JSON.parse(
      output('gh', [
        'api',
        '--method',
        'GET',
        `repos/${repoSlug}/actions/workflows/${WORKFLOW_FILE}/runs`,
        '-f',
        `branch=${branch}`,
        '-f',
        'event=workflow_dispatch',
        '-f',
        'per_page=20',
      ]),
    );

    const run = response.workflow_runs.find((candidate) =>
      candidate.head_sha === headSha
      && new Date(candidate.created_at).getTime() >= createdAfterMs
    );

    if (run) {
      return run;
    }

    await sleep(5000);
  }

  throw new Error(`Timed out waiting for a ${WORKFLOW_FILE} run for ${headSha} on ${branch}`);
}

async function waitForRunCompletion(repoSlug, runId) {
  while (true) {
    const run = JSON.parse(
      output('gh', ['api', `repos/${repoSlug}/actions/runs/${runId}`]),
    );

    console.log(`Workflow run ${runId}: ${run.status}${run.conclusion ? ` / ${run.conclusion}` : ''}`);
    if (run.status === 'completed') {
      if (run.conclusion !== 'success') {
        throw new Error(`Workflow run failed: ${run.html_url}`);
      }
      return run;
    }

    await sleep(10000);
  }
}

async function dispatchAndWait(repoSlug, branch, headSha, distTag, dryRun, safariValidated) {
  const dispatchStartMs = Date.now() - 60_000;

  run('gh', [
    'workflow',
    'run',
    WORKFLOW_FILE,
    '--ref',
    branch,
    '-f',
    `dist_tag=${distTag}`,
    '-f',
    `dry_run=${dryRun ? 'true' : 'false'}`,
    '-f',
    `safari_validation_completed=${safariValidated ? 'true' : 'false'}`,
  ]);

  const runInfo = await waitForWorkflowRun(repoSlug, branch, headSha, dispatchStartMs);
  console.log(`Watching workflow run: ${runInfo.html_url}`);
  return await waitForRunCompletion(repoSlug, runInfo.id);
}

async function main() {
  const { options, flags } = parseArgs(process.argv.slice(2));
  const distTag = options.get('--dist-tag') ?? 'latest';
  const commitStaged = flags.has('--commit-staged');
  const safariValidated = flags.has('--safari-validation-completed');
  const validateOnly = flags.has('--validate-only');

  if ((distTag === 'latest' || distTag === 'rc') && !safariValidated) {
    throw new Error(
      `dist-tag ${distTag} requires --safari-validation-completed to match the release workflow gate.`,
    );
  }

  const rootManifest = readJson(join(ROOT_DIR, 'package.json'));
  const version = rootManifest.version;
  const repoSlug = getRepoSlug(rootManifest.repository.url);
  const branch = output('git', ['branch', '--show-current']);

  if (branch !== 'main') {
    throw new Error(`Release automation must start from main; current branch is ${branch}`);
  }

  run('gh', ['auth', 'status']);
  if (!validateOnly) {
    const secretList = output('gh', ['secret', 'list', '--repo', repoSlug]);
    if (!/^NPM_TOKEN\s/mu.test(secretList)) {
      throw new Error(`GitHub repo secret NPM_TOKEN is not configured for ${repoSlug}`);
    }
  }

  run('git', ['fetch', 'origin', 'main', '--tags']);
  const baseMainSha = output('git', ['rev-parse', 'origin/main']);
  const localHeadSha = output('git', ['rev-parse', 'HEAD']);

  if (run('git', ['merge-base', '--is-ancestor', baseMainSha, localHeadSha], {
    allowFailure: true,
  }).status !== 0) {
    throw new Error('Local main is behind or diverged from origin/main; sync before releasing.');
  }

  const stagedFiles = output('git', ['diff', '--cached', '--name-only'])
    .split('\n')
    .filter(Boolean);
  const unstagedFiles = output('git', ['diff', '--name-only'])
    .split('\n')
    .filter(Boolean);
  const untrackedFiles = output('git', ['ls-files', '--others', '--exclude-standard'])
    .split('\n')
    .filter(Boolean);

  if (commitStaged) {
    if (stagedFiles.length === 0) {
      throw new Error('No staged changes to commit.');
    }

    if (unstagedFiles.length > 0 || untrackedFiles.length > 0) {
      console.log(
        'Unstaged or untracked files are present; only the staged release slice will be committed and released.',
      );
    }

    run('git', ['commit', '-m', `release ${version}`]);
  } else if (stagedFiles.length > 0) {
    throw new Error('Staged changes are present. Re-run with --commit-staged or commit them first.');
  }

  const targetSha = output('git', ['rev-parse', 'HEAD']);
  const candidateBranch = options.get('--candidate-branch')
    ?? `release/${version}-candidate-${Date.now()}`;

  run('git', ['push', 'origin', `${targetSha}:refs/heads/${candidateBranch}`]);

  console.log(`Running dry-run release validation on ${candidateBranch}`);
  await dispatchAndWait(repoSlug, candidateBranch, targetSha, distTag, true, safariValidated);

  if (validateOnly) {
    console.log(`Dry-run validation succeeded for ${targetSha} on ${candidateBranch}`);
    return;
  }

  run('git', ['fetch', 'origin', 'main', '--tags']);
  const refreshedMainSha = output('git', ['rev-parse', 'origin/main']);
  if (refreshedMainSha !== baseMainSha) {
    throw new Error(
      `origin/main moved from ${baseMainSha} to ${refreshedMainSha} during validation; aborting publish.`,
    );
  }

  run('git', ['push', 'origin', `${targetSha}:refs/heads/main`]);

  console.log(`Running publish workflow on main for ${targetSha}`);
  await dispatchAndWait(repoSlug, 'main', targetSha, distTag, false, safariValidated);

  const tagName = `v${version}`;
  const remoteTag = run('git', ['ls-remote', '--tags', 'origin', `refs/tags/${tagName}`], {
    capture: true,
  }).stdout.trim();
  if (remoteTag.length > 0) {
    throw new Error(`Remote tag ${tagName} already exists.`);
  }

  run('git', ['tag', '-a', tagName, targetSha, '-m', `release ${version}`]);
  run('git', ['push', 'origin', `refs/tags/${tagName}`]);

  const releaseNotes = extractChangelogNotes(version);
  run('gh', [
    'release',
    'create',
    tagName,
    '--repo',
    repoSlug,
    '--verify-tag',
    '--title',
    tagName,
    '--notes',
    releaseNotes,
    ...(distTag === 'latest' ? ['--latest'] : []),
  ]);

  const expectedPackages = [
    { name: rootManifest.name, version },
    ...discoverNativePackages(),
  ];

  for (const pkg of expectedPackages) {
    const tags = npmDistTags(pkg.name);
    if (tags[distTag] !== pkg.version) {
      throw new Error(
        `npm dist-tag verification failed for ${pkg.name}: expected ${distTag} -> ${pkg.version}, got ${JSON.stringify(tags)}`,
      );
    }
  }

  console.log(`Release ${version} completed successfully.`);
}

await main();
