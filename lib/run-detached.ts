/**
 * Run a promise-returning operation in the background without awaiting it
 * at the call site, while still guaranteeing its rejection is observed.
 *
 * Constructors, plain (non-async) event-handler callbacks, and
 * setTimeout/setInterval callbacks can't `await` — the operations they
 * kick off are necessarily detached. `void someAsyncCall()` documents that
 * intent to the type checker, but it's exactly as capable of producing an
 * unhandled promise rejection as writing nothing at all: `void` only says
 * "I know I'm discarding this," it does not attach a rejection handler.
 * This project's convention is that no promise anywhere — detached or
 * otherwise — may fail silently, so every detached call site uses this
 * helper instead of `void` and supplies the same error handling its
 * synchronous callers would have reached for (an `_emitError`-style
 * method, a logger, etc.).
 *
 * This alone only fixes half the problem: it stops a detached failure
 * from vanishing, but gives nothing else a way to know when (or whether)
 * the detached work is actually done — the exact gap that let a timed-out
 * test's own background connection attempts keep running for several
 * minutes after the test had already been reported as failed (see
 * test/interop/quic-high-conn.test.ts's history). Anywhere a detached
 * operation's own object has a shutdown path (`close()`/`destroy()`),
 * prefer {@link DetachedTasks} instead so that shutdown can actually wait
 * for it — reach for this bare function only where no such shutdown path
 * exists (e.g. a per-request handler with no session-level lifecycle to
 * hook).
 */
export function runDetached(operation: Promise<unknown>, onError: (error: Error) => void): void {
  operation.then(
    () => { /* fire-and-forget: success needs no further action */ },
    (err: unknown) => {
      onError(err instanceof Error ? err : new Error(String(err)));
    },
  );
}

/**
 * Registry of in-flight detached operations for a single object instance
 * (a client session, a server, an event source), so its own
 * `close()`/`destroy()` can drain them before reporting shutdown complete
 * instead of leaving them to finish on their own, unobserved, arbitrarily
 * later.
 */
export class DetachedTasks {
  private readonly _pending = new Set<Promise<void>>();

  /**
   * Run `operation` in the background: tracked here until it settles
   * (removing itself from the pending set), with any rejection routed to
   * `onError` instead of becoming an unhandled promise rejection.
   */
  run(operation: Promise<unknown>, onError: (error: Error) => void): void {
    const settled: Promise<void> = operation.then(
      () => { /* fire-and-forget: success needs no further action */ },
      (err: unknown) => {
        onError(err instanceof Error ? err : new Error(String(err)));
      },
    ).then(() => {
      this._pending.delete(settled);
    });
    this._pending.add(settled);
  }

  /** Number of detached operations still in flight. */
  get size(): number {
    return this._pending.size;
  }

  /**
   * Wait for every currently-tracked task to settle. Safe to call from a
   * `close()`/`destroy()` method: tasks `run()` calls itself during the
   * drain (e.g. a reconnect kicked off by one of the very tasks being
   * awaited) are picked up too, since `_pending` is read live on each
   * pass rather than snapshotted once.
   */
  async drain(): Promise<void> {
    while (this._pending.size > 0) {
      await Promise.all(this._pending);
    }
  }
}
