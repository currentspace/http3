import assert from 'node:assert';
import { describe, it } from 'node:test';
import {
  connect,
  connectQuic,
  createQuicServer,
  createSecureServer,
} from '../../lib/index.js';
import { binding } from '../../lib/event-loop.js';

const MIN_SUPPORTED_NODE_MAJOR = 24;
const MIN_NAPI_VERSION = 8;

function runtimeLabel(): string {
  return `Node ${process.versions.node} (N-API ${process.versions.napi ?? 'unavailable'})`;
}

describe('Node runtime support policy', () => {
  it('loads the public API and native binding on supported Node runtimes', () => {
    const nodeMajor = Number(process.versions.node.split('.')[0]);
    assert.ok(
      Number.isInteger(nodeMajor) && nodeMajor >= MIN_SUPPORTED_NODE_MAJOR,
      `unsupported runtime: ${runtimeLabel()}`,
    );

    const napiVersion = Number(process.versions.napi ?? 0);
    assert.ok(
      Number.isInteger(napiVersion) && napiVersion >= MIN_NAPI_VERSION,
      `unsupported N-API version: ${runtimeLabel()}`,
    );

    assert.strictEqual(typeof createSecureServer, 'function', `createSecureServer missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof connect, 'function', `connect missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof createQuicServer, 'function', `createQuicServer missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof connectQuic, 'function', `connectQuic missing on ${runtimeLabel()}`);

    assert.strictEqual(typeof binding.NativeWorkerServer, 'function', `NativeWorkerServer missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof binding.NativeWorkerClient, 'function', `NativeWorkerClient missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof binding.NativeQuicServer, 'function', `NativeQuicServer missing on ${runtimeLabel()}`);
    assert.strictEqual(typeof binding.NativeQuicClient, 'function', `NativeQuicClient missing on ${runtimeLabel()}`);
  });
});
