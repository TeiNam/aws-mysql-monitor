# 13. API 명세

## 1. 공통 규약

| 항목 | 규약 |
|---|---|
| 베이스 | `https://<도메인>/api` |
| 인증 | `Authorization: Bearer <cognito_access_token>`. **미인증 허용은 아래 4개뿐** |
| 컨텐츠 | `application/json`, UTF-8 |
| 시각 | 요청·응답 모두 RFC3339 UTC (`2026-08-18T14:23:11Z`). epoch millis도 함께 제공 |
| 스펙 | `/api/openapi.json` (OpenAPI 3.1). 코드에서 생성 (NFR-M-04) |
| 상관관계 | 요청 `X-Request-Id` 수락, 없으면 생성. 응답에 항상 포함 |
| 압축 | gzip / br |
| CORS | 프로덕션은 동일 오리진. 개발용 `localhost:5173`만 허용 |

### 1.1 페이지네이션

커서 기반. **커서는 HMAC으로 서명한다** (T-32).

```
GET /api/slow-queries?limit=50&cursor=eyJ…
→ { "items": [...], "next_cursor": "eyJ…" | null, "has_more": true }
```

**커서 구조**
```
payload = {
  sub:      요청자 Cognito sub          ← 커서 공유·탈취 차단
  filters:  정규화된 필터 집합의 해시     ← 필터 바꿔치기 차단
  exp:      만료 시각 (기본 1시간)
  position: <started_at_ms>|<thread_id>|<instance_id>   ← 정렬 총순서의 한 점
}
cursor = base64url(payload) + "." + HMAC-SHA256(server_key, base64url(payload))
```

**`position` 은 `LastEvaluatedKey` 가 아니다.** 목록 조회는 인스턴스 N대에 팬아웃해 전역
정렬하므로 저장소 재개 키 하나로는 위치를 표현할 수 없다. N개를 담으면 커서가 인스턴스
수에 비례해 커지고, 인스턴스가 추가·삭제되는 순간 그 커서는 뜻을 잃는다.

그래서 **정렬 순서의 좌표**를 담는다. 총순서는 `(순서 키 내림, 인스턴스 id 오름)` 이고
순서 키는 저장소 정렬 키와 같은 `<started_at_ms:013>#<thread_id>` 사전순이다
(`dbmon_core::slow_query::list_order_key`).

| 성질 | 왜 필요한가 |
|---|---|
| 정렬과 저장소 범위가 **같은 사전순**을 쓴다 | `thread_id` 는 0 패딩이 아니라 `"…#6" > "…#50"` 이다. 한쪽만 숫자순으로 보면 커서가 `thread_id=50` 일 때 같은 밀리초의 `6` 이 **어느 페이지에도 없다** |
| 인스턴스 id 가 마지막 갈래다 | 같은 `(ms, thread_id)` 가 인스턴스마다 하나씩 있을 수 있다. 갈래가 없으면 그 행이 1페이지(정렬이 뒤로 놓아서)에도 2페이지(경계값이라 걸러져서)에도 없다 |
| 저장소는 경계값을 **포함**해 돌려준다 | 위 갈래를 호출부가 판정할 수 있어야 한다. 어댑터가 경계값을 빼면 그 기회가 없다 |
| 재개하면 읽을 구간의 위쪽을 커서 시점으로 좁힌다 | 안 좁히면 커서보다 새로운 날짜 파티션을 매 페이지 다시 질의하고 결과는 전부 빈 응답이다 (파티션 예산 낭비) |

페이지를 넘기는 사이에 들어온 새 행은 **1페이지에만** 나타난다 — 조사 도구에서 그게 맞는
방향이다(오프셋 방식은 그 행이 경계를 밀어 같은 행을 두 번 보여준다).

**검증 규칙**
- 서명 불일치 / 만료 / `sub` 불일치 → `400 invalid_cursor`
- **커서에 담긴 필터를 신뢰하지 않는다.** 재개 시에도 요청 파라미터에서 필터를 다시 유도하고,
  커서의 필터 해시와 불일치하면 거부한다. 커서의 필터를 그대로 쓰면 환경 스코프·
  `can_see_literals`·시간 범위 재검증이 건너뛰어진다.
- 초기 설계는 `LastEvaluatedKey`를 base64로만 인코딩했다. **불투명이 아니라 가역적**이라
  내부 키 구조(PK/SK 패턴, `record_id` 구성)가 노출되고 위조가 가능했다.

