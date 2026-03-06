#!/usr/bin/env bash
set -euo pipefail

# Cleanup script for the isolated QUIC-LB test deployment resources.
# It deletes only resources derived from a test prefix, for example:
#   PREFIX=h3q-0305110905
#
# Usage examples:
#   AWS_PROFILE=dash AWS_REGION=us-west-2 PREFIX=h3q-0305110905 bash scripts/cleanup-aws-quiclb-test.sh
#   AWS_PROFILE=dash AWS_REGION=us-west-2 TEST_ID=0305110905 bash scripts/cleanup-aws-quiclb-test.sh

AWS_PROFILE="${AWS_PROFILE:-dash}"
AWS_REGION="${AWS_REGION:-us-west-2}"
PREFIX="${PREFIX:-}"
TEST_ID="${TEST_ID:-}"

if [[ -z "${PREFIX}" ]]; then
  if [[ -n "${TEST_ID}" ]]; then
    PREFIX="h3q-${TEST_ID}"
  else
    echo "Set PREFIX (or TEST_ID) before running cleanup." >&2
    exit 2
  fi
fi

if [[ -z "${TEST_ID}" && "${PREFIX}" == h3q-* ]]; then
  TEST_ID="${PREFIX#h3q-}"
fi

if [[ -z "${TEST_ID}" ]]; then
  echo "Unable to derive TEST_ID from PREFIX='${PREFIX}'. Set TEST_ID explicitly." >&2
  exit 2
fi

ECR_REPO="${ECR_REPO:-${PREFIX}-repo}"
CLUSTER_NAME="${CLUSTER_NAME:-${PREFIX}-cluster}"
SG_NAME="${SG_NAME:-${PREFIX}-sg}"
NLB_NAME="${NLB_NAME:-h3qnlb-${TEST_ID}}"
TG_NAME="${TG_NAME:-h3qtg-${TEST_ID}}"
ACCEL_NAME="${ACCEL_NAME:-${PREFIX}-ga}"
SECRET_NAME="${SECRET_NAME:-${PREFIX}/tls}"
LOG_GROUP="${LOG_GROUP:-/ecs/${PREFIX}}"
EXEC_ROLE_NAME="${EXEC_ROLE_NAME:-${PREFIX}-exec-role}"
TASK_ROLE_NAME="${TASK_ROLE_NAME:-${PREFIX}-task-role}"

aws_regional() {
  aws --profile "${AWS_PROFILE}" --region "${AWS_REGION}" "$@"
}

aws_global() {
  # Global Accelerator is a global service; region is not required.
  aws --profile "${AWS_PROFILE}" "$@"
}

log() {
  echo "[cleanup] $*"
}

if command -v docker >/dev/null 2>&1; then
  if docker image inspect amazon/aws-cli:latest >/dev/null 2>&1; then
    AWS_CLI_DOCKER=1
  else
    AWS_CLI_DOCKER=0
  fi
else
  AWS_CLI_DOCKER=0
fi

describe_tg_arn() {
  aws_regional elbv2 describe-target-groups \
    --names "${TG_NAME}" \
    --query "TargetGroups[0].TargetGroupArn" \
    --output text 2>/dev/null || true
}

describe_nlb_arn() {
  aws_regional elbv2 describe-load-balancers \
    --names "${NLB_NAME}" \
    --query "LoadBalancers[0].LoadBalancerArn" \
    --output text 2>/dev/null || true
}

describe_accel_arn() {
  aws_global globalaccelerator list-accelerators \
    --query "Accelerators[?Name=='${ACCEL_NAME}'].AcceleratorArn | [0]" \
    --output text 2>/dev/null || true
}

non_empty() {
  [[ -n "${1:-}" && "${1}" != "None" && "${1}" != "null" ]]
}

