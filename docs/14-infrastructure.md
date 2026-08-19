# 14. 인프라와 운영

> **⚠ 배포 방식이 ECS Fargate 로 바뀌었다** ([ADR-022](03-decisions.md), 2026-08-20).
> 이 문서의 EC2 ASG · systemd · Launch Template · `sd_notify` · Lifecycle Hook ·
> `SetInstanceHealth` 관련 절은 **더 이상 적용되지 않는다.**
> 실제 구성은 [`infra/layers/40-compute`](../infra/layers/40-compute) 와
> [`Dockerfile`](../Dockerfile) 이 정본이다.
>
> | 이 문서의 서술 | 실제 |
> |---|---|
> | EC2 ASG (min2/desired2) | ECS 서비스 `desired_count = 2` |
> | systemd `Type=notify` + `sd_notify` | 태스크 정의 `healthCheck` → `/healthz` |
> | ASG Lifecycle Hook `Terminating:Wait` | `stopTimeout = 60` (Fargate 상한 120초) |
> | `SetInstanceHealth` 자체 판정 | 프로세스 종료 → ECS 가 태스크 교체 |
> | CloudWatch agent | `awslogs` 로그 드라이버 |
> | AMI 빌드·패치 | 컨테이너 이미지 재빌드 |
>
> **두 헬스체크의 역할이 다르다.** 컨테이너 헬스체크는 `/healthz`(항상 200)를 보고,
> 대상 그룹 헬스체크는 `/readyz` 를 본다. 컨테이너 쪽이 `/readyz` 를 보면 standby 를
> 계속 죽이는 무한 루프가 난다(F1).
>
> 나머지(DynamoDB·S3·KMS·알람·런북·재해복구)는 그대로 유효하다.

## 1. Terraform 구성

**인프라 배포는 애플리케이션 코드와 독립적으로 동작한다.** 레이어를 나누고 각 레이어에
**독립 state**를 둔다. 앱 바이너리가 없어도 대부분의 레이어를 apply할 수 있고,
필요한 시점에 필요한 레이어만 올린다.

```
infra/
  layers/
    00-bootstrap/       # TF state 백엔드 자체 (S3 + DynamoDB 락). local state 로 1회
    10-foundation/      # 네트워크 조회(data), 게이트웨이 엔드포인트, KMS, S3, DynamoDB
    20-data/            # S3 Tables 네임스페이스·테이블, Glue DB, Athena 워크그룹
    30-identity/        # Cognito, IAM Role·정책
    40-compute/         # ALB, ASG, Launch Template  ← 유일하게 아티팩트가 있으면 좋은 레이어
    50-observability/   # 로그그룹, 메트릭 필터, 알람, SNS, Budgets
    60-seed/            # dev 전용: 시드 MySQL + 부하 생성기
  modules/
    network/        # 선택: VPC 없으면 생성. 있으면 data source.
                    #   크로스 리전 패브릭(피어링/TGW)은 이 프로젝트 범위 밖 — §3.4
    iam/            # 앱 Role + Instance Profile + 정책 (08 §5.1)
                    #   + auth-admin Role (분리, AssumeRole 대상)
                    #   + CI Role (GitHub OIDC) — §6.1
                    #   + 로테이션 Lambda Role (폴백 경로용)
                    #   + SSM 관리형 정책 (AmazonSSMManagedInstanceCore)
    dynamodb/       # dbmon-data, dbmon-config (PITR, TTL, GSI, CMK)
    storage/        # S3 버킷 8개 (§2) + S3 Tables 네임스페이스·테이블 9개
    athena/         # 워크그룹 2개(사용자 조회 / 아카이브·리포트), Glue DB, 결과 버킷 정책
    cognito/        # User Pool, App Client, Domain, Groups, 초기 admin,
                    #   Pre-Token Lambda (V2 필요 여부는 OPEN-Q-18)
    compute/        # Launch Template, ASG(+enabled_metrics, Lifecycle Hook),
                    #   ALB, Target Group, ACM, WAF
    observability/  # CloudWatch Log Group, 로그 메트릭 필터, 대시보드, 알람,
                    #   SNS 토픽·구독·토픽 정책, AWS Budgets
    bedrock/        # Guardrail
    secrets/        # 로테이션 Lambda + 함수 (폴백 경로용)
  envs/
    dev.tfvars
    prd.tfvars
```

| 레이어 | 앱 코드 필요? | 단독 apply 시 얻는 것 |
|---|---|---|
| `00-bootstrap` | ✗ | state 백엔드 |
| `10-foundation` | ✗ | DynamoDB·S3·KMS·엔드포인트. **로컬 개발이 실제 AWS 저장소를 쓴다** |
| `20-data` | ✗ | Iceberg 테이블·Athena 워크그룹. 콘솔에서 직접 쿼리 가능 |
| `30-identity` | ✗ | Cognito·IAM. **로컬 SPA가 실제 로그인 플로우를 쓴다** |
| `40-compute` | 있으면 좋음 | ASG·ALB. `desired_capacity=0`으로도 apply 가능 |
| `50-observability` | ✗ | 로그 그룹·알람·예산 |
| `60-seed` | ✗ | 시드 MySQL — **모니터링 대상이 생긴다** |

**닭·달걀 문제가 레이어 분리로 해소된다.** 초기 설계(§6.4)는 "ASG가 아티팩트를 못 찾아
부팅 루프"를 `asg_desired=0` 트릭으로 다뤘다. 레이어를 나누면 `40-compute`를 나중에
apply하면 되므로 그 트릭이 불필요하다.

**레이어 간 참조는 `terraform_remote_state`** — 레이어가 7개뿐이고 의존이 단방향이다.
SSM Parameter Store로 느슨하게 결합하는 것이 더 깔끔하지만 배선이 늘어난다.
레이어가 늘거나 팀이 커지면 그때 전환한다.
state 파일에 민감 정보(Cognito client_id, KMS ARN)가 들어가므로 state 버킷을
SSE-KMS + 버전 관리 + 퍼블릭 차단으로 만들고 읽기 권한을 통제한다.

**기존 네트워크는 조회만 한다 (관리하지 않는다)** — VPC·서브넷·라우트 테이블·IGW를
Terraform이 관리하면 `destroy` 시 다른 워크로드를 지울 수 있다. `data` source로만 참조한다.
개발계 계정의 실제 구성과 그에 따른 결정은 [18-dev-environment.md](18-dev-environment.md).

- 상태: S3 backend + DynamoDB 락. 환경별 key 분리(`dev/10-foundation.tfstate`).
- 프로바이더 버전 고정. `terraform.lock.hcl` 커밋.
- `default_tags`로 `Project=dbmon`·`Environment`·`TFLayer`를 전 리소스에 강제.
  **prd 워크로드와 계정을 공유하면 태그 없이는 비용을 분리할 수 없다.**
- `prevent_destroy = true`: KMS 키, DynamoDB 테이블, S3 Tables 테이블 버킷,
  리포트·아티팩트 버킷, Cognito User Pool.
