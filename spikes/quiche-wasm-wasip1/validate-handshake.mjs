#!/usr/bin/env node
// Throwaway validation script (Phase 2 bonus, docs/WASM_CLIENT_PLAN.md
// Phase 0 gate): drives the compiled crates/http3-wasm module's h3c_* ABI
// directly over a raw node:dgram socket against this repo's own native H3
// server (in-process, same construction as
// test/support/native-test-helpers.ts's createH3Pair — runtimeMode
// 'portable', self-signed cert, disableRetry), completing a full QUIC+TLS
// handshake and one H3 GET request/response entirely through the wasm
// artifact + the hand-rolled WASI shim (mini-shim.mjs) — no lib/wasm/ TS
// runtime involved (that's Phase 3).
//
// Usage: node spikes/quiche-wasm-wasip1/validate-handshake.mjs
// Prerequisite: `pnpm run build:wasm` (produces dist/wasm/http3_client.wasm)
// and `pnpm run build:test` (compiles test/support/*.ts this script reuses).
import { createRequire } from 'node:module';
import { existsSync, readFileSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import dgram from 'node:dgram';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { makePreview1 } from './mini-shim.mjs';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..', '..');

const EVENT_HEADERS = 3;
const EVENT_HANDSHAKE_COMPLETE = 11;

function log(msg) {
  console.log(`validate-handshake: ${msg}`);
}

async function main() {
  const helpers = require(join(repoRoot, 'dist-test/test/support/native-test-helpers.js'));
  const { loadBinding, generateTestCerts, createEventCollector } = helpers;

  const wasmPath = join(repoRoot, 'dist/wasm/http3_client.wasm');
  if (!existsSync(wasmPath)) {
    throw new Error(`${wasmPath} not found — run \`pnpm run build:wasm\` first`);
  }

  // --- 1. Start the native H3 server (same shape as createH3Pair, server-only) ---
  const binding = loadBinding();
  const certs = generateTestCerts();
  const serverEvents = createEventCollector();
  const server = new binding.NativeWorkerServer(
    { key: certs.key, cert: certs.cert, disableRetry: true, runtimeMode: 'portable' },
    serverEvents.callback,
  );
  const addr = server.listen(0, '127.0.0.1');
  log(`native H3 server listening on 127.0.0.1:${addr.port}`);

  // --- 2. Load the wasm module + WASI shim ---
  const bytes = readFileSync(wasmPath);
  const mod = new WebAssembly.Module(bytes);
  const { wasi_snapshot_preview1, setMemory } = makePreview1();
  const inst = new WebAssembly.Instance(mod, { wasi_snapshot_preview1 });
  setMemory(inst.exports.memory);
  const wasm = inst.exports;
  log(`instantiated ${wasmPath} (${(bytes.length / 1024).toFixed(0)} KiB)`);

  // --- 3. Real UDP socket for the wasm client side ---
  const clientSock = dgram.createSocket('udp4');
  await new Promise((resolve) => clientSock.bind(0, '127.0.0.1', resolve));
  const localPort = clientSock.address().port;

  // --- helpers respecting the crate's buffer-lifetime contract: copy
  // everything out synchronously, before any other call on the handle ---
  function wasmWriteString(str) {
    const strBytes = Buffer.from(str, 'utf8');
    const ptr = wasm.wasm_alloc(strBytes.length);
    new Uint8Array(wasm.memory.buffer).set(strBytes, ptr);
    return { ptr, len: strBytes.length };
  }

  const outPtrPtr = wasm.wasm_alloc(4); // reused scratch cell for out_ptr_ptr

  function readLastError(handle) {
    const errPtr = wasm.wasm_alloc(512);
    const n = Number(wasm.h3c_last_error(handle, errPtr, 512));
    const msg = n > 0 ? Buffer.from(wasm.memory.buffer, errPtr, n).toString('utf8') : '(no message)';
    wasm.wasm_free(errPtr, 512);
    return msg;
  }

  function drainEvents(handle) {
    const len = Number(wasm.h3c_drain_events(handle, outPtrPtr));
    if (len <= 0) return [];
    const ptr = new DataView(wasm.memory.buffer).getUint32(outPtrPtr, true);
    const json = Buffer.from(wasm.memory.buffer, ptr, len).toString('utf8');
    const events = JSON.parse(json);
    // Buffer-lifetime rule: copy every dataOff/dataLen payload out NOW —
    // `data_scratch` is overwritten on the next call on this handle.
    for (const e of events) {
      if (typeof e.dataOff === 'number' && typeof e.dataLen === 'number' && e.dataLen > 0) {
        const view = Buffer.from(wasm.memory.buffer, e.dataOff, e.dataLen);
        e.dataBuf = Buffer.from(view); // real copy (Buffer.from(view) copies; view itself doesn't)
      }
    }
    return events;
  }

  function pumpSend(handle) {
    for (;;) {
      const n = Number(wasm.h3c_next_send(handle));
      if (n <= 0) break;
      const txPtr = wasm.h3c_tx_buffer(handle);
      const view = Buffer.from(wasm.memory.buffer, txPtr, n);
      const payload = Buffer.from(view); // copy — tx_buffer is reused by the next h3c_next_send
      clientSock.send(payload, addr.port, '127.0.0.1');
    }
  }

  // --- 4. h3c_new ---
  const scidHex = randomBytes(20).toString('hex');
  const optsJson = JSON.stringify({
    serverAddr: `127.0.0.1:${addr.port}`,
    serverName: 'localhost',
    localAddr: `127.0.0.1:${localPort}`,
    scidHex,
    rejectUnauthorized: false,
    disablePacing: true,
  });
  const { ptr: optsPtr, len: optsLen } = wasmWriteString(optsJson);
  const handle = wasm.h3c_new(optsPtr, optsLen);
  wasm.wasm_free(optsPtr, optsLen);
  if (handle === 0) {
    throw new Error(`h3c_new failed: ${readLastError(0)}`);
  }
  log(`h3c_new -> handle ${handle}`);

  const allEvents = [];
  clientSock.on('message', (msg) => {
    const rxPtr = wasm.h3c_rx_buffer(handle);
    new Uint8Array(wasm.memory.buffer).set(msg, rxPtr);
    wasm.h3c_recv(handle, msg.length);
    allEvents.push(...drainEvents(handle));
    pumpSend(handle);
  });

  pumpSend(handle); // flush the Initial ClientHello

  async function waitUntil(predicate, label, timeoutMs = 5000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return;
      await new Promise((r) => setTimeout(r, 15));
    }
    throw new Error(`timed out waiting for: ${label}`);
  }

  await waitUntil(
    () => allEvents.some((e) => e.eventType === EVENT_HANDSHAKE_COMPLETE),
    'EVENT_HANDSHAKE_COMPLETE on the wasm client',
  );
  log('QUIC + TLS handshake completed through the wasm module');

  // --- 5. GET request ---
  const headersJson = JSON.stringify([
    { name: ':method', value: 'GET' },
    { name: ':path', value: '/' },
    { name: ':authority', value: 'localhost' },
    { name: ':scheme', value: 'https' },
  ]);
  const { ptr: hPtr, len: hLen } = wasmWriteString(headersJson);
  const streamId = Number(wasm.h3c_send_request(handle, hPtr, hLen, 1));
  wasm.wasm_free(hPtr, hLen);
  if (streamId < 0) {
    throw new Error(`h3c_send_request failed (${streamId}): ${readLastError(handle)}`);
  }
  log(`sent GET / on stream ${streamId}`);
  pumpSend(handle);

  // --- 6. Server responds once it sees HEADERS ---
  const headersEvent = await serverEvents.waitForEvent(EVENT_HEADERS);
  server.sendResponseHeaders(
    headersEvent.connHandle,
    headersEvent.streamId,
    [
      { name: ':status', value: '200' },
      { name: 'x-validated-by', value: 'http3-wasm-bonus' },
    ],
    false,
  );
  server.streamSend(
    headersEvent.connHandle,
    headersEvent.streamId,
    Buffer.from('hello from the native H3 server, received by the wasm client'),
    true,
  );
  log('native server sent response headers + body');

  // --- 7. Wait for the wasm client to observe the response ---
  let respHeaders = null;
  const bodyChunks = [];
  let processedCount = 0;
  await waitUntil(() => {
    // Only scan events newly appended since the last check — the predicate
    // is polled repeatedly, and `allEvents` only grows, so re-scanning from
    // the start on every poll would double-count body chunks.
    for (; processedCount < allEvents.length; processedCount++) {
      const e = allEvents[processedCount];
      if (e.streamId !== streamId) continue;
      if (e.eventType === EVENT_HEADERS && e.headers) respHeaders = e.headers;
      if (e.dataBuf) bodyChunks.push(e.dataBuf);
    }
    return respHeaders && bodyChunks.length > 0;
  }, 'response headers + body on the wasm client');

  const body = Buffer.concat(bodyChunks).toString('utf8');
  console.log('\n=== VALIDATED: full handshake + H3 GET through the wasm module ===');
  console.log('response headers:', JSON.stringify(respHeaders));
  console.log('response body:', JSON.stringify(body));
  console.log('====================================================================\n');

  // --- cleanup ---
  wasm.h3c_close(handle, 0, 0, 0);
  pumpSend(handle);
  wasm.h3c_free(handle);
  clientSock.close();
  try {
    server.requestShutdown();
    server.joinWorker();
  } catch {
    /* already shut down */
  }
}

main()
  .then(() => {
    process.exit(0);
  })
  .catch((err) => {
    console.error('validate-handshake: FAILED:', err);
    process.exit(1);
  });
