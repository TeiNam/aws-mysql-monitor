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
      # S3 플랜 오프로드 권한을 주지 않는다.
      #
      # 코드에 그 경로가 없다 — `storage.plan_bucket` 은 선언만 있고 읽는 곳이 없어서
      # 지웠다(교차 리뷰가 잡았다). 쓰지 않는 권한을 남겨 두면 침해 시 열람 범위만
      # 넓어진다. 오프로드를 구현할 때 이 문을 다시 넣는다.
      #
      # 버킷(`10-foundation`)은 **그대로 둔다** — 지우면 데이터가 사라진다.
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
      # ⚠ 여기에 조건 없는 `cloudwatch:GetMetricData` 문을 두지 않는다.
      #
      # 예전에 `Sid = "Metrics"` 로 `GetMetricData` + `ListMetrics` 를 조건 없이
      # 허용하는 문이 아래 `MetricsRead` **앞에** 있었다. IAM 은 허용의 합집합이므로
      # 뒤 문의 네임스페이스 조건이 아무것도 좁히지 못했고 — 좁히려고 쓴 주석만 남아
      # 있었다(교차 리뷰가 잡았다). 조건을 걸려면 **넓은 문이 없어야** 한다.
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
        # **`GetMetricData` 는 리소스 수준 권한을 지원하지 않는다** — `rds:Describe*` 와
        # 같은 사정이다. 대신 네임스페이스 조건으로 좁힌다: 이 롤은 `AWS/RDS` 메트릭만
        # 읽는다. 조건이 없으면 계정의 모든 메트릭(비용·보안 지표 포함)이 읽힌다.
        #
        # `ListMetrics` 는 주지 않는다 — 카탈로그는 코드가 갖고 있다
        # (`dbmon_core::cw_metrics`). 목록을 조회할 이유가 없다.
        Sid      = "MetricsRead"
        Effect   = "Allow"
        Action   = ["cloudwatch:GetMetricData"]
        Resource = "*"
        Condition = {
          StringEquals = { "cloudwatch:namespace" = "AWS/RDS" }
        }
      },
      {
        Sid    = "SlowLogRead"
        Effect = "Allow"
        # `DescribeLogStreams` 는 코드가 부르지 않는다 — `FilterLogEvents` 만 쓴다.
        # 쓰지 않는 액션을 남겨 두면 권한 범위가 근거 없이 넓어진다.
        Action = ["logs:FilterLogEvents"]
        # 슬로우로그에는 **SQL 리터럴이 들어간다** — 개인정보가 실릴 수 있다.
        #
        # 열거된 그룹이 있으면 그것만. 없으면 인스턴스 슬로우로그 그룹으로 좁힌다:
        # `/aws/rds/instance/*/slowquery` 는 `/aws/rds/*` 보다 훨씬 좁다 —
        # 후자는 error/general/audit 로그와 클러스터 로그까지 포함한다.
        # 특히 **audit 로그는 모든 문장을 담으므로** 노출 범위가 전혀 다르다.
        Resource = length(var.slowlog_log_group_arns) > 0 ? var.slowlog_log_group_arns : [
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/instance/*/slowquery:*",
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/instance/*/slowquery"
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

# ─────────────────────────────────────────────────────────────────────────────
# 크로스 계정 탐색 (`settings.discovery.accounts`)
# ─────────────────────────────────────────────────────────────────────────────

# **역할 이름으로 좁힌다.** `sts:AssumeRole` 을 `*` 로 열면 이 태스크가 신뢰 정책만
# 맞는 아무 역할이나 맡을 수 있고, 그건 계정 경계를 무의미하게 만든다.
#
# 목록이 비어 있으면 정책 자체를 만들지 않는다 — 쓰지 않는 권한을 두지 않는다
# (멀티 계정을 쓰지 않는 배포가 기본이다).
#
# 대상 계정 쪽에는 같은 이름의 역할이 있어야 하고, 그 신뢰 정책이 이 태스크 롤만
# 허용해야 한다. 권한은 `rds:Describe*` + `rds:ListTagsForResource` +
# `cloudwatch:GetMetricData` 로 충분하다 — **DB 접속은 별 경로**(rds-db:connect)다.
resource "aws_iam_role_policy" "task_cross_account" {
  count = length(var.discovery_account_ids) > 0 ? 1 : 0
  name  = "dbmon-cross-account-discovery"
  role  = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid    = "AssumeDiscoveryRole"
      Effect = "Allow"
      Action = ["sts:AssumeRole"]
      Resource = [
        for id in var.discovery_account_ids :
        "arn:aws:iam::${id}:role/${var.discovery_role_name}"
      ]
    }]
  })
}

# ─────────────────────────────────────────────────────────────────────────────
# AI 튜닝 (Bedrock) + 알림 채널 비밀
# ─────────────────────────────────────────────────────────────────────────────

# **모델을 열거한다.** `bedrock:InvokeModel` 을 `*` 로 열면 이 태스크가 계정의 모든
# 모델을 부를 수 있고, 그중에는 이미지·비디오 모델처럼 비용 단가가 전혀 다른 것도 있다.
#
# 교차 리전 추론 프로파일(`global.*`·`apac.*`)은 **프로파일 ARN 과 그것이 라우팅하는
# 기반 모델 ARN 둘 다** 필요하다 — 프로파일만 허용하면 호출 시점에 AccessDenied 다.
resource "aws_iam_role_policy" "task_bedrock" {
  count = var.enable_ai_tuning ? 1 : 0
  name  = "dbmon-bedrock"
  role  = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid    = "InvokeTuningModel"
      Effect = "Allow"
      Action = ["bedrock:InvokeModel"]
      Resource = concat(
        [
          for id in var.bedrock_model_ids :
          "arn:aws:bedrock:${var.region}:${local.account_id}:inference-profile/${id}"
        ],
        [
          # 프로파일이 라우팅하는 기반 모델. 리전을 `*` 로 두는 이유가 교차 리전이다 —
          # 프로파일이 어느 리전으로 보낼지는 AWS 가 정한다.
          for id in var.bedrock_model_ids :
          "arn:aws:bedrock:*::foundation-model/${replace(replace(replace(id, "global.", ""), "apac.", ""), "us.", "")}"
        ]
      )
    }]
  })
}

# 알림 채널 비밀 (`settings.notify.slack_secret`).
#
# **값을 설정에 담지 않는 대가**로 이 권한이 필요하다([10 §3.4]). 접두어로 좁힌다 —
# `*` 로 열면 이 태스크가 계정의 모든 비밀(DB 마스터 암호 포함)을 읽을 수 있다.
resource "aws_iam_role_policy" "task_channel_secrets" {
  count = var.enable_notifications ? 1 : 0
  name  = "dbmon-channel-secrets"
  role  = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid      = "ReadChannelSecret"
      Effect   = "Allow"
      Action   = ["secretsmanager:GetSecretValue"]
      Resource = ["arn:aws:secretsmanager:${var.region}:${local.account_id}:secret:${var.channel_secret_prefix}*"]
    }]
  })
}
