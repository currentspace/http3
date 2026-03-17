# Hono Example

Demonstrates running a Hono application over HTTP/3 using `serveFetch`.

## Setup

```bash
npm install
npm run build
cd examples/hono && npm install
cd ../certs && ./generate.sh
```

## Run

```bash
npx tsx examples/hono/server.ts
```

## Test

```bash
npx tsx examples/raw-api/client.ts localhost:4433 /
npx tsx examples/raw-api/client.ts localhost:4433 /json
npx tsx examples/raw-api/client.ts localhost:4433 /hello/world
```
