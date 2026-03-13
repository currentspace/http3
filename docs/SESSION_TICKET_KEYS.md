# Session Ticket Keys Across Instances

Use the same `sessionTicketKeys` on every instance that might terminate QUIC/TLS
for the same hostname.

This is recommended for multi-instance deployments such as:

- AWS Global Accelerator to EC2 instance endpoints
- NLB or any other L4 load balancer in front of multiple app instances
- rolling restarts where a reconnecting client might land on a different node

## Is The API Exposed?

Yes.

Server applications can pass `sessionTicketKeys` to `createSecureServer()`:

```ts
import { createSecureServer } from '@currentspace/http3';

const server = createSecureServer({
  key: process.env.TLS_KEY_PEM!,
  cert: process.env.TLS_CERT_PEM!,
  sessionTicketKeys: Buffer.from(process.env.HTTP3_SESSION_TICKET_KEYS_B64!, 'base64'),
});
```

The public server options surface includes:

- `sessionTicketKeys?: Buffer`

## What This Does

Sharing `sessionTicketKeys` lets clients resume a previous QUIC/TLS session even
when a later connection is routed to a different instance.

This improves:

- session resumption hit rate across instances
- 0-RTT eligibility across instances
- resumption continuity during restarts and instance replacement

## What This Does Not Do

Sharing `sessionTicketKeys` does **not** share live QUIC connection state.

It does not provide:

- active connection failover between instances
- migration of an already-established QUIC connection to another node
- synchronization of Retry token secrets in this package

If a live QUIC connection is moved to another node, the new node still does not
have that connection's packet number space, congestion state, stream state, or
connection memory.

## Default Behavior

If you do not set `sessionTicketKeys`, quiche generates and rotates a ticket key
internally for the local process.

That is fine for:

- single-instance deployments
- cases where resumption across instances is not important

It is not ideal for:

- multi-instance fleets behind Global Accelerator, NLB, or DNS load balancing
- restarts where clients are expected to resume on a different process or host

## Current Key Format

This package currently uses `quiche` with its default BoringSSL-backed build.
For that build, the session ticket key is expected to be 48 raw bytes.

Validate the decoded length before starting the server:

```ts
function loadSessionTicketKeys(): Buffer {
  const raw = process.env.HTTP3_SESSION_TICKET_KEYS_B64;
  if (!raw) {
    throw new Error('HTTP3_SESSION_TICKET_KEYS_B64 is required');
  }

  const key = Buffer.from(raw, 'base64');
  if (key.length !== 48) {
    throw new Error('HTTP3_SESSION_TICKET_KEYS_B64 must decode to exactly 48 bytes');
  }

  return key;
}
```

Then use it:

```ts
const server = createSecureServer({
  key: process.env.TLS_KEY_PEM!,
  cert: process.env.TLS_CERT_PEM!,
  sessionTicketKeys: loadSessionTicketKeys(),
});
```

## Generating A Key

Generate one random 48-byte secret and inject the same value into every instance
in the fleet.

OpenSSL:

```bash
openssl rand -base64 48
```

Node.js:

```bash
node -e "process.stdout.write(require('node:crypto').randomBytes(48).toString('base64'))"
```

## Recommended Deployment Pattern

- Store the value in a secret manager or equivalent secure config store.
- Use a different value per environment.
- Inject the exact same value into every instance serving the same hostname.
- Do not generate a new random key on every boot if you want cross-instance resumption.

For example, all production instances serving `api.example.com` should share one
production ticket key, while staging should use a different one.

## Rotation Guidance

When you provide `sessionTicketKeys`, you are responsible for rotation.

Because the current API accepts a single key, rotation is coarse-grained:

- existing open connections continue unaffected
- old tickets presented to a node with the new key will stop resuming
- those clients should fall back to a full handshake
- resumption and 0-RTT hit rate may dip during rollout

Practical advice:

- rotate during a planned rollout window
- expect temporary resumption misses while old tickets age out
- avoid depending on uninterrupted 0-RTT acceptance during key rotation

## AWS Global Accelerator Note

For Global Accelerator to EC2 instance endpoints, sharing `sessionTicketKeys`
is recommended.

It helps when:

- a reconnecting client lands on a different healthy instance
- an instance is replaced during a deployment
- traffic is rebalanced across the fleet

It should be treated as a resumption optimization, not as live-session failover.

## Programmatic Clients Using This Library

Browsers and standard CLI clients usually manage tickets automatically.

If you are using this library as a client, you can persist tickets yourself:

```ts
import { connectAsync } from '@currentspace/http3';

const session = await connectAsync('example.com:443', {
  sessionTicket: previouslyStoredTicket,
});

session.on('sessionTicket', (ticket: Buffer) => {
  // Save for a future reconnect.
});
```
