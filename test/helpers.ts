/**
 * Shared test helpers used across test files.
 */

export async function waitFor(condition: () => boolean, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => { setTimeout(resolve, 10); });
  }
}

export async function readBody(stream: NodeJS.EventEmitter): Promise<string> {
  return await new Promise<string>((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
    stream.on('error', reject);
    stream.on('end', () => { resolve(Buffer.concat(chunks).toString()); });
  });
}
