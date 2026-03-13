# @currentspace/http3

HTTP/3 + HTTP/2 server/client package for Node.js 24+, powered by Rust + quiche.

## Install

```bash
npm install @currentspace/http3
```

## Quick server example

```ts
import { createSecureServer } from '@currentspace/http3';

const server = createSecureServer({
  key: process.env.TLS_KEY_PEM,
  cert: process.env.TLS_CERT_PEM,
}, (stream, headers) => {
  stream.respond({ ':status': '200', 'content-type': 'text/plain' });
  stream.end(`hello ${String(headers[':path'] ?? '/')}`);
});

server.listen(443, '0.0.0.0');
```

## Quick client example

```ts
import { connectAsync } from '@currentspace/http3';

const session = await connectAsync('example.com:443');
const stream = session.request({
  ':method': 'GET',
  ':path': '/',
  ':authority': 'example.com',
  ':scheme': 'https',
}, { endStream: true });
```

## Compatibility surfaces

- `@currentspace/http3` - canonical API.
- `@currentspace/http3/parity` - http2-style aliases for migrations.
- `@currentspace/http3/h3` - HTTP/3-specific extension namespace.

## Production docs

- [Production docs index](./docs/README.md)
- [HTTP/2 parity matrix](./docs/HTTP2_PARITY_MATRIX.md)
- [ECS/Fargate deployment](./docs/ECS_FARGATE_DEPLOYMENT.md)
- [AWS NLB QUIC passthrough](./docs/AWS_NLB_QUIC_PASSTHROUGH.md)
- [Session ticket keys across instances](./docs/SESSION_TICKET_KEYS.md)
- [Release runbook](./docs/RELEASE_RUNBOOK.md)

