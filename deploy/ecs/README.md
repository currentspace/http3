# ECS Deployment Reference

This directory contains ECS/NLB deployment assets for `@currentspace/http3`.

## Files

- `full-stack.template.yaml`: end-to-end VPC + ECS + NLB reference stack.
- `task-definition.template.json`: task definition-only template.
- `nlb-dual-443.template.yaml`: dual UDP/TCP 443 NLB listener template.
- `nlb-tcp-quic-443.template.yaml`: QUIC-LB-oriented `TCP_QUIC` template.
- `parameters.example.json`: example input values for full stack deploy.

## Full stack deploy

Build and push image first:

```bash
bash scripts/build-push-ecr.sh <region> <account-id> <tag>
```

Then deploy:

```bash
aws cloudformation deploy \
  --template-file deploy/ecs/full-stack.template.yaml \
  --stack-name http3-reference \
  --parameter-overrides file://deploy/ecs/parameters.example.json \
  --capabilities CAPABILITY_NAMED_IAM
```

After deploy:

```bash
bash scripts/verify-aws-http3.sh <nlb-dns-name>
```

## Operational notes

- For Fargate, prefer dual UDP/TCP listener mode (`nlb-dual-443.template.yaml`).
- For EC2 QUIC-LB mode, use `nlb-tcp-quic-443.template.yaml` and register
  each target with explicit `QuicServerId`.
- Keep readiness endpoints active on port `8080` for health checks.