- `prd`는 `terraform plan` 결과를 PR에 붙이고 승인 후 apply(CI 게이트).
- CDK를 쓰지 않는 이유: 이 인프라는 정적이고 리소스 종류가 명확하다. Terraform의
  `plan` 출력이 리뷰하기 쉽다. 팀 표준이 CDK면 바꿔도 된다.

## 2. S3 버킷

| 버킷 | 용도 | Lifecycle | 버전 관리 |
|---|---|---|---|
| `dbmon-raw-<acct>` | DynamoDB 내보내기 산출물. `AWSDynamoDB/` 는 **공유 폴더라 7일 만료 불가** ([04 §4.3.1](04-data-model.md)) | `AWSDynamoDB/` 90일, `staging/` 은 잡이 직접 삭제 | 비활성 |
| `dbmon-tables-<acct>` | S3 Tables 테이블 버킷 | 관리형 유지보수 (스냅샷 보존 **최소 7일** 명시) | — |
| `dbmon-athena-results-<acct>` | Athena 쿼리 결과 | 7일 만료 | 비활성 |
| `dbmon-reports-<acct>` | 생성된 리포트 HTML·JSON | 3년 | 활성 |
| `dbmon-plans-<acct>` | 300KB 초과 플랜 오프로드. **키에 만료 티어 포함** (F27) | `ttl35/` 40일, `ttl400/` 405일 | 비활성 |
| `dbmon-artifacts-<acct>` | **릴리스 바이너리·프론트 자산** (User Data가 다운로드) | 최근 10개 버전 유지 | 활성 |
| `dbmon-alb-logs-<acct>` | ALB 액세스 로그 | 90일 | 비활성 |
| `dbmon-s3-logs-<acct>` | 위 버킷들의 액세스 로그 (자기 자신은 제외 — 루프 방지) | 90일 | 비활성 |

공통: 퍼블릭 차단, SSE-KMS, `aws:SecureTransport=false` 거부, 비암호화 업로드 거부,
액세스 로깅(`dbmon-s3-logs-<acct>` 로).

**앱 Role의 S3 권한은 이 8개 버킷 ARN을 열거하고 `aws:ResourceAccount` 조건을 붙인다.**
`arn:aws:s3:::dbmon-*` 와일드카드는 S3 버킷 이름이 **전역 네임스페이스**이므로
다른 계정의 `dbmon-xxx` 버킷에도 쓰기를 허용한다(T-25).
`dbmon-alb-logs-*`와 `dbmon-reports-*`는 `DeleteObject`를 부여하지 않는다(증거 파괴 방지).

## 3. 컴퓨트

### 3.1 1단계 (인스턴스 ~150대)

```
Launch Template
  AMI          Amazon Linux 2023 (ARM64)
  인스턴스 타입 c7g.large (2 vCPU, 4GB)
  IMDSv2       required, hop limit 1
  EBS          gp3 30GB, 암호화, 삭제 시 삭제
  User Data    S3에서 바이너리 다운로드 → systemd 유닛 설치 → 시작
  IAM          dbmon-instance-profile

ASG
  min 2 / desired 2 / max 3          ← 1대는 active, 1대는 standby (ADR-018 1단계)
  헬스체크    ELB + EC2, grace 120초
  종료 정책    OldestLaunchTemplate
  enabled_metrics  GroupInServiceInstances 등 (기본 미수집 → 알람이 데이터 없이 뜬다)
  Lifecycle Hook   autoscaling:EC2_INSTANCE_TERMINATING, heartbeat 120초
  인스턴스 리프레시  MinHealthyPercentage=50, InstanceWarmup=120

ALB
  HTTPS 443 (ACM), HTTP → HTTPS 리다이렉트
  대상 그룹    포트 8080, 헬스체크 **/readyz** (간격 15s, 임계 2, 비정상 임계 2)
  등록 해제 지연 60초  (>= TimeoutStopSec 45초)
  스티키 세션  사용하지 않는다
  유휴 타임아웃 300초 (WS 유지)
  WAF          AWS 관리형 규칙 + 레이트 기반 규칙
               부트스트랩 경로는 본문 검사 제외 (T-30)
```

