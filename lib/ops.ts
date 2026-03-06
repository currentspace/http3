import { createServer, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { Http3SecureServer } from './server.js';

export interface HealthSnapshot {
  readonly startedAt: string;
  readonly uptimeSec: number;
  readonly ready: boolean;
  readonly shuttingDown: boolean;
  readonly shutdownSignal: string | null;
}

export class HealthController {
  private readonly startedAtMs = Date.now();
  private readyState = false;
  private shuttingDownState = false;
  private shutdownSignalState: string | null = null;

  setReady(ready: boolean): void {
    this.readyState = ready;
  }

  beginShutdown(signal?: string): void {
    this.readyState = false;
    this.shuttingDownState = true;
    this.shutdownSignalState = signal ?? null;
  }

  snapshot(): HealthSnapshot {
    return {
      startedAt: new Date(this.startedAtMs).toISOString(),
      uptimeSec: Math.floor((Date.now() - this.startedAtMs) / 1000),
      ready: this.readyState,
      shuttingDown: this.shuttingDownState,
      shutdownSignal: this.shutdownSignalState,
    };
  }

  writeHealthz(res: ServerResponse): void {
    const body = this.snapshot();
    res.statusCode = 200;
    res.setHeader('content-type', 'application/json; charset=utf-8');
    res.end(JSON.stringify(body));
  }

  writeReadyz(res: ServerResponse): void {
    const body = this.snapshot();
    res.statusCode = body.ready && !body.shuttingDown ? 200 : 503;
    res.setHeader('content-type', 'application/json; charset=utf-8');
    res.end(JSON.stringify(body));
  }
}

export function createHealthController(initialReady = false): HealthController {
  const controller = new HealthController();
  controller.setReady(initialReady);
  return controller;
}

export interface HealthServerOptions {
  host?: string;
  port?: number;
}

export interface HealthServerHandle {
  readonly server: ReturnType<typeof createServer>;
  readonly address: AddressInfo;
  close(): Promise<void>;
}

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

export interface GracefulShutdownOptions {
  signals?: NodeJS.Signals[];
  timeoutMs?: number;
  health?: HealthController;
  onSignal?: (signal: NodeJS.Signals) => void;
  onError?: (err: Error) => void;
}

export interface GracefulShutdownHandle {
  close(): void;
}

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
