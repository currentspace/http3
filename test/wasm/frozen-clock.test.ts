/**
 * C6 (docs/WASM_CLIENT_PLAN.md §7): frozen-clock simulation.
 *
 * Cloudflare Workers freezes `Date.now()`/`performance.now()` during
 * synchronous JS execution and only advances the clock when the isolate
 * actually wakes for an I/O event (a datagram arrives, a timer fires, ...)
 * — see docs/WASM_CLIENT_PLAN.md §4.7 and §9 constraint C3. That is
 * fundamentally different from Node's normal free-running wall clock, and
 * the wasm client's pump-and-single-timer design needs to tolerate it
 * correctly, years before real workerd UDP exists (§10 risk R8).
 *
 * This test installs a clock that never advances on its own via
 * `lib/wasm/wasi-shim.ts`'s injectable `ShimOptions.nowNs` (constructing
 * `WasmH3ClientEventLoop` directly — exactly like
 * test/wasm/deterministic-clock.test.ts's C5 test — because
 * `connect()`/`connectAsync()` have no public option to inject a mock
 * clock; the shim is an internal testing hook, not a supported production
 * customization point). The test itself advances the clock explicitly at
 * the points real I/O would occur, mirroring workerd's documented
 * semantics:
 *
 *  - after every real datagram *send*    (wraps `DatagramTransport.send`)
 *  - after every real datagram *receive* (wraps `DatagramTransport.onMessage`)
 *  - after every timer *fire*            (see the `_forceTimeoutCheck()`
 *    call site below for why this one is simulated rather than
 *    intercepted)
 *
 * A full QUIC+TLS handshake and one HTTP/3 GET are driven end-to-end
 * against a real native server (reusing the same native-server +
 * wasm-client pairing `test/support/wasm-test-helpers.ts`'s
 * `createWasmH3Pair` uses, constructed by hand here because that helper
 * goes through the public `connectAsync()` API and has no way to inject a
 * mock clock or a wrapped transport). The assertion is simply that the
 * whole exchange still completes, proving the timer/pump design does not
 * depend on time silently advancing between explicit I/O ticks.
 *
 * Gated by HTTP3_WASM=1 + a built dist/wasm/http3_client.wasm — see
 * test/support/wasm-test-helpers.ts's wasmSkipReason(). Self-skips cleanly
 * (never fails) when the toolchain/artifact is absent.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { loadBinding, generateTestCerts, createEventCollector } from '../support/native-test-helpers.js';
import { wasmArtifactPath, wasmSkipReason } from '../support/wasm-test-helpers.js';
import type { DatagramTransport } from '../../lib/wasm/datagram-transport.js';
import type { WasmEvent } from '../../lib/wasm/events.js';

const EVENT_HEADERS = 3;
const EVENT_DATA = 4;
const EVENT_FINISHED = 5;
const EVENT_HANDSHAKE_COMPLETE = 11;

/**
 * Milliseconds the simulated clock advances per real datagram send/receive
 * tick. Small and "realistic", but the exact value doesn't matter: the
 * whole point is that the *sum* of these explicit ticks across an entire
 * handshake+GET stays far below any timeout threshold, because nothing
 * else ever advances this clock — proving the exchange completes without
 * relying on real wall-clock time leaking into quiche's view of "now".
 */
const IO_STEP_MS = 1;
/** Milliseconds advanced for the one deliberately-simulated timer-fire tick. */
const TIMER_STEP_MS = 5;
/**
 * Simulated milliseconds advanced per real-time tick while `close()` is in
 * flight — see `FrozenClock.duringClose()`'s doc comment for why this needs
 * to keep advancing repeatedly rather than jumping once.
 */
const CLOSE_DRAIN_TICK_MS = 500;
/** Real milliseconds between `duringClose()` ticks. */
const CLOSE_DRAIN_TICK_INTERVAL_MS = 5;

/**
 * A clock that never advances on its own: `nowNs()` always returns the
 * same frozen value until one of the `after*` methods below is called.
 * Wired into `WasmH3ClientEventLoop` via `ShimOptions.nowNs`
 * (`lib/wasm/wasi-shim.ts`), which is exactly what quiche's internal
 * `Instant::now()` resolves to inside the wasm module (§4.7) — no
 * timestamp ever crosses the wasm ABI directly, so freezing this one
 * function is sufficient to freeze the whole protocol core's view of time.
 */
class FrozenClock {
  private frozenNs = 0n;
  sendTicks = 0;
  receiveTicks = 0;
  timerFireTicks = 0;

  readonly nowNs = (): bigint => this.frozenNs;

  private advanceBy(ms: number): void {
    this.frozenNs += BigInt(Math.round(ms * 1_000_000));
  }

  /** Call immediately after a real datagram send completes — an outbound I/O event. */
  afterSend(): void {
    this.advanceBy(IO_STEP_MS);
    this.sendTicks += 1;
  }

