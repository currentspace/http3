import { before, describe, it } from 'node:test';
import assert from 'node:assert';
import { connect, createSecureServer, createSseStream } from '../../lib/index.js';
import { generateTestCerts } from '../support/generate-certs.js';

interface ParsedSse {
  event: string;
  data: string;
  id: string;
}

function parseSseFrames(payload: string): ParsedSse[] {
  const frames = payload.split('\n\n').filter(Boolean);
  const parsed: ParsedSse[] = [];
  for (const frame of frames) {
    const record: ParsedSse = { event: 'message', data: '', id: '' };
    for (const line of frame.split('\n')) {
      if (line.startsWith(':')) continue;
      const splitAt = line.indexOf(':');
      if (splitAt < 0) continue;
      const field = line.slice(0, splitAt);
      const value = line.slice(splitAt + 1).trimStart();
      if (field === 'event') record.event = value;
      if (field === 'id') record.id = value;
      if (field === 'data') record.data = record.data ? `${record.data}\n${value}` : value;
    }
    if (record.data.length > 0) {
      parsed.push(record);
    }
  }
  return parsed;
}

describe('SSE over H3', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('streams compliant SSE frames over HTTP/3', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream, headers) => {
      if (headers[':path'] !== '/events') {
        stream.respond({ ':status': '404' }, { endStream: true });
        return;
      }
      const sse = createSseStream(stream);
      void (async () => {
        await sse.send({ id: '1', event: 'tick', data: 'one' });
        await sse.send({ id: '2', event: 'tick', data: ['two', 'line2'] });
        await sse.comment('done');
        sse.close();
      })();
    });

    const port = await new Promise<number>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        resolve(addr.port);
      });
      server.listen(0, '127.0.0.1');
    });

    const session = connect(`127.0.0.1:${port}`, { rejectUnauthorized: false });
    await new Promise<void>((resolve) => {
      session.on('connect', () => resolve());
    });

    const stream = session.request({
      ':method': 'GET',
      ':path': '/events',
      ':authority': 'localhost',
      ':scheme': 'https',
    }, { endStream: true });

    let status = '';
    const chunks: Buffer[] = [];
    await new Promise<void>((resolve, reject) => {
      stream.on('response', (headers: Record<string, string>) => {
        status = headers[':status'] ?? '';
      });
      stream.on('data', (chunk: Buffer) => chunks.push(chunk));
      stream.on('end', () => resolve());
      stream.on('error', reject);
    });

    const parsed = parseSseFrames(Buffer.concat(chunks).toString('utf8'));
    assert.strictEqual(status, '200');
    assert.strictEqual(parsed.length, 2);
    assert.deepStrictEqual(parsed[0], { event: 'tick', data: 'one', id: '1' });
    assert.deepStrictEqual(parsed[1], { event: 'tick', data: 'two\nline2', id: '2' });

    await session.close();
    await server.close();
  });
});
