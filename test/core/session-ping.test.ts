import assert from 'node:assert';
import { describe, it } from 'node:test';
import { Http3Session } from '../../lib/index.js';

describe('session ping callback parity', () => {
  it('returns an RTT snapshot and invokes the optional callback asynchronously', async () => {
    const session = new Http3Session();
    session._lastMetrics = {
      packetsIn: 0,
      packetsOut: 0,
      bytesIn: 0,
      bytesOut: 0,
      handshakeTimeMs: 0,
      rttMs: 12,
      cwnd: 0,
      datagramQueueDepth: 0,
    };

    let sync = true;
    const callback = new Promise<void>((resolve) => {
      const returned = session.ping((err, duration) => {
        assert.strictEqual(sync, false);
        assert.strictEqual(err, null);
        assert.strictEqual(duration, 12);
        resolve();
      });
      assert.strictEqual(returned, 12);
    });
    sync = false;

    await callback;
  });
});
