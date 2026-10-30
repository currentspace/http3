import { test } from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { Server } from 'node:net';
import { createSocket } from 'node:dgram';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createSecureServer } from '../../lib/server.js';
import { createDevTls } from '../../lib/dev-cert.js';

const modes = process.env.HTTP3_WASM === '1' ? ['portable', 'wasm'] as const : ['portable'] as const;
for (const runtimeMode of modes) {
  for (const scenario of ['retry', 'exhausted', 'explicit'] as const) {
    test(`${runtimeMode}: ephemeral TCP collision ${scenario}`, async t => {
      const directory = mkdtempSync(join(tmpdir(), 'http3-ephemeral-'));
      t.after(() => rmSync(directory, { recursive: true, force: true }));
      const server = createSecureServer({ ...createDevTls({ directory }), runtimeMode });
      t.after(() => server.close());
      // Applied below with the original receiver using Reflect.apply.
      // eslint-disable-next-line @typescript-eslint/unbound-method
      const originalListen = Server.prototype.listen;
      let attempts = 0;
      const ports: number[] = [];
      // Force the TCP half of startup to fail after the real UDP bind, so the
      // test covers retry cleanup without relying on a random OS collision.
      t.mock.method(Server.prototype, 'listen', function(this: Server, ...args: unknown[]) {
        attempts++;
        ports.push(args[0] as number);
        if (scenario !== 'retry' || attempts === 1) {
          process.nextTick(() => this.emit('error', Object.assign(new Error('occupied TCP port'), { code: 'EADDRINUSE' })));
          return this;
        }
        return Reflect.apply(originalListen, this, args) as Server;
      });
      if (scenario === 'explicit') {
        const reserve = createSocket('udp6');
        reserve.bind(0, '::'); await once(reserve, 'listening');
        const port = reserve.address().port;
        await new Promise<void>(resolve => reserve.close(() => resolve()));
        const error = once(server, 'error');
        server.listen(port);
        assert.equal((await error)[0].code, 'EADDRINUSE');
        assert.equal(attempts, 1);
        assert.equal(ports[0], port);
      } else if (scenario === 'exhausted') {
        const error = once(server, 'error');
        server.listen(0);
        assert.equal((await error)[0].code, 'EADDRINUSE');
        assert.equal(attempts, 9);
      } else {
        let callbacks = 0;
        const ready = once(server, 'listening');
        server.listen(0, () => callbacks++);
        await ready;
        assert.equal(callbacks, 1);
        assert.equal(attempts, 2);
      }
      await server.close();
      assert.equal(server.address(), null);
      // Every partial UDP listener, including the last successful one, must
      // be released. Otherwise retries can leak sockets and worker threads.
      for (const port of new Set(ports)) {
        const socket = createSocket('udp6');
        const ready = once(socket, 'listening');
        socket.bind(port, '::'); await ready;
        await new Promise<void>(resolve => socket.close(() => resolve()));
      }
    });
  }
}
