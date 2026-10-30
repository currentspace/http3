import 'reflect-metadata';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { NestFactory } from '@nestjs/core';
import { Module } from '@nestjs/common';
import { ExpressAdapter } from '@nestjs/platform-express';
import express from 'express';
import cookieParser from 'cookie-parser';
import { connect as connectH2 } from 'node:http2';
import { get } from 'node:https';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { listenNest, connectAsync, createDevTls } from '@currentspace/http3';
import { AppModule } from '../dist/app.module.js';

test('listenNest runs actual Express Nest cookies, SSE and shutdown lifecycle over H1/H2/H3', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'http3-nest-helper-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const { key, cert, ca } = createDevTls({ directory });
  const lifecycle = [];
  class TestModule {}
  Module({ imports: [AppModule], providers: [{ provide: 'lifecycle', useValue: {
    onModuleDestroy() { lifecycle.push('destroy'); },
    beforeApplicationShutdown() { lifecycle.push('before'); },
    onApplicationShutdown() { lifecycle.push('shutdown'); },
  } }] })(TestModule);
  const instance = express(); instance.use(cookieParser());
  const app = await NestFactory.create(TestModule, new ExpressAdapter(instance), { logger: false, abortOnError: false });
  app.setGlobalPrefix('bff');
  const listener = await listenNest(app, { port: 0, tls: { key, cert }, runtimeMode: 'portable' });
  t.after(() => listener.close());
  const origin = `https://localhost:${listener.server.address().port}`;
  for (const protocol of ['h1', 'h2', 'h3']) {
    for (const path of ['/bff/identity', '/bff/events']) {
      let text = '';
      if (protocol === 'h1') {
        text = await new Promise((resolve, reject) => {
          get(origin + path, { ca, agent: false, headers: { cookie: 'demo-session=abc; demo-preference=compact' } }, async response => {
            assert.equal(response.statusCode, 200);
            const chunks = []; for await (const chunk of response) chunks.push(chunk);
            resolve(Buffer.concat(chunks).toString());
          }).once('error', reject);
        });
      } else {
        const session = protocol === 'h2' ? connectH2(origin, { ca }) : await connectAsync(origin, { ca, runtimeMode: 'portable' });
        try {
          const request = session.request({ ':method': 'GET', ':path': path, ':authority': new URL(origin).host,
            cookie: ['demo-session=abc', 'demo-preference=compact'] }, { endStream: true });
          request.once('response', headers => assert.equal(Number(headers[':status']), 200));
          const chunks = []; for await (const chunk of request) chunks.push(chunk);
          text = Buffer.concat(chunks).toString();
        } finally { await session.close(); }
      }
      if (path.endsWith('identity')) {
        const identity = JSON.parse(text);
        assert.equal(identity.session, 'abc'); assert.equal(identity.preference, 'compact');
      } else { assert.match(text, /data:.*"id":0/); assert.match(text, /data:.*"id":2/); }
    }
  }
  await Promise.all([listener.close(), listener.close()]);
  assert.deepEqual(lifecycle, ['destroy', 'before', 'shutdown']);
});
