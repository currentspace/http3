import assert from 'node:assert';
import { describe, it } from 'node:test';
import { binding } from '../lib/event-loop.js';
import { connectQuic } from '../lib/quic-client.js';
import { createQuicServer } from '../lib/quic-server.js';

describe('raw QUIC prebuild guards', () => {
  it('throws a clear error when NativeQuicClient is missing', () => {
    const mutableBinding = binding as { NativeQuicClient?: unknown };
    const original = mutableBinding.NativeQuicClient;
    mutableBinding.NativeQuicClient = undefined;

    try {
      assert.throws(
        () => connectQuic('127.0.0.1:4433'),
        /NativeQuicClient|incomplete prebuild/u,
      );
    } finally {
      mutableBinding.NativeQuicClient = original;
    }
  });

  it('rejects listen() with a clear error when NativeQuicServer is missing', async () => {
    const mutableBinding = binding as { NativeQuicServer?: unknown };
    const original = mutableBinding.NativeQuicServer;
    mutableBinding.NativeQuicServer = undefined;

    try {
      const server = createQuicServer({
        key: Buffer.from('key'),
        cert: Buffer.from('cert'),
      });
      await assert.rejects(
        async () => server.listen(0),
        /NativeQuicServer|incomplete prebuild/u,
      );
    } finally {
      mutableBinding.NativeQuicServer = original;
    }
  });
});