**핫/콜드 통합 페이지네이션 (미구현)** — 경계를 교차하는 조회는 `phase`로 단계를 관리한다.
지금은 핫 티어(DynamoDB)만 페이지를 넘긴다. `phase` 는 커서에 없다 — 쓰지 않는 필드를 담으면
"콜드도 된다" 로 읽힌다.
```
phase=hot  : DynamoDB 구간을 소진할 때까지 (ddb.LastEvaluatedKey 로 진행)
             소진되면 next_cursor 의 phase 를 cold 로 전환하고 Athena 실행을 시작
phase=cold : Athena 결과를 next_token 으로 진행
             경계 구간의 중복은 record_id 로 제거 (hot 에서 본 record_id 집합을
             커서에 담기엔 크므로, 시간 경계를 정확히 나눠 중복이 나오지 않게 한다)
```
경계를 나눌 때 **DynamoDB 구간은 `[hot_boundary, to]`, Athena 구간은 `[from, hot_boundary)`**
로 배타적으로 잘라 중복을 원천 제거한다.

- `limit`: 1~500(`MAX_LIMIT`), 기본 50. **페이지 크기이고 천장이 아니다** — 그 뒤는 커서로 간다.
- 오프셋 페이지네이션은 제공하지 않는다.
- 총 건수는 반환하지 않는다. 필요하면 `/api/slow-queries/count`가 근사치를 반환한다.

### 1.2 에러

```json
{
  "error": {
    "code": "instance_not_found",
    "message": "인스턴스를 찾을 수 없습니다",
    "details": { "instance_id": "ap-northeast-2/nope" },
    "request_id": "01J…"
  }
}
```

| HTTP | code 예 | 의미 |
|---|---|---|
| 400 | `invalid_request`, `range_too_large`, `invalid_cursor` | 입력 오류 |
| 401 | `unauthenticated`, `token_expired` | 토큰 없음·만료 |
| 403 | `forbidden` (+ `required_role`), `env_out_of_scope` | 권한 부족 |
| 404 | `instance_not_found`, `digest_not_found`, `record_not_found` | |
| 409 | `plan_stale`, `already_running` | 상태 충돌 |
| 422 | `bootstrap_precondition_failed`, `unsupported_version` | 전제 미충족 |
| 429 | `rate_limited` (+ `Retry-After`) | 레이트 리밋 |
| 502 | `upstream_error` (+ `upstream`: `aws_rds` 등) | 외부 의존성 실패 |
| 503 | `budget_exceeded`, `service_degraded` | 예산 초과·부분 장애 |

**에러 메시지에 내부 정보를 넣지 않는다** — SQL 전문, ARN, 스택 트레이스는 로그에만.
`request_id`로 추적한다.

### 1.3 레이트 리밋

| 대상 | 한도 |
|---|---|
| 사용자당 전체 | 분당 600 요청 |
| Athena 조회 시작 | 사용자당 분당 10 |
| 어드바이저 실행 | 사용자당 분당 5, 전체 분당 20 |
| 부트스트랩 | 사용자당 분당 2 |
| CloudWatch 온디맨드 | 사용자당 분당 30 |
| WebSocket 연결 | 사용자당 5 |

응답 헤더: `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`.

## 2. 엔드포인트

권한 표기: `A`=admin, `O`=operator, `V`=viewer.

### 2.0 미인증 허용 엔드포인트 (전체 목록)

**이 4개가 전부다.** 새 엔드포인트를 미인증으로 추가하려면 이 표를 갱신하고
계약 테스트의 예외 목록에도 넣어야 한다([15 §5](15-testing.md)).

| 엔드포인트 | 이유 | 노출 정보 | 보호 |
|---|---|---|---|
| `GET /healthz` | ALB·systemd 헬스체크 | 없음 (200 고정) | — |
| `GET /readyz` | ALB 대상 등록 판정 | 준비 여부만. **어떤 의존성이 실패했는지 노출하지 않는다** | 결과 5초 캐시 + WAF IP 레이트 리밋 (DoS 방어) |
| `GET /api/auth/config` | SPA가 로그인 전에 필요 | 공개 OIDC 메타데이터 | WAF 레이트 리밋 |
| `GET /api/openapi.json` | 개발 편의 | API 구조 | **프로덕션에서는 인증 필요로 전환** (설정 `api.public_openapi = false`, 기본 false) |

