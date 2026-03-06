# Deployment Examples

## AWS ECS/Fargate dual-listener mode

- Use `deploy/ecs/task-definition.template.json`.
- Use `deploy/ecs/nlb-dual-443.template.yaml` for UDP/TCP 443.
- Run readiness endpoint on port `8080`.
- Build/push container image: `bash scripts/build-push-ecr.sh <region> <account-id> <tag>`.

## AWS EC2 QUIC-LB mode

- Use `deploy/ecs/nlb-tcp-quic-443.template.yaml`.
- Enable app options:
  - `quicLb: true`
  - `serverId: <8-byte-id>`
- Register each target with matching `QuicServerId`.

## Post-deploy checks

```bash
curl --http3-only -k https://<hostname>/
curl --http2 -k https://<hostname>/
curl -sf http://<task-ip>:8080/healthz
curl -sf http://<task-ip>:8080/readyz
```

