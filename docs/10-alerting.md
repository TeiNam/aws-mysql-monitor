# 10. 알림

## 1. 규칙 모델

```json
{
  "rule_id": "repl-lag-prd",
  "name": "prd 복제 지연",
  "enabled": true,
  "source": "replication",
  "metric": "seconds_behind_source",
  "condition": { "op": ">", "value": 60 },
  "for_duration_sec": 120,
  "scope": {
    "envs": ["prd"],
    "instance_ids": [],
    "tags": { "team": "orders" },
    "exclude_instance_ids": ["123456789012/ap-northeast-2/orders-prd-analytics"]
  },
  "severity": "critical",
  "channels": ["slack-dba", "telegram-oncall", "inapp"],
  "dedup": { "group_by": ["instance_id"], "renotify_after_sec": 1800 },
  "quiet_hours": null,
  "annotations": {
    "summary": "{{instance_id}} 복제 지연 {{value}}초",
    "runbook_url": "https://wiki/…"
  }
}
```

### 1.1 소스와 메트릭

| `source` | 사용 가능한 `metric` | 평가 시점 |
|---|---|---|
| `slow_query` | `duration_ms`, `rows_examined`, `rows_examined_ratio`, `count_per_min` | 슬로우 쿼리 확정 시 |
| `digest` | `exec_count_delta`, `total_time_ms_delta`, `avg_time_ms`, `p95_ms`, `spike_ratio`, `is_new` | 다이제스트 델타 계산 후(60초) |
| `self_metric` | `qps`, `threads_running`, `lock_waits_per_sec`, `tmp_disk_ratio`, `buffer_hit_ratio`, `full_scan_ratio`, `connection_usage_pct` | 자체 지표 tick(5초) |
| `cw_metric` | CloudWatch 메트릭명 | 플릿 폴링(15분) 또는 상세 조회 시 |
| `replication` | `seconds_behind_source`, `io_running`, `sql_running`, `aurora_replica_lag_ms` | health tick(30초) |
| `lock` | `max_wait_age_sec`, `waiting_count`, `deadlock_count`, `long_txn_age_sec` | health tick(30초) |
| `index_hygiene` | `autoinc_ratio`, `unused_index_count` | daily |
| `collector` | `consecutive_failures`, `staleness_sec`, `diag_failed` | 상시 |
| `plan_change` | `plan_fingerprint_changed`, `access_type_degraded` | 플랜 수집 후 (FR-PLN-06) |
| `rds_event` | `event_category` (failover / failure / maintenance / configuration change …) | RDS 이벤트 폴링(15분) |
| `certificate` | `days_to_expiry` | daily (FR-DSC-13) |
| `platform` | `dropped_records`, `archive_job_failed`, `ai_budget_pct`, `cw_budget_pct`, `athena_scan_gb`, `masking_postcondition_failed`, `emf_metric_count` | 상시 |

**`plan_change`가 열려 있던 루프였다** — FR-PLN-06이 플랜 변경 이벤트를 기록하고
`Event.kind`에 `PLAN_CHANGE`까지 정의했는데 알림 소스 목록에 없어서
**아무도 통지받지 못했다.** 플랜이 나빠지는 것은 슬로우 쿼리보다 **먼저 오는 신호**다.
- `plan_fingerprint_changed`: 같은 다이제스트의 대표 플랜 지문이 바뀜
- `access_type_degraded`: 접근 방식이 나빠짐 (`ref`→`range`→`index`→`ALL` 방향)
  이쪽만 알림하면 오탐이 크게 줄어든다(플랜이 좋아진 변경은 통지할 필요가 없다)
- `rds_event`와 시간이 겹치면 알림 본문에 함께 담는다("파라미터 그룹 적용 12분 후 플랜 변경").

`spike_ratio` = 현재 시간창의 값 ÷ 지난 7일 같은 시간대 중앙값.
급증 탐지에 절대 임계값보다 유용하다(트래픽 패턴이 시간대별로 다르므로).
데이터가 7일치 없으면 평가하지 않는다(오탐 방지).

### 1.2 조건

```
op: > >= < <= == != 
value: number | bool | string
for_duration_sec: 조건이 연속으로 이 시간 이상 참이어야 발화 (0 = 즉시)
```

