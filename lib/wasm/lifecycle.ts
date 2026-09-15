/** One deadline timer shared by the four WASM event loops. */
export class DeadlineTimer {
  private timer: ReturnType<typeof setTimeout> | null = null;
  private deadline: number | null = null;

  cancel(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.deadline = null;
  }

  arm(relativeMs: number, fire: () => void): void {
    if (relativeMs < 0) { this.cancel(); return; }
    const deadline = Date.now() + relativeMs;
    if (this.deadline !== null && Math.abs(deadline - this.deadline) <= 1) return;
    this.cancel();
    this.deadline = deadline;
    const timer = setTimeout(() => {
      // Clear before invoking the callback: it may arm the same deadline.
      this.cancel();
      fire();
    }, relativeMs);
    const unrefable = timer as unknown as { unref?: () => void };
    unrefable.unref?.();
    this.timer = timer;
  }
}

/** Drive protocol close, bounded independently of changes to wall time. */
export async function drainUntilDone(isDone: () => boolean, tick: () => void): Promise<void> {
  const deadline = performance.now() + 2000;
  while (!isDone() && performance.now() < deadline) {
    // Keep this timer referenced: callers are awaiting resource release.
    await new Promise<void>(resolve => { setTimeout(resolve, 5); });
    tick();
  }
}
