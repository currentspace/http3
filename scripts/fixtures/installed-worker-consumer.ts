import {
  WasmMemoryError, ERR_HTTP3_WASM_MEMORY_LIMIT, createUnavailableDatagramTransport,
  loadHttp3WasmCore, WasmH3ClientEventLoop, WasmQuicClientEventLoop,
  type WasmH3ClientEventLoopOptions, type WasmQuicClientEventLoopOptions,
  type CommonWasmClientOptions, type DatagramTransport, type WasmEvent, type ShimOptions,
} from '@currentspace/http3/wasm';

const transport: DatagramTransport = createUnavailableDatagramTransport();
void transport; void loadHttp3WasmCore; void WasmH3ClientEventLoop; void WasmQuicClientEventLoop;
export function requestedBytes(error: unknown): number | undefined {
  if (error instanceof WasmMemoryError) {
    const code: typeof ERR_HTTP3_WASM_MEMORY_LIMIT = error.code;
    void code;
    return error.requestedBytes;
  }
  return undefined;
}
// @ts-expect-error The Worker entry must not expose Node's UDP adapter.
import { connectNodeUdp } from '@currentspace/http3/wasm';
void connectNodeUdp;
export type WorkerTypes = [WasmH3ClientEventLoopOptions, WasmQuicClientEventLoopOptions,
  CommonWasmClientOptions, WasmEvent, ShimOptions];
