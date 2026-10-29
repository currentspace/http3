'use client';

import { useState } from 'react';

export default function Upload() {
  const [file, setFile] = useState(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('Choose a file to verify a streaming upload.');

  async function upload(event) {
    event.preventDefault();
    if (!file || busy) return;
    setBusy(true);
    setMessage('Uploading…');
    try {
      const response = await fetch('/api/upload', {
        method: 'POST',
        headers: { 'Content-Type': 'application/octet-stream', 'X-File-Size': String(file.size) },
        body: file.stream(),
        duplex: 'half',
        signal: AbortSignal.timeout(60000),
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const result = await response.json();
      if (result.received !== file.size) throw new Error('Received byte count does not match the file');
      setMessage(`Received ${result.received} bytes. SHA-256: ${result.sha256}`);
    } catch (error) {
      setMessage(`Upload failed: ${error.message}. Check that your browser supports streaming fetch over HTTP/2 or HTTP/3.`);
    } finally {
      setBusy(false);
    }
  }

  return (
    <section>
      <h2>Streaming file upload</h2>
      <p>The server counts and hashes chunks as they arrive. It does not store the file.</p>
      <form onSubmit={upload}>
        <label>File to upload <input type="file" disabled={busy} onChange={event => setFile(event.target.files[0] ?? null)} /></label>
        <button type="submit" disabled={!file || busy}>Upload file</button>
      </form>
      <p role="status">{message}</p>
    </section>
  );
}
