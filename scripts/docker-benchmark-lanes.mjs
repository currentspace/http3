export function createDockerLaneMatrix(includePrivileged = false) {
  const lanes = [
    {
      name: 'ordinary portable',
      benchmarkArgs: ['--runtime-mode', 'portable', '--fallback-policy', 'error'],
    },
    {
      name: 'ordinary auto fallback',
      benchmarkArgs: ['--runtime-mode', 'auto', '--fallback-policy', 'warn-and-fallback'],
    },
    {
      name: 'ordinary fast unavailable (expected)',
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
      expectFailure: true,
    },
    {
      name: 'cap-add fast unavailable (expected)',
      dockerFlags: ['--cap-add', 'SYS_ADMIN'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
      expectFailure: true,
    },
    {
      name: 'unconfined portable',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: ['--runtime-mode', 'portable', '--fallback-policy', 'error'],
    },
    {
      name: 'unconfined fast',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
    },
    {
      name: 'unconfined server fast, client portable',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: [
        '--server-runtime-mode', 'fast',
        '--server-fallback-policy', 'error',
        '--client-runtime-mode', 'portable',
        '--client-fallback-policy', 'error',
      ],
    },
    {
      name: 'unconfined server portable, client fast',
      dockerFlags: ['--security-opt', 'seccomp=unconfined'],
      benchmarkArgs: [
        '--server-runtime-mode', 'portable',
        '--server-fallback-policy', 'error',
        '--client-runtime-mode', 'fast',
        '--client-fallback-policy', 'error',
      ],
    },
  ];

  if (includePrivileged) {
    lanes.push({
      name: 'privileged fast',
      dockerFlags: ['--privileged'],
      benchmarkArgs: ['--runtime-mode', 'fast', '--fallback-policy', 'error'],
    });
  }

  return lanes;
}
