/**
 * Buffer-then-echo helper for QUIC streams.
 *
 * `stream.pipe(stream)` deadlocks bidirectional QUIC streams under load
 * because Node's pipe() pauses the readable side when the writable side
 * stalls on flow control — but the peer needs us to keep reading for the
 * connection to make progress.
 *
 * This helper buffers all incoming data and echoes it back on FIN,
 * avoiding the bidirectional backpressure deadlock entirely.
 */
import type { QuicStream } from '../../lib/quic-stream.js';

export function echoStream(stream: QuicStream): void {
  const chunks: Buffer[] = [];
  stream.on('data', (chunk: Buffer) => { chunks.push(chunk); });
  stream.on('end', () => { stream.end(Buffer.concat(chunks)); });
}
