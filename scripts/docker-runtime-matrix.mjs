import { execFileSync } from 'node:child_process';

const composeArgs = ['compose', '-f', 'docker-compose.runtime-tests.yml'];
const defaultRuntimePlatform = process.env.DOCKER_RUNTIME_PLATFORM ?? inferDockerRuntimePlatform();

function inferDockerRuntimePlatform() {
  if (process.arch === 'arm64') {
    return 'linux/arm64';
  }
  if (process.arch === 'x64') {
    return 'linux/amd64';
  }
  return null;
}

function runDocker(args, env = {}) {
  execFileSync('docker', [...composeArgs, ...args], {
    stdio: 'inherit',
    env: {
      ...process.env,
      ...(defaultRuntimePlatform ? { DOCKER_RUNTIME_PLATFORM: defaultRuntimePlatform } : {}),
      ...env,
    },
  });
}

function down() {
  try {
    runDocker(['down', '--remove-orphans']);
  } catch {
    // Best-effort cleanup between lanes.
  }
}

function runLane(name, args, env = {}) {
  console.log(`\n=== ${name} ===`);
  down();
  try {
    runDocker(args, env);
  } finally {
    down();
  }
}

if (defaultRuntimePlatform) {
  console.log(`Using Docker runtime test platform: ${defaultRuntimePlatform}`);
}

runDocker(['build', 'sfu']);

runLane('portable lane', [
  'up',
  '--abort-on-container-exit',
  '--exit-code-from',
  'client',
  'client',
], {
  SERVER_RUNTIME_MODE: 'portable',
  SERVER_FALLBACK_POLICY: 'error',
  CLIENT_RUNTIME_MODE: 'portable',
  CLIENT_FALLBACK_POLICY: 'error',
  EXPECT_CLIENT_SELECTED_MODE: 'portable',
  EXPECT_CLIENT_FALLBACK: 'false',
  EXPECT_SERVER_SELECTED_MODE: 'portable',
  EXPECT_SERVER_FALLBACK: 'false',
});

runLane('auto fallback lane', [
  'up',
  '--abort-on-container-exit',
  '--exit-code-from',
  'client',
  'client',
], {
  SERVER_RUNTIME_MODE: 'auto',
  SERVER_FALLBACK_POLICY: 'warn-and-fallback',
  CLIENT_RUNTIME_MODE: 'auto',
  CLIENT_FALLBACK_POLICY: 'warn-and-fallback',
  EXPECT_CLIENT_SELECTED_MODE: 'portable',
  EXPECT_CLIENT_FALLBACK: 'true',
  EXPECT_SERVER_SELECTED_MODE: 'portable',
  EXPECT_SERVER_FALLBACK: 'true',
});

runLane('fast failure lane', ['run', '--rm', 'fast-check']);

runLane('cap-add fast failure lane', ['run', '--rm', 'fast-cap-add-check']);

runLane('unconfined fast lane', [
  'up',
  '--abort-on-container-exit',
  '--exit-code-from',
  'client-unconfined',
  'client-unconfined',
]);

if (process.env.HTTP3_RUNTIME_TEST_PRIVILEGED === '1') {
  runLane('privileged fast lane', [
    'up',
    '--abort-on-container-exit',
    '--exit-code-from',
    'client-privileged',
    'client-privileged',
  ]);
}
