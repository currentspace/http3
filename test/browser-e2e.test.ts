import { before, after, describe, it } from 'node:test';
import assert from 'node:assert';
import { serveFetch, createSseFetchResponse } from '../lib/fetch-adapter.js';
import { generateTestCerts } from './generate-certs.js';

const ENABLED = process.env.HTTP3_BROWSER_E2E === '1';

describe('browser e2e interop', { skip: !ENABLED && 'set HTTP3_BROWSER_E2E=1 to enable browser tests' }, () => {
  let port = 0;
  let closeServer: (() => Promise<void>) | null = null;

  before(async () => {
    const certs = generateTestCerts();
    const server = serveFetch({
      port: 0,
      host: '127.0.0.1',
      key: certs.key,
      cert: certs.cert,
      disableRetry: true,
      allowHTTP1: true,
      fetch: async (req: Request): Promise<Response> => {
        const url = new URL(req.url);
        if (url.pathname === '/api/ping') {
          return new Response(JSON.stringify({ ok: true }), {
            headers: { 'content-type': 'application/json' },
          });
        }
        if (url.pathname === '/events') {
          async function* events(): AsyncGenerator<{ event: string; data: string }> {
            yield { event: 'ready', data: 'sse-ok' };
          }
          return createSseFetchResponse(events());
        }
        return new Response(
          `<!doctype html>
          <html>
            <body>
              <div id="fetch-status">pending</div>
              <div id="sse-status">pending</div>
              <script>
                fetch('/api/ping')
                  .then((r) => r.json())
                  .then((payload) => {
                    document.getElementById('fetch-status').textContent = payload.ok ? 'fetch-ok' : 'fetch-failed';
                  })
                  .catch(() => {
                    document.getElementById('fetch-status').textContent = 'fetch-failed';
                  });
                const es = new EventSource('/events');
                es.addEventListener('ready', (event) => {
                  document.getElementById('sse-status').textContent = event.data;
                  es.close();
                });
              </script>
            </body>
          </html>`,
          { headers: { 'content-type': 'text/html; charset=utf-8', 'alt-svc': 'h3=":443"; ma=300' } },
        );
      },
    });
    await new Promise<void>((resolve) => {
      server.on('listening', () => {
        const addr = server.address();
        assert.ok(addr);
        port = addr.port;
        resolve();
      });
    });
    closeServer = async () => server.close();
  });

  after(async () => {
    if (closeServer) await closeServer();
  });

  async function runBrowserInterop(kind: 'chromium' | 'firefox'): Promise<void> {
    const playwright = await import('playwright');
    const launcher = kind === 'chromium' ? playwright.chromium : playwright.firefox;
    const browser = await launcher.launch({ headless: true });
    try {
      const context = await browser.newContext({ ignoreHTTPSErrors: true });
      const page = await context.newPage();
      await page.goto(`https://127.0.0.1:${String(port)}/`, { waitUntil: 'domcontentloaded' });
      await page.waitForFunction(() => document.getElementById('fetch-status')?.textContent === 'fetch-ok');
      await page.waitForFunction(() => document.getElementById('sse-status')?.textContent === 'sse-ok');
      const protocol = await page.evaluate(() => {
        const nav = performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming | undefined;
        return nav?.nextHopProtocol ?? '';
      });
      assert.strictEqual(typeof protocol, 'string');
      await context.close();
    } finally {
      await browser.close();
    }
  }

  it('works in Chromium', async () => {
    await runBrowserInterop('chromium');
  });

  it('works in Firefox', async () => {
    await runBrowserInterop('firefox');
  });
});

