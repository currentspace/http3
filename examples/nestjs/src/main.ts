import { readFileSync } from 'node:fs';
import { createApp } from './bootstrap.js';

const port = Number(process.env.PORT ?? 3443);
if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('Invalid PORT');
const app = await createApp({
  key: readFileSync(process.env.TLS_KEY_FILE ?? '.certs/key.pem'),
  cert: readFileSync(process.env.TLS_CERT_FILE ?? '.certs/cert.pem'),
});
app.enableShutdownHooks(['SIGINT', 'SIGTERM']);
try {
  await app.listen(port, process.env.HTTP3_HOST ?? '::');
  // Nest 12.3.2's getUrl() checks instanceof tls.Server; the facade fails that
  // check even though every supported protocol on this listener uses TLS.
  const address = app.getHttpServer().address();
  if (!address || typeof address === 'string') throw new Error('Expected a TCP/UDP listener');
  console.log(`Ready: https://localhost:${address.port}`);
} catch (error) {
  await app.close();
  throw error;
}