### 2.1 시스템

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/healthz` | — | 프로세스 생존. 200 고정. systemd 워치독용 |
| GET | `/readyz` | — | 의존성 확인 + **리더 여부**. active면 200, standby면 503. ALB 헬스체크 경로 |
| GET | `/api/me` | V | 현재 사용자·역할·환경 스코프·기능 플래그 |
| GET | `/api/openapi.json` | — | OpenAPI 스펙 |
| GET | `/api/version` | V | 빌드 버전, 커밋, 빌드 시각 |
| GET | `/api/usage` | V | 이번 달 사용량(CloudWatch API, Athena 스캔, Bedrock 토큰) 및 예산 |

`/api/me` 응답:
```json
{
  "sub": "…", "email": "a@b.com",
  "role": "operator",
  "env_scope": null,
  "can_see_literals": true,
  "features": { "ai_enabled": true, "cwlog_enabled": true, "rds_modify_enabled": false }
}
```

### 2.2 인스턴스

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/instances` | V | 목록. `?env=&region=&engine=&state=&q=&enabled=` |
| GET | `/api/instances/{id}` | V | 상세 (메타 + 수집 상태 + 자가진단 요약) |
| PATCH | `/api/instances/{id}` | O | `{ enabled?, poll_interval_ms?, slow_threshold_ms? }` — **`literal_policy` 는 여기 없다** |
| PUT | `/api/instances/{id}/literal-policy` | **A** | `{ policy }`. admin 전용으로 분리 (T-10 확장, [08 §4.3](08-security-auth.md)) |
| PUT | `/api/instances/{id}/network-access` | O | `{ state, requested_to? }` 네트워크 접근 요청 상태 ([14 §3.4](14-infrastructure.md)) |
| PATCH | `/api/instances/{id}/env` | A | `{ env }` 수동 지정(오버라이드) |
| POST | `/api/instances/discover` | A | 즉시 재탐색. `{ regions?: [] }` |
| GET | `/api/instances/{id}/diagnostics` | V | 자가진단 결과 |
| POST | `/api/instances/{id}/diagnostics` | O | 자가진단 재실행 |
| GET | `/api/instances/{id}/live` | V | 실시간 지표 스냅샷(WS 미사용 클라이언트용) |
| GET | `/api/instances/{id}/replication` | V | 복제 상태 현재값 + 최근 이벤트 |
| GET | `/api/instances/{id}/locks` | V | 현재 락 대기 체인 + 장기 트랜잭션 |
| GET | `/api/instances/{id}/indexes` | V | 인덱스 위생 스냅샷 (`?date=`) |
| GET | `/api/instances/{id}/params` | V | 주요 파라미터 현재값 + 기준선 diff |

`{id}`는 URL 인코딩된 `instance_id`(`ap-northeast-2%2Forders-prd-01`).

### 2.3 슬로우 쿼리

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/slow-queries` | V | 목록 |
| GET | `/api/slow-queries/{record_id}` | V | 상세 (플랜 포함) |
| GET | `/api/slow-queries/{record_id}/plan` | V | 플랜만 (원본 JSON / TREE / 정규화 트리) |
| GET | `/api/slow-queries/count` | V | 근사 건수 |

목록 쿼리 파라미터:
```
from, to           (필수, RFC3339. 최대 400일)
env                (복수 가능)
instance_id        (복수 가능)
schema_name
db_user
app_digest
min_duration_ms    (기본 없음)
max_duration_ms
statement_type     (복수 가능)
has_plan           (true/false)
plan_source        (for_connection/rerun/none)
no_index_used      (true/false)
sort               (started_at | duration_ms | rows_examined, 접두 '-' 로 내림차순)
limit, cursor
```

응답 항목(목록):
```json
{
  "record_id": "123456789012/ap-northeast-2/orders-prd-01:8842119:1755500400",
  "started_at": "2026-08-18T14:23:11Z", "started_at_ms": 1755500591000,
  "ended_at": "2026-08-18T14:23:15Z",
  "duration_ms": 4213, "duration_source": "slowlog",
  "instance_id": "…", "env": "prd", "schema_name": "shop",
  "db_user": "app", "db_host": "10.0.3.44:51234",
  "app_digest": "9f2c…", "statement_type": "SELECT",
  "sql_preview": "SELECT o.* FROM orders o JOIN users u ON …",
  "sql_masked": false,
  "rows_examined": 8213445, "rows_sent": 12,
  "no_index_used": true, "tmp_disk_tables": 1,
  "has_plan": true, "plan_source": "for_connection"
}
```

- **목록의 `sql_preview`는 항상 정규화 텍스트(`digest_text`)를 200자로 자른 것이다.**
  리터럴 원문은 상세 조회에서만 제공한다. 이유:
  (a) 200자 절단이 마스킹을 우회하는 경로가 되지 않게 하고,
  (b) `full_restricted` 정책의 `data.literal_view` 감사 이벤트가 목록 조회마다 폭증하지 않게
  한다([08 §6.1](08-security-auth.md)).
- `sql_masked`는 상세 응답에서 원문이 마스킹됐는지를 나타낸다.

### 2.4 다이제스트

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/digests` | V | 목록 (집계) |
| GET | `/api/digests/{app_digest}` | V | 상세 (텍스트 + 집계 + 참조 테이블) |
| GET | `/api/digests/{app_digest}/timeseries` | V | 시간별 추이. `?from=&to=&instance_id=&granularity=hour\|day` |
| GET | `/api/digests/{app_digest}/samples` | V | **실행 가능한 샘플 목록** |
| GET | `/api/digests/{app_digest}/tables` | V | 참조 테이블 스키마·인덱스·카디널리티 |
| GET | `/api/digests/{app_digest}/advice` | V | 캐시된 어드바이저 결과 |
| POST | `/api/digests/{app_digest}/advice` | O | 어드바이저 실행. `{ instance_id?, force? }` |
| POST | `/api/digests/{app_digest}/advice/feedback` | O | `{ recommendation_index, feedback, comment? }` |

