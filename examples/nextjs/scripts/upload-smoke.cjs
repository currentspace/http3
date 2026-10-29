const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');

module.exports = async function checkUploads(page, waitForPrefix) {
  await page.exposeFunction('__waitForUploadPrefix', waitForPrefix);
  const results = [];
  for (const [kind, size] of [['file', 65536], ['file', 8 * 1024 * 1024], ['gated', 8 * 1024 * 1024]]) {
    const id = `${kind}-${size}`;
    const result = await page.evaluate(async ({ kind, size, id }) => {
      const bytes = new Uint8Array(size);
      for (let i = 0; i < size; i++) bytes[i] = (i * 31 + 17) % 251;
      const file = new File([bytes], 'sample.bin', { type: 'application/octet-stream' });
      let body = file.stream();
      let prefixReceived = 0;
      if (kind === 'gated') {
        let offset = 0;
        body = new ReadableStream({
          async pull(controller) {
            if (offset === 65536) {
              // The server must consume some of the first chunk while the
              // browser still withholds the rest of the file and upload EOF.
              prefixReceived = await window.__waitForUploadPrefix(id);
            }
            if (offset === bytes.length) { controller.close(); return; }
            controller.enqueue(bytes.slice(offset, offset + 65536));
            offset = Math.min(bytes.length, offset + 65536);
          },
        });
      }
      const response = await fetch('/api/upload', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/octet-stream',
          'X-File-Size': String(file.size),
          'X-Upload-Id': id,
        },
        body,
        duplex: 'half',
        signal: AbortSignal.timeout(30000),
      });
      return { kind, size, prefixReceived, status: response.status, result: await response.json() };
    }, { kind, size, id });
    const expected = Buffer.alloc(size);
    for (let i = 0; i < size; i++) expected[i] = (i * 31 + 17) % 251;
    assert.equal(result.status, 200);
    assert.equal(result.result.received, size);
    assert.equal(result.result.sha256, createHash('sha256').update(expected).digest('hex'));
    if (kind === 'gated') assert.ok(result.prefixReceived > 0 && result.prefixReceived <= 65536);
    results.push(result);
  }

  // Exercise the actual form as well as the scripted streaming consumers.
  const file = Buffer.alloc(65536, 42);
  const hash = createHash('sha256').update(file).digest('hex');
  await page.getByLabel('File to upload').setInputFiles({ name: 'ui-test.bin', mimeType: 'application/octet-stream', buffer: file });
  await page.getByRole('button', { name: 'Upload file', exact: true }).click();
  await page.getByRole('status').filter({ hasText: `Received ${file.length} bytes. SHA-256: ${hash}` }).waitFor();
  results.push({ kind: 'form', size: file.length, sha256: hash, passed: true });
  return results;
};