**표현식을 지원하지 않는다.** `rows_examined / rows_sent > 1000` 같은 파생값이 필요하면
파생 메트릭(`rows_examined_ratio`)을 코드에 추가한다. 사용자가 임의 표현식을 넣게 하면
파서·샌드박스·오류 처리가 전부 새 문제가 된다. → 필요한 파생 메트릭이 늘어나면 그때 재검토.

### 1.3 스코프 매칭

```
매칭 = (envs 비었거나 인스턴스 env ∈ envs)
     AND (instance_ids 비었거나 인스턴스 ∈ instance_ids)
     AND (tags 비었거나 인스턴스 태그가 전부 일치)
     AND 인스턴스 ∉ exclude_instance_ids
```

스코프가 비면 **전체 인스턴스**에 적용된다. 규칙 저장 시 "이 규칙은 현재 N대에 적용됩니다"를
미리 보여준다(실수로 전체에 걸리는 것을 방지).

## 2. 평가 엔진

### 2.0 평가와 발송의 분리 (워커 다중화 대응)

평가에 필요한 데이터(실시간 지표, 다이제스트 델타, 락 상태)는 **그 인스턴스를 소유한
collector 워커의 메모리**에 있다. 반면 그룹핑·레이트 리밋·의존 억제는 **전역 시야**가
필요하다. 이 둘을 한 곳에서 하면 워커 다중화에서 깨진다.

```
[collector 워커: 인스턴스를 소유한 쪽]           [control 워커: 단일 리더]
                                              
평가 (데이터가 로컬에 있음)                      발송 (전역 시야 필요)
  ├ 규칙 필터 + 스코프 매칭                        ├ 큐 드레인 (10초 주기)
  ├ 조건 평가                                     ├ 60초 윈도우 그룹핑
  ├ 지문 계산                                     ├ 의존 억제 (collector/platform 우선)
  ├ AlertState 상태 전이                          ├ 음소거·점검창 확인
  │   → DynamoDB 조건부 업데이트                  ├ 레이트 리밋 (전역·규칙·채널)
  │     (동시 전이 1회만 성공)                     └ 채널 팬아웃 + 재시도
  └ 전이 성공 시에만 발송 의도를 큐에 적재
      dbmon-config / NOTIFY / <ts>#<fp>
```

- **상태 전이는 DynamoDB 조건부 쓰기로 직렬화한다.**
  `ConditionExpression = "attribute_not_exists(#s) OR (#s = :from AND since = :since)"`
  → 두 워커가 같은 지문을 동시에 평가해도 전이는 한 번만 일어나고, 발송 의도도 한 번만 쌓인다.
  샤드 리스가 잠깐 겹칠 때(중복 수집 구간)도 중복 발화가 없다.
- 발송 의도 큐는 DynamoDB 항목이다. 별도 큐 서비스를 도입하지 않는다.
  TTL 1시간(오래된 의도는 발송 가치가 없다).
- 단일 프로세스 모드(1단계)에서도 같은 코드가 돈다. 큐가 메모리를 거치지 않고
  DynamoDB를 왕복하는 비용(10초 지연)은 알림에 무해하다.

### 2.1 평가 흐름

```
이벤트 도착 (또는 tick)
   ↓
활성 규칙 중 source 일치하는 것 필터
   ↓
스코프 매칭
   ↓
조건 평가 → true/false
   ↓
지문 계산  fp = sha256(rule_id + group_by 값들)[..16]
   ↓
AlertState(fp) 조건부 전이  → 성공하면 발송 의도 적재, 실패(경합)하면 무시
```

### 2.2 상태 머신

```
       조건 true                for_duration 경과
INACTIVE ────────► PENDING ──────────────────► FIRING
    ▲                 │                          │
    │   조건 false     │        조건 false         │
    └─────────────────┘◄─────────────────────────┘
    │                              │
    │                              ▼ (resolve_after 경과)
    └──────────────────────────RESOLVED ──► (7일 후 상태 삭제)
```

- `PENDING` → `FIRING` 전이 시 발화 알림 발송.
- `FIRING` 중 조건이 false가 되면 즉시 `RESOLVED`로 가지 않는다.
  `resolve_after_sec`(기본 300초) 동안 false가 유지되어야 해소한다. → 플래핑 방지.