**active / standby 구조** — [ADR-018](03-decisions.md) 1단계에서 수집은 리더 리스를 잡은
1대만 한다. standby는 `/readyz`를 **503**으로 응답해 ALB 대상에서 자동으로 빠진다.
따라서 클라이언트는 항상 active 노드에 붙고, WS 팬아웃 문제가 발생하지 않는다.
→ **스티키 세션이 불필요하다.** (초기 설계는 "실시간 지표 링버퍼를 같은 워커에서 읽으려면
스티키가 필요하다"고 했지만, active가 1대면 그 근거가 성립하지 않는다.)

**헬스체크 경로를 `/readyz`로 한다** — `/healthz`는 [13 §2.1](13-api-spec.md)에서 "200 고정"으로
정의되므로 DynamoDB·Cognito가 죽어도 InService로 남는다. ELB 헬스체크가 무의미해진다.
- `/healthz`: 프로세스 생존만. systemd 워치독용
- `/readyz`: 의존성 확인 + **리더 여부**. active면 200, standby면 503

**collector / control ASG는 ALB에 연결되지 않는다** → EC2 헬스체크(하이퍼바이저 수준)만
적용되므로 프로세스가 죽거나 systemd가 재시작을 포기해도 인스턴스는 healthy로 남는다.
→ **자체 unhealthy 판정**을 넣는다: 리스 보유 실패나 `CollectStaleness` 초과가 5분 지속되면
`autoscaling:SetInstanceHealth`로 스스로 Unhealthy를 선언한다(ASG가 교체).

### 3.2 2단계 (~500대)

```
ASG-api        c7g.large  × 2   --role api        ALB 대상
ASG-collector  c7g.xlarge × 4   --role collector  ALB 미연결
ASG-control    c7g.medium × 1   --role control    ALB 미연결
```

이 단계에서 [OPEN-Q-04](OPEN-QUESTIONS.md)(워커 간 실시간 이벤트 팬아웃)를 해결해야 한다.

### 3.3 systemd 유닛

```ini
[Unit]
Description=dbmon
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
WatchdogSec=30
ExecStart=/opt/dbmon/bin/dbmon serve
Restart=always
RestartSec=5
StartLimitIntervalSec=300
StartLimitBurst=5
User=dbmon
Group=dbmon
EnvironmentFile=/etc/dbmon/env
StateDirectory=dbmon
LogsDirectory=dbmon

# 셧다운: 리스 반납 + 버퍼 플러시 시간 확보
KillSignal=SIGTERM
TimeoutStopSec=45

# 강화
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
ProtectProc=invisible
ProcSubset=pid
RestrictSUIDSGID=true
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
MemoryHigh=2.5G
MemoryMax=3G
LimitNOFILE=65536
LimitCORE=0
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
```

**`Type=notify`를 쓰면 앱이 `sd_notify(READY=1)`을 보내야 한다.**
보내지 않으면 systemd가 `TimeoutStartSec`(기본 90초)까지 기다린 뒤 **기동 실패로 판정하고
`Restart=always`로 무한 재시작**한다. ASG 헬스체크 grace와 겹쳐 Instance Refresh가 롤백
루프에 빠진다.
→ M0-8 수용 기준: **`sd_notify(READY=1)` 전송 후 `/readyz` 200**.
`WatchdogSec=30`을 켰으므로 15초마다 `sd_notify(WATCHDOG=1)`도 보내야 한다.
구현이 부담스러우면 **`Type=simple`로 시작**하고 워치독을 나중에 붙인다.

**설정 근거**

| 항목 | 이유 |
|---|---|
| `LimitCORE=0` | 패닉·OOM 시 코어 덤프에 마스터 자격증명이 평문으로 들어갈 수 있다 (T-30) |
| `LimitMEMLOCK=infinity` | 비밀 페이지를 `mlock`해 스왑으로 새는 것을 막는다 |
| `ProtectProc=invisible` / `ProcSubset=pid` | SSM 세션에서 `/proc/<pid>/mem` 접근 차단 |
| `RestrictAddressFamilies`에 **`AF_NETLINK` 포함** | glibc `getaddrinfo`가 netlink로 인터페이스·IPv6 지원을 조회한다. 빼면 **DNS 해석이 실패하거나 열화된다** (하드닝 대표 함정) |
| `MemoryHigh=2.5G` + `MemoryMax=3G` | `MemoryMax` 단독은 초과 시 **커널이 프로세스를 죽인다.** `MemoryHigh`가 스로틀·reclaim으로 압박을 만들어 앱이 PSI(`memory.pressure`)로 감지할 수 있게 한다 |
| `StateDirectory` / `LogsDirectory` | systemd가 디렉터리와 소유권을 만들어 준다. `ReadWritePaths` 수동 지정 + User Data에서의 디렉터리 생성이 불필요해진다 |
| `LimitNOFILE=65536` | 인스턴스 500대 × 연결 3개 + HTTP/WS 소켓 |

User Data는 `dbmon` 사용자·그룹 생성, 아티팩트 다운로드, 유닛 설치, `systemctl enable --now`만
한다(디렉터리 생성은 systemd가 담당).

### 3.4 네트워크

```
앱 서브넷        프라이빗. NAT 게이트웨이 경유 아웃바운드
ALB 서브넷       퍼블릭 (또는 인터널 ALB + 사내 접근)
보안 그룹
  ALB     인바운드 443 ← 허용된 CIDR (사내 IP 대역 또는 0.0.0.0/0 + WAF)
  앱      인바운드 8080 ← ALB SG. 아웃바운드 전체
  대상 DB  인바운드 3306 ← 앱 SG  ← 이 규칙 추가가 대상 DB 쪽에 필요하다
```

**대상 RDS 접근** — 앱 SG를 각 대상 RDS의 SG에 인바운드로 추가해야 한다.

| 배치 | 방법 | 난도 |
|---|---|---|
| 같은 VPC | SG 참조 (`source_security_group_id`) | 낮음 |
| 같은 리전 다른 VPC (피어링) | SG 참조 가능 | 낮음 |
| **다른 리전 (피어링)** | **SG 참조 불가.** 우리 서브넷 CIDR을 허용해야 한다 | **높음** — CIDR 허용은 승인이 어렵다 |
| 다른 리전 (TGW) | CIDR 허용 | 높음 + 비용 ([16 §2.9](16-cost.md)) |

**이 작업은 Terraform 모듈이 하지 않는다.** 대상 RDS의 SG를 우리가 변경하는 것은
프로덕션 변경이다. 필요한 규칙을 **복사해서 실행할 수 있는 Terraform/CLI 스니펫**으로
생성해 대상 팀에게 준다.

**요청 상태를 추적한다** — 500대 규모에서 "UI가 보여주고 대상 팀이 적용"만으로는 관리되지
않는다. 인스턴스 레지스트리에 `network_access_state`를 둔다.

```
none        → 아직 요청하지 않음
requested   → 요청함 (requested_at, requested_to)
granted     → 적용 확인됨 (자가진단 통과)
regressed   → 되던 것이 안 되게 됨 (SG 규칙 삭제 등)
```
- `unreachable`이면서 상태가 `requested`인 인스턴스는 **알림을 억제**한다
  (매시간 같은 알림이 오면 소음이 된다).
- 요청 후 14일이 지나면 에스컬레이션(알림 1회 + 목록에 강조).
- `granted` → `regressed` 전이는 즉시 알림(누가 규칙을 지웠다는 신호).

**VPC 엔드포인트** — [16 §2.1](16-cost.md)의 비용 분석에 따라 **기본은 게이트웨이만** 둔다.
- **게이트웨이(무료)**: S3, DynamoDB → 반드시 둔다
- **인터페이스(시간당 과금)**: 기본 없음. 트래픽이 커진 것만 손익분기 계산 후 추가
- **SSM 접근** — 프라이빗 서브넷의 인스턴스에 SSM Session Manager로 접속하려면
  `ssm`, `ssmmessages`, `ec2messages` 엔드포인트가 필요하거나 **NAT 경유**여야 한다.
  우리는 NAT가 있으므로 엔드포인트 없이 동작한다. 다만
  **`AmazonSSMManagedInstanceCore` 정책을 Instance Profile에 붙여야 한다**
  (초기 IAM 설계에서 빠져 있었다). Session Manager 세션 로그는 CloudWatch Logs
  `/dbmon/ssm-sessions`에 남기고 감사 대상으로 둔다.
- **엔드포인트는 리전 로컬이다.** 멀티 리전 대상의 AWS API 호출은 여전히 NAT를 탄다.

### 3.5 ECS/Fargate 대안

같은 바이너리 + Dockerfile로 ECS Fargate 배포가 가능하다. IAM은 Task Role로 받는다
(코드는 자격증명 공급자 체인을 그대로 쓰므로 변경 없음).

EC2를 기본으로 하는 이유는 요구사항이 EC2이기 때문이다. Fargate가 나은 점:
패치 관리 불필요, 스케일링 단순. 나쁜 점: 시간당 비용 높음, 로컬 디스크 캐시 없음.
전환 시 `compute` 모듈만 교체하면 된다.

## 4. 스케줄 잡

**EventBridge Scheduler를 쓰지 않고 앱 내부 cron을 쓴다.**
이유: EventBridge → HTTP 호출은 인증·재시도·멱등성을 또 설계해야 한다.
앱 내부 cron + DynamoDB 리더 리스가 더 단순하다.
→ 따라서 Terraform 모듈 목록에서 `scheduler/`를 **삭제했다**(초기 설계에 남아 있어 모순이었다).
쓰이지 않는 모듈은 IaC 드리프트와 권한 확장의 원인이 된다.
비용표의 EventBridge 라인도 제거했다.

**침묵 감지는 CloudWatch 알람이 담당한다** — 앱 내부 cron의 약점은 "앱이 멈추면 cron도
멈춘다"는 것이다. 외부 워치독을 두는 대신, 각 잡이 성공 시 `JobHeartbeat` 메트릭을 발행하고
`TreatMissingData=breaching` 알람으로 감시한다([§5.3](#53-자체-cloudwatch-알람)).
CloudWatch가 외부 워치독 역할을 한다.

| 잡 | 주기 | 리더 필요 | 실패 시 |
|---|---|---|---|
| 인스턴스 탐색 | 5분 | ✓ | 다음 주기 재시도 |
| 자가진단 | 1시간 | ✓ | 인스턴스별 개별 실패 허용 |
| 아카이브 (증분 내보내기 + MERGE) | 1일 02:00 KST | ✓ | 체크포인트 유지, 다음 주기가 누락 구간 포함. 3회 실패 시 알림 |
| Iceberg 보관 만료 | 매월 1일 04:00 | ✓ | 알림 |
| 월간 리포트 | 매월 1일 03:00 | ✓ | 부분 실패 허용 |
| 인덱스 위생 스냅샷 | 1일 (인스턴스별 지터) | ✗ (샤드 소유자) | 다음 주기 |
| 테이블 통계 스냅샷 | 1일 | ✗ | 다음 주기 |
| 데드락 수집 | 5분 | ✗ | 다음 주기 |
| 다이제스트 시간 롤업 플러시 | 정시 + 지터 | ✗ | 부분 데이터 표시 |
| CloudWatch 플릿 폴링 | 15분 | ✓ | 다음 주기 |
| CA 번들 만료 점검 | 1일 | ✓ | 경고 |
| 사용량 집계 (비용) | 1시간 | ✓ | 다음 주기 |

**리더 리스** — `dbmon-config / LEASE / LEADER#cron`. TTL 30초, 10초마다 갱신.
리더가 아닌 워커는 리더 필요 잡을 건너뛴다.

## 5. 관측성

### 5.1 로그

- 구조화 JSON (`tracing` + `tracing-subscriber` JSON layer).
- 필수 필드: `ts`, `level`, `target`, `msg`, `request_id`, `instance_id`, `user_sub`,
  `span_id`, `trace_id`.
- CloudWatch Logs 로그 그룹 `/dbmon/app`, 보관 30일, CMK 암호화.
- 로그 레벨: 기본 `info`. 인스턴스별 `debug` 활성화를 설정으로 켤 수 있게 한다
  (특정 인스턴스 문제 조사 시).
- **마스킹 레이어 필수** ([07 §1.3](07-credentials-bootstrap.md)).
- SQL 텍스트는 로그에 남기지 않는다. 남겨야 하면 `app_digest`만.

### 5.2 메트릭 (EMF)

CloudWatch Embedded Metric Format으로 로그에 메트릭을 심는다.
`PutMetricData` API 호출 비용을 피할 수 있다.

**차원에 `instance_id`를 넣지 않는다.** EMF 메트릭은 커스텀 메트릭으로 **메트릭당 월 $0.30**
과금되므로, `instance_id`를 차원에 넣으면 500대 기준 약 7,000개 = **월 $2,100**이 된다
([16 §2.3](16-cost.md), [ADR-009](03-decisions.md)).

| 메트릭 | 차원 | 고유 수 | 용도 |
|---|---|---|---|
| `CollectTickDuration` | — (분포 통계) | 1 | 수집 루프 소요 시간 |
| `CollectFailures` | reason (6종) | 6 | 실패 분류 |
| `TargetDbQueryDuration` | purpose (6종) | 6 | 대상 DB 부하 |
| `SlowQueriesCaptured` | env (4종) | 4 | 캡처 건수 |
| `PlanCaptureResult` | source, result | 6 | in-flight 성공률 (NFR-P-04) |
| `DigestRollupItems` | — | 1 | 롤업 볼륨 |
| `DroppedRecords` | reason (4종) | 4 | **유실 (NFR-R-06)** |
| `MaskingPostconditionFailed` | — | 1 | 마스킹 실패 (T-36) |
| `DynamoWriteThrottles` | table (2종) | 2 | |
| `DynamoWriteLatency` | table, op | 8 | |
| `ArchiveJobResult` | result | 2 | |
| `JobHeartbeat` | job (12종) | 12 | **침묵 감지** |
| `LeaderHeld` | — | 1 | 리더 부재 감지 |
| `ShardsOwned` | — (합계) | 1 | 샤딩 커버리지 |
| `LeaseRenewFailures` | — | 1 | |
| `AthenaScannedBytes` | workgroup, query_type | 10 | 비용 |
| `CloudWatchMetricsRequested` | — | 1 | 비용 |
| `BedrockTokens` | direction, env | 8 | 비용 |
| `EmfMetricCount` | — | 1 | **메트릭 수 자체를 감시** (예산 초과 방지) |
| `ApiRequestDuration` | route(그룹핑), status_class | 30 | |
| `WsConnections` | — | 1 | |
| `CollectStaleness` | — (최댓값) | 1 | 가장 오래된 인스턴스의 경과 시간 |
| **watched 인스턴스 (화이트리스트 20대)** | instance_id | 120 | 개별 알람이 필요한 소수 |
| **합계** | | **228개 = 월 $69** | ([16 §2.3](16-cost.md)과 동일 값) |

**인스턴스별 값은 구조화 로그 필드로만 남긴다.** 조회는 Logs Insights 또는
CloudWatch Contributor Insights를 쓴다. "어느 인스턴스가 실패했나"는 우리 UI와 알림 본문이
답하므로(알림 평가를 앱이 하므로 CloudWatch 메트릭이 필요 없다), CloudWatch 알람은
**플릿 수준의 이상**만 감시하면 된다.

`ApiRequestDuration`의 `route` 차원은 **경로 템플릿으로 그룹핑**한다
(`/api/instances/:id`, `/api/slow-queries/:id`). 실제 ID를 넣으면 카디널리티가 폭발한다.

### 5.3 자체 CloudWatch 알람

**"값이 임계 초과" 알람만으로는 침묵을 감지하지 못한다.** 스케줄러 리더가 리스를 잡은 채
멈추거나 cron 잡이 죽으면 **메트릭이 발행되지 않아 어떤 알람도 울리지 않는다.**
`ArchiveJobResult failure ≥ 1`은 잡이 실행됐을 때만 유효하고, "02시에 안 돌았다"는 못 잡는다.

→ **잡마다 성공 하트비트를 발행하고 `TreatMissingData=breaching`으로 알람을 건다.**

| 알람 | 조건 | 결측 처리 |
|---|---|---|
| **잡 미실행 (침묵)** | `JobHeartbeat{job}` — 각 잡의 주기 × 2 동안 데이터 없음 | **breaching** |
| **리더 부재** | `LeaderHeld` (리더가 1로 발행) 5분간 데이터 없음 | **breaching** |
| **샤드 커버리지** | `ShardsOwned` 합계 < 64, 5분 | breaching |
| 워커 다운 | ASG `GroupInServiceInstances` < desired, 5분 (`enabled_metrics` 필요) | breaching |
| ALB 5xx | 5분간 `HTTPCode_Target_5XX_Count` > 10 | notBreaching |
| ALB 비정상 대상 | `UnHealthyHostCount` > 0, 5분 | breaching |
| 수집 지연 | `CollectStaleness` p95 > 300초 | breaching |
| 유실 발생 | `DroppedRecords` Sum > 0 | notBreaching |
| 마스킹 후조건 실패 | `MaskingPostconditionFailed` Sum > 0 | notBreaching |
| DynamoDB 스로틀 (자체 계측) | `DynamoWriteThrottles` > 0 | notBreaching |
| **DynamoDB 스로틀 (AWS 네임스페이스)** | `AWS/DynamoDB` `ThrottledRequests` > 0 | notBreaching |
| 아카이브 실패 | `ArchiveJobResult` failure ≥ 1 | notBreaching |
| Athena 스캔 급증 | 일 `AthenaScannedBytes` > 예산 80% | notBreaching |
| Bedrock 토큰 급증 | 일 예산의 20% 초과 | notBreaching |
| 리스 갱신 실패 | `LeaseRenewFailures` > 5/분 | notBreaching |
| EC2 CPU | > 80%, 10분 | breaching |
| EC2 메모리 | > 85%, 10분 (**CloudWatch Agent 설치 필요** — Launch Template User Data에 포함) | breaching |
| 로그 에러율 | **로그 메트릭 필터** `level=error` > 50/5분 (Terraform `observability` 모듈) | notBreaching |
| **ACM 인증서 만료** | `AWS/CertificateManager` `DaysToExpiry` < 30 | breaching |
| **Secrets 로테이션 실패** | 로테이션 Lambda 에러 > 0 | notBreaching |
| **WAF 차단 급증** | `BlockedRequests` 급증 (평시 대비 10배) | notBreaching |

알람 → **SNS 토픽** → 이메일·SMS 구독으로 관리자에게 **직접** 발송한다.
앱의 알림 채널(Slack/Telegram)과 별도 경로여야 한다 — 자기 자신이 죽었을 때
자기 알림 채널로 보내면 못 받는다.
SNS 토픽·구독·토픽 정책은 Terraform `observability` 모듈에 포함한다.

### 5.4 대시보드

CloudWatch 대시보드 1개(Terraform 정의):
플릿 수집 상태 / 지연 / 실패 / 유실 / API 지연·에러율 / 비용 지표 / 워커 리소스.

## 6. 배포

### 6.1 CI/CD

**러너는 arm64를 쓴다.** 출하 바이너리가 aarch64이므로 x86 러너에서 크로스 컴파일하면
(a) 크로스 링커·cmake 툴체인이 필요하고(`zstd-sys`, TLS 백엔드가 C를 빌드한다),
(b) **glibc 스큐**로 배포 당일에 터지고(ubuntu-latest glibc 2.39로 링크 → AL2023 glibc 2.34에서
`GLIBC_2.3x not found`), (c) **통합 테스트가 x86에서 돌아 출하 바이너리는 한 번도 검증되지
않는다.**

```
PR  (GitHub runner: ubuntu-24.04-arm)
  ├ cargo fmt --check
  ├ cargo clippy -- -D warnings
  ├ cargo test --workspace                    (단위, 목표 3분)
  ├ cargo audit / cargo deny
  ├ gitleaks
  ├ web: pnpm lint / tsc --noEmit / vitest
  ├ web: openapi 타입 재생성 후 diff 확인
  ├ terraform fmt / validate / plan (dev)
  ├ API 계약 테스트 (라우터 순회 인증·권한)
  └ Playwright: 컴포넌트·모킹 E2E만  ← 실 E2E는 dev 배포 후여야 하므로 여기 둘 수 없다

main 머지 (arm64 러너)
  ├ AL2023 arm64 컨테이너 안에서 cargo build --release
  │    (또는 aarch64-unknown-linux-musl 정적 링크)
  ├ web build → 정적 자산을 바이너리에 임베드 (rust-embed)
  ├ 스모크: 빌드 산출물을 AL2023 컨테이너에서 `dbmon --version` 실행  ← glibc 스큐 게이트
  ├ cargo test --features integration (testcontainers, arm64 이미지)
  ├ 아티팩트 → s3://dbmon-artifacts-<acct>/<version>/
  ├ dev 자동 배포 (ASG Instance Refresh)
  ├ **Playwright 실 E2E (dev 배포 후)**
  └ prd 는 수동 승인 후 배포

일 1회 (실패 시 알림, 머지 차단 안 함)
  ├ 아카이브 경로 테스트 (dev AWS 실제 리소스)
  ├ axe-core 접근성
  └ 부하 테스트 축소판 (인스턴스 100대)
```

**초기 설계의 순서 오류** — PR 블록에 "Playwright E2E (dev 환경 배포 후)"가 있었는데
dev 배포는 main 머지 블록에 있다. PR 시점에는 배포된 dev가 없다.
→ PR에서는 모킹 E2E만, 실 E2E는 dev 배포 뒤로 옮긴다.

### 6.1.1 CI의 AWS 인증

**장기 액세스 키를 쓰지 않는다.** GitHub OIDC로 `AssumeRoleWithWebIdentity`를 쓴다.

```
iam 모듈이 생성하는 것:
  GitHub OIDC provider (token.actions.githubusercontent.com)
  dbmon-ci-dev  : terraform plan/apply(dev) + S3 put(artifacts) + asg refresh(dev)
  dbmon-ci-prd  : terraform plan 전용
  dbmon-ci-prd-apply : apply 권한. GitHub Environment "prd" 승인 게이트 뒤에서만

신뢰 정책 조건:
  token.actions.githubusercontent.com:sub =
    repo:<org>/<repo>:environment:dev      (dev Role)
    repo:<org>/<repo>:environment:prd      (prd Role)
  aud = sts.amazonaws.com
```

`sub` 조건으로 **레포·브랜치·환경을 제한**한다. 조건 없이 OIDC를 신뢰하면 어떤 레포에서든
이 Role을 가정할 수 있다.

일 1회 아카이브 테스트도 `dbmon-ci-dev`를 쓴다(dev 계정의 실제 리소스 접근).

**프론트를 바이너리에 임베드하는 이유**: 배포 단위가 하나면 프론트·백엔드 버전 스큐가 없다.
CloudFront + S3 정적 호스팅으로 분리하면 캐시 무효화와 버전 정합 문제가 생긴다.
정적 자산이 수백 KB 수준이라 바이너리 크기 부담도 없다.

### 6.2 무중단 배포 (FR-OPS-06)

```
ASG Instance Refresh
  MinHealthyPercentage 50, InstanceWarmup 120초
  ↓
새 인스턴스 기동 → sd_notify(READY=1) → /readyz 200 → ALB 등록
  ↓
구 인스턴스 종료 시작
  ↓
[ASG Lifecycle Hook: EC2_INSTANCE_TERMINATING → Wait 상태로 정지]
  앱이 훅 이벤트를 감지(IMDS 또는 SNS→앱)하고 정리를 수행한다.
  120초 heartbeat 안에 끝내지 못하면 RecordLifecycleActionHeartbeat 로 연장.
  ├ /readyz 를 503으로 전환 → ALB 등록 해제 시작 (deregistration_delay 60초)
  ├ WS 연결에 "재연결 요청" 메시지 전송 후 종료
  ├ 진행 중 HTTP 요청 완료 대기 (최대 20초)
  ├ 리더/샤드 리스 명시적 반납  → 다른 워커가 다음 스캔(20초)에 인수
  ├ 다이제스트 누산기 플러시 (부분 시간 데이터 저장, partial=true)
  ├ 쓰기 버퍼 플러시
  └ CompleteLifecycleAction(CONTINUE)
  ↓
systemd SIGTERM → 프로세스 종료 → 인스턴스 종료
```

**Lifecycle Hook 없이는 정리 시간이 보장되지 않는다.** ASG 종료·Instance Refresh는
OS 셧다운을 거치지만 EC2는 짧은 시간 뒤 강제 전원 차단하므로 `TimeoutStopSec=45`가
보장되지 않는다. `Terminating:Wait` 훅이 유일한 보장 장치다.

**`deregistration_delay`(60초) ≥ `TimeoutStopSec`(45초)** 로 맞춘다.
초기 설계는 30초 < 45초여서 ALB가 먼저 라우팅을 끊는데 앱은 더 살아 있었다.

**리스 타이밍의 단일 출처는 [05 §7](05-collector.md)이다.** 이 문서와 02는 참조만 한다
(초기에는 세 문서가 "10초", "60초 TTL / 20초 갱신", "60초 내 인수"로 서로 달랐다).

**수집 중단 시간**:
- 명시적 반납 → 최대 **1 스캔 주기(20초)**
- 워커 급사 → **TTL 60초 + 스캔 20초 = 최대 80초**

1단계에서 워커 2대(active + standby)를 유지하므로 배포 중에도 standby가 즉시 인수한다.
추가 비용 **$42/월**([16 §3](16-cost.md) c7g.large 1대, SP 적용).

### 6.3 롤백

```
ASG Instance Refresh 롤백 (AWS 기능) 또는
이전 버전 아티팩트로 Launch Template 버전 되돌린 뒤 Refresh
```

- 데이터 스키마는 하위호환을 유지하므로(`v` 필드) 롤백이 데이터를 깨지 않는다.
- Iceberg 스키마 변경(컬럼 추가)은 롤백해도 무해하다(구 버전은 새 컬럼을 무시).
- **컬럼 삭제·타입 변경은 하지 않는다.** 필요하면 새 컬럼 추가 + 구 컬럼 폐기 표시.

## 6.4 최초 설치 순서

앱은 인증을 요구하고, 인증에는 Cognito 사용자가 필요하고, 사용자 관리는 admin이 한다.
첫 admin은 어디서 오는가?

**아티팩트 닭·달걀은 레이어 분리로 해소됐다** — `40-compute`를 아티팩트 업로드 후에
apply하면 된다(§1). `asg_desired=0` 트릭이 불필요하다.

```
0. 레이어 00 → 10 → (20/30/50/60 은 필요한 시점에) → 아티팩트 업로드 → 40-compute
   개발계 상세 순서는 18-dev-environment.md §10

1. 30-identity apply (envs/<env>.tfvars)
     - cognito 모듈이 초기 admin 사용자를 생성한다
         var.initial_admin_email 로 AdminCreateUser + dbmon-admin 그룹 추가
         MessageAction = SUPPRESS (Cognito 초대 메일 발송, 임시 비밀번호)
     - 이 사용자는 Terraform 상태에 남으며, 이후 삭제해도 무해하다
2. 초대 메일의 임시 비밀번호로 로그인 → 강제 비밀번호 변경
3. 설정 → AWS: 리전 목록 지정 → "지금 재탐색"
4. 인스턴스 목록에서 자가진단 확인 (대부분 unreachable 또는 권한 오류로 나온다)
5. 대상 RDS의 보안 그룹에 앱 SG 인바운드 추가 (대상 팀 작업)
     UI가 필요한 SG 규칙을 생성해 보여준다
6. 부트스트랩: dev 인스턴스 1대로 먼저 시도 → 성공 확인 → stg → prd
7. 수집 시작 확인 (자가진단 전 항목 통과, 슬로우 쿼리 수집 확인)
8. 알림 채널 등록 + 기본 규칙 중 필요한 것 활성화
9. IdP 연동 (선택) → 나머지 사용자 초대
10. 첫 달 실측 후 16-cost.md 갱신
```

**`initial_admin_email`이 없으면 apply가 실패하게 한다.** 기본값을 두지 않는다
(기본 관리자 계정은 보안 사고의 고전이다).

## 6.5 데이터 스키마 마이그레이션

DynamoDB는 스키마가 없지만 우리가 쓰는 항목 구조는 있다. `v` 필드로 관리한다.

| 변경 유형 | 절차 |
|---|---|
| 속성 추가 | 그냥 추가한다. 읽기 코드는 `Option<T>`로 받는다. `v` 증가 없음 |
| 속성 의미 변경 | **하지 않는다.** 새 속성을 추가하고 구 속성은 폐기 표시 |
| 속성 삭제 | 쓰기를 먼저 멈추고(1 릴리스), TTL로 자연 소멸시킨 뒤(35일) 읽기 코드 제거 |
| 키 구조 변경 | 신규 키로 이중 쓰기 → 35일 대기 → 구 키 읽기 제거. 백필하지 않는다 |
| Iceberg 컬럼 추가 | `ALTER TABLE ADD COLUMN`. 구 파티션은 NULL |
| Iceberg 컬럼 삭제·타입 변경 | **하지 않는다** (롤백이 불가능해진다) |

**핵심 규칙: 마이그레이션 스크립트를 쓰지 않는다.** 관측 데이터는 35일이면 사라지므로
"쓰기를 바꾸고 기다리기"가 항상 가능하다. 이게 이 저장소 선택의 숨은 이득이다.

예외는 `dbmon-config`(설정·규칙·인스턴스)다. 여기는 TTL이 없으므로 변환이 필요하다.
항목 수가 수천 개이므로 기동 시 lazy 마이그레이션(`v`가 낮으면 읽으면서 변환 후 재저장)으로
처리한다. 별도 배치를 만들지 않는다.

## 7. 로컬 개발

```
docker-compose.yml
  mysql-84      MySQL 8.4, performance_schema=ON, slow_query_log=ON
  mysql-8032    MySQL 8.0.32 (버전 호환 테스트)
  dynamodb      amazon/dynamodb-local
  minio         S3 호환 (선택)

.env.local
  AWS_PROFILE=dev-sso          # SSO 프로파일로 실제 AWS 읽기 (RDS 탐색 등)
  # DynamoDB Local. 설정 키는 `storage.endpoint_url` 이고 `dev` + 루프백만 허용된다.
  DBMON__STORAGE__ENDPOINT_URL=http://127.0.0.1:18000
  # 로컬 MySQL 은 IAM 인증을 지원하지 않으므로 고정 비밀번호로 붙는다.
  # `deployment_env=dev` 여야 적용된다. 비-dev 배포에 남아 있으면 기동이 거부된다.
  DBMON_TARGET_PASSWORD=dbmon-local-monitor
  DBMON_ARCHIVE_ENABLED=false
  DBMON_AI_ENABLED=false
```

```bash
just dev          # 백엔드 watch + 프론트 dev 서버
just seed         # 로컬 MySQL 에 샘플 스키마 + 느린 쿼리 생성 부하
just test         # 단위
just test-int     # testcontainers 통합
just golden       # 다이제스트 골든 코퍼스 재생성
```

**`just seed`가 중요하다.** 슬로우 쿼리를 재현 가능하게 만드는 스크립트가 없으면
개발·테스트가 실제 인스턴스에 의존하게 된다. 인덱스 없는 큰 테이블 + 의도적으로 느린
쿼리를 반복 실행하는 부하 생성기를 제공한다.

로컬은 AWS SSO 프로파일을, EC2는 Instance Profile을 쓴다.
코드는 표준 자격증명 공급자 체인을 그대로 쓰므로 분기가 없다(1세대의 SSO/Role 분기 코드는
불필요했다).

## 8. 런북

### 8.1 특정 인스턴스가 수집되지 않는다

```
1. UI → 인스턴스 상세 → 자가진단 탭
   → 대부분 여기서 원인이 나온다 (버전/권한/네트워크/performance_schema)
2. 나오지 않으면 로그 조회:
   CloudWatch Logs Insights
   fields @timestamp, level, msg, reason
   | filter instance_id = "123456789012/ap-northeast-2/orders-prd-01"
   | sort @timestamp desc | limit 100
3. 샤드 소유 확인: dbmon-config 에서 해당 인스턴스의 shard_key 소유 워커 확인
4. 서킷 브레이커 상태 확인 (메트릭 CollectFailures by reason)
```

### 8.2 대상 DB에 부하를 주고 있다는 의심

```
1. UI → 인스턴스 상세 → "수집기 부하" 패널
   (우리 쿼리의 초당 건수, 평균 응답, 대상 DB 전체 실행시간 중 비중)
2. 대상 DB에서 직접 확인 — **계정 기반이다.**
   (`DIGEST_TEXT LIKE '/* dbmon:%'` 는 작동하지 않는다: 다이제스트가 주석을 제거한다)

   SELECT event_name, total, total_latency, max_latency
   FROM sys.user_summary_by_statement_type
   WHERE user = 'dbmon'
   ORDER BY total_latency DESC;

   -- 서버 전체 대비 비중
   SELECT user, statements, statement_latency
   FROM sys.user_summary ORDER BY statement_latency DESC;
3. 즉시 완화: 해당 인스턴스 폴링 주기 상향 또는 수집 비활성 (재시작 불필요)
```

### 8.3 아카이브 잡이 계속 실패한다

```
1. 메트릭 ArchiveJobResult, 로그의 job=archive 항목 확인
2. 흔한 원인:
   - PITR 비활성 → DescribeContinuousBackups 확인
   - 내보내기 시작 시각이 PITR 보관 창(35일) 밖 → 체크포인트를 창 안으로 조정
   - Athena MERGE 실패 → 스키마 불일치. Iceberg 컬럼 추가 필요
   - S3 raw 버킷 Lifecycle 이 내보내기 완료 전에 삭제 → 7일 확인
3. 수동 재실행: dbmon archive --from <ts> --to <ts>
4. TTL 35일 이내라면 데이터가 DynamoDB 에 남아 있으므로 복구 가능
```

### 8.4 알림이 폭주한다

```
1. UI → 설정 → 알림 → 규칙별 발화 통계 (최근 1시간 순 정렬)
2. 원인 규칙 즉시 비활성 또는 전체 음소거 (관리자)
3. 근본 원인이 인스턴스 장애면 의존 억제가 왜 작동하지 않았는지 확인
   (collector 규칙이 활성화되어 있는지)
4. 임계값·for_duration 조정
```

### 8.5 CloudWatch 비용이 예상보다 크다

```
1. /api/usage 또는 설정 화면에서 CloudWatchMetricsRequested 일별 추이 확인
2. 원인 후보:
   - 플릿 폴링 메트릭 수가 늘었다 → 설정 확인
   - 상세 화면 캐시가 동작하지 않는다 → cached 필드 확인
   - 여러 사용자가 서로 다른 기간을 조회 → period 정렬이 깨졌는지 확인
3. 완화: 예산 상한을 낮춰 캐시 강제 모드 진입
```

### 8.6 데이터 유실 알림 (`DroppedRecords > 0`)

```
1. reason 차원 확인: dynamo_throttle | buffer_full | parse_error | plan_queue_full
2. dynamo_throttle → 온디맨드인데 스로틀? 파티션 핫스팟 가능성.
   → 04 §2.3 의 시간 샤드 추가 검토
3. buffer_full → 워커 처리량 부족. 인스턴스 수 대비 워커 증설
4. parse_error → 로그에서 원본 확인 (마스킹된 형태). 파서 버그 → 이슈 등록
5. 유실 구간을 슬로우로그 백필로 복구 가능한지 확인 (FR-CWL-02)
```

### 8.7 프로덕션 DB를 건드릴 수 있는 작업 목록

앱이 프로덕션 대상 DB·RDS에 영향을 줄 수 있는 지점은 다음이 전부다. 이 목록을 유지한다.

| 작업 | 트리거 | 기본 활성 | 안전장치 |
|---|---|---|---|
| 읽기 전용 조회 (폴링) | 상시 | ✓ | 타임아웃, `max_execution_time`, 서킷 브레이커 |
| `EXPLAIN FOR CONNECTION` | 슬로우 쿼리 탐지 | ✓ | 타임아웃 3초, 별도 연결 |
| `EXPLAIN` 재실행 | 플랜 폴백 | ✓ | SELECT만, 타임아웃 |
| `SHOW ENGINE INNODB STATUS` | 5분 주기 | ✓ | 출력 크기 상한 |
| `CREATE USER` / `GRANT` | 부트스트랩 | 수동 | dry-run, SQL 표시, 타이핑 확인, 감사 |
| `ALTER USER` (비밀번호 로테이션) | 30일 | 폴백 경로만 | 자기 계정만 |
| `ModifyDBInstance` | 부트스트랩 | **✗ (권한 미부여)** | 수동 명령 생성으로 대체 |
| DDL 적용 | — | **없음** | 어드바이저는 문장 제시만 |
| `KILL` | — | **없음** | 락 화면은 명령 표시만 |
| `ANALYZE TABLE` | — | **없음** | 어드바이저 권고 문장으로만 |

이 표가 늘어날 때마다 리뷰에서 안전장치를 확인한다.

### 8.8 재해 복구

| 사고 | 영향 | 복구 |
|---|---|---|
| `dbmon-data` 손상·오삭제 | 최근 35일 데이터 유실 | 아래 §8.8.1 절차 |
| `dbmon-config` 손상 | 인스턴스 레지스트리·설정·규칙 유실 | PITR 복원. 최악의 경우 재탐색(5분) + 설정 재입력. **알림 규칙과 채널 설정은 재입력이 필요하므로 Terraform으로 관리하는 것을 검토** → [OPEN-Q-14](OPEN-QUESTIONS.md) |
| S3 Tables 손상 | 31일 초과 이력 유실 | Iceberg 시간 여행(스냅샷)으로 복원. 스냅샷 만료 기간 내에서만 가능. 그 밖이면 **복구 불가** — 아카이브 데이터는 DynamoDB에서 재파생할 수 없다(TTL로 원천이 사라짐) |
| 리전 장애 (앱 리전) | 수집·조회 전면 중단 | **다중 리전 DR을 하지 않는다.** 관측 도구가 관측 대상보다 가용성이 높을 필요가 없다. 복구는 다른 리전에 Terraform 재적용 + DynamoDB 복원(글로벌 테이블 미사용) |
| 리전 장애 (대상 리전) | 해당 리전 인스턴스 수집 중단 | 자동. 서킷 브레이커가 열리고 알림. 회복 시 자동 재개 |
| Cognito User Pool 삭제 | 로그인 불가 | Terraform 재생성 + 사용자 재초대. `initial_admin_email`로 첫 진입 |
| KMS 키 삭제 예약 | 전 데이터 복호화 불가 | **키 삭제 방지 필수.** 키 정책에서 `kms:ScheduleKeyDeletion`을 관리자만 허용하고, CloudTrail 알람을 설정 |

#### 8.8.1 DynamoDB PITR 복원 절차 (리허설 대상)

PITR 복원은 **새 테이블 이름으로만** 가능하다. 그래서 앱이 테이블 이름을 바꿀 수 있어야 한다.

```
설정 키: DBMON_TABLE_SUFFIX  (EnvironmentFile /etc/dbmon/env)
  기본값 없음 → 테이블명 = dbmon-data / dbmon-config
  값이 있으면 → dbmon-data-<suffix> / dbmon-config-<suffix>
IAM 정책의 리소스가 arn:...:table/dbmon-* 이므로 복원 테이블도 자동으로 커버된다.
```

**절차**

```
1. 수집 중단:  설정 CFG/GLOBAL/collection.enabled = false
   (복원 중 새 쓰기가 구 테이블로 가면 복원 시점 이후 데이터가 갈라진다)
   → 이 구간의 데이터는 슬로우로그 백필(FR-CWL-02)로 메꾼다. 다이제스트 롤업은 복구 불가
2. RestoreTableToPointInTime(TargetTableName=dbmon-data-r20260819, RestoreDateTime=<T>)
   → 수 시간 소요. 진행 상황은 DescribeTable
3. 복원 완료 후 TTL·PITR·GSI 설정 확인 (복원 시 일부 설정이 기본값으로 돌아간다)
     - TTL 속성 재설정 필요
     - PITR 재활성 필요
     - GSI는 복원된다 (확인 필요)
4. /etc/dbmon/env 에 DBMON_TABLE_SUFFIX=r20260819 추가 → 롤링 재시작
5. 검증: 최근 데이터 조회, 자가진단, 아카이브 체크포인트 확인
6. 수집 재개: collection.enabled = true
7. 슬로우로그 백필로 공백 구간 복구
8. 구 테이블은 7일 보관 후 삭제 (즉시 삭제하지 않는다)
```

**리허설 필수** — 한 번도 실행해 본 적 없는 복구 절차는 실패한다.
**연 1회 dev 환경에서 게임데이**를 수행한다(M12-20).

#### 8.8.2 S3 Tables 시간 여행

Iceberg 스냅샷 기반 복원은 **스냅샷 보존 기간 안에서만** 가능하다.
관리형 유지보수가 스냅샷을 만료시키므로 **보존 기간을 최소 7일로 명시적으로 설정**해야 한다.
기본값에 의존하면 복원 창이 없을 수 있다.

```sql
-- 특정 스냅샷 시점 조회
SELECT * FROM "s3tablescatalog/<bucket>"."dbmon"."slow_queries"
FOR VERSION AS OF <snapshot_id>;
```

**의도적 결정: 아카이브에 대한 백업을 따로 두지 않는다.**
S3 Tables는 S3 내구성을 상속하고, 이 데이터는 재생성 불가하지만 손실의 영향이
"과거 성능 이력을 잃음"이다. 그 위험을 감수하는 대신 복잡도를 줄인다.
용납할 수 없다면 S3 Tables 테이블 버킷의 크로스 리전 복제를 검토한다.

### 8.9 설정 값 검증

잘못된 설정으로 스스로를 망가뜨리는 것을 막는다. 저장 시점에 거부한다.

| 설정 | 허용 범위 | 거부 이유 |
|---|---|---|
| `poll_interval_ms` | 500 ~ 60000 | 500 미만은 대상 DB 부하, 60초 초과는 탐지 무의미 |
| `slow_threshold_ms` | 1000 ~ 600000 | 1초 미만은 `TIME` 초 단위 해상도로 불가 |
| `digest_snapshot_interval_sec` | 30 ~ 900 | 30초 미만은 페이로드 낭비 |
| `digest_top_n` | 10 ~ 2000 | 2000 초과는 비용 폭발 (경고 + 예상 비용 표시) |
| `digest_min_total_time_ms` | 0 ~ 60000 | |
| `status_interval_sec` | 5 ~ 300 | |
| `hot_retention_days` | 1 ~ 35 | 35 초과는 PITR 창을 넘어 아카이브 복구 여유가 없어짐 |
| `cold_retention_days` | 7 ~ 3650 | |
| `plan_timeout_ms` | 500 ~ 10000 | |
| `query_timeout_ms` | 1000 ~ 60000 | `poll_interval_ms`보다 크면 tick이 밀린다 → 교차 검증 |
| `max_deep_dive_per_tick` | 1 ~ 200 | |
| `regions` | 유효 리전 코드 + `DescribeRegions` 결과에 존재 | |
| `report_timezone` | 유효 IANA 시간대. 30분 오프셋은 경고 | [12 §0.1](12-reporting.md) |
| `ai_monthly_token_budget` | 0 이상 | 0은 AI 비활성과 동일 |

**교차 검증** — 단일 값 범위만 보면 안 된다. `query_timeout_ms × 예상 쿼리 수`가
`poll_interval_ms`를 넘으면 tick이 항상 밀린다. 저장 시 계산해 경고한다.
