#!/usr/bin/env bash
# 지금 도는 ECS 태스크의 웹 주소를 찍는다. Fargate 는 배포마다 ENI 를 새로 만들어서
# 사설 IP 가 바뀐다 — 고정 주소가 필요하면 Cloud Map(서비스 디스커버리)을 붙여야 한다.
#
#   ./scripts/dbmon-url.sh              # 주소만
#   open "$(./scripts/dbmon-url.sh)"    # 브라우저로 열기
#
# ponytail: 조회 스크립트 하나. ALB(월 $16+)나 Cloud Map 은 주소가 실제로 자주 깨질 때.
set -euo pipefail

# 리전은 **AWS_REGION 을 물려받지 않는다.** 이 셸에는 다른 리전(us-west-2)이 박혀 있을 수
# 있고, 그러면 "클러스터가 없다" 로 실패해서 태스크가 죽은 것처럼 보인다. 실제로 걸렸다.
REGION="${DBMON_REGION:-ap-northeast-2}"
CLUSTER="${DBMON_CLUSTER:-dbmon-dev}"
PORT="${DBMON_PORT:-8080}"

task=$(aws ecs list-tasks --region "$REGION" --cluster "$CLUSTER" \
  --desired-status RUNNING --query 'taskArns[0]' --output text)

if [[ -z "$task" || "$task" == "None" ]]; then
  echo "실행 중인 태스크가 없다: 클러스터 $CLUSTER ($REGION)" >&2
  exit 1
fi

ip=$(aws ecs describe-tasks --region "$REGION" --cluster "$CLUSTER" --tasks "$task" \
  --query 'tasks[0].containers[0].networkInterfaces[0].privateIpv4Address' --output text)

if [[ -z "$ip" || "$ip" == "None" ]]; then
  echo "태스크에 사설 IP 가 없다 (아직 프로비저닝 중일 수 있다): $task" >&2
  exit 1
fi

echo "http://${ip}:${PORT}"