stop_ecs_tasks_and_cluster() {
  local cluster_arn
  cluster_arn="$(aws_regional ecs describe-clusters --clusters "${CLUSTER_NAME}" --query "clusters[0].clusterArn" --output text 2>/dev/null || true)"
  if ! non_empty "${cluster_arn}"; then
    log "ECS cluster not found: ${CLUSTER_NAME}"
    return
  fi

  log "Stopping ECS tasks in cluster ${CLUSTER_NAME}"
  local running pending
  running="$(aws_regional ecs list-tasks --cluster "${CLUSTER_NAME}" --desired-status RUNNING --query "taskArns[]" --output text 2>/dev/null || true)"
  pending="$(aws_regional ecs list-tasks --cluster "${CLUSTER_NAME}" --desired-status PENDING --query "taskArns[]" --output text 2>/dev/null || true)"

  local all_tasks=""
  if non_empty "${running}"; then all_tasks="${running}"; fi
  if non_empty "${pending}"; then all_tasks="${all_tasks} ${pending}"; fi

  if non_empty "${all_tasks}"; then
    for t in ${all_tasks}; do
      aws_regional ecs stop-task --cluster "${CLUSTER_NAME}" --task "${t}" --reason "cleanup ${PREFIX}" >/dev/null || true
    done
    aws_regional ecs wait tasks-stopped --cluster "${CLUSTER_NAME}" --tasks ${all_tasks} || true
  fi

  local services
  services="$(aws_regional ecs list-services --cluster "${CLUSTER_NAME}" --query "serviceArns[]" --output text 2>/dev/null || true)"
  if non_empty "${services}"; then
    log "Deleting ECS services in cluster ${CLUSTER_NAME}"
    for s in ${services}; do
      aws_regional ecs delete-service --cluster "${CLUSTER_NAME}" --service "${s}" --force >/dev/null || true
    done
    aws_regional ecs wait services-inactive --cluster "${CLUSTER_NAME}" --services ${services} || true
  fi

  log "Deleting ECS cluster ${CLUSTER_NAME}"
  aws_regional ecs delete-cluster --cluster "${CLUSTER_NAME}" >/dev/null || true
}

deregister_task_definitions() {
  for fam in "${PREFIX}-a" "${PREFIX}-b"; do
    local defs
    defs="$(aws_regional ecs list-task-definitions --family-prefix "${fam}" --status ACTIVE --query "taskDefinitionArns[]" --output text 2>/dev/null || true)"
    if non_empty "${defs}"; then
      log "Deregistering task definitions for family ${fam}"
      for d in ${defs}; do
        aws_regional ecs deregister-task-definition --task-definition "${d}" >/dev/null || true
      done
    fi
  done
}

delete_global_accelerator() {
  local acc_arn
  acc_arn="$(describe_accel_arn)"
  if ! non_empty "${acc_arn}"; then
    log "Global Accelerator not found: ${ACCEL_NAME}"
    return
  fi

  log "Deleting Global Accelerator listeners/endpoint groups for ${ACCEL_NAME}"
  local listeners
  listeners="$(aws_global globalaccelerator list-listeners --accelerator-arn "${acc_arn}" --query "Listeners[].ListenerArn" --output text 2>/dev/null || true)"
  if non_empty "${listeners}"; then
    for l in ${listeners}; do
      local egs
      egs="$(aws_global globalaccelerator list-endpoint-groups --listener-arn "${l}" --query "EndpointGroups[].EndpointGroupArn" --output text 2>/dev/null || true)"
      if non_empty "${egs}"; then
        for eg in ${egs}; do
          aws_global globalaccelerator delete-endpoint-group --endpoint-group-arn "${eg}" >/dev/null || true
        done
      fi
      aws_global globalaccelerator delete-listener --listener-arn "${l}" >/dev/null || true
    done
  fi

  log "Deleting Global Accelerator ${ACCEL_NAME}"
  aws_global globalaccelerator delete-accelerator --accelerator-arn "${acc_arn}" >/dev/null || true
}

