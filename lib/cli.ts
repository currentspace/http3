import { readFileSync } from 'node:fs';
import { runDevCertCli } from './dev-cert.js';

export async function runCli(args: string[]): Promise<void> {
  if (args[0] === 'dev-cert') { runDevCertCli(args); return; }
  if (!args.length || args.includes('--help') || args.includes('-h')) {
    console.log('Usage: http3 dev-cert [generate|trust|untrust] [--out DIR] [--ca-dir DIR] [--trust]\n       http3 doctor https://localhost:3443 [--ca FILE] [--chrome-profile DIR] [--json]\nDoctor is read-only. It probes TCP/UDP reachability, trust and Alt-Svc, and checks curl and a specified Chrome profile.');
    return;
  }
  try {
    if (args.shift() !== 'doctor' || !args[0]) throw new Error('Expected dev-cert or doctor URL; use --help');
    const url = args.shift() ?? ''; 
    let ca: Buffer | undefined;
    let chromeProfile: string | undefined;
    let json = false;
    while (args.length) {
      const option = args.shift();
      if (option === '--json') json = true;
      else if (option === '--ca' && args[0] && !args[0].startsWith('--')) ca = readFileSync(args.shift() ?? '');
      else if (option === '--chrome-profile' && args[0] && !args[0].startsWith('--')) chromeProfile = args.shift();
      else throw new Error(`Unknown or incomplete option: ${String(option)}`);
    }
    // Certificates and --help must work even before a native addon is installed.
    const { doctor } = await import('./doctor.js');
    const report = await doctor(url, { ca, chromeProfile });
    console.log(json ? JSON.stringify(report, null, 2) : report.checks.map(check => `${check.status.toUpperCase()} ${check.name}: ${check.detail}`).join('\n'));
    if (report.checks.some(check => check.status === 'fail')) process.exitCode = 1;
  } catch (error) { console.error(error instanceof Error ? error.message : String(error)); process.exitCode = 1; }
}
