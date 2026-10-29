import { createHash } from 'node:crypto';

export const runtime = 'nodejs';

export async function POST(request) {
  if (!request.body) return Response.json({ error: 'A file body is required' }, { status: 400 });

  const hash = createHash('sha256');
  const reader = request.body.getReader();
  let received = 0;
  let chunks = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      received += value.byteLength;
      chunks++;
      hash.update(value);
      // Test-only observation: the browser test withholds the rest of its body
      // until this confirms that the route consumed a chunk before upload EOF.
      if (chunks === 1 && process.env.HTTP3_SMOKE_UPLOAD_PROGRESS === '1') {
        console.log('HTTP3_UPLOAD_PREFIX ' + JSON.stringify({
          id: request.headers.get('x-upload-id'), received,
        }));
      }
    }
  } finally {
    reader.releaseLock();
  }

  // Only the digest and counters are retained; no file is saved or echoed back.
  return Response.json({ received, chunks, sha256: hash.digest('hex') });
}
