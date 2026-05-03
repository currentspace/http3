import { describe, it, before } from 'node:test';
import assert from 'node:assert';
import { createSecureServer, connectAsync, createQuicServer, connectQuicAsync } from '../../lib/index.js';
import type {
  Http3SecureServer,
  Http3ClientSession,
  QuicServer,
  QuicServerSession,
} from '../../lib/index.js';
import type { ClientHttp3Stream, ServerHttp3Stream } from '../../lib/stream.js';
import type { QuicStream } from '../../lib/quic-stream.js';
import { generateTestCerts } from '../support/generate-certs.js';

function onceError(stream: ClientHttp3Stream | ServerHttp3Stream | QuicStream, timeoutMs = 5000): Promise<Error> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error('timed out waiting for stream error'));
    }, timeoutMs);
    const cleanup = (): void => {
      clearTimeout(timer);
      stream.removeListener('error', onError);
      stream.removeListener('end', onEnd);
    };
    const onError = (err: Error): void => {
      cleanup();
      resolve(err);
    };
    const onEnd = (): void => {
      cleanup();
      reject(new Error('stream ended without reset error'));
    };
    stream.once('error', onError);
    stream.once('end', onEnd);
  });
}

function onceTerminated(stream: QuicStream, timeoutMs = 5000): Promise<'error' | 'end' | 'close'> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error('timed out waiting for stream termination'));
    }, timeoutMs);
    const cleanup = (): void => {
      clearTimeout(timer);
      stream.removeListener('error', onError);
      stream.removeListener('end', onEnd);
      stream.removeListener('close', onClose);
    };
    const onError = (): void => {
      cleanup();
      resolve('error');
    };
    const onEnd = (): void => {
      cleanup();
      resolve('end');
    };
    const onClose = (): void => {
      cleanup();
      resolve('close');
    };
    stream.once('error', onError);
    stream.once('end', onEnd);
    stream.once('close', onClose);
  });
}

async function listenH3(server: Http3SecureServer): Promise<number> {
  return new Promise<number>((resolve) => {
    server.on('listening', () => {
      const addr = server.address();
      assert.ok(addr);
      resolve(addr.port);
    });
    server.listen(0, '127.0.0.1');
  });
}

describe('stream reset interop', () => {
  let certs: { key: Buffer; cert: Buffer };

  before(() => {
    certs = generateTestCerts();
  });

  it('H3 server reset is surfaced as a client stream error', async () => {
    const server = createSecureServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    }, (stream) => {
      stream.respond({ ':status': '200' });
      stream.write(Buffer.from('partial'));
      stream.close(0x010c);
    });

    let client: Http3ClientSession | null = null;
    try {
      const port = await listenH3(server);
      client = await connectAsync(`127.0.0.1:${port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.request({
        ':method': 'GET',
        ':path': '/reset',
        ':authority': 'localhost',
        ':scheme': 'https',
      }, { endStream: true });
      stream.resume();

      const err = await onceError(stream);
      assert.match(err.message, /stream reset/i);
    } finally {
      if (client) await client.close();
      await server.close();
    }
  });

  it('raw QUIC server reset terminates the client stream', async () => {
    const server: QuicServer = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    server.on('session', (session: QuicServerSession) => {
      session.on('stream', (stream: QuicStream) => {
        stream.once('data', () => {
          stream.write(Buffer.from('partial'));
          stream.close(0x42);
        });
      });
    });

    let client: Awaited<ReturnType<typeof connectQuicAsync>> | null = null;
    try {
      const addr = await server.listen(0, '127.0.0.1');
      client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.openStream();
      const terminated = onceTerminated(stream);
      stream.write(Buffer.from('trigger'));
      stream.resume();

      const event = await terminated;
      assert.ok(event === 'error' || event === 'close' || event === 'end');
    } finally {
      if (client) await client.close();
      await server.close();
    }
  });

  it('raw QUIC client reset terminates the server stream', async () => {
    const server: QuicServer = createQuicServer({
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
    });

    const serverReset = new Promise<'error' | 'end' | 'close'>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error('timed out waiting for server reset'));
      }, 5000);
      server.on('session', (session: QuicServerSession) => {
        session.on('stream', (stream: QuicStream) => {
          const finish = (event: 'error' | 'end' | 'close'): void => {
            clearTimeout(timer);
            resolve(event);
          };
          stream.once('error', () => { finish('error'); });
          stream.once('end', () => { finish('end'); });
          stream.once('close', () => { finish('close'); });
        });
      });
    });

    let client: Awaited<ReturnType<typeof connectQuicAsync>> | null = null;
    try {
      const addr = await server.listen(0, '127.0.0.1');
      client = await connectQuicAsync(`127.0.0.1:${addr.port}`, {
        rejectUnauthorized: false,
      });

      const stream = client.openStream();
      stream.on('error', () => { /* reset path under test is server-side */ });
      stream.write(Buffer.from('trigger'));
      await new Promise<void>((resolve) => { setTimeout(resolve, 20); });
      stream.close(0x42);

      const event = await serverReset;
      assert.ok(event === 'error' || event === 'close' || event === 'end');
    } finally {
      if (client) await client.close();
      await server.close();
    }
  });
});
