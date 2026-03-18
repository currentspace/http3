# ECS/Fargate Deployment (QUIC + TCP 443)

This package is designed to run as an application-terminated TLS origin with
both HTTP/3 and HTTP/2 exposed on port 443.

For runtime-mode semantics and the broader Linux capability matrix, see
[`RUNTIME_MODES.md`](./RUNTIME_MODES.md).

## Topology

- Optional Global Accelerator in front of the NLB for anycast ingress.
- NLB with either:
  - legacy dual-listener mode: UDP `443` + TCP `443`
  - QUIC-LB mode: single `TCP_QUIC` listener on `443`
- ECS/Fargate service in `awsvpc` network mode
- Task/container port mapping on `443` (app server)
- Separate health port (recommended `8080`) for `/healthz` and `/readyz`

## QUIC-LB Mode (AWS GA + NLB)

When using NLB QUIC passthrough with server-ID CID routing:

- Enable application options:
  - `quicLb: true`
  - `serverId: 8-byte ID` (hex form like `0x1122334455667788` is supported)
  - `disableActiveMigration: false` (recommended for mobile migration resilience)
- Register each target with a unique `QuicServerId`:
  - `aws elbv2 register-targets --target-group-arn <arn> --targets Id=<target>,Port=443,QuicServerId=0x1122334455667788`
- Keep `QuicServerId` stable for the lifetime of the target.
- If you rotate `QuicServerId`, deregister/re-register the target and drain old
  sessions before removal.

## Fargate vs EC2

- **EC2-first QUIC-LB path:** use `TCP_QUIC` target groups with explicit
  `QuicServerId` registration.
- **Fargate path:** keep the legacy dual UDP/TCP listener template unless your
  region/account supports your desired QUIC target registration model.
- **Runtime recommendation:** default Fargate/container deployments to
  `runtimeMode: 'portable'` unless you have explicitly validated a seccomp
  policy that allows `io_uring_*`.

## Required Network Settings

- Security group ingress:
  - UDP/443 from approved client ranges
  - TCP/443 from approved client ranges
  - TCP/8080 from internal health-checking source
- NACLs must permit stateless return traffic for UDP/TCP flows.
- Prefer static EIPs attached to NLB subnets for stable allowlisting.

## Task Definition Notes

- Set `ulimits` and task CPU/memory based on concurrency test outputs.
- Configure `stopTimeout` high enough for graceful stream drain.
- Use health check grace period with readiness endpoint.

## Startup Sequence

1. Load TLS key/cert (and optional CA) from secret material.
2. Start health server (`/healthz`, `/readyz`) as not ready.
3. Start unified H3/H2 server on 443 with an explicit runtime mode.
4. If `HTTP3_QUIC_LB=1`, ensure `HTTP3_SERVER_ID` is set to an 8-byte value.
5. Mark ready after `listening`.

## Shutdown Sequence

1. Receive `SIGTERM`.
2. Mark not-ready immediately.
3. Stop accepting new requests and drain active streams.
4. Exit process when drain completes or timeout is reached.

## Files

- Full stack template: [`deploy/ecs/full-stack.template.yaml`](../deploy/ecs/full-stack.template.yaml)
- Full stack parameters example: [`deploy/ecs/parameters.example.json`](../deploy/ecs/parameters.example.json)
- Task definition template: [`deploy/ecs/task-definition.template.json`](../deploy/ecs/task-definition.template.json)
- NLB reference template: [`deploy/ecs/nlb-dual-443.template.yaml`](../deploy/ecs/nlb-dual-443.template.yaml)
- NLB QUIC-LB template: [`deploy/ecs/nlb-tcp-quic-443.template.yaml`](../deploy/ecs/nlb-tcp-quic-443.template.yaml)

## Post-deploy verification

Use the protocol verification helper:

```bash
bash scripts/verify-aws-http3.sh <nlb-dns-name>
```
