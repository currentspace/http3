#!/usr/bin/env node
import { execFileSync } from 'node:child_process';

const disabledModes = new Set(['0', 'false', 'no', 'off']);
const mode = (process.env.HTTP3_BROWSER_SECURITY_SUDO ?? 'auto').toLowerCase();

if (process.platform !== 'darwin' || disabledModes.has(mode)) {
  process.exit(0);
}

try {
  execFileSync('sudo', ['-n', 'true'], {
    stdio: 'ignore',
    timeout: 5_000,
  });
  process.exit(0);
} catch {
  // Continue to an interactive sudo prompt when the command is run from a terminal.
}

if (!process.stdin.isTTY || !process.stderr.isTTY) {
  console.error('macOS browser H3 tests require cached sudo credentials for certificate trust. Run `sudo -v` before test:browser:e2e.');
  process.exit(1);
}

console.error('macOS browser H3 tests need sudo once to install and remove a temporary test CA in System.keychain.');
execFileSync('sudo', ['-v'], { stdio: 'inherit' });
