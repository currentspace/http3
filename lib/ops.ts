import { createServer, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { Http3SecureServer } from './server.js';

/** Point-in-time health check data returned by {@link HealthController.snapshot}. */
export interface HealthSnapshot {
  /** ISO-8601 timestamp when the controller was created. */
  readonly startedAt: string;
  /** Seconds since the controller was created. */
  readonly uptimeSec: number;
  /** Whether the server is ready to accept traffic. */
  readonly ready: boolean;
  /** Whether a graceful shutdown is in progress. */
  readonly shuttingDown: boolean;
  /** The signal that initiated shutdown, or `null`. */
  readonly shutdownSignal: string | null;
}

/**
 * Tracks server health state and serves `/healthz` and `/readyz` endpoints.
 */
export class HealthController {
  private readonly startedAtMs = Date.now();
  private readyState = false;
  private shuttingDownState = false;
  private shutdownSignalState: string | null = null;

  /** Mark the server as ready or not ready. */
  setReady(ready: boolean): void {
    this.readyState = ready;
  }

  /** Transition to shutdown state, marking the server as not ready. */
  beginShutdown(signal?: string): void {
    this.readyState = false;
    this.shuttingDownState = true;
    this.shutdownSignalState = signal ?? null;
  }

  /** Return a {@link HealthSnapshot} of the current state. */
  snapshot(): HealthSnapshot {
    return {
      startedAt: new Date(this.startedAtMs).toISOString(),
      uptimeSec: Math.floor((Date.now() - this.startedAtMs) / 1000),
      ready: this.readyState,
      shuttingDown: this.shuttingDownState,
      shutdownSignal: this.shutdownSignalState,
    };
  }

  /** Write a JSON healthz response (always 200). */
  writeHealthz(res: ServerResponse): void {
    const body = this.snapshot();
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json; charset=utf-8');
    res.end(JSON.stringify(body));
  }

  /** Write a JSON readyz response (200 if ready, 503 otherwise). */
  writeReadyz(res: ServerResponse): void {
    const body = this.snapshot();
    res.statusCode = body.ready && !body.shuttingDown ? 200 : 503;
    res.setHeader('content-type', 'application/json; charset=utf-8');
    res.end(JSON.stringify(body));
  }
}

/** Create a {@link HealthController} with an optional initial ready state. */
export function createHealthController(initialReady = false): HealthController {
  const controller = new HealthController();
  controller.setReady(initialReady);
  return controller;
}

/** Options for {@link startHealthServer}. */
export interface HealthServerOptions {
  /** Bind address. Default: `'0.0.0.0'`. */
  host?: string;
  /** HTTP port for health endpoints. Default: 8080. */
  port?: number;
}

/** Handle returned by {@link startHealthServer}. */
export interface HealthServerHandle {
  /** The underlying `node:http` server. */
  readonly server: ReturnType<typeof createServer>;
  /** The bound address. */
  readonly address: AddressInfo;
  /** Stop the health server. */
  close(): Promise<void>;
}

/**
 * Start a plain HTTP health-check server with `/healthz` and `/readyz` routes.
 */
export async function startHealthServer(
  controller: HealthController,
  options?: HealthServerOptions,
): Promise<HealthServerHandle> {
  const server = createServer((req, res) => {
    if (req.url === '/healthz') {
      controller.writeHealthz(res);
      return;
    }
    if (req.url === '/readyz') {
      controller.writeReadyz(res);
      return;
    }
    res.statusCode = 404;
    res.end();
  });
  const host = options?.host ?? '0.0.0.0';
  const port = options?.port ?? 8080;
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, () => {
      server.off('error', reject);
      resolve();
    });
  });
  const address = server.address();
  if (!address || typeof address === 'string') {
    throw new Error('health server failed to expose an address');
  }
  return {
    server,
    address,
    async close() {
      await new Promise<void>((resolve, reject) => {
        server.close((err) => {
          if (err) {
            reject(err);
            return;
          }
          resolve();
        });
      });
    },
  };
}

/** Options for {@link installGracefulShutdown}. */
export interface GracefulShutdownOptions {
  /** OS signals to listen for. Default: `['SIGTERM', 'SIGINT']`. */
  signals?: NodeJS.Signals[];
  /** Maximum time in ms to wait for graceful close. Default: 15 000. */
  timeoutMs?: number;
  /** Optional health controller to transition on shutdown. */
  health?: HealthController;
  /** Callback invoked when a signal is received. */
  onSignal?: (signal: NodeJS.Signals) => void;
  /** Callback invoked if shutdown times out or errors. */
  onError?: (err: Error) => void;
}

/** Handle for removing the installed graceful shutdown listeners. */
export interface GracefulShutdownHandle {
  /** Remove all signal listeners installed by {@link installGracefulShutdown}. */
  close(): void;
}

/**
 * Install OS signal handlers that gracefully close the HTTP/3 server.
 */
export function installGracefulShutdown(
  server: Http3SecureServer,
  options?: GracefulShutdownOptions,
): GracefulShutdownHandle {
  const signals = options?.signals ?? ['SIGTERM', 'SIGINT'];
  const timeoutMs = options?.timeoutMs ?? 15000;
  let shuttingDown = false;

  const shutdown = async (signal: NodeJS.Signals): Promise<void> => {
    if (shuttingDown) return;
    shuttingDown = true;
    options?.onSignal?.(signal);
    options?.health?.beginShutdown(signal);

    const timeout = new Promise<never>((_, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`graceful shutdown timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      timer.unref();
    });

    try {
      await Promise.race([server.close(), timeout]);
    } catch (err: unknown) {
      const error = err instanceof Error ? err : new Error(String(err));
      options?.onError?.(error);
    }
  };

  const onSignal = (signal: NodeJS.Signals): void => {
    void shutdown(signal);
  };

  for (const signal of signals) {
    process.on(signal, onSignal);
  }

  return {
    close(): void {
      for (const signal of signals) {
        process.off(signal, onSignal);
      }
    },
  };
}