  /**
   * Call immediately before delivering a just-arrived datagram to the
   * core. Advancing *before* delivery (rather than strictly after) means
   * the core's synchronous `h3c_recv`/pump processing of the packet
   * observes the post-tick time, matching workerd's documented "Date.now()
   * reflects the actual elapsed time once the isolate wakes for I/O"
   * behavior (docs/WASM_CLIENT_PLAN.md §4.7/§9 C3).
   */
  beforeReceiveDelivery(): void {
    this.advanceBy(IO_STEP_MS);
    this.receiveTicks += 1;
  }

  /** Call immediately before forcing a timer-fire check — see the call site below for why this test simulates the "timer fire" I/O point this way. */
  beforeTimerFire(): void {
    this.advanceBy(TIMER_STEP_MS);
    this.timerFireTicks += 1;
  }

  /**
   * Call repeatedly (via a real interval — see the call site) for the
   * duration of `close()`. `WasmH3ClientEventLoop`'s close path
   * (`lib/wasm/h3-client-event-loop.ts`'s `doClose`) computes quiche's
   * drain-period deadline *once*, as "whatever `nowNs()` reports right when
   * `h3c_close()` runs, plus quiche's own PTO/drain duration" — then polls
   * `h3c_on_timeout()` against real wall-clock sleeps until a *later*
   * `nowNs()` reading crosses that already-fixed deadline. A single one-off
   * jump taken *before* `close()` begins doesn't help: it only shifts what
   * "now" the deadline gets computed relative to, not whether any
   * subsequent on_timeout() call perceives having crossed it. Under Node's
   * normal free-running clock this falls out for free (real time keeps
   * moving during the real sleeps); here it doesn't unless something keeps
   * nudging `nowNs()` forward across that same real-time window. Each tick
   * is a safe no-op once the deadline is already crossed — this is a
   * teardown convenience only, unrelated to the frozen-clock claim this
   * test exists to verify, which the handshake + GET above already fully
   * proved.
   */
  duringClose(): void {
    this.advanceBy(CLOSE_DRAIN_TICK_MS);
  }
}

/**
 * Wrap a real `DatagramTransport` so every send/receive ticks the frozen
 * clock at exactly the point the real I/O occurs. The transport itself (a
 * real connected Node UDP socket via `connectNodeUdp`) stays completely
 * real — only the wasm module's *perceived* clock is frozen-and-stepped.
 */
function wrapWithFrozenClock(transport: DatagramTransport, clock: FrozenClock): DatagramTransport {
  return {
    send(datagram: Uint8Array): void {
      transport.send(datagram);
      clock.afterSend();
    },
    onMessage(cb: (datagram: Uint8Array) => void): void {
      transport.onMessage((datagram) => {
        clock.beforeReceiveDelivery();
        cb(datagram);
      });
    },
    localAddress: () => transport.localAddress(),
    close: () => transport.close(),
  };
}

/**
 * Accumulate a client-side H3 response (status headers + body) from raw
 * dispatched events for one streamId. Resolves on `EVENT_FINISHED` *or* a
 * `fin` flag on any event for that stream — never gate solely on `data`
 * presence, since a FIN-only final frame carries no data and would hang a
 * data-only check. Mirrors test/wasm/h3-loopback.test.ts's
 * `collectServerBody` (same event contract, opposite direction).
 */
function collectClientResponse(
  dispatched: WasmEvent[],
  streamId: number,
  timeoutMs = 5000,
): Promise<{ status: string; headers: Record<string, string>; body: Buffer }> {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const headers: Record<string, string> = {};
    const chunks: Uint8Array[] = [];
    let processed = 0;

    const check = (): void => {
      for (; processed < dispatched.length; processed++) {
        const evt = dispatched[processed];
        if (!evt || evt.streamId !== streamId) continue;
        if (evt.eventType === EVENT_HEADERS && evt.headers) {
          for (const h of evt.headers) headers[h.name] = h.value;
        }
        // A HEADERS event can carry inline body data too (the H3 client
        // dispatch in lib/client.ts's _onHeaders() handles this exact case:
        // "if (event.data) { stream._pushData(event.data); }") — never gate
        // data collection on EVENT_DATA alone. Mirrors
        // test/wasm/h3-loopback.test.ts's collectServerBody.
        if ((evt.eventType === EVENT_DATA || evt.eventType === EVENT_HEADERS) && evt.data) {
          chunks.push(evt.data);
        }
        if (evt.eventType === EVENT_FINISHED || evt.fin) {
          resolve({ status: headers[':status'] ?? '', headers, body: Buffer.concat(chunks) });
          return;
        }
      }
      if (Date.now() > deadline) {
        reject(new Error(`collectClientResponse timed out waiting for streamId=${streamId}`));
        return;
      }
      setTimeout(check, 10);
    };
    check();
  });
}

