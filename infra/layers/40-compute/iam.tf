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

# 공유 접속 토큰을 주입하기 위한 권한. **실행 롤**이다 — 태스크 롤이 아니다.
#
# 주입은 컨테이너 기동 **전에** ECS 에이전트가 한다. 앱에게 이 권한을 주면 앱이
# 침해됐을 때 자기 자신의 열쇠를 다시 읽을 수 있게 되고, 그건 필요 없는 능력이다.
resource "aws_iam_role_policy" "execution_secrets" {
  count = var.auth_token_secret_arn == "" ? 0 : 1
  name  = "auth-token-secret"
  role  = aws_iam_role.execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["secretsmanager:GetSecretValue"]
      Resource = var.auth_token_secret_arn
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

# **인가·감사 레코드에 대한 쓰기를 명시적으로 거부한다** (08 §5.2, T-20·FR-CRD-10).
#
# # 왜 별도 정책인가
#
# `Deny` 는 어떤 `Allow` 보다 강하다. 아래 `task_storage` 가 config 테이블 전체에
# 쓰기를 허용하는데, 그 안에 두 종류의 특별한 키가 있다:
#
# | 키 | 왜 앱이 쓰면 안 되나 |
# |---|---|
# | `USER#<sub>` | 앱이 침해되면 **자기 권한을 admin 으로 올릴 수 있다** (T-20 의 핵심) |
# | `AUDIT#<yyyy-mm>` | 침해된 앱이 **자기 흔적을 지울 수 있다** (FR-CRD-10 이 무의미해진다) |
#
# 코드는 이미 읽기만 한다(`store::users` 는 쓰기 함수가 없고, `store::audit` 은
# `PutItem` 만 한다). 그런데 **코드가 제한이 아니다** — 태스크 자격증명을 훔치면
# DynamoDB API 를 직접 부를 수 있다. `store/users.rs` 의 주석이 "IAM 에서 Deny 된다"
# 고 적어 놓고 정책이 없었다(교차 리뷰 5차가 잡았다).
#
# `AUDIT#` 은 **`PutItem` 을 허용한다** — 감사 레코드를 앱이 만든다. 대신 수정·삭제를
# 막고, 어댑터의 조건식(`attribute_not_exists`)이 덮어쓰기를 막는다.
resource "aws_iam_role_policy" "task_deny_authz_writes" {
  name = "dbmon-deny-authz-writes"
  role = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "DenyUserRecordWrites"
        Effect = "Deny"
        Action = [
          "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:DeleteItem",
          "dynamodb:BatchWriteItem",
        ]
        Resource = [local.foundation.config_table_arn]
        Condition = {
          # ⚠ **`ForAnyValue` 다. `ForAllValues` 가 아니다.**
          #
          # `ForAllValues` 는 요청의 **모든** 값이 일치할 때만 참이다. Deny 에 쓰면
          # 우회가 된다: `BatchWriteItem` 에 `USER#me` 와 `CFG` 를 섞으면 조건이
          # 거짓이 되어 Deny 가 적용되지 않고, 넓은 Allow 가 그 배치를 통째로
          # 승인한다. **`simulate-principal-policy` 로 재현했다** — 섞은 요청이
          # `allowed` 였다(교차 리뷰 6차).
          #
          # `ForAnyValue` 는 "요청에 하나라도 있으면 거부" 다. 그게 우리가 원하는 것이고,
          # 섞인 배치는 통째로 거부된다 — 안전한 방향이다.
          "ForAnyValue:StringLike" = {
            "dynamodb:LeadingKeys" = ["USER#*"]
          }
        }
      },
      {
        Sid    = "DenyAuditMutation"
        Effect = "Deny"
        # **`PutItem` 은 빠져 있다** — 앱이 감사 레코드를 만든다(단건).
        #
        # `BatchWriteItem` 은 **넣는다.** 그건 삭제도 할 수 있고, 앱은 감사에 배치를
        # 쓰지 않는다(`store::audit` 은 `PutItem` 하나뿐이다). 빼 두면 배치로 감사
        # 기록을 지울 수 있었다(교차 리뷰 6차).
        Action = [
          "dynamodb:UpdateItem", "dynamodb:DeleteItem", "dynamodb:BatchWriteItem",
        ]
        Resource = [local.foundation.config_table_arn]
        Condition = {
          "ForAnyValue:StringLike" = {
            "dynamodb:LeadingKeys" = ["AUDIT#*"]
          }
        }
      },
    ]
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
        # **`GetMetricData` 는 리소스 수준 권한도, 네임스페이스 조건도 쓸 수 없다.**
        #
        # 전에는 `StringEquals { "cloudwatch:namespace" = "AWS/RDS" }` 조건이 붙어 있었다.
        # 그 조건 키는 **이 액션의 요청에 실려 오지 않는다**(`PutMetricData` 와 다르다).
        # 그래서 문이 절대 매치되지 않고 결과는 `implicitDeny` 였다 — 좁히려고 쓴 조건이
        # 기능을 끄고 있었다.
        #
        # 실제 배포에서 드러났다: 화면이 "CloudWatch 값을 가져오지 못했다 — 태스크 롤에
        # cloudwatch:GetMetricData" 를 표시했고, `aws iam simulate-principal-policy` 로
        # 확인했다(조건 컨텍스트를 손으로 주면 `allowed`, 안 주면 `implicitDeny`).
        # 로컬 개발에서는 관리자 자격증명으로 돌아 보이지 않는 부류다.
        #
        # **노출 범위를 정직하게 적는다.** 이 롤은 계정의 **모든** CloudWatch 지표를 읽을
        # 수 있다. IAM 정책으로는 좁힐 수 없으므로, 좁혀야 하면 **권한 경계나 SCP** 를
        # 쓴다 — 이 레이어 밖의 결정이다. 코드가 조회하는 목록은
        # `dbmon_core::cw_metrics` 의 카탈로그가 전부이고 `AWS/RDS` 만 있다.
        #
        # 대안은 `GetMetricStatistics`(네임스페이스 조건이 실제로 걸린다)인데 배치 조회를
        # 잃어 지표당 1호출이 된다 — 500대 × 지표 수만큼 호출이 늘어난다.
        #
        # `ListMetrics` 는 여전히 주지 않는다 — 카탈로그가 코드에 있으므로 목록을 조회할
        # 이유가 없다.
        Sid      = "MetricsRead"
        Effect   = "Allow"
        Action   = ["cloudwatch:GetMetricData"]
        Resource = "*"
      },
      {
        Sid    = "SlowLogRead"
        Effect = "Allow"
        # `DescribeLogStreams` 는 코드가 부르지 않는다 — `FilterLogEvents` 만 쓴다.
        # 쓰지 않는 액션을 남겨 두면 권한 범위가 근거 없이 넓어진다.
        Action = ["logs:FilterLogEvents"]
        # 슬로우로그에는 **SQL 리터럴이 들어간다** — 개인정보가 실릴 수 있다.
        #
        # 열거된 그룹이 있으면 그것만. 없으면 슬로우로그 그룹으로 좁힌다:
        # `/aws/rds/{instance,cluster}/*/slowquery` 는 `/aws/rds/*` 보다 훨씬 좁다 —
        # 후자는 error/general/audit 로그까지 포함한다. 특히 **audit 로그는 모든 문장을
        # 담으므로** 노출 범위가 전혀 다르다.
        #
        # **`cluster` 형태가 있어야 Aurora 가 된다.** Aurora 는 슬로우로그를 클러스터 단위
        # 그룹(`/aws/rds/cluster/<클러스터>/slowquery`)에 쓴다 — 인스턴스 형태만 두면 코드가
        # 맞아도 권한이 막는다. 그 반대(코드가 인스턴스 이름만 만들던 것)가 실제 결함이었다.
        Resource = length(var.slowlog_log_group_arns) > 0 ? var.slowlog_log_group_arns : [
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/instance/*/slowquery:*",
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/instance/*/slowquery",
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/cluster/*/slowquery:*",
          "arn:aws:logs:${var.region}:${local.account_id}:log-group:/aws/rds/cluster/*/slowquery"
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

# ─────────────────────────────────────────────────────────────────────────────
# 부트스트랩 마스터 자격증명 (M3, docs/07-credentials-bootstrap.md §1)
# ─────────────────────────────────────────────────────────────────────────────

# **이 정책이 이 태스크에게 가장 큰 능력을 준다.** 대상 DB 의 마스터 비밀번호를 읽고,
# 그것으로 `CREATE USER`/`GRANT` 를 실행할 수 있다.
#
# # 두 경로만 열고, 각각 다르게 좁힌다
#
# | 경로 | 리소스 | 왜 이렇게 좁히나 |
# |---|---|---|
# | (a) RDS 관리형 | `secret:rds!*` | RDS 가 만드는 이름 규칙이다. 사람이 만든 비밀은 이 접두어를 쓸 수 없다 |
# | (b) 지정 ARN | 태그 조건 `dbmon=true` | 임의 ARN 을 받으면 이 앱이 계정의 모든 비밀을 읽는 도구가 된다 |
#
# 앱도 같은 태그를 확인한다(`bootstrap::secret::is_tagged_for_dbmon`) — 정책이
# 느슨해져도 코드가 한 겹 막고, 코드가 뚫려도 정책이 막는다.
#
# # `*` 로 열지 않는다
#
# `secretsmanager:GetSecretValue` 를 `*` 로 주면 이 태스크가 침해됐을 때 계정의
# 모든 비밀(다른 서비스의 API 키, 다른 DB 의 마스터 암호)이 함께 나간다.
resource "aws_iam_role_policy" "task_bootstrap" {
  count = var.enable_bootstrap ? 1 : 0
  name  = "dbmon-bootstrap"
  role  = aws_iam_role.task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "ReadRdsManagedMasterSecret"
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue", "secretsmanager:DescribeSecret"]
        Resource = ["arn:aws:secretsmanager:${var.region}:${local.account_id}:secret:rds!*"]
      },
      {
        Sid      = "ReadTaggedMasterSecret"
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue", "secretsmanager:DescribeSecret"]
        Resource = ["arn:aws:secretsmanager:${var.region}:${local.account_id}:secret:*"]
        Condition = {
          StringEquals = { "secretsmanager:ResourceTag/dbmon" = "true" }
        }
      },
      # 관리형 시크릿이 고객 관리 KMS 키로 암호화돼 있으면 복호화 권한이 필요하다.
      # `ViaService` 로 좁혀서 이 키를 다른 용도로 쓸 수 없게 한다.
      {
        Sid      = "DecryptSecretsManagerKeys"
        Effect   = "Allow"
        Action   = ["kms:Decrypt"]
        Resource = ["*"]
        Condition = {
          StringEquals = {
            "kms:ViaService" = "secretsmanager.${var.region}.amazonaws.com"
          }
        }
      },
    ]
  })
}

# ⚠ **`ModifyDBInstance` 를 주지 않는다** (NFR-S-09, 07 §2.1).
#
# IAM DB 인증이 꺼진 인스턴스는 계획이 **CLI 명령을 보여주고** 사람이 실행한다.
# 앱이 프로덕션 RDS 를 변경할 권한을 기본으로 갖지 않는 것이 그 결정이다.
# 정책을 추가하려면 `bootstrap.allow_rds_modify` 설정과 함께 여기에 문장을 더한다.
