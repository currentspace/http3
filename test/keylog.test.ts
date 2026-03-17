/**
 * Unit tests for keylog utilities.
 */

import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert';
import { existsSync, writeFileSync, unlinkSync, appendFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { randomUUID } from 'node:crypto';
import { prepareKeylogFile, subscribeKeylog } from '../lib/keylog.js';

describe('keylog', () => {
  const tempFiles: string[] = [];

  afterEach(() => {
    for (const f of tempFiles) {
      try { unlinkSync(f); } catch { /* cleanup */ }
    }
    tempFiles.length = 0;
  });

  describe('prepareKeylogFile', () => {
    it('should return null for falsy values', () => {
      assert.strictEqual(prepareKeylogFile(undefined), null);
      assert.strictEqual(prepareKeylogFile(false), null);
    });

    it('should create a file when true is passed', () => {
      const saved = process.env.SSLKEYLOGFILE;
      delete process.env.SSLKEYLOGFILE;

      const path = prepareKeylogFile(true);
      assert.ok(path);
      assert.ok(existsSync(path));
      assert.strictEqual(process.env.SSLKEYLOGFILE, path);
      tempFiles.push(path);

      // Restore
      if (saved) {
        process.env.SSLKEYLOGFILE = saved;
      } else {
        delete process.env.SSLKEYLOGFILE;
      }
    });

    it('should use the exact path when a string is passed', () => {
      const target = join(tmpdir(), `keylog-test-${randomUUID()}.log`);
      const path = prepareKeylogFile(target);
      assert.strictEqual(path, target);
      assert.ok(existsSync(target));
      tempFiles.push(target);
    });
  });

  describe('subscribeKeylog', () => {
    it('should return an unsubscribe function', () => {
      const target = join(tmpdir(), `keylog-sub-${randomUUID()}.log`);
      writeFileSync(target, '');
      tempFiles.push(target);

      const lines: string[] = [];
      const unsub = subscribeKeylog(target, (line) => {
        lines.push(line.toString());
      });

      assert.strictEqual(typeof unsub, 'function');
      unsub();
    });

    it('should emit lines appended to the keylog file', async () => {
      const target = join(tmpdir(), `keylog-emit-${randomUUID()}.log`);
      writeFileSync(target, '');
      tempFiles.push(target);

      const lines: string[] = [];
      const unsub = subscribeKeylog(target, (line) => {
        lines.push(line.toString().trim());
      });

      // Append some lines
      appendFileSync(target, 'CLIENT_RANDOM abc123 def456\n');
      appendFileSync(target, 'CLIENT_RANDOM 789abc 012def\n');

      // Wait for poll interval to pick up changes (100ms poll + margin)
      await new Promise<void>((resolve) => { setTimeout(resolve, 300); });

      assert.ok(lines.length >= 2, `expected at least 2 lines, got ${lines.length}`);
      assert.ok(lines.some(l => l.includes('abc123')));
      assert.ok(lines.some(l => l.includes('789abc')));

      unsub();
    });

    it('should stop polling after last listener unsubscribes', async () => {
      const target = join(tmpdir(), `keylog-stop-${randomUUID()}.log`);
      writeFileSync(target, '');
      tempFiles.push(target);

      const lines: string[] = [];
      const unsub = subscribeKeylog(target, (line) => {
        lines.push(line.toString().trim());
      });

      unsub();

      // Append after unsubscribe — should not be received
      appendFileSync(target, 'SHOULD_NOT_APPEAR\n');
      await new Promise<void>((resolve) => { setTimeout(resolve, 200); });

      assert.ok(!lines.some(l => l.includes('SHOULD_NOT_APPEAR')));
    });
  });
});
