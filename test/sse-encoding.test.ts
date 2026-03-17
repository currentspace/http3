/**
 * Unit tests for SSE encoding utilities.
 * Pure logic tests — no server, no network.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { encodeSseEvent, encodeSseComment, sseHeaders, createSseReadableStream } from '../lib/sse.js';

describe('SSE encoding', () => {
  describe('encodeSseEvent', () => {
    it('should encode a plain string as a data-only event', () => {
      const result = encodeSseEvent('hello');
      assert.strictEqual(result, 'data: hello\n\n');
    });

    it('should encode a full SseEvent object', () => {
      const result = encodeSseEvent({
        event: 'update',
        id: '42',
        retry: 5000,
        data: 'payload',
      });
      assert.ok(result.includes('event: update\n'));
      assert.ok(result.includes('id: 42\n'));
      assert.ok(result.includes('retry: 5000\n'));
      assert.ok(result.includes('data: payload\n'));
      assert.ok(result.endsWith('\n\n'));
    });

    it('should split multi-line data into separate data: lines', () => {
      const result = encodeSseEvent('line1\nline2\nline3');
      assert.strictEqual(result, 'data: line1\ndata: line2\ndata: line3\n\n');
    });

    it('should handle data array', () => {
      const result = encodeSseEvent({ data: ['a', 'b', 'c'] });
      assert.strictEqual(result, 'data: a\ndata: b\ndata: c\n\n');
    });

    it('should floor and clamp retry to non-negative integer', () => {
      const result = encodeSseEvent({ data: 'x', retry: -100 });
      assert.ok(result.includes('retry: 0\n'));
    });
  });

  describe('encodeSseComment', () => {
    it('should encode a single-line comment', () => {
      const result = encodeSseComment('keepalive');
      assert.strictEqual(result, ': keepalive\n\n');
    });

    it('should encode a multi-line comment', () => {
      const result = encodeSseComment('line1\nline2');
      assert.strictEqual(result, ': line1\n: line2\n\n');
    });

    it('should handle empty comment', () => {
      const result = encodeSseComment('');
      assert.strictEqual(result, ': \n\n');
    });
  });

  describe('sseHeaders', () => {
    it('should return default SSE headers', () => {
      const headers = sseHeaders();
      assert.strictEqual(headers[':status'], '200');
      assert.ok(String(headers['content-type']).includes('text/event-stream'));
    });

    it('should merge extra headers', () => {
      const headers = sseHeaders({ 'x-custom': 'yes' });
      assert.strictEqual(headers['x-custom'], 'yes');
      assert.strictEqual(headers[':status'], '200');
    });
  });

  describe('createSseReadableStream', () => {
    it('should convert an async iterable to a ReadableStream', async () => {
      async function* generate() {
        yield 'event1';
        yield 'event2';
      }

      const stream = createSseReadableStream(generate());
      const reader = stream.getReader();
      const decoder = new TextDecoder();
      const chunks: string[] = [];

      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(decoder.decode(value));
      }

      assert.strictEqual(chunks.length, 2);
      assert.ok(chunks[0].includes('data: event1'));
      assert.ok(chunks[1].includes('data: event2'));
    });
  });
});