- `RESOLVED` 전이 시 해소 알림 발송(FR-ALT-06).
- `FIRING` 상태가 지속되면 `renotify_after_sec` 간격으로 재통지.
  재통지 메시지는 "여전히 발생 중 (N분째)"로 구분한다.

### 2.3 억제

| 억제 | 규칙 |
|---|---|
| 음소거 | 인스턴스·규칙·환경 단위. 기간 지정. 음소거 중에는 상태 전이는 하되 발송만 막는다 |
| 점검 창 | 반복 일정(cron 표현식) + 기간. 창 안에서는 전체 발송 중단 |
| 그룹핑 | 같은 규칙의 여러 지문이 60초 내에 발화하면 1개 메시지로 묶는다("orders-prd-01 외 4대") |
| 의존 억제 | `collector.consecutive_failures`가 발화 중인 인스턴스는 다른 규칙 발송을 억제한다 (수집이 안 되는데 "슬로우 쿼리 없음" 알림은 무의미) |
| 플랫폼 억제 | `platform.*` 규칙이 발화 중이면 데이터 기반 규칙의 발송을 억제한다 |

**의존 억제가 실제로 중요하다.** 대상 DB가 죽으면 복제 지연·수집 실패·메트릭 이상이
동시에 터져 알림 폭풍이 된다. 근본 원인 하나만 보내야 한다.

### 2.4 발송 폭주 방어

- 전역: 분당 최대 30건. 초과분은 "N건 억제됨" 요약 1건으로 대체.
- 규칙별: 분당 최대 5건.
- 채널별: 채널 API 레이트 리밋을 준수(Slack 초당 1건, Telegram 초당 30건/채팅당 분당 20건).
  토큰 버킷으로 조절하고, 초과 시 큐에 넣는다(큐 상한 500, 초과는 드롭 + 메트릭).

## 3. 채널

### 3.1 Slack

두 방식을 지원한다.

| 방식 | 설정 | 장점 | 단점 |
|---|---|---|---|
| Incoming Webhook | URL 1개 | 설정 최소 | 채널 고정, 스레드·업데이트 불가 |
| Bot Token (`chat.postMessage`) | 봇 토큰 + 채널 ID | 채널 선택, 스레드 답글, 메시지 업데이트(해소 시 원본 수정) | 앱 설치 필요 |

**⚠ 알림 본문에 리터럴을 넣지 않는다 (T-23)** — 초기 설계는 슬로우 쿼리 알림에
`sql_preview`를 200자로 잘라 담았다. `literal_policy=full` 인스턴스면 그 프리뷰에
**운영 데이터 리터럴이 들어가고, Slack/Telegram(제3자 SaaS, 조직 외부, 채널 멤버 전원)으로
전송된다.** 사용자 권한(`can_see_literals`)·env 스코프와 무관하며 감사 이벤트도 없다.

[08 §8](08-security-auth.md)이 "`data.export`가 리터럴이 조직 밖으로 나가는 **유일한**
합법 경로"라고 적은 전제가 틀렸다.

→ **알림 본문의 SQL은 항상 `digest_text`(정규화)만 쓴다.** 리터럴은 딥링크 뒤에 둔다.
클릭하면 인증·권한·감사가 걸린 화면으로 간다. 이게 옳은 경계다.

**egress 인벤토리** — 리터럴이 조직 밖으로 나갈 수 있는 경로를 전부 열거하고 각각 통제한다.

| 경로 | 통제 |
|---|---|
| 알림 채널 (Slack/Telegram) | `digest_text`만. 리터럴 전송 불가 |
| 리포트 (S3 → Slack 링크) | 앱 프록시 + 인증·스코프·감사 ([12 §5](12-reporting.md)) |
| Athena 결과 버킷 | KMS + 앱만 접근. 결과 조회에 감사 이벤트 |
| Bedrock | `digest_text` + 정규화 플랜만 (옵트인 시에만 샘플 1건) |
| CSV/JSON 내보내기 | `data.export` 감사 이벤트 + operator 이상 |
| 브라우저 화면 | `full_restricted`는 `data.literal_view` 감사 |

메시지 형식(Block Kit):

```
🔴 [prd] 복제 지연 · orders-prd-01
지연 142초 (임계 60초, 2분 이상 지속)

인스턴스   123456789012/ap-northeast-2/orders-prd-01 (aurora-mysql 8.0.39)
시작        14:21:03 KST (12분 전)
소스 호스트 orders-prd-writer.xxx.rds.amazonaws.com

[상세 보기]  [음소거 1시간]  [런북]
```

