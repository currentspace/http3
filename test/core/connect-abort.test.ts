/**
 * Audit finding #15. Connect-time AbortSignal must be honored. Today's
 * test exercises the resolve step (the most common stall point) — a
 * caller-supplied AbortSignal that fires before lookup completes should
 * surface as an abort error rather than waiting for DNS to time out.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { resolveConnectionEndpoint } from '../../lib/endpoint.js';
import { connect, connectQuic } from '../../lib/index.js';

function nextImmediate(): Promise<void> {
  return new Promise((resolve) => {
    setImmediate(resolve);
  });
}

function isAbortErrorWith(message: RegExp): (err: unknown) => boolean {
  return (err: unknown): boolean => (
    err instanceof Error
    && err.name === 'AbortError'
    && message.test(err.message)
  );
}

describe('resolveConnectionEndpoint AbortSignal', () => {
  it('throws synchronously if signal is already aborted', async () => {
    const ac = new AbortController();
    ac.abort(new Error('cancelled'));
    await assert.rejects(
      resolveConnectionEndpoint('does-not-resolve.invalid', {
        defaultScheme: 'https',
        defaultPort: 443,
        signal: ac.signal,
      }),
      /cancelled/,
    );
  });

  it('throws when signal aborts during lookup', async () => {
    const ac = new AbortController();
    setImmediate(() => { ac.abort(new Error('mid-flight')); });
    await assert.rejects(
      resolveConnectionEndpoint('example.invalid', {
        defaultScheme: 'https',
        defaultPort: 443,
        signal: ac.signal,
      }),
      /mid-flight/,
    );
  });

  it('IP literal short-circuits without lookup (no abort needed)', async () => {
    const ac = new AbortController();
    ac.abort(new Error('would-cancel'));
    // The signal is aborted, but the resolver checks pre-lookup. For an
    // IP literal we skip the DNS path entirely, so the abort still
    // takes effect at the throwIfAborted entry — verify that.
    await assert.rejects(
      resolveConnectionEndpoint('127.0.0.1:443', {
        defaultScheme: 'https',
        defaultPort: 443,
        signal: ac.signal,
      }),
      /would-cancel/,
    );
  });

  it('rejects raw QUIC ready() and closes native resources when aborted during handshake', async () => {
    const ac = new AbortController();
    const session = connectQuic('127.0.0.1:19999', {
      rejectUnauthorized: false,
      signal: ac.signal,
      maxIdleTimeoutMs: 10_000,
    });

    await nextImmediate();
    ac.abort(new Error('cancel raw handshake'));

    await assert.rejects(
      session.ready(),
      isAbortErrorWith(/cancel raw handshake/),
    );
    await session.close();
  });

  it('rejects HTTP/3 ready() and closes native resources when aborted during handshake', async () => {
    const ac = new AbortController();
    const session = connect('https://127.0.0.1:19998', {
      rejectUnauthorized: false,
      signal: ac.signal,
      maxIdleTimeoutMs: 10_000,
    });

    await nextImmediate();
    ac.abort(new Error('cancel h3 handshake'));

    await assert.rejects(
      session.ready(),
      isAbortErrorWith(/cancel h3 handshake/),
    );
    await session.close();
  });
});