function waitForEvent(dispatched: WasmEvent[], eventType: number, timeoutMs: number): Promise<WasmEvent> {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const check = (): void => {
      const found = dispatched.find((e) => e.eventType === eventType);
      if (found) {
        resolve(found);
        return;
      }
      if (Date.now() > deadline) {
        reject(new Error(`timed out after ${String(timeoutMs)}ms waiting for eventType=${String(eventType)}`));
        return;
      }
      setTimeout(check, 5);
    };
    check();
  });
}

describe('wasm frozen-clock simulation (C6)', { skip: wasmSkipReason() }, () => {
  it('completes a full handshake + GET with a clock frozen except at explicit I/O ticks', async () => {
    const { WasmH3ClientEventLoop } = await import('../../lib/wasm/h3-client-event-loop.js');
    const { connectNodeUdp } = await import('../../lib/wasm/node-udp-adapter.js');
    const { loadHttp3WasmCoreFromFile } = await import('../../lib/wasm/node-core-loader.js');

    const binding = loadBinding();
    const certs = generateTestCerts();
    const serverEvents = createEventCollector();
    const server = new binding.NativeWorkerServer(
      { key: certs.key, cert: certs.cert, disableRetry: true, runtimeMode: 'portable' },
      serverEvents.callback,
    );
    const addr = server.listen(0, '127.0.0.1') as { address: string; port: number };

    const clock = new FrozenClock();
    const dispatched: WasmEvent[] = [];
    const eventLoop = new WasmH3ClientEventLoop(
      {
        core: loadHttp3WasmCoreFromFile(wasmArtifactPath(), { nowNs: clock.nowNs }),
        rejectUnauthorized: false,
        transportFactory: async (host, port) => wrapWithFrozenClock(await connectNodeUdp(host, port), clock),
      },
      (events) => {
        dispatched.push(...events);
      },
    );

    try {
      await eventLoop.connect(`127.0.0.1:${String(addr.port)}`, 'localhost');
      await waitForEvent(dispatched, EVENT_HANDSHAKE_COMPLETE, 5000);

      // The handshake alone already exercised real sends/receives — prove
      // the instrumentation actually engaged before moving on.
      assert.ok(clock.sendTicks > 0, 'expected at least one simulated send tick during the handshake');
      assert.ok(clock.receiveTicks > 0, 'expected at least one simulated receive tick during the handshake');

      // Simulate the third I/O boundary this plan calls out: a timer fire.
      // Driven via the existing `_forceTimeoutCheck()` test-only hook (the
      // same one test/wasm/deterministic-clock.test.ts's C5 test uses)
      // rather than intercepting the real internal `setTimeout` from
      // outside the class: `rearmTimer()` schedules that timer from real
      // `Date.now()`, and reliably reaching into a private timer callback
      // from a test would require either editing production code (out of
      // scope for this task) or globally monkey-patching `setTimeout`
      // (unsafe under `--test-isolation=none`, where unrelated
      // concurrently-running test files share this process).
      // `_forceTimeoutCheck()` drives exactly the same code path
      // (`h3c_on_timeout` + pump) a real fire would, with the clock ticked
      // forward first — mirroring workerd's documented "Date.now() inside a
      // setTimeout callback reflects the scheduled fire time", and is a
      // safe no-op if quiche's own deadline isn't actually due yet.
      clock.beforeTimerFire();
      (eventLoop as unknown as { _forceTimeoutCheck(): void })._forceTimeoutCheck();

      const streamId = eventLoop.sendRequest(
        [
          { name: ':method', value: 'GET' },
          { name: ':path', value: '/hello' },
          { name: ':authority', value: 'localhost' },
          { name: ':scheme', value: 'https' },
        ],
        true,
      );

      const headersEvent = await serverEvents.waitForEvent(EVENT_HEADERS);
      server.sendResponseHeaders(headersEvent.connHandle, headersEvent.streamId, [
        { name: ':status', value: '200' },
      ], false);
      server.streamSend(headersEvent.connHandle, headersEvent.streamId, Buffer.from('frozen clock ok'), true);

      const res = await collectClientResponse(dispatched, streamId);
      assert.equal(res.status, '200');
      assert.equal(res.body.toString('utf8'), 'frozen clock ok');

      assert.ok(clock.sendTicks > 0, 'expected simulated send ticks across the whole exchange');
      assert.ok(clock.receiveTicks > 0, 'expected simulated receive ticks across the whole exchange');
      assert.equal(clock.timerFireTicks, 1, 'expected exactly one simulated timer-fire tick');
    } finally {
      // See FrozenClock.duringClose()'s doc comment — keeps teardown fast
      // instead of burning the close path's full real-time fallback.
      const closeTicker = setInterval(() => { clock.duringClose(); }, CLOSE_DRAIN_TICK_INTERVAL_MS);
      try {
        await eventLoop.close();
      } catch {
        /* already closed / never fully connected */
      } finally {
        clearInterval(closeTicker);
      }
      try { server.requestShutdown(); } catch { /* already shut down */ }
      try { server.joinWorker(); } catch { /* already joined */ }
    }
  });
});
