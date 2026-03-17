/**
 * Unit tests for lib/error-map.ts.
 * Pure logic tests — no server, no network.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { toStreamError, toSessionError } from '../lib/error-map.js';
import { Http3Error, ERR_HTTP3_STREAM_ERROR, ERR_HTTP3_SESSION_ERROR } from '../lib/errors.js';
import type { NativeEvent } from '../lib/event-loop.js';

describe('error-map', () => {
  describe('toStreamError', () => {
    it('should use meta.errorReason and meta.errorCode when present', () => {
      const event: NativeEvent = {
        eventType: 6,
        connHandle: 1,
        streamId: 0,
        meta: {
          errorReason: 'flow control exceeded',
          errorCode: 3,
        },
      };

      const err = toStreamError(event);

      assert.ok(err instanceof Error);
      assert.ok(err instanceof Http3Error);
      assert.strictEqual(err.message, 'flow control exceeded');
      assert.strictEqual(err.code, ERR_HTTP3_STREAM_ERROR);
      assert.strictEqual(err.h3Code, 3);
    });

    it('should use fallback message when meta is absent', () => {
      const event: NativeEvent = {
        eventType: 6,
        connHandle: 1,
        streamId: 0,
      };

      const err = toStreamError(event);

      assert.strictEqual(err.message, 'stream error');
      assert.strictEqual(err.code, ERR_HTTP3_STREAM_ERROR);
      assert.strictEqual(err.h3Code, undefined);
    });

    it('should use custom fallback string when provided', () => {
      const event: NativeEvent = {
        eventType: 6,
        connHandle: 1,
        streamId: 4,
      };

      const err = toStreamError(event, 'request stream failed');

      assert.strictEqual(err.message, 'request stream failed');
      assert.strictEqual(err.code, ERR_HTTP3_STREAM_ERROR);
    });
  });

  describe('toSessionError', () => {
    it('should use meta.errorReason and meta.errorCode when present', () => {
      const event: NativeEvent = {
        eventType: 7,
        connHandle: 1,
        streamId: 0,
        meta: {
          errorReason: 'idle timeout',
          errorCode: 0,
        },
      };

      const err = toSessionError(event);

      assert.ok(err instanceof Error);
      assert.ok(err instanceof Http3Error);
      assert.strictEqual(err.message, 'idle timeout');
      assert.strictEqual(err.code, ERR_HTTP3_SESSION_ERROR);
      assert.strictEqual(err.quicCode, 0);
    });

    it('should use fallback message when meta is absent', () => {
      const event: NativeEvent = {
        eventType: 7,
        connHandle: 1,
        streamId: 0,
      };

      const err = toSessionError(event);

      assert.strictEqual(err.message, 'session error');
      assert.strictEqual(err.code, ERR_HTTP3_SESSION_ERROR);
      assert.strictEqual(err.quicCode, undefined);
    });
  });

  describe('Http3Error shape', () => {
    it('should have name Http3Error and be instanceof Error', () => {
      const event: NativeEvent = {
        eventType: 6,
        connHandle: 0,
        streamId: 0,
        meta: { errorReason: 'test', errorCode: 42 },
      };

      const err = toStreamError(event);

      assert.strictEqual(err.name, 'Http3Error');
      assert.ok(err instanceof Error);
      assert.ok(err instanceof Http3Error);
    });
  });
});
