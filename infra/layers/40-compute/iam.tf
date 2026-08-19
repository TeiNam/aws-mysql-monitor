# IAM — **실행 역할과 태스크 역할을 분리한다.**
#
# | 역할 | 누가 쓰는가 | 무엇을 하는가 |
# |---|---|---|
# | 실행 역할 (`execution`) | **ECS 에이전트** | ECR 풀, 로그 그룹 쓰기, Secrets 주입 |
# | 태스크 역할 (`task`) | **우리 프로세스** | DynamoDB, S3, RDS 탐색, CloudWatch |
#
# 분리하는 이유: 실행 역할은 컨테이너 기동 전에 쓰이므로 우리 코드가 침해돼도
# 그 권한을 쓸 수 없다. 하나로 합치면 앱이 ECR 과 Secrets 전체에 접근하게 된다.

# ─────────────────────────────────────────────────────────────────────────────
# 실행 역할
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_iam_role" "execution" {
  name = "${local.name}-ecs-execution"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        # 혼동된 대리인(confused deputy) 방어. 다른 계정의 ECS 가 이 역할을 못 쓰게 한다.
        StringEquals = { "aws:SourceAccount" = local.account_id }
      }
    }]
  })
}

resource "aws_iam_role_policy_attachment" "execution_managed" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# ECR 이미지가 CMK 로 암호화돼 있으므로 풀 시점에 복호화 권한이 필요하다.
resource "aws_iam_role_policy" "execution_kms" {
  name = "ecr-kms-decrypt"
  role = aws_iam_role.execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["kms:Decrypt"]
      Resource = local.foundation.kms_key_arn
      Condition = {
        StringEquals = { "kms:ViaService" = "ecr.${var.region}.amazonaws.com" }
      }
    }]
  })
}

# ─────────────────────────────────────────────────────────────────────────────
# 태스크 역할 — 우리 프로세스의 권한
# ─────────────────────────────────────────────────────────────────────────────
resource "aws_iam_role" "task" {
  name = "${local.name}-ecs-task"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = { "aws:SourceAccount" = local.account_id }
      }
    }]
  })
}

resource "aws_iam_role_policy" "task_storage" {
  name = "dbmon-storage"
  role = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "DynamoDbData"
        Effect = "Allow"
        Action = [
          "dynamodb:GetItem", "dynamodb:BatchGetItem", "dynamodb:Query",
          "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:BatchWriteItem",
          "dynamodb:DeleteItem", "dynamodb:DescribeTable",
        ]
        # 인덱스까지 명시한다. `Resource: "*"` 로 두면 계정의 다른 테이블에 접근할 수 있고,
        # 이 계정에는 프로덕션 워크로드가 함께 있다 (T-37).
        Resource = [
          local.foundation.data_table_arn,
          "${local.foundation.data_table_arn}/index/*",
          local.foundation.config_table_arn,
          "${local.foundation.config_table_arn}/index/*",
        ]
      },
      {
        # `Scan` 을 주지 않는다. [ADR-003](../../../docs/03-decisions.md) 이 금지한 접근이고,
        # 실수로 전체 스캔을 하면 비용이 폭발한다. 권한으로 막는다.
        Sid      = "DenyScan"
        Effect   = "Deny"
        Action   = ["dynamodb:Scan"]
        Resource = "*"
      },
      {
        Sid      = "PlanOffload"
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:CopyObject"]
        Resource = "arn:aws:s3:::${local.foundation.plan_bucket}/*"
      },
      {
        Sid      = "PlanBucketList"
        Effect   = "Allow"
        Action   = ["s3:ListBucket"]
        Resource = "arn:aws:s3:::${local.foundation.plan_bucket}"
      },
      {
        Sid      = "KmsUse"
        Effect   = "Allow"
        Action   = ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"]
        Resource = local.foundation.kms_key_arn
        Condition = {
          # **`StringLike` 를 쓴다.** DynamoDB 는 `dynamodb.<region>.amazonaws.com` 이지만
          # S3 는 요청 경로에 따라 값이 달라질 수 있어 `StringEquals` 로 좁히면
          # 프로덕션에서만 거부되는 함정이 생긴다 (ADR-012).
          StringLike = {
            "kms:ViaService" = [
              "dynamodb.${var.region}.amazonaws.com",
              "s3.${var.region}.amazonaws.com",
            ]
          }
        }
      },
    ]
  })
}