- 심각도별 이모지·색: critical 🔴 `#e01e5a`, warning 🟠 `#ecb22e`, info 🔵 `#36c5f0`,
  resolved 🟢 `#2eb67d`.
- Bot 방식이면 해소 시 원본 메시지를 수정하고 스레드에 해소 답글을 단다
  (채널 스크롤이 알림으로 덮이지 않는다).
- **버튼(음소거)은 Slack 상호작용 엔드포인트가 필요하다** → P2. 1차는 링크만.

### 3.2 Telegram

```
POST https://api.telegram.org/bot<token>/sendMessage
{ chat_id, text, parse_mode: "HTML", disable_web_page_preview: true }
```

- HTML 파스 모드. `&`, `<`, `>` 이스케이프 필수.
- 텍스트 4096자 제한 → SQL 프리뷰는 200자로 자른다.
- 해소 시 `editMessageText`로 원본 수정(메시지 ID 보관).
- 그룹 채팅이면 `message_thread_id`(토픽) 지원.

### 3.3 인앱

- `AlertEvent` 저장 + WebSocket `alert` 푸시.
- 알림 센터에서 확인(ack)·음소거. 확인한 알림은 배지에서 제외되지만 목록에는 남는다.
- 브라우저 알림(Notification API)은 사용자가 명시적으로 허용한 경우만.

### 3.4 채널 자격증명

- Secrets Manager `dbmon/channel/<channel_id>`에 저장.
- UI는 등록 후 **다시 표시하지 않는다**(마스킹 표시만: `https://hooks.slack.com/…/T04**`).
- 등록 시 즉시 테스트 발송해 유효성을 확인한다. 실패하면 저장하지 않는다.
- 웹훅 URL 검증: https 필수, 도메인 허용목록(`hooks.slack.com`, `slack.com`,
  `api.telegram.org`), 사설·링크로컬 IP 거부(T-12).
  DNS 리바인딩 방어를 위해 **해석된 IP도 검증**한 뒤 그 IP로 연결한다.

## 4. 기본 제공 규칙 템플릿 (FR-ALT-10)

설치 시 비활성 상태로 생성된다. 관리자가 스코프·임계값을 조정해 활성화한다.

| 이름 | 소스/메트릭 | 조건 | 지속 | 심각도 |
|---|---|---|---|---|
| 복제 지연 | `replication.seconds_behind_source` | > 60 | 120s | critical |
| 복제 중단 | `replication.sql_running` | == false | 60s | critical |
| Aurora 리더 지연 | `replication.aurora_replica_lag_ms` | > 30000 | 120s | warning |
| 슬로우 쿼리 급증 | `digest.spike_ratio` | > 5 | 300s | warning |
| 신규 느린 다이제스트 | `digest.is_new` AND `avg_time_ms > 1000` | == true | 0 | info |
| 초장기 쿼리 | `slow_query.duration_ms` | > 60000 | 0 | warning |
| 비효율 스캔 | `slow_query.rows_examined_ratio` | > 100000 | 0 | info |
| 락 대기 장기화 | `lock.max_wait_age_sec` | > 30 | 30s | critical |
| 데드락 발생 | `lock.deadlock_count` | > 0 | 0 | warning |
| **플랜 악화** | `plan_change.access_type_degraded` | == true | 0 | warning |
| **RDS 페일오버** | `rds_event.event_category` | == `failover` | 0 | critical |
| **RDS 장애 이벤트** | `rds_event.event_category` | == `failure` | 0 | critical |
| **대상 인증서 만료 임박** | `certificate.days_to_expiry` | < 30 | 0 | critical |
| **마스킹 후조건 실패** | `platform.masking_postcondition_failed` | > 0 | 0 | critical |
| **EMF 메트릭 수 초과** | `platform.emf_metric_count` | > 360 | 0 | warning |
| 장기 트랜잭션 | `lock.long_txn_age_sec` | > 300 | 0 | warning |
| 연결 포화 | `self_metric.connection_usage_pct` | > 85 | 120s | critical |
| 임시 디스크 테이블 과다 | `self_metric.tmp_disk_ratio` | > 0.3 | 600s | info |
| CPU 포화 | `cw_metric.CPUUtilization` | > 90 | 600s | warning |
| 스토리지 부족 | `cw_metric.FreeStorageSpace` | < 10GB | 300s | critical |
| 버스트 소진 | `cw_metric.BurstBalance` | < 20 | 300s | warning |
| 퍼지 지연 | `cw_metric.RollbackSegmentHistoryListLength` | > 1000000 | 600s | warning |
| AUTO_INCREMENT 고갈 | `index_hygiene.autoinc_ratio` | > 0.8 | 0 | critical |
| 수집 실패 | `collector.consecutive_failures` | > 10 | 0 | warning |
| 데이터 지연 | `collector.staleness_sec` | > 300 | 0 | warning |
| 아카이브 실패 | `platform.archive_job_failed` | == true | 0 | warning |
| 레코드 유실 | `platform.dropped_records` | > 0 | 0 | critical |
| AI 예산 임박 | `platform.ai_budget_pct` | > 80 | 0 | info |
| CW 예산 임박 | `platform.cw_budget_pct` | > 80 | 0 | info |