delete_nlb_and_target_group() {
  local nlb_arn tg_arn
  nlb_arn="$(describe_nlb_arn)"
  tg_arn="$(describe_tg_arn)"

  if non_empty "${tg_arn}"; then
    log "Deregistering targets from target group ${TG_NAME}"
    local ips
    ips="$(aws_regional elbv2 describe-target-health --target-group-arn "${tg_arn}" --query "TargetHealthDescriptions[].Target.Id" --output text 2>/dev/null || true)"
    if non_empty "${ips}"; then
      local target_args=()
      for ip in ${ips}; do
        target_args+=("Id=${ip},Port=443")
      done
      if [[ "${AWS_CLI_DOCKER}" -eq 1 ]]; then
        docker run --rm \
          -v "${HOME}/.aws:/root/.aws" \
          -e AWS_PROFILE="${AWS_PROFILE}" \
          -e AWS_REGION="${AWS_REGION}" \
          amazon/aws-cli:latest \
          elbv2 deregister-targets \
          --target-group-arn "${tg_arn}" \
          --targets "${target_args[@]}" >/dev/null || true
      else
        aws_regional elbv2 deregister-targets --target-group-arn "${tg_arn}" --targets "${target_args[@]}" >/dev/null || true
      fi
    fi
  fi

  if non_empty "${nlb_arn}"; then
    log "Deleting listeners and NLB ${NLB_NAME}"
    local listeners
    listeners="$(aws_regional elbv2 describe-listeners --load-balancer-arn "${nlb_arn}" --query "Listeners[].ListenerArn" --output text 2>/dev/null || true)"
    if non_empty "${listeners}"; then
      for l in ${listeners}; do
        aws_regional elbv2 delete-listener --listener-arn "${l}" >/dev/null || true
      done
    fi
    aws_regional elbv2 delete-load-balancer --load-balancer-arn "${nlb_arn}" >/dev/null || true
    aws_regional elbv2 wait load-balancers-deleted --load-balancer-arns "${nlb_arn}" || true
  else
    log "NLB not found: ${NLB_NAME}"
  fi

  if non_empty "${tg_arn}"; then
    log "Deleting target group ${TG_NAME}"
    aws_regional elbv2 delete-target-group --target-group-arn "${tg_arn}" >/dev/null || true
  else
    log "Target group not found: ${TG_NAME}"
  fi
}

delete_security_group() {
  local sg_id
  sg_id="$(aws_regional ec2 describe-security-groups --filters "Name=group-name,Values=${SG_NAME}" "Name=vpc-id,Values=$(aws_regional ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)" --query "SecurityGroups[0].GroupId" --output text 2>/dev/null || true)"
  if non_empty "${sg_id}"; then
    log "Deleting security group ${SG_NAME} (${sg_id})"
    aws_regional ec2 delete-security-group --group-id "${sg_id}" >/dev/null || true
  else
    log "Security group not found: ${SG_NAME}"
  fi
}

delete_log_group_and_secret() {
  log "Deleting log group ${LOG_GROUP}"
  aws_regional logs delete-log-group --log-group-name "${LOG_GROUP}" >/dev/null || true

  log "Deleting secret ${SECRET_NAME}"
  aws_regional secretsmanager delete-secret --secret-id "${SECRET_NAME}" --force-delete-without-recovery >/dev/null || true
}

delete_roles() {
  for role in "${EXEC_ROLE_NAME}" "${TASK_ROLE_NAME}"; do
    if aws iam get-role --profile "${AWS_PROFILE}" --role-name "${role}" >/dev/null 2>&1; then
      log "Deleting IAM role ${role}"
      local attached inline
      attached="$(aws iam list-attached-role-policies --profile "${AWS_PROFILE}" --role-name "${role}" --query "AttachedPolicies[].PolicyArn" --output text 2>/dev/null || true)"
      if non_empty "${attached}"; then
        for p in ${attached}; do
          aws iam detach-role-policy --profile "${AWS_PROFILE}" --role-name "${role}" --policy-arn "${p}" >/dev/null || true
        done
      fi
      inline="$(aws iam list-role-policies --profile "${AWS_PROFILE}" --role-name "${role}" --query "PolicyNames[]" --output text 2>/dev/null || true)"
      if non_empty "${inline}"; then
        for ip in ${inline}; do
          aws iam delete-role-policy --profile "${AWS_PROFILE}" --role-name "${role}" --policy-name "${ip}" >/dev/null || true
        done
      fi
      aws iam delete-role --profile "${AWS_PROFILE}" --role-name "${role}" >/dev/null || true
    else
      log "IAM role not found: ${role}"
    fi
  done
}

delete_ecr_repo() {
  log "Deleting ECR repo ${ECR_REPO}"
  aws_regional ecr delete-repository --repository-name "${ECR_REPO}" --force >/dev/null || true
}

log "Starting cleanup for PREFIX=${PREFIX} in profile=${AWS_PROFILE}, region=${AWS_REGION}"
delete_global_accelerator
stop_ecs_tasks_and_cluster
deregister_task_definitions
delete_nlb_and_target_group
delete_security_group
delete_log_group_and_secret
delete_roles
delete_ecr_repo
log "Cleanup completed for PREFIX=${PREFIX}"