resource "aws_iam_role_policy" "task_discovery" {
  name = "dbmon-discovery"
  role = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # ⚠ **RDS Describe 계열은 리소스 수준 권한을 지원하지 않는다.**
        # `Resource: "*"` 가 불가피하며, 그래서 **이 계정의 prd 인스턴스도 보인다**(T-37).
        #
        # 앱 쪽 `discovery.allowed_vpc_ids` 필터가 마지막 방어선이고,
        # dev 배포는 그 값 없이 기동을 거부한다.
        Sid    = "DiscoveryReadOnly"
        Effect = "Allow"
        Action = [
          "rds:DescribeDBInstances",
          "rds:DescribeDBClusters",
          "rds:DescribeDBParameters",
          "rds:DescribeDBParameterGroups",
          "rds:DescribeDBClusterParameters",
          "rds:DescribeEvents",
          "rds:DescribePendingMaintenanceActions",
          "rds:ListTagsForResource",
        ]
        Resource = "*"
      },
      {
        Sid      = "Metrics"
        Effect   = "Allow"
        Action   = ["cloudwatch:GetMetricData", "cloudwatch:ListMetrics"]
        Resource = "*"
      },
      {
        Sid      = "PutOwnMetrics"
        Effect   = "Allow"
        Action   = ["cloudwatch:PutMetricData"]
        Resource = "*"
        Condition = {
          # 우리 네임스페이스에만 쓴다. 다른 팀의 지표를 오염시킬 수 없게.
          StringEquals = { "cloudwatch:namespace" = "dbmon" }
        }
      },
      {
        Sid    = "SlowLogRead"
        Effect = "Allow"
        Action = ["logs:FilterLogEvents", "logs:DescribeLogStreams"]
        # 슬로우로그에는 SQL 리터럴이 들어간다. 비-prd 는 열거를 강제한다
        # (`slowlog_log_group_arns` 의 validation).
        Resource = length(var.slowlog_log_group_arns) > 0 ? var.slowlog_log_group_arns : [
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/*"
        ]
      },
    ]
  })
}

# IAM DB 인증 토큰. **비밀번호를 저장하지 않는 근거**다 (ADR-007).
#
# `dbuser:*/dbmon` 는 "모든 인스턴스의 dbmon 계정" 을 뜻한다. prd 인스턴스도 포함되므로
# dev 에서는 `var.db_auth_resource_ids` 로 열거해 좁힌다 (T-37, [18 §6](../../../docs/18-dev-environment.md)).
resource "aws_iam_role_policy" "task_db_auth" {
  name = "dbmon-db-auth"
  role = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = ["rds-db:connect"]
      Resource = length(var.db_auth_resource_ids) > 0 ? [
        for id in var.db_auth_resource_ids :
        "arn:aws:rds-db:${var.region}:${local.account_id}:dbuser:${id}/${var.monitor_db_user}"
        ] : [
        "arn:aws:rds-db:${var.region}:${local.account_id}:dbuser:*/${var.monitor_db_user}"
      ]
    }]
  })
}

# ECS Exec — 컨테이너에 셸로 붙는다. **dev 에서만 켠다.**
resource "aws_iam_role_policy" "task_exec_channel" {
  count = var.enable_ecs_exec ? 1 : 0
  name  = "dbmon-ecs-exec"
  role  = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "ssmmessages:CreateControlChannel",
        "ssmmessages:CreateDataChannel",
        "ssmmessages:OpenControlChannel",
        "ssmmessages:OpenDataChannel",
      ]
      Resource = "*"
    }]
  })
}
