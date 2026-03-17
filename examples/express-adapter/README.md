# Express Adapter Example

Demonstrates running Express middleware and route handlers over HTTP/3 using `createExpressAdapter`.

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