목록 파라미터:
```
from, to      (필수)
env, instance_id, schema_name, statement_type
min_exec_count, min_total_time_ms, min_avg_time_ms
flags         (new | spike | no_index_used | tmp_disk | full_join, 복수 가능)
sort          (total_time_ms | exec_count | avg_time_ms | max_time_ms |
               p95_ms | rows_examined | examined_sent_ratio)
limit, cursor
```

**`/samples` 응답** — 요구사항의 핵심 엔드포인트.

```json
{
  "digest_text": "SELECT `o`.* FROM `orders` `o` JOIN … WHERE `o`.`status` = ? …",
  "digest_text_source": "mysql",
  "digest_text_truncated": false,
  "digest_confidence": "high",
  "samples": [
    {
      "kind": "slowest",
      "record_id": "123456789012/ap-northeast-2/orders-prd-01:8842119:1755184320000",
      "instance_id": "123456789012/ap-northeast-2/orders-prd-01",
      "captured_at": "2026-08-14T03:12:00Z",
      "duration_ms": 4213,
      "rows_examined": 8213445,
      "rows_sent": 12,
      "sql_text": "SELECT o.* FROM orders o JOIN users u ON o.user_id = u.id WHERE o.status = 'PENDING' AND o.created_at BETWEEN '2026-07-01' AND '2026-08-14' ORDER BY o.id DESC LIMIT 100",
      "executable": true,
      "masked": false,
      "has_plan": true,
      "plan_source": "for_connection"
    },
    { "kind": "recent",      "…": "…" },
    { "kind": "rows_max",    "…": "…" },
    {
      "kind": "ps_sample",
      "instance_id": "123456789012/ap-northeast-2/orders-prd-02",
      "captured_at": "2026-08-18T14:20:00Z",
      "sql_text": "SELECT o.* FROM orders o JOIN users…",
      "executable": true,
      "masked": false,
      "truncated": true,
      "has_plan": false,
      "note": "performance_schema QUERY_SAMPLE_TEXT. 1024바이트에서 절단될 수 있습니다."
    }
  ],
  "sample_availability": {
    "captured_samples": 3,
    "has_ps_sample": true,
    "literal_policy": "full",
    "reason_if_empty": null
  }
}
```

`kind` 값: `slowest` | `recent` | `rows_max` | `ps_sample`.
`executable=false`인 경우(`masked`/`off` 정책, 또는 절단) 사유를 `note`에 담는다.

