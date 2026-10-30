import { readFileSync } from 'node:fs';
import { connectAsync } from '@currentspace/http3';

// This explicit H3 client lets server rendering and a Next route call the BFF
// through the same listener. Ordinary Node fetch does not select this client.
export async function bffIdentity(cookie) {
  const session = await connectAsync(`https://127.0.0.1:${process.env.PORT ?? 3443}`, {
    // Deployment supplies TLS files; do not trace them into the Next build.
    ca: readFileSync(/* turbopackIgnore: true */ process.env.TLS_CA_FILE ?? process.env.TLS_CERT_FILE ?? '.certs/cert.pem'),
    servername: 'localhost', runtimeMode: 'portable',
  });
  try {
    const stream = session.request({ ':method': 'GET', ':path': '/bff/identity',
      ...(cookie ? { cookie } : {}),
    }, { endStream: true });
    const response = new Promise((resolve, reject) => {
      stream.once('response', resolve);
      stream.once('error', reject);
    });
    // Observe header errors immediately even if reading the body also fails.
    void response.catch(() => {});
    const chunks = [];
    for await (const chunk of stream) chunks.push(chunk);
    const headers = await response;
    if (Number(headers[':status']) !== 200) throw new Error(`BFF returned ${headers[':status']}`);
    return JSON.parse(Buffer.concat(chunks).toString());
  } finally { await session.close(); }
}