**`platform.dropped_records`를 critical로 두는 이유** — NFR-R-06: 유실은 허용하지만
반드시 관측 가능해야 한다. 조용한 유실이 관측 도구에서 가장 나쁜 실패 모드다.

## 5. 알림 품질 원칙

1. **알림에는 다음 행동이 있어야 한다.** "CPU 90%"만 보내면 받는 사람이 할 일이 없다.
   → 그 시점의 상위 다이제스트, 락 대기 여부, 최근 배포 여부(있으면)를 함께 담는다.
2. **딥링크 필수** (FR-ALT-07). 클릭 한 번으로 해당 화면의 해당 시각으로 간다.
   URL에 `?from=...&to=...&instance=...` 를 포함한다.
3. **해소 알림을 보낸다.** 발화만 보내는 시스템은 신뢰를 잃는다.
4. **오탐이 반복되면 규칙을 고치도록 유도한다.** 규칙별 발화 횟수·평균 지속시간·확인율을
   설정 화면에 표시하고, 확인율이 낮은 규칙에 "튜닝 필요" 배지를 붙인다.
5. **정기 점검 시간에 조용해야 한다.** 점검 창을 반드시 지원한다.

## 6. 발송 신뢰성

```
발송 실패 → 지수 백오프 재시도 (1s, 4s, 16s)
3회 실패 → 인앱 알림으로 "채널 X 발송 실패" 기록 + platform 알림
채널이 연속 10회 실패 → 채널 자동 비활성 + 관리자 인앱 알림
```

- 발송 결과를 `AlertEvent.notified_channels`에 채널별로 기록한다
  (`{"slack-dba": "ok", "telegram-oncall": "failed:429"}`).
- 발송은 알림 평가와 **별도 태스크**에서 한다. 채널 장애가 평가를 막지 않는다(NFR-R-05).
- 재시도 중 프로세스가 죽으면 해당 발송은 유실된다. 큐를 영속화하지 않는다
  — 알림은 시간 민감성이 높아 나중에 보내는 게 의미가 없고, `FIRING` 상태가 유지되므로
  `renotify_after_sec` 후에 다시 발송된다. 이게 사실상의 복구 메커니즘이다.

## 7. 테스트

| 대상 | 방법 |
|---|---|
| 상태 머신 | 조건 시퀀스 → 기대 전이 테이블 주도 테스트 (플래핑 포함) |
| 스코프 매칭 | 규칙 × 인스턴스 조합 테스트 |
| `spike_ratio` | 7일 미만 데이터에서 평가하지 않는지 |
| 의존 억제 | 수집 실패 중 다른 알림이 억제되는지 |
| 그룹핑 | 60초 내 다중 지문 발화 → 1건 |
| 레이트 리밋 | 토큰 버킷 경계 |
| 채널 어댑터 | HTTP 모의 서버. 429/500/타임아웃 응답 처리 |
| SSRF | 웹훅 URL 검증 (사설 IP, 리다이렉트, DNS 리바인딩) |
| Telegram 이스케이프 | `<script>` 포함 SQL이 안전하게 렌더되는지 |
| 메시지 길이 | 4096자 초과 SQL 절단 |