### 2.5 메트릭

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/metrics/cloudwatch` | V | `?instance_id=&metrics=&from=&to=&stat=` (복수 인스턴스 가능) |
| GET | `/api/metrics/live` | V | `?instance_id=` 자체 수집 최근 60분 |
| GET | `/api/metrics/catalog` | V | 엔진별 사용 가능 메트릭 목록 |
| GET | `/api/metrics/os` | V | Enhanced Monitoring 최근 값 (P2) |

`GET /api/metrics/cloudwatch` 응답:
```json
{
  "period_sec": 60,
  "requested_period_sec": 60,
  "period_adjusted": false,
  "cached": true,
  "cached_at": "2026-08-18T14:23:00Z",
  "series": [
    {
      "instance_id": "…", "metric": "CPUUtilization", "stat": "Average", "unit": "Percent",
      "timestamps_ms": [1755500400000, 1755500460000],
      "values": [42.1, 44.8]
    }
  ],
  "budget": { "used_pct": 34.2, "throttled": false }
}
```

시계열을 **병렬 배열**(`timestamps_ms` + `values`)로 반환한다.
객체 배열보다 페이로드가 40% 작고, uPlot이 요구하는 형태와 일치한다.

### 2.6 이벤트·알림

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/events` | V | `?instance_id=&env=&kind=&from=&to=` |
| GET | `/api/alerts` | V | `?state=firing\|resolved\|all&env=&severity=&from=&to=` |
| GET | `/api/alerts/{id}` | V | 상세 |
| POST | `/api/alerts/{id}/ack` | O | 확인 |
| GET | `/api/alert-rules` | V | 규칙 목록 (발화 통계 포함) |
| POST | `/api/alert-rules` | O | 생성 |
| GET | `/api/alert-rules/{id}` | V | 상세 |
| PUT | `/api/alert-rules/{id}` | O | 수정 |
| DELETE | `/api/alert-rules/{id}` | O | 삭제 |
| POST | `/api/alert-rules/{id}/test` | O | 테스트 발송 |
| POST | `/api/alert-rules/preview-scope` | O | `{ scope }` → 매칭되는 인스턴스 목록 |
| GET | `/api/channels` | O | 채널 목록 (자격증명 마스킹) |
| POST | `/api/channels` | A | 생성 (자격증명 → Secrets Manager). 즉시 테스트 발송 |
| PUT | `/api/channels/{id}` | A | 수정 |
| DELETE | `/api/channels/{id}` | A | 삭제 |
| GET | `/api/mutes` | O | 음소거 목록 |
| POST | `/api/mutes` | O | 생성 |
| DELETE | `/api/mutes/{id}` | O | 해제 |
| GET | `/api/maintenance-windows` | O | 점검 창 목록 |
| POST | `/api/maintenance-windows` | A | 생성 |

