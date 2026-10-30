# Express Adapter Example

Demonstrates running Express middleware and route handlers over HTTP/3 using `createExpressAdapter`.

For `createNodeServerFactory`, use `createExpressHandler(app)` from
`@currentspace/http3/express` (0.9.6+). The bridge preserves stream methods
when Express replaces request/response prototypes, including mounted apps. See
the [combined Next.js/NestJS example](../nextjs-nestjs/README.md) for a complete
Express-based server with H3, H2, and HTTP/1.1 fallback.

## Setup

```bash
npm install
npm run build
cd examples/express-adapter && npm install
cd ../certs && ./generate.sh
```

## Run

```bash
npx tsx examples/express-adapter/server.ts
```

## Test

```bash
npx tsx examples/raw-api/client.ts localhost:4433 /
npx tsx examples/raw-api/client.ts localhost:4433 /json
npx tsx examples/raw-api/client.ts localhost:4433 /hello/world
```
