import { once } from 'node:events';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { createExpressHandler } from './express-adapter.js';
import { createNodeServerFactory } from './node-server.js';
import type { Http3NodeServer, NodeServerOptions } from './node-server.js';
import type { ShutdownController } from './shutdown.js';

/** Structural contract: no runtime or peer dependency on Nest. ExpressAdapter only. */
export interface NestExpressLikeApplication {
  init(): Promise<unknown>;
  close(signal?: string): Promise<void>;
  getHttpAdapter(): { getType(): string; getInstance(): unknown };
}
export interface ListenNestOptions extends Omit<NodeServerOptions, 'key' | 'cert'> {
  port: number;
  host?: string;
  tls: Pick<NodeServerOptions, 'key' | 'cert'>;
  /** Run app.close(), including Nest lifecycle hooks, after draining. Default: true. */
  closeApp?: boolean;
  /** Own SIGINT/SIGTERM and exit after cleanup. Do not also enable Nest signal handlers. */
  closeOnSignals?: boolean;
}
export interface NestHttp3Listener {
  server: Http3NodeServer;
  close(signal?: NodeJS.Signals): Promise<void>;
  disposeSignals(): void;
}

/** Initialize an Express-backed Nest app and bind one H3/H2/H1 listener. */
export async function listenNest(app: NestExpressLikeApplication, options: ListenNestOptions): Promise<NestHttp3Listener> {
  const adapter = app.getHttpAdapter();
  if (adapter.getType() !== 'express') throw new TypeError('listenNest requires Nest ExpressAdapter; use serverFactory with FastifyAdapter');
  const instance = adapter.getInstance();
  if (typeof instance !== 'function') throw new TypeError('Nest Express adapter did not return a request handler');
  const { port, host, tls, closeApp = true, closeOnSignals = false, ...serverOptions } = options;
  const server = createNodeServerFactory({ allowHTTP1: true, ...serverOptions, ...tls })(
    createExpressHandler(instance as (req: IncomingMessage, res: ServerResponse) => void),
  );
  let cleanup: Promise<void> | undefined;
  const close = async (signal?: NodeJS.Signals): Promise<void> => cleanup ??= (async () => {
    try { await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())); }
    finally { if (closeApp) await app.close(signal); }
  })();
  try {
    await app.init();
    const listening = once(server, 'listening');
    server.listen({ port, host });
    await listening;
  } catch (error) {
    await close().catch(() => {});
    throw error;
  }
  const controller: ShutdownController | undefined = closeOnSignals ? server.closeOnSignals({
    onClose: async signal => { if (closeApp) await app.close(signal); },
  }) : undefined;
  return { server, close: controller?.close ?? close, disposeSignals: controller?.dispose ?? (() => {}) };
}
