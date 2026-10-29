import 'reflect-metadata';
import { NestFactory } from '@nestjs/core';
import { FastifyAdapter } from '@nestjs/platform-fastify';
import type { NestFastifyApplication } from '@nestjs/platform-fastify';
import type { FastifyServerOptions } from 'fastify';
import { createNodeServerFactory } from '@currentspace/http3/node';
import type { NodeServerOptions } from '@currentspace/http3/node';
import type { Server } from 'node:https';
import { AppModule } from './app.module.js';

export async function createApp(tls: Pick<NodeServerOptions, 'key' | 'cert'>) {
  const factory = createNodeServerFactory({
    ...tls,
    runtimeMode: 'portable',
    allowHTTP1: true,
    shutdownTimeoutMs: 5000,
  });
  const adapter = new FastifyAdapter<Server>({
    https: tls,
    bodyLimit: 1024 * 1024,
    // Fastify only types built-in Node servers. The experimental facade supports
    // the subset exercised here; it is not a complete node:https.Server.
    serverFactory: factory as unknown as FastifyServerOptions<Server>['serverFactory'],
  });
  const app = await NestFactory.create<NestFastifyApplication>(AppModule, adapter, {
    rawBody: true,
    logger: false,
    abortOnError: false,
  });
  app.enableCors({ origin: 'https://example.com' });
  app.use((_req: unknown, res: { setHeader(name: string, value: string): void }, next: () => void) => {
    res.setHeader('x-example-middleware', 'nestjs');
    next();
  });
  return app;
}