### 2.7 부트스트랩

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/bootstrap/sources` | A | 인스턴스별 사용 가능한 자격증명 소스 |
| POST | `/api/bootstrap/plan` | A | 계획 생성 (dry-run) |
| POST | `/api/bootstrap/apply` | A | 실행 |
| GET | `/api/bootstrap/jobs/{job_id}` | A | 진행 상황 |
| GET | `/api/bootstrap/manual-script` | A | 수동 실행용 SQL·CLI 생성 |
| GET | `/api/bootstrap/privileges/{instance_id}` | A | 현재 권한 vs 필요 권한 diff |

`POST /api/bootstrap/plan` 요청:
```json
{
  "instance_ids": ["123456789012/ap-northeast-2/orders-prd-01"],
  "credential_source": { "kind": "rds_managed" },
  "auth_method": "iam",
  "privilege_mode": "least",
  "schemas": ["shop", "orders"],
  "monitor_user": "dbmon"
}
```

`credential_source.kind`: `rds_managed` | `secret_arn` | `manual`.
`manual`인 경우 `{ "kind": "manual", "username": "...", "password": "..." }`.
**이 필드는 요청 본문에만 존재하며 응답·로그·DB에 절대 나타나지 않는다.**

응답:
```json
{
  "plan_id": "01J…",
  "expires_at": "2026-08-18T14:33:00Z",
  "items": [
    {
      "instance_id": "123456789012/ap-northeast-2/orders-prd-01",
      "env": "prd",
      "current_state": {
        "user_exists": false, "auth_plugin": null,
        "iam_auth_enabled": false, "grants": []
      },
      "actions": [
        { "kind": "rds_modify", "detail": "EnableIAMDatabaseAuthentication=true",
          "risk": "high", "executable_by_app": false,
          "manual_command": "aws rds modify-db-instance --db-instance-identifier orders-prd-01 --enable-iam-database-authentication --apply-immediately --region ap-northeast-2" },
        { "kind": "sql", "risk": "low",
          "statement": "CREATE USER IF NOT EXISTS 'dbmon'@'%' IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS' REQUIRE SSL" },
        { "kind": "sql", "risk": "low",
          "statement": "GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon'@'%'" }
      ],
      "warnings": ["프로덕션 환경입니다. 확인 타이핑이 필요합니다.",
                   "IAM DB 인증 활성화는 앱 권한 밖입니다. 수동 명령을 실행하세요."],
      "requires_typed_confirmation": true
    }
  ]
}
```

`POST /api/bootstrap/apply`:
```json
{
  "plan_id": "01J…",
  "confirmations": { "123456789012/ap-northeast-2/orders-prd-01": "orders-prd-01" },
  "credential_source": { "kind": "manual", "username": "…", "password": "…" }
}
```

- `plan_id`가 만료됐거나 대상 상태가 계획 시점과 다르면 `409 plan_stale`.
- `manual` 자격증명은 plan에 저장되지 않으므로 apply에서 다시 보낸다.
- 응답은 `{ job_id }`. 진행 상황은 WebSocket `bootstrap_progress` 또는 폴링.

### 2.8 리포트

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/reports` | V | `?type=monthly\|improvement&env=&from=&to=` |
| GET | `/api/reports/{id}` | V | 메타 + HTML presigned URL |
| GET | `/api/reports/{id}/data` | V | 원본 집계 JSON |
| POST | `/api/reports/monthly` | O | 생성. `{ env, month: "2026-07" }` |
| POST | `/api/reports/improvement` | O | 생성. [12 §3.1](12-reporting.md) 참조 |
| POST | `/api/reports/{id}/sections/retry` | O | `{ sections: [4, 9, 11] }` **실패 섹션만 재생성.** 저장된 `snapshot_ids`를 재사용하므로 나머지 섹션 수치가 바뀌지 않는다 (F22) |
| POST | `/api/digests/{app_digest}/watch` | O | 히스토그램 관찰 등록 ([12 §3.2.1](12-reporting.md)). 어드바이저 실행 시 자동 등록 |
| DELETE | `/api/digests/{app_digest}/watch` | O | 관찰 해제 |
| DELETE | `/api/reports/{id}` | A | 삭제 |

### 2.9 비동기 조회 (Athena)

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| POST | `/api/queries` | V | 시작. `{ type, params }` |
| GET | `/api/queries/{id}` | V | 상태 |
| GET | `/api/queries/{id}/results` | V | 결과 (커서 페이지네이션) |
| DELETE | `/api/queries/{id}` | V | 취소 |

**권한은 라우트가 아니라 `type`에서 강제한다 (T-17).**
`POST /api/queries` 자체를 viewer에게 열어두면 `audit_search`로 감사 로그를,
`slow_query_text_search`로 리터럴 전문 검색을 할 수 있다.

| `type` | 최소 권한 | 비고 |
|---|---|---|
| `slow_queries_range` | V | env 스코프 필터 필수 |
| `digest_rollups_range` | V | |
| `digest_history` | V | |
| `top_digests` | V | |
| `instance_trend` | V | |
| `plan_history` | V | |
| `alert_history` | V | |
| `slow_query_text_search` | **O** + `can_see_literals` | 리터럴 검색이므로 |
| `audit_search` | **A** | [08 §4.2](08-security-auth.md)의 "감사 로그 조회 = admin" |

**Athena 결과도 마스킹·스코프를 적용한다.** 결과가 행 배열이라 DTO가 아니므로
[08 §4.3](08-security-auth.md)의 `Redactable` 직렬화 계층을 타지 않는다.
→ Athena 결과 후처리 계층에서 **컬럼 단위 마스킹 + env 스코프 필터**를 강제한다.
쿼리 템플릿마다 "어느 컬럼이 리터럴인가 / 어느 컬럼이 env인가"를 메타데이터로 선언하고,
후처리가 그것을 읽어 적용한다.

**`execution_id`를 요청자에게 바인딩한다.** `GET /api/queries/{id}` 와 `/results`는
`sub`가 일치하지 않으면 404(존재 여부 노출 방지). 남의 `execution_id`를 폴링할 수 없다.

**결과 조회도 감사 이벤트를 남긴다** (`athena.results_fetched`).
리터럴이 조직 밖으로 나가는 경로이기 때문이다([08 §8](08-security-auth.md)).

