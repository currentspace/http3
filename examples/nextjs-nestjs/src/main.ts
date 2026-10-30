import 'reflect-metadata';
import { NestFactory } from '@nestjs/core';
import { ExpressAdapter } from '@nestjs/platform-express';
import type { NestExpressApplication } from '@nestjs/platform-express';
import express from 'express';
import cookieParser from 'cookie-parser';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createNodeServerFactory } from '@currentspace/http3/node';
import { createExpressHandler } from '@currentspace/http3/express';
import { AppModule } from './app.module.js';

// Next is CommonJS; require keeps its runtime export consistent with NodeNext.
const next: typeof import('next/dist/server/next.js').default = createRequire(import.meta.url)('next');

const port = Number(process.env.PORT ?? 3443);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid PORT');
const key = readFileSync(process.env.TLS_KEY_FILE ?? '.certs/key.pem');
const cert = readFileSync(process.env.TLS_CERT_FILE ?? '.certs/cert.pem');
const expressApp = express();
expressApp.disable('x-powered-by');
expressApp.use(cookieParser());
const child = express();
child.get('/info', (req, res) => {
  res.json({ framework: 'express', pid: process.pid, httpVersion: req.httpVersion,
    secure: req.secure, originalUrl: req.originalUrl });
});
expressApp.use('/express', child);
const handleExpress = createExpressHandler(expressApp);
const owns = (path: string, prefix: string) => path === prefix || path.startsWith(prefix + '/');

const server = createNodeServerFactory({ key, cert, runtimeMode: 'portable', allowHTTP1: true,
  shutdownTimeoutMs: 2000,
})((req, res) => {
  res.setHeader('x-app-pid', String(process.pid));
  const path = (req.url ?? '/').split('?')[0]!;
  // Dispatch before either framework parses bodies or decorates req/res.
  if (owns(path, '/bff') || owns(path, '/express')) handleExpress(req, res);
  else void web.getRequestHandler()(req, res).catch(error => {
    console.error(error);
    if (!res.headersSent) { res.statusCode = 500; res.end(); }
    else res.destroy(error as Error);
  });
});
const web = next({ dev: process.env.NODE_ENV !== 'production',
  dir: fileURLToPath(new URL('../web/', import.meta.url)), hostname: 'localhost', port,
  httpServer: server as unknown as import('node:http').Server, turbopack: true,
});
const nest = await NestFactory.create<NestExpressApplication>(AppModule, new ExpressAdapter(expressApp), {
  rawBody: true, logger: false, abortOnError: false,
});
nest.setGlobalPrefix('bff');

let stopping: Promise<void> | undefined;
function stop() {
  return stopping ??= (async () => {
    // Keep services alive while accepted requests drain; one bootstrap owns the
    // shared listener. Nest's internally allocated HTTP server is never bound.
    await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
    await Promise.all([nest.close(), web.close()]);
    console.log('Closed both applications');
  })();
}
function fail(error: unknown) {
  console.error(error);
  void stop().then(() => process.exit(1), shutdownError => { console.error(shutdownError); process.exit(1); });
}
server.on('error', fail);
for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.on(signal, () => { void stop().then(() => process.exit(0), fail); });
}
try {
  await nest.init();
  await web.prepare();
  server.listen({ port, host: process.env.HTTP3_HOST ?? '::' }, () => console.log(`Ready: https://localhost:${port} pid=${process.pid}`));
} catch (error) { fail(error); }
