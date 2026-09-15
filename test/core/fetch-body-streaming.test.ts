import { it } from 'node:test';
import assert from 'node:assert/strict';
import https from 'node:https';
import { connect as connectH2 } from 'node:http2';
import type { IncomingHttpHeaders } from 'node:http2';
import { connectAsync } from '../../lib/client.js';
import { serveFetch } from '../../lib/fetch-adapter.js';
import { generateTestCerts } from '../support/generate-certs.js';
import type { Readable, Writable } from 'node:stream';

it('H3 resets an upload whose Fetch handler does not consume the receive buffer', async () => {
  const certs = generateTestCerts();
  let aborted = false;
  const server = serveFetch({
    ...certs, port: 0, host: '127.0.0.1', runtimeMode: 'portable', disableRetry: true,
    fetch: async request => {
      await new Promise<void>(resolve => {
        request.signal.addEventListener('abort', () => { aborted = true; resolve(); }, { once: true });
      });
      return new Response('closed');
    },
  });
  const port = await new Promise<number>(resolve => {
    server.once('listening', () => resolve(server.address()!.port));
  });
  const client = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false, runtimeMode: 'portable' });
  try {
    const stream = client.request({ ':method': 'POST', ':path': '/', ':authority': 'localhost', ':scheme': 'https' });
    const reset = new Promise<Error>(resolve => { stream.once('error', resolve); });
    stream.end(Buffer.alloc(2 * 1024 * 1024));
    assert.ok(await reset);
    assert.equal(aborted, true);
  } finally {
    await client.close();
    await server.close();
  }
});

for (const protocol of ['h1', 'h2', 'h3'] as const) {
  for (const oversized of [false, true]) {
    it(`${protocol} ${oversized ? 'rejects a body over the limit without Content-Length' : 'invokes Fetch before the upload ends and preserves body bytes'}`, async () => {
      const certs = generateTestCerts();
      let started!: () => void;
      const handlerStarted = new Promise<void>(resolve => { started = resolve; });
      const server = serveFetch({
        ...certs, port: 0, host: '127.0.0.1', runtimeMode: 'portable',
        disableRetry: true, allowHTTP1: true, maxBodyBytes: 32,
        fetch: async request => {
          started();
          return new Response(await request.text());
        },
      });
      const port = await new Promise<number>((resolve, reject) => {
        server.once('listening', () => resolve(server.address()!.port));
        server.once('error', reject);
      });
      let closeClient: () => Promise<void> = async () => {};
      let timer: NodeJS.Timeout | undefined;
      try {
        let input: Writable;
        let complete!: (result: { status: number; body: string }) => void;
        let fail!: (error: Error) => void;
        const response = new Promise<{ status: number; body: string }>((resolve, reject) => {
          complete = resolve; fail = reject;
        });
        const collect = (stream: Readable, getStatus: () => number): void => {
          const chunks: Buffer[] = [];
          stream.on('data', (chunk: Buffer) => chunks.push(chunk));
          stream.once('end', () => complete({ status: getStatus(), body: Buffer.concat(chunks).toString() }));
          stream.once('error', fail);
        };
        if (protocol === 'h1') {
          const req = https.request(`https://127.0.0.1:${port}/`, {
            method: 'POST', rejectUnauthorized: false, agent: false,
          }, res => collect(res, () => res.statusCode ?? 0));
          input = req;
          req.once('error', fail);
          closeClient = async () => { req.destroy(); };
        } else if (protocol === 'h2') {
          const client = connectH2(`https://127.0.0.1:${port}`, { rejectUnauthorized: false });
          const req = client.request({ ':method': 'POST', ':path': '/' }, { endStream: false });
          let status = 0;
          req.on('response', (headers: IncomingHttpHeaders) => { status = Number(headers[':status']); });
          collect(req, () => status);
          input = req;
          closeClient = async () => { client.destroy(); };
        } else {
          const client = await connectAsync(`127.0.0.1:${port}`, { rejectUnauthorized: false, runtimeMode: 'portable' });
          const req = client.request({ ':method': 'POST', ':path': '/', ':authority': 'localhost', ':scheme': 'https' });
          let status = 0;
          req.on('response', headers => { status = Number(headers[':status']); });
          collect(req, () => status);
          input = req;
          closeClient = async () => { await client.close(); };
        }
        // Observe response errors even if startup itself fails first.
        const received = response.then(result => ({ result }), error => ({ error: error as Error }));
        input.write('first');
        await Promise.race([handlerStarted, new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new Error('Fetch waited for the entire upload')), 2000);
        })]);
        clearTimeout(timer);
        input.end(oversized ? 'x'.repeat(64) : '-last');
        const outcome = await received;
        if ('error' in outcome) throw outcome.error;
        assert.equal(outcome.result.status, oversized ? 413 : 200);
        if (!oversized) assert.equal(outcome.result.body, 'first-last');
      } finally {
        clearTimeout(timer);
        await closeClient();
        await server.close();
      }
    });
  }
}
