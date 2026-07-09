/**
 * C5 (docs/WASM_CLIENT_PLAN.md §7): deterministic timer test using the
 * WASI shim's injectable clock (`lib/wasm/wasi-shim.ts`'s `ShimOptions.nowNs`)
 * to fast-forward an idle timeout without a real wall-clock wait —
 * asserting `EVENT_SESSION_CLOSE` fires with idle-timeout detail. This is
 * the whole point of making the clock injectable in §6.2.
 *
 * Constructs `WasmH3ClientEventLoop` directly (bypassing
 * `connect()`/`connectAsync()`, which has no public option to inject a
 * mock clock — the shim is an internal testing hook, not a supported
 * production customization point) against a real native H3 server.
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips
 * cleanly (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { loadBinding, generateTestCerts, createEventCollector } from '../support/native-test-helpers.js';
import { wasmArtifactPath, wasmSkipReason } from '../support/wasm-test-helpers.js';

const EVENT_HANDSHAKE_COMPLETE = 11;
const EVENT_SESSION_CLOSE = 7;

/** Idle timeout configured on the wasm client — kept short for test clarity; the whole point of this test is that we never actually wait this long. */
const MAX_IDLE_TIMEOUT_MS = 3000;
/** How far the mock clock jumps forward once past the handshake — comfortably past MAX_IDLE_TIMEOUT_MS. */
const CLOCK_JUMP_MS = 10_000;
/** Real wall-clock budget for observing EVENT_SESSION_CLOSE after the jump — must be far under CLOCK_JUMP_MS/MAX_IDLE_TIMEOUT_MS to prove this is actually wall-clock-free. */
const REAL_WAIT_BUDGET_MS = 2000;

async function waitForEvent(
  events: Array<{ eventType: number; meta?: { errorReason?: string } }>,
  eventType: number,
  timeoutMs: number,
): Promise<{ eventType: number; meta?: { errorReason?: string } }> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = events.find((e) => e.eventType === eventType);
    if (found) return found;
    if (Date.now() > deadline) {
      throw new Error(`timed out after ${timeoutMs}ms waiting for eventType=${eventType}`);
    }
    await new Promise<void>((resolve) => setTimeout(resolve, 5));
  }
}

describe('wasm deterministic clock (C5)', { skip: wasmSkipReason() }, () => {
  it('fast-forwards an idle timeout via a mocked clock without a real wall-clock wait', async () => {
    const { WasmH3ClientEventLoop } = await import('../../lib/wasm/h3-client-event-loop.js');
    const { loadHttp3WasmCoreFromFile } = await import('../../lib/wasm/node-core-loader.js');
    const { connectNodeUdp } = await import('../../lib/wasm/node-udp-adapter.js');

    const binding = loadBinding();
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const server = new binding.NativeWorkerServer(
      { key: certs.key, cert: certs.cert, disableRetry: true, runtimeMode: 'portable' },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

    // Mock clock: tracks real elapsed time (so the handshake proceeds with
    // realistic-looking timings) plus an adjustable offset the test can
    // jump forward on demand — see lib/wasm/wasi-shim.ts's ShimOptions.nowNs.
    let clockOffsetNs = 0n;
    const nowNs = (): bigint => BigInt(Math.round(performance.now() * 1_000_000)) + clockOffsetNs;

    const dispatched: Array<{ eventType: number; meta?: { errorReason?: string } }> = [];
    const eventLoop = new WasmH3ClientEventLoop(
      {
        core: loadHttp3WasmCoreFromFile(wasmArtifactPath(), { nowNs }),
        transportFactory: connectNodeUdp,
        rejectUnauthorized: false,
        maxIdleTimeoutMs: MAX_IDLE_TIMEOUT_MS,
      },
      (events) => {
        dispatched.push(...(events as unknown as Array<{ eventType: number; meta?: { errorReason?: string } }>));
      },
    );

    try {
      await eventLoop.connect(`127.0.0.1:${addr.port}`, 'localhost');
      await waitForEvent(dispatched, EVENT_HANDSHAKE_COMPLETE, 5000);

      // Fast-forward: jump the mocked clock well past the idle timeout.
      // No real sleep — the whole point of this test.
      clockOffsetNs += BigInt(CLOCK_JUMP_MS) * 1_000_000n;

      // Force an on_timeout check now instead of waiting for the armed
      // setTimeout (from *before* the jump) to fire on its own real-time
      // schedule. Deliberately NOT ping() (or any command that sends
      // data): sending a fresh ack-eliciting packet after the jump would
      // let quiche observe post-jump send activity and "revive" its own
      // idle-time bookkeeping, defeating the test — see
      // _forceTimeoutCheck's doc comment on WasmH3ClientEventLoop.
      (eventLoop as unknown as { _forceTimeoutCheck(): void })._forceTimeoutCheck();

      const before = Date.now();
      const closeEvent = await waitForEvent(dispatched, EVENT_SESSION_CLOSE, REAL_WAIT_BUDGET_MS);
      const elapsed = Date.now() - before;

      assert.ok(
        elapsed < REAL_WAIT_BUDGET_MS,
        `observed EVENT_SESSION_CLOSE after ${elapsed}ms real time (budget ${REAL_WAIT_BUDGET_MS}ms) — ` +
        `configured idle timeout was ${MAX_IDLE_TIMEOUT_MS}ms, so this proves the clock mock actually fast-forwarded it`,
      );
      assert.match(closeEvent.meta?.errorReason ?? '', /idle timeout/i);
    } finally {
      try { await eventLoop.close(); } catch { /* already closed / never fully connected */ }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    }
  });
});