**사용자별 일일 스캔 예산** (T-35) — viewer 1명이 분당 10회 × 10GB로 워크그룹 일 한도를
2분에 소진할 수 있다. 그러면 월간 리포트와 과거 조회가 전부 실패한다.
→ 사용자별 일일 스캔 바이트 예산을 앱이 집행하고(잔여를 응답에 표시), 초과 시 503.
리포트·아카이브는 **별도 워크그룹**을 써서 사용자 조회와 한도를 격리한다.

상태 응답:
```json
{
  "execution_id": "…", "state": "running",
  "started_at": "…", "elapsed_ms": 4210,
  "scanned_bytes": 128000000,
  "estimated_cost_usd": 0.0006,
  "cached": false
}
```

### 2.10 설정

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/settings` | V | 병합된 유효 설정 (계층별 출처 표시) |
| GET | `/api/settings/{scope}` | A | `scope` = `global` \| `env/{env}` \| `instance/{id}` |
| PUT | `/api/settings/{scope}` | A | 수정 (부분 업데이트) |
| GET | `/api/settings/aws` | A | 리전·탐색 주기·태그 매핑 |
| PUT | `/api/settings/aws` | A | |
| GET | `/api/settings/iam-diagnostics` | A | 권한 진단 |
| GET | `/api/settings/regions/available` | A | `ec2:DescribeRegions` 결과 |

`GET /api/settings` 응답의 출처 표시:
```json
{
  "poll_interval_ms": { "value": 1000, "source": "global" },
  "slow_threshold_ms": { "value": 3000, "source": "env/prd" },
  "literal_policy": { "value": "masked", "source": "instance/ap-northeast-2/orders-prd-01" }
}
```

어떤 계층에서 온 값인지 보여줘야 사용자가 왜 이 값인지 이해할 수 있다.

### 2.11 인증 관리

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/auth/idps` | A | IdP 목록 |
| POST | `/api/auth/idps` | A | 등록 (Cognito + App Client 동시 갱신) |
| PUT | `/api/auth/idps/{name}` | A | 수정 |
| DELETE | `/api/auth/idps/{name}` | A | 삭제 |
| GET | `/api/auth/users` | A | 사용자 목록 |
| POST | `/api/auth/users` | A | 초대 |
| PATCH | `/api/auth/users/{sub}` | A | 그룹·활성 상태·환경 스코프 |
| POST | `/api/auth/users/{sub}/reset-password` | A | 비밀번호 초기화 |
| GET | `/api/auth/group-mapping` | A | IdP 그룹 → Cognito 그룹 매핑 |
| PUT | `/api/auth/group-mapping` | A | |
| GET | `/api/auth/config` | — | **미인증 허용.** SPA가 로그인 전에 필요한 값 |

`GET /api/auth/config` (미인증):
```json
{
  "cognito_domain": "https://dbmon.auth.ap-northeast-2.amazoncognito.com",
  "user_pool_id": "ap-northeast-2_XXXX",
  "client_id": "…",
  "scopes": ["openid", "email", "profile"],
  "idp_buttons": [{ "name": "okta", "display_name": "Okta 로그인" }]
}
```

민감정보가 아니다(모두 공개 OIDC 메타데이터). 이 엔드포인트가 없으면 프론트에
빌드 타임 환경변수로 넣어야 하고, IdP 추가 시 프론트 재빌드가 필요해진다.

### 2.12 감사

| 메서드 | 경로 | 권한 | 설명 |
|---|---|---|---|
| GET | `/api/audit` | A | `?from=&to=&actor=&event=&instance_id=` |

## 3. WebSocket

```
wss://<도메인>/ws
```

