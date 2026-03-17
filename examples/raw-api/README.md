# Raw API Example

Demonstrates the low-level `createSecureServer` / `connect` API for HTTP/3.

## Setup

```bash
npm install
npm run build
cd examples/certs && ./generate.sh
```

## Run the server

```bash
npx tsx examples/raw-api/server.ts
```

## Run the client

```bash
npx tsx examples/raw-api/client.ts localhost:4433 /
npx tsx examples/raw-api/client.ts localhost:4433 /json
```

## Test with curl

```bash
curl --http3-only -k https://localhost:4433/
curl --http3-only -k https://localhost:4433/json
```
