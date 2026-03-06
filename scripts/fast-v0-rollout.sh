#!/usr/bin/env bash
set -euo pipefail

log() {
  echo "[fast-v0-rollout] $*"
}

: "${AWS_PROFILE:=dash}"
: "${AWS_REGION:=us-west-2}"
: "${DEPLOY_PREFIX:=h3q-0305110905}"
: "${ASSIGN_PUBLIC_IP:=ENABLED}"
: "${ACME_ENABLED_OVERRIDE:=}"

CLUSTER="${DEPLOY_PREFIX}-cluster"
LB_NAME="h3qnlb-${DEPLOY_PREFIX#h3q-}"
FAMILY_A="${DEPLOY_PREFIX}-a"
FAMILY_B="${DEPLOY_PREFIX}-b"

awsx() {
  aws --profile "${AWS_PROFILE}" --region "${AWS_REGION}" "$@"
}

wait_targets_cleared() {
  local tries=0
  while true; do
    local current
    current="$(awsx elbv2 describe-target-health --target-group-arn "${TG_ARN}" --query 'TargetHealthDescriptions[].Target.Id' --output text || true)"
    if [[ -z "${current}" ]]; then
      return 0
    fi
    tries=$((tries + 1))
    if (( tries > 40 )); then
      log "Timed out waiting for old targets to clear: ${current}"
      return 1
    fi
    sleep 2
  done
}

LB_ARN="$(awsx elbv2 describe-load-balancers --names "${LB_NAME}" --query 'LoadBalancers[0].LoadBalancerArn' --output text)"
TG_ARN="$(awsx elbv2 describe-listeners --load-balancer-arn "${LB_ARN}" --query 'Listeners[?Protocol==`TCP_QUIC` && Port==`443`].DefaultActions[0].TargetGroupArn | [0]' --output text)"

if [[ "${TG_ARN}" == "None" || -z "${TG_ARN}" ]]; then
  log "Could not resolve TCP_QUIC target group for ${LB_NAME}"
  exit 1
fi

TASKDEF_A="$(awsx ecs list-task-definitions --family-prefix "${FAMILY_A}" --sort DESC --status ACTIVE --query 'taskDefinitionArns[0]' --output text)"
TASKDEF_B="$(awsx ecs list-task-definitions --family-prefix "${FAMILY_B}" --sort DESC --status ACTIVE --query 'taskDefinitionArns[0]' --output text)"

if [[ "${TASKDEF_A}" == "None" || "${TASKDEF_B}" == "None" || -z "${TASKDEF_A}" || -z "${TASKDEF_B}" ]]; then
  log "Could not resolve latest active task definitions for ${FAMILY_A}/${FAMILY_B}"
  exit 1
fi

SERVER_ID_A="$(awsx ecs describe-task-definition --task-definition "${TASKDEF_A}" --query 'taskDefinition.containerDefinitions[0].environment[?name==`HTTP3_SERVER_ID`].value | [0]' --output text)"
SERVER_ID_B="$(awsx ecs describe-task-definition --task-definition "${TASKDEF_B}" --query 'taskDefinition.containerDefinitions[0].environment[?name==`HTTP3_SERVER_ID`].value | [0]' --output text)"

RUNNING_TASKS="$(awsx ecs list-tasks --cluster "${CLUSTER}" --desired-status RUNNING --query 'taskArns' --output text)"
if [[ -z "${RUNNING_TASKS}" ]]; then
  log "No running tasks found in ${CLUSTER}"
  exit 1
fi

FIRST_TASK="$(awk '{print $1}' <<< "${RUNNING_TASKS}")"
FIRST_ENI="$(awsx ecs describe-tasks --cluster "${CLUSTER}" --tasks "${FIRST_TASK}" --query 'tasks[0].attachments[0].details[?name==`networkInterfaceId`].value | [0]' --output text)"
SUBNET_ID="$(awsx ecs describe-tasks --cluster "${CLUSTER}" --tasks "${FIRST_TASK}" --query 'tasks[0].attachments[0].details[?name==`subnetId`].value | [0]' --output text)"
SG_ID="$(awsx ec2 describe-network-interfaces --network-interface-ids "${FIRST_ENI}" --query 'NetworkInterfaces[0].Groups[0].GroupId' --output text)"

log "Speed mode: setting target-group deregistration delay to 0s"
awsx elbv2 modify-target-group-attributes \
  --target-group-arn "${TG_ARN}" \
  --attributes Key=deregistration_delay.timeout_seconds,Value=0 Key=deregistration_delay.connection_termination.enabled,Value=true >/dev/null

