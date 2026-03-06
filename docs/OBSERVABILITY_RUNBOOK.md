# Observability And On-Call Runbook

## Minimum Metrics

- Handshake success/failure rate per protocol (`h3`, `h2`)
- Active sessions and streams
- Stream reset/error counts
- SSE open connections and reconnect rates
- Request latency histogram (p50/p95/p99)
- Process memory/CPU and event loop lag
- NLB QUIC metrics:
  - `QUIC_Unknown_Server_ID_Packet_Drop_Count`
  - `ProcessedBytes_QUIC`
  - `NewFlowCount_QUIC`

## Minimum Logs

- Protocol, path, status, duration
- Session close reason and transport errors
- TLS/certificate load and rotation events
- Graceful shutdown start/finish and timeout outcomes

## Alerts

- Handshake failures above threshold
- Sudden stream reset spikes
- Readiness failing across >N tasks
- Certificate load/rotation failures
- Concurrency gate regression in CI
- Any increase in `QUIC_Unknown_Server_ID_Packet_Drop_Count`

## Incident Quick Actions

### High handshake failure rate

1. Verify cert/key freshness and trust chain.
2. Validate NLB TCP+UDP listeners and SG/NACL rules.
3. Run curl H3/H2 smoke checks from trusted host.

### Cloudflare lockout

1. Confirm Cloudflare cert chain and mTLS settings.
2. Confirm IP allowlist automation did not remove current ranges.
3. Temporarily switch to controlled public mode only if incident policy allows.

### Slow drain/failed deploy shutdown

1. Check readiness transition happened on signal.
2. Confirm `stopTimeout` and graceful timeout values.
3. Inspect long-lived SSE streams and backpressure behavior.

### QUIC unknown server ID drops

1. Verify each registered NLB target has the expected `QuicServerId`.
2. Verify app config (`quicLb`, `serverId`) matches NLB registrations.
3. Check for deregistered targets still receiving migrated client packets.