프로토콜 상세는 [09 §4.1](09-frontend.md#41-websocket-프로토콜).

**토픽 인가 매트릭스** — `env` 차원이 없는 토픽이 문제였다(T-22).
`slowq:digest=`는 다이제스트가 여러 환경에 걸쳐 있을 수 있어 dev 스코프 사용자가 prd 이벤트를
받을 수 있었고, `job:`/`query:`는 소유자 검증이 없어 다른 사용자의 부트스트랩 진행 상황
(인스턴스 식별자·실패 SQL 문맥)을 볼 수 있었다.

| 토픽 | 인가 판정 |
|---|---|
| `slowq:env=<env>` | `env ∈ 사용자 env_scope` |
| `slowq:inst=<instance_id>` | 인스턴스의 env가 스코프 안 |
| `slowq:digest=<app_digest>` | **구독은 허용. 단 서버가 이벤트별로 env를 확인해 스코프 밖은 전송하지 않는다** (다이제스트는 크로스 환경이라 구독 시점 판정이 불가) |
| `status:inst=<instance_id>` | 인스턴스의 env가 스코프 안 |
| `status:env=<env>` | `env ∈ 스코프` |
| `alert:env=<env>` | `env ∈ 스코프` |
| `job:<job_id>` | **잡의 `created_by == sub`** 이거나 admin |

**`query:<execution_id>` 토픽은 없다** — Athena 완료 통지는 WS를 쓰지 않고 HTTP 폴링으로
한다([ADR-015](03-decisions.md)). 2단계에서 팬아웃 의존성을 하나 더 만들지 않기 위함이다.

- 와일드카드·접두 구독(`slowq:*`)은 **거부한다**.
- 클라이언트당 토픽 최대 50개.
- `status:env=prd`는 인스턴스 500개를 개별 구독하지 않도록 서버가 집계해 보낸다
  (플릿 개요 화면용). 상위 20대 + 환경 합계.
- 인가 매트릭스는 **토픽 × 역할 × env 스코프 테이블 주도 테스트**로 강제한다.

## 4. 캐시 헤더

| 대상 | 헤더 |
|---|---|
| 인스턴스 목록 | `Cache-Control: private, max-age=30` |
| 슬로우 쿼리 상세 (확정된 레코드) | `Cache-Control: private, max-age=3600` + `ETag` |
| 다이제스트 텍스트 | `Cache-Control: private, max-age=3600` + `ETag` |
| 플랜 | `Cache-Control: private, max-age=86400, immutable` |
| 메트릭 | `Cache-Control: private, max-age=60` |
| 목록 조회 | `Cache-Control: no-store` (필터가 다양해 캐시 효율 낮음) |
| 설정·인증 | `Cache-Control: no-store` |
| OpenAPI | `Cache-Control: public, max-age=300` |

플랜은 불변이다(한 번 수집되면 안 바뀐다) → `immutable`. 큰 페이로드라 캐시 효과가 크다.

## 5. 구현 규약

```rust
// 라우터는 권한을 타입으로 요구한다. 미들웨어에 의존하지 않는다.
Router::new()
    .route("/api/instances",            get(list_instances))          // Authed
    .route("/api/instances/:id",        patch(patch_instance))        // OperatorOnly
    .route("/api/bootstrap/apply",      post(bootstrap_apply))        // AdminOnly
```

- 핸들러 시그니처의 추출자(`Authed` / `OperatorOnly` / `AdminOnly`)가 권한을 강제한다.
  라우트에 미들웨어를 붙이는 방식은 새 라우트 추가 시 빠뜨릴 수 있다.
- 응답 DTO는 `Redactable`을 구현하고, 응답 래퍼가 `can_see_literals`에 따라 자동 마스킹한다
  ([08 §4.3](08-security-auth.md)).
- 모든 목록 엔드포인트는 `TimeRange`를 필수 인자로 받는다(무한 범위 조회 원천 차단).
- OpenAPI는 `utoipa` 매크로로 코드에서 생성한다. 스펙과 구현이 갈라지지 않는다.
- 프론트 타입은 `openapi-typescript`로 생성하고 CI에서 diff 검사한다
  (스펙 변경 후 타입 재생성을 잊으면 CI 실패).

## 6. 테스트

| 대상 | 방법 |
|---|---|
| 전 엔드포인트 인증 필수 | 라우터 목록을 순회해 토큰 없이 호출 → 401 확인 |
| 권한 매트릭스 | 역할 × 엔드포인트 테이블 주도 테스트 |
| 환경 스코프 | 제한된 사용자로 조회 시 결과 필터링 확인 |
| 리터럴 마스킹 | `can_see_literals=false` 응답에 리터럴 부재 |
| 입력 검증 | 각 파라미터의 경계·부정 케이스 |
| 커서 왕복 | 페이지네이션으로 전체 순회 시 중복·누락 없음 |
| 에러 형식 | 모든 에러 코드가 정의된 스키마를 따르는지 |
| OpenAPI 정합성 | 생성된 스펙으로 실제 응답 검증 (스키마 검증 미들웨어를 테스트 모드에서 활성) |
| 레이트 리밋 | 한도 초과 시 429 + `Retry-After` |
| 부트스트랩 자격증명 누출 | apply 요청 후 응답·로그·DynamoDB 전수 검사 |