CURRENT_TARGETS="$(awsx elbv2 describe-target-health --target-group-arn "${TG_ARN}" --query 'TargetHealthDescriptions[].Target.Id' --output text || true)"
if [[ -n "${CURRENT_TARGETS}" ]]; then
  TARGET_ARGS=()
  for ip in ${CURRENT_TARGETS}; do
    TARGET_ARGS+=("Id=${ip},Port=443")
  done
  log "Deregistering current targets: ${CURRENT_TARGETS}"
  awsx elbv2 deregister-targets --target-group-arn "${TG_ARN}" --targets "${TARGET_ARGS[@]}" >/dev/null
fi
wait_targets_cleared

log "Stopping old running tasks"
for task in ${RUNNING_TASKS}; do
  awsx ecs stop-task --cluster "${CLUSTER}" --task "${task}" >/dev/null || true
done

log "Launching replacement tasks from latest definitions"
if [[ -n "${ACME_ENABLED_OVERRIDE}" ]]; then
  OVERRIDES_JSON="$(cat <<JSON
{"containerOverrides":[{"name":"http3-server","environment":[{"name":"ACME_ENABLED","value":"${ACME_ENABLED_OVERRIDE}"}]}]}
JSON
)"
  NEW_A="$(awsx ecs run-task --cluster "${CLUSTER}" --launch-type FARGATE --task-definition "${TASKDEF_A}" --network-configuration "awsvpcConfiguration={subnets=[${SUBNET_ID}],securityGroups=[${SG_ID}],assignPublicIp=${ASSIGN_PUBLIC_IP}}" --overrides "${OVERRIDES_JSON}" --query 'tasks[0].taskArn' --output text)"
  NEW_B="$(awsx ecs run-task --cluster "${CLUSTER}" --launch-type FARGATE --task-definition "${TASKDEF_B}" --network-configuration "awsvpcConfiguration={subnets=[${SUBNET_ID}],securityGroups=[${SG_ID}],assignPublicIp=${ASSIGN_PUBLIC_IP}}" --overrides "${OVERRIDES_JSON}" --query 'tasks[0].taskArn' --output text)"
else
  NEW_A="$(awsx ecs run-task --cluster "${CLUSTER}" --launch-type FARGATE --task-definition "${TASKDEF_A}" --network-configuration "awsvpcConfiguration={subnets=[${SUBNET_ID}],securityGroups=[${SG_ID}],assignPublicIp=${ASSIGN_PUBLIC_IP}}" --query 'tasks[0].taskArn' --output text)"
  NEW_B="$(awsx ecs run-task --cluster "${CLUSTER}" --launch-type FARGATE --task-definition "${TASKDEF_B}" --network-configuration "awsvpcConfiguration={subnets=[${SUBNET_ID}],securityGroups=[${SG_ID}],assignPublicIp=${ASSIGN_PUBLIC_IP}}" --query 'tasks[0].taskArn' --output text)"
fi

awsx ecs wait tasks-running --cluster "${CLUSTER}" --tasks "${NEW_A}" "${NEW_B}"

NEW_A_IP="$(awsx ecs describe-tasks --cluster "${CLUSTER}" --tasks "${NEW_A}" --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value | [0]' --output text)"
NEW_B_IP="$(awsx ecs describe-tasks --cluster "${CLUSTER}" --tasks "${NEW_B}" --query 'tasks[0].attachments[0].details[?name==`privateIPv4Address`].value | [0]' --output text)"

log "Registering new targets"
for attempt in $(seq 1 20); do
  if docker run --rm -v "${HOME}/.aws:/root/.aws" \
    -e AWS_PROFILE="${AWS_PROFILE}" \
    -e AWS_REGION="${AWS_REGION}" \
    amazon/aws-cli:latest elbv2 register-targets \
    --target-group-arn "${TG_ARN}" \
    --targets "Id=${NEW_A_IP},Port=443,QuicServerId=${SERVER_ID_A}" "Id=${NEW_B_IP},Port=443,QuicServerId=${SERVER_ID_B}" >/dev/null 2>&1; then
    break
  fi
  if (( attempt == 20 )); then
    log "Failed to register new targets after retries"
    exit 1
  fi
  sleep 2
done

log "Waiting for target health"
for _ in $(seq 1 24); do
  HEALTH_JSON="$(awsx elbv2 describe-target-health --target-group-arn "${TG_ARN}" --query 'TargetHealthDescriptions[].{id:Target.Id,state:TargetHealth.State}' --output json)"
  echo "${HEALTH_JSON}"
  healthy_count="$(python3 - <<'PY' "${HEALTH_JSON}"
import json,sys
items=json.loads(sys.argv[1] or "[]")
print(sum(1 for i in items if i.get("state")=="healthy"))
PY
)"
  if [[ "${healthy_count}" == "2" ]]; then
    log "Rollout complete: ${NEW_A_IP} (${SERVER_ID_A}), ${NEW_B_IP} (${SERVER_ID_B})"
    exit 0
  fi
  sleep 5
done

log "Timed out waiting for both targets to become healthy"
exit 1
