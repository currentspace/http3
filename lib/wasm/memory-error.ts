/** A payload allocation failed before the protocol layer took ownership. */
export class WasmMemoryError extends Error {
  readonly code = 'ERR_HTTP3_WASM_MEMORY_LIMIT';

  constructor(readonly requestedBytes: number) {
    super(`WASM could not allocate ${requestedBytes} payload bytes within its memory limit; use smaller writes and respect backpressure`);
    this.name = 'WasmMemoryError';
  }
}

export function wasmMemoryError(bytes: number): WasmMemoryError {
  return new WasmMemoryError(bytes);
}

/** -5 is the fallible payload-copy result shared by the four WASM ABIs. */
export function checkPayloadAllocation(result: number, bytes: number): number {
  if (result === -5) throw wasmMemoryError(bytes);
  return result;
}
