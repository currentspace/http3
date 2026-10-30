import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';

export const directory = fileURLToPath(new URL('../', import.meta.url));
export const port = Number(process.env.HTTP3_TEST_PORT ?? 3457);
export const origin = `https://localhost:${port}`;
export const certFile = process.env.TLS_CERT_FILE ?? directory + '.certs/cert.pem';
export async function until(predicate, timeout, description) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, `Timed out: ${description}`);
    await new Promise(resolve => setTimeout(resolve, 50));
  }
}
export async function startServer() {
  const child = spawn(process.execPath, ['dist/main.js'], {
    cwd: directory,
    env: { ...process.env, NEXT_TELEMETRY_DISABLED: '1', PORT: String(port),
      TLS_CERT_FILE: certFile, TLS_KEY_FILE: process.env.TLS_KEY_FILE ?? directory + '.certs/key.pem',
      HTTP3_TEST_PROGRESS: '1', NODE_EXTRA_CA_CERTS: certFile },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '', ended = false;
  child.stdout.on('data', data => { logs += data; });
  child.stderr.on('data', data => { logs += data; });
  child.on('error', error => { logs += String(error); });
  const exit = once(child, 'exit').then(([code, signal]) => { ended = true; return { code, signal }; });
  async function kill() {
    if (!ended) { child.kill('SIGKILL'); await exit; }
  }
  try {
    await until(() => ended || logs.includes(`Ready: ${origin}`), 45000, 'combined server startup');
    assert.equal(ended, false, logs);
  } catch (error) { await kill(); throw new Error(`${error}\n${logs}`); }
  return {
    pid: child.pid, logs: () => logs, kill,
    async prefix(id) {
      const find = () => logs.slice(0, logs.lastIndexOf('\n') + 1).split('\n')
        .filter(line => line.startsWith('UPLOAD_PREFIX '))
        .map(line => JSON.parse(line.slice('UPLOAD_PREFIX '.length))).find(entry => entry.id === id);
      await until(() => find(), 15000, 'Nest received upload prefix before producer EOF');
      return find().bytes;
    },
    async stop() {
      const started = Date.now();
      child.kill('SIGTERM');
      await until(() => ended, 10000, 'coordinated shutdown');
      assert.deepEqual(await exit, { code: 0, signal: null }, logs);
      assert.ok(logs.includes('Closed both applications'), logs);
      return Date.now() - started;
    },
  };
}
