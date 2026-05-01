/**
 * Audit finding #15. Connect-time AbortSignal must be honored. Today's
 * test exercises the resolve step (the most common stall point) — a
 * caller-supplied AbortSignal that fires before lookup completes should
 * surface as an abort error rather than waiting for DNS to time out.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { resolveConnectionEndpoint } from '../../lib/endpoint.js';

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
});
