/**
 * JS runtime entrypoint for Docker/ECS-style runs.
 * Uses dist build output and the local example Hono dependency.
 */

import { Hono } from 'hono';
import http3Pkg from '../../dist/index.js';
import fetchPkg from '../../dist/fetch-adapter.js';

const {
  installGracefulShutdown,
  loadTlsOptionsFromAwsEnv,
  startHealthServer,
  createHealthController,
} = http3Pkg;
const { serveFetch, createSseFetchResponse } = fetchPkg;

const app = new Hono();
const startedAtMs = Date.now();
const state = {
  requestCount: 0,
  activeSseClients: 0,
  nextSessionId: 1,
  sessions: new Map(),
};

function sleepAbortable(ms, signal) {
  return new Promise((resolve) => {
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    if (signal?.aborted) {
      clearTimeout(timer);
      resolve();
      return;
    }
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

function loadSessionTicketKeysFromEnv() {
  const raw = process.env.HTTP3_SESSION_TICKET_KEYS_B64;
  if (!raw) return undefined;

  const key = Buffer.from(raw, 'base64');
  if (key.length !== 48) {
    throw new Error('HTTP3_SESSION_TICKET_KEYS_B64 must decode to exactly 48 bytes');
  }

  return key;
}

function safeMetrics(metrics) {
  if (!metrics) return null;
  return {
    packetsIn: Number(metrics.packetsIn ?? 0),
    packetsOut: Number(metrics.packetsOut ?? 0),
    bytesIn: Number(metrics.bytesIn ?? 0),
    bytesOut: Number(metrics.bytesOut ?? 0),
    handshakeTimeMs: Number(metrics.handshakeTimeMs ?? 0),
    rttMs: Number(metrics.rttMs ?? 0),
    cwnd: Number(metrics.cwnd ?? 0),
  };
}

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

function getHelloPayload() {
  return {
    ok: true,
    message: 'hello from hono on HTTP/3 capable server',
  };
}

function getTimePayload() {
  return { now: new Date().toISOString() };
}

function getRandomPayload() {
  return { random: Math.random() };
}

function buildStatsSnapshot() {
  const sessions = Array.from(state.sessions.values()).map((session) => ({
    id: session.id,
    alpn: session.alpn,
    remoteAddress: session.remoteAddress,
    remotePort: session.remotePort,
    serverName: session.serverName,
    openedAt: session.openedAt,
    lastUpdatedAt: session.lastUpdatedAt,
    metrics: safeMetrics(session.metrics),
  }));

  const totals = sessions.reduce((acc, session) => {
    if (!session.metrics) return acc;
    acc.bytesIn += session.metrics.bytesIn;
    acc.bytesOut += session.metrics.bytesOut;
    acc.packetsIn += session.metrics.packetsIn;
    acc.packetsOut += session.metrics.packetsOut;
    return acc;
  }, { bytesIn: 0, bytesOut: 0, packetsIn: 0, packetsOut: 0 });

  const activeH3Sessions = sessions.filter((session) => session.alpn === 'h3').length;
  const activeH2Sessions = sessions.filter((session) => session.alpn === 'h2').length;

  return {
    now: new Date().toISOString(),
    uptimeSec: Math.floor((Date.now() - startedAtMs) / 1000),
    requestsTotal: state.requestCount,
    activeSseClients: state.activeSseClients,
    activeSessions: sessions.length,
    activeH3Sessions,
    activeH2Sessions,
    totals,
    sessions,
  };
}

function renderJsonCard(cardId, title, route, payload) {
  return `<article id="${escapeHtml(cardId)}" class="min-w-0 rounded-xl border border-slate-800 bg-slate-950/80 p-4">
    <div class="mb-2 flex items-center justify-between">
      <h3 class="text-sm font-medium text-slate-100">${escapeHtml(title)}</h3>
      <span class="rounded-full border border-cyan-400/30 bg-cyan-400/10 px-2 py-0.5 text-[11px] text-cyan-300">${escapeHtml(route)}</span>
    </div>
    <pre class="overflow-auto whitespace-pre-wrap break-words text-xs leading-relaxed text-slate-200">${escapeHtml(JSON.stringify(payload, null, 2))}</pre>
  </article>`;
}

function renderApiResultsFragment(snapshot = buildStatsSnapshot()) {
  return `<section
    id="api-results"
    class="grid grid-cols-1 gap-4 xl:grid-cols-2"
  >
    ${renderJsonCard('api-card-hello', 'Hello', '/api/hello', getHelloPayload())}
    ${renderJsonCard('api-card-time', 'Time', '/api/time', getTimePayload())}
    ${renderJsonCard('api-card-random', 'Random', '/api/random', getRandomPayload())}
    ${renderJsonCard('api-card-summary', 'Stats Summary', '/api/stats', {
      activeSessions: snapshot.activeSessions,
      activeH3Sessions: snapshot.activeH3Sessions,
      activeH2Sessions: snapshot.activeH2Sessions,
      requestsTotal: snapshot.requestsTotal,
      uptimeSec: snapshot.uptimeSec,
    })}
  </section>`;
}

function renderStatsInner(snapshot) {
  return `<div data-active-h3="${snapshot.activeH3Sessions}" class="space-y-4">
    <div class="grid gap-3 sm:grid-cols-2">
      <div class="rounded-xl border border-slate-800 bg-slate-950/80 p-3">
        <p class="text-xs uppercase tracking-wide text-slate-400">Sessions</p>
        <p class="mt-1 text-lg font-semibold text-slate-100">${snapshot.activeSessions}</p>
      </div>
      <div class="rounded-xl border border-slate-800 bg-slate-950/80 p-3">
        <p class="text-xs uppercase tracking-wide text-slate-400">H3 / H2</p>
        <p class="mt-1 text-lg font-semibold text-cyan-300">${snapshot.activeH3Sessions} / ${snapshot.activeH2Sessions}</p>
      </div>
    </div>
    <pre class="max-h-[24rem] overflow-auto whitespace-pre-wrap break-words rounded-xl border border-slate-800 bg-slate-950 p-4 text-xs leading-relaxed text-emerald-200">${escapeHtml(JSON.stringify(snapshot, null, 2))}</pre>
  </div>`;
}

function renderHomePage() {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>HTTP/3 Demo</title>
    <script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4"></script>
    <script src="https://unpkg.com/idiomorph@0.3.0/dist/idiomorph.min.js"></script>
  </head>
  <body class="min-h-screen bg-slate-950 text-slate-100">
    <main class="mx-auto max-w-6xl px-6 py-10">
      <header class="mb-8 rounded-2xl border border-slate-800 bg-slate-900/70 p-6 shadow-2xl shadow-slate-950/60 backdrop-blur">
        <div class="flex flex-col gap-4 md:flex-row md:items-center md:justify-between">
          <div>
            <p class="mb-2 inline-flex rounded-full border border-cyan-500/30 bg-cyan-500/10 px-3 py-1 text-xs font-medium tracking-wide text-cyan-300">
              HTTP/3 + QUIC Demo
            </p>
            <h1 class="text-3xl font-semibold tracking-tight md:text-4xl">Live Transport Dashboard</h1>
            <p class="mt-3 max-w-3xl text-sm text-slate-300">
              Calls several routes and keeps a live EventSource connection open. Stats refresh every 5 seconds.
            </p>
          </div>
          <div class="flex items-center gap-3">
            <button id="reload-page" class="rounded-xl bg-cyan-500 px-4 py-2 text-sm font-medium text-slate-950 transition hover:bg-cyan-400 active:scale-[0.98]">
              Reload page
            </button>
            <span
              id="sse-status"
              class="rounded-full border border-amber-400/40 bg-amber-400/10 px-3 py-1 text-xs font-medium text-amber-300"
            >
              connecting...
            </span>
          </div>
        </div>
      </header>

      <section class="grid gap-6 lg:grid-cols-2">
        <article class="rounded-2xl border border-slate-800 bg-slate-900/70 p-5 shadow-xl shadow-slate-950/50">
          <h2 class="mb-3 text-lg font-medium text-slate-100">API Results (SSE)</h2>
          <p class="mb-4 text-sm text-slate-400">Server-pushed HTML fragment updates via SSE event <code>api</code>.</p>
          ${renderApiResultsFragment()}
        </article>

        <article class="rounded-2xl border border-slate-800 bg-slate-900/70 p-5 shadow-xl shadow-slate-950/50">
          <h2 class="mb-3 text-lg font-medium text-slate-100">SSE Live Stats</h2>
          <p class="mb-4 text-sm text-slate-400">SSE event <code>stats</code> refreshes the panel every 5s.</p>
          <div id="stats-live">
            ${renderStatsInner(buildStatsSnapshot())}
          </div>
        </article>
      </section>
    </main>

    <script>
      const statusEl = document.getElementById('sse-status');
      const reloadButton = document.getElementById('reload-page');
      const parser = new DOMParser();
      let eventSource = null;
      const setStatus = (text, className) => {
        statusEl.textContent = text;
        statusEl.className = className;
      };
      reloadButton?.addEventListener('click', () => window.location.reload());

      const morphFromHtml = (targetId, html, mode) => {
        const target = document.getElementById(targetId);
        if (!target) return;
        const doc = parser.parseFromString(html, 'text/html');
        if (mode === 'outer') {
          const incoming = doc.body.firstElementChild;
          if (!incoming) return;
          if (window.Idiomorph && typeof window.Idiomorph.morph === 'function') {
            window.Idiomorph.morph(target, incoming);
          } else {
            target.outerHTML = html;
          }
          return;
        }
        target.innerHTML = html;
      };

      const connectSse = () => {
        if (eventSource && eventSource.readyState !== EventSource.CLOSED) return;
        eventSource = new EventSource('/events/stats');

        eventSource.addEventListener('open', () => {
          setStatus(
            'SSE connected',
            'rounded-full border border-emerald-400/40 bg-emerald-400/10 px-3 py-1 text-xs font-medium text-emerald-300'
          );
        });
        eventSource.addEventListener('error', () => {
          setStatus(
            'SSE reconnecting...',
            'rounded-full border border-amber-400/40 bg-amber-400/10 px-3 py-1 text-xs font-medium text-amber-300'
          );
        });
        eventSource.addEventListener('api', (event) => {
          morphFromHtml('api-results', event.data, 'outer');
        });
        eventSource.addEventListener('stats', (event) => {
          morphFromHtml('stats-live', event.data, 'inner');
          const marker = document.querySelector('#stats-live [data-active-h3]');
          if (marker && Number(marker.getAttribute('data-active-h3')) > 0) {
            setStatus(
              'SSE connected (h3 sessions observed)',
              'rounded-full border border-cyan-400/40 bg-cyan-400/10 px-3 py-1 text-xs font-medium text-cyan-300'
            );
          }
        });
      };

      const closeSse = () => {
        if (!eventSource) return;
        eventSource.close();
        eventSource = null;
      };

      window.addEventListener('pagehide', closeSse);
      window.addEventListener('beforeunload', closeSse);
      connectSse();
    </script>
  </body>
</html>`;
}

app.use('*', async (c, next) => {
  state.requestCount += 1;
  await next();
  // Advertise QUIC so browsers can upgrade from initial h2 to h3.
  c.res.headers.set('alt-svc', 'h3=":443"; ma=86400');
});

app.get('/', (c) => c.html(renderHomePage()));
app.get('/favicon.ico', (c) => c.body(null, 204));

app.get('/api/hello', (c) => c.json(getHelloPayload()));
app.get('/api/time', (c) => c.json(getTimePayload()));
app.get('/api/random', (c) => c.json(getRandomPayload()));
app.get('/api/stats', (c) => c.json(buildStatsSnapshot()));

app.get('/events/stats', (c) => {
  const signal = c.req.raw.signal;
  async function* events() {
    let seq = 0;
    state.activeSseClients += 1;
    try {
      while (!signal.aborted) {
        seq += 1;
        const snapshot = buildStatsSnapshot();
        yield {
          id: `${String(seq)}-api`,
          event: 'api',
          data: renderApiResultsFragment(snapshot),
        };
        yield {
          id: `${String(seq)}-stats`,
          event: 'stats',
          data: renderStatsInner(snapshot),
        };
        await sleepAbortable(5000, signal);
      }
    } finally {
      state.activeSseClients = Math.max(0, state.activeSseClients - 1);
    }
  }
  return createSseFetchResponse(events());
});

async function main() {
  const tls = loadTlsOptionsFromAwsEnv();
  const sessionTicketKeys = loadSessionTicketKeysFromEnv();
  const port = Number.parseInt(process.env.PORT ?? '443', 10);
  const host = process.env.HOST ?? '::';
  const healthPort = Number.parseInt(process.env.HEALTH_PORT ?? '8080', 10);
  const healthHost = process.env.HEALTH_HOST ?? '::';
  const quicLb = process.env.HTTP3_QUIC_LB === '1';
  const serverId = process.env.HTTP3_SERVER_ID;

  const health = createHealthController(false);
  const healthServer = await startHealthServer(health, {
    host: healthHost,
    port: healthPort,
  });
  console.log(`health server listening on http://${healthHost}:${healthServer.address.port}`);

  const server = serveFetch({
    port,
    host,
    ...tls,
    sessionTicketKeys,
    // Keep h3/h2 preferred, but allow h1 fallback for broader browser compatibility.
    allowHTTP1: true,
    disableRetry: process.env.HTTP3_DISABLE_RETRY === '1',
    quicLb,
    serverId,
    fetch: app.fetch,
  });

  server.on('session', (session) => {
    const sessionId = state.nextSessionId++;
    const snapshot = {
      id: sessionId,
      alpn: session.alpnProtocol ?? 'unknown',
      remoteAddress: session.remoteAddress ?? '',
      remotePort: session.remotePort ?? 0,
      serverName: session.serverName ?? '',
      openedAt: new Date().toISOString(),
      lastUpdatedAt: new Date().toISOString(),
      metrics: typeof session.getMetrics === 'function' ? session.getMetrics() : null,
    };
    state.sessions.set(sessionId, snapshot);
    session.on('metrics', (metrics) => {
      snapshot.metrics = metrics;
      snapshot.lastUpdatedAt = new Date().toISOString();
    });
    session.once('close', () => {
      state.sessions.delete(sessionId);
    });
  });

  server.on('listening', () => {
    health.setReady(true);
    const address = server.address();
    if (sessionTicketKeys) {
      console.log('shared session ticket keys enabled from HTTP3_SESSION_TICKET_KEYS_B64');
    }
    console.log(`http3 server listening on https://${address?.address ?? host}:${address?.port ?? port}`);
  });
  server.on('error', (err) => {
    console.error('server error', err);
  });

  const shutdown = installGracefulShutdown(server, {
    health,
    timeoutMs: Number.parseInt(process.env.GRACEFUL_TIMEOUT_MS ?? '15000', 10),
    onSignal(signal) {
      console.log(`received ${signal}, draining traffic`);
    },
    onError(err) {
      console.error('graceful shutdown error', err);
    },
  });

  process.on('exit', () => {
    void shutdown.close();
    void healthServer.close();
  });
}

void main().catch((err) => {
  console.error(err);
  process.exit(1);
});
