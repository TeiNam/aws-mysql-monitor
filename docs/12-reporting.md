# 12. 리포팅

## 1. 원칙

1. **수치가 들어간 문장은 LLM이 만들지 않는다.** 표·차트는 Athena 결과를 직접 렌더하고,
   수치 요약 문장은 **typed template**으로 만든다. LLM은 **숫자 없는 해설**만 담당한다
   ([ADR-017](03-decisions.md), §4).
2. **재현 가능해야 한다.** 같은 기간으로 다시 생성하면 같은 수치가 나온다.
   집계 쿼리와 데이터 버전을 리포트에 기록한다.
3. **관측 커버리지를 명시한다.** 다이제스트 상위 N 제한(`_other`) 때문에 집계가 전체의
   일부일 수 있다. "관측된 총 실행시간의 97.3%를 상위 200개 다이제스트가 설명함"을 적는다.
   커버리지는 시간 버킷 단위로 계산해 가중 평균한다(§2.2 쿼리 14).
6. **`partial` 시간 버킷의 처리 규칙을 하나로 고정한다** (§0.2).
4. **PDF를 직접 만들지 않는다.** 인쇄 최적화 HTML + 브라우저 인쇄로 충분하다.
5. **리포트의 "월"은 리포트 시간대 기준이다.** 저장은 UTC지만 사람이 읽는 "7월 리포트"는
   현지 시간대의 7월이다. 이 변환을 명시하지 않으면 매달 9시간이 잘못된 달에 들어간다.

### 0.2 `partial` 시간 버킷 처리 규칙 (F13)

워커 재시작·리더 교체·플러시 실패로 시간 일부만 관측된 버킷은 `partial=true`다.
초기 설계는 이걸 저장하고 커버리지 쿼리에서 개수만 셌지만, **총량·순위·신규 판정 쿼리는
`partial`을 전혀 고려하지 않았다.** 워커 재시작이 잦은 달에는 총량이 실제보다 낮게 나오고
"개선"으로 오판된다.

**규칙 (전 섹션 공통)**

| 지표 유형 | 처리 | 이유 |
|---|---|---|
| 순위 (Top 20 다이제스트·인스턴스) | **partial 포함** | 분모도 같이 줄어 상대 순위 왜곡이 작다 |
| 건당 지표 (평균, 건당 rows_examined) | **partial 포함** | 실행수로 나누므로 관측 시간과 무관 |
| **총량** (총 실행시간, 총 실행수) | **partial 포함 + 관측 시간 비율로 정규화 표시** | 그대로 쓰면 과소 집계 |
| 신규/사라진 다이제스트 판정 | **partial 제외** | 관측이 끊긴 시간을 "미관측"으로 오판하면 안 된다 |
| 전월 대비 증감 | **양쪽 관측 비율이 90% 미만이면 "비교 불가"** | 비교의 전제가 깨진다 |
| 개선 리포트 판정 | 한쪽 기간에 partial이 20% 이상이면 **"판정 불가"** | §3.4 |

**정규화를 위해 `partial_minutes`(그 시간 중 실제 관측된 분 수)를 롤업에 저장한다**
([04 §2.3](04-data-model.md)). 그러면:

```
관측 비율 = SUM(partial_minutes) / (버킷 수 × 60)
정규화 총량 = 실측 총량 / 관측 비율
```

리포트 본문에는 **실측값과 정규화값을 함께** 표시하고, 관측 비율이 95% 미만이면 각주를 단다.
정규화값만 보여주면 추정을 사실로 제시하는 것이 된다.

### 0.1 시간대 처리 (놓치면 매달 틀리는 부분)

데이터는 전부 UTC로 저장된다([04 §0](04-data-model.md)). 리포트 시간대는 설정값
`report_timezone`(기본 `Asia/Seoul`)이다.

```
"2026-07 월간 리포트" (Asia/Seoul 기준)
  = hour_ts >= 2026-06-30T15:00:00Z
  AND hour_ts <  2026-07-31T15:00:00Z
```

- 경계 계산은 **애플리케이션에서** 하고 Athena에는 UTC 타임스탬프를 파라미터로 넘긴다.
  Athena의 `AT TIME ZONE`을 쿼리 안에서 쓰면 파티션 프루닝이 깨질 수 있다.
- 시간 롤업 버킷(`hour_ts`)은 UTC 정시 경계다. KST는 UTC+9로 시간 오프셋이 정수이므로
  버킷 경계와 KST 정시 경계가 일치한다. **30분 오프셋 시간대(예: `Asia/Kolkata` +05:30)를
  쓰면 버킷이 쪼개진다** — `report_timezone`을 정수 오프셋으로 제한하거나, 해당 경우
  경계 버킷을 비례 배분하지 않고 "경계 시간 포함" 규칙을 명시한다. 기본값이 KST이므로
  현재는 문제되지 않지만, 설정 검증에서 30분 오프셋을 경고한다.
- 리포트 본문의 모든 시각 표기에 시간대를 명시한다(`2026-07-15 03:12 KST`).
- 리포트 메타에 `report_timezone`과 계산된 UTC 경계를 기록한다(재현성).
- 서머타임이 있는 시간대는 월 경계 계산에 DST 전환이 포함될 수 있다.
  `chrono-tz`로 처리하고, 전환이 있었으면 리포트에 각주를 남긴다.

## 2. 월간 리포트

### 2.1 구성

| 섹션 | 내용 | 데이터 소스 |
|---|---|---|
| 1. 표지·요약 | 대상 기간, 인스턴스 수, 총 슬로우 쿼리, 전월 대비 | 전 섹션 집계 |
| 2. 환경별 요약 | prd/stg/dev 각각의 인스턴스 수, 슬로우 쿼리 수, 총 실행시간, 상위 다이제스트 3개 | `digest_rollups` |
| 3. 인스턴스 순위 | 총 슬로우 실행시간 상위 20대. 전월 대비 증감 | `digest_rollups` |
| 4. 다이제스트 Top 20 | 총 실행시간 기준. 실행수·평균·rows_examined 비율. **p95는 누적 추정값이므로 이 표에 넣지 않는다** ([ADR-010](03-decisions.md)) | `digest_rollups` + `digest_texts` |
| 5. 신규 등장 다이제스트 | 당월 첫 관측 + 총 실행시간 상위 10 | `digest_rollups` |
| 6. 사라진 다이제스트 | 전월 상위 20 중 당월 미관측 | 비교 |
| 7. 급증 다이제스트 | 전월 대비 총 실행시간 3배 이상 | 비교 |
| 8. 개선된 다이제스트 | 전월 대비 50% 이상 감소 | 비교 |
| 9. 락·데드락 | 데드락 발생 횟수, 락 대기 장기화 이벤트 상위 | `instance_events` |
| 10. 복제 | 지연 발생 횟수·최대 지연·중단 이벤트 | `instance_events` + `metric_rollups` |
| 11. 리소스 추세 | CPU·메모리·스토리지·연결 수 월간 추이(인스턴스별 최대·평균) | CloudWatch (월 1회 조회) |
| 12. 인덱스 위생 | 미사용 인덱스 수, 중복 인덱스 수, AUTO_INCREMENT 경고 | `daily_snapshots` |
| 13. 미해결 권고 | AI 어드바이저 권고 중 `feedback != applied` | `advisor_results` |
| 14. 관측 품질 | 커버리지, 수집 실패 시간, 플랜 수집 성공률, 유실 건수 | 플랫폼 메트릭 |
| 15. 서술 | 위 데이터에 대한 자연어 요약과 권고 | Bedrock |

**14번(관측 품질)을 리포트에 넣는 이유** — 데이터가 얼마나 믿을 만한지를 같은 문서에서
알 수 있어야 한다. 수집 실패가 30% 있던 달의 "슬로우 쿼리 감소"는 개선이 아니다.

### 2.2 집계 쿼리 예

```sql
-- 4. 다이제스트 Top 20 (총 실행시간 기준)
WITH agg AS (
  SELECT
    r.app_digest,
    SUM(r.exec_count)                                    AS exec_count,
    SUM(r.total_time_ms)                                 AS total_time_ms,
    SUM(r.total_time_ms) * 1.0 / NULLIF(SUM(r.exec_count),0) AS avg_time_ms,
    MAX(r.max_time_ms)                                   AS max_time_ms,
    SUM(r.rows_examined_sum)                             AS rows_examined,
    SUM(r.rows_sent_sum)                                 AS rows_sent,
    SUM(r.tmp_disk_tables_sum)                           AS tmp_disk_tables,
    SUM(r.no_index_used_count)                           AS no_index_used,
    COUNT(DISTINCT r.instance_id)                        AS instance_count,
    ARBITRARY(r.schema_name)                             AS schema_name
  FROM dbmon.digest_rollups r
  WHERE r.hour_ts >= ? AND r.hour_ts < ?
    AND r.env = ?
    AND r.app_digest <> '_other'
  GROUP BY r.app_digest
)
SELECT a.*,
       t.digest_text,
       t.statement_type,
       a.rows_examined * 1.0 / NULLIF(a.rows_sent, 0) AS examined_sent_ratio
FROM agg a
LEFT JOIN dbmon.digest_texts t ON t.app_digest = a.app_digest
ORDER BY a.total_time_ms DESC
LIMIT 20;
```

```sql
-- 14. 관측 커버리지 — 시간 버킷 단위로 계산한 뒤 가중 평균한다
WITH per_hour AS (
  SELECT
    hour_ts, instance_id,
    MAX(CASE WHEN app_digest = '_other' THEN total_time_all_ms END)   AS all_ms,
    SUM(CASE WHEN app_digest <> '_other' THEN total_time_ms ELSE 0 END) AS stored_ms,
    MAX(CASE WHEN app_digest = '_other' THEN other_digest_count END)  AS other_count,
    MAX(CASE WHEN app_digest = '_other' THEN truncated_candidates END) AS truncated,
    MAX(top_n) AS top_n, MAX(threshold_ms) AS threshold_ms,
    MAX(CASE WHEN partial THEN 1 ELSE 0 END) AS is_partial,
    MAX(partial_minutes) AS observed_minutes
  FROM dbmon.digest_rollups
  WHERE hour_ts >= ? AND hour_ts < ? AND env = ?
  GROUP BY hour_ts, instance_id
)
SELECT
  SUM(stored_ms)                                     AS stored_ms,
  SUM(all_ms)                                        AS all_ms,
  SUM(stored_ms) * 1.0 / NULLIF(SUM(all_ms), 0)       AS coverage_ratio,  -- 가중 평균
  SUM(is_partial)                                    AS partial_hours,
  SUM(observed_minutes) * 1.0 / (COUNT(*) * 60)       AS observation_ratio,
  MIN(top_n) AS min_top_n, MAX(top_n) AS max_top_n,   -- 설정이 바뀌었으면 범위로 드러난다
  MIN(threshold_ms) AS min_threshold, MAX(threshold_ms) AS max_threshold
FROM per_hour;
```

**분모는 `total_time_all_ms`다** — `SUM(total_time_ms)`를 분모로 쓰면 저장된 것끼리만 비교해
커버리지가 항상 100%에 가깝게 나온다. `_other` 행이 저장하는 `total_time_all_ms`(그 시간의
전체 총 실행시간)를 분모로 써야 한다([04 §2.3](04-data-model.md) F6).

`min_top_n != max_top_n`이면 리포트 기간 중 설정이 바뀐 것이므로 리포트에 경계를 표시한다.

```sql
-- 5. 신규 등장 다이제스트
WITH cur AS (
  SELECT app_digest, SUM(total_time_ms) AS total_time_ms, SUM(exec_count) AS exec_count
  FROM dbmon.digest_rollups
  WHERE hour_ts >= ? AND hour_ts < ? AND env = ? AND app_digest <> '_other'
  GROUP BY app_digest
),
prev AS (
  SELECT DISTINCT app_digest
  FROM dbmon.digest_rollups
  WHERE hour_ts < ? AND hour_ts >= date_add('month', -6, ?) AND env = ?
)
SELECT c.*, t.digest_text
FROM cur c
LEFT JOIN prev p ON p.app_digest = c.app_digest
LEFT JOIN dbmon.digest_texts t ON t.app_digest = c.app_digest
WHERE p.app_digest IS NULL
ORDER BY c.total_time_ms DESC
LIMIT 10;
```

"신규"의 정의를 **직전 6개월 미관측**으로 잡는다. 전월만 보면 격월 배치 쿼리가 매번
"신규"로 잡힌다.

집계 쿼리 전량은 `queries/reports/monthly/*.sql`에 파일로 둔다. 재컴파일 없이 수정 가능하고,
리포트에 쿼리 파일 해시를 기록해 재현성을 확보한다.

### 2.3 생성 파이프라인

```
앱 내부 cron (매월 1일 03:00 KST) — 스케줄러 리더만 실행 ([14 §4](14-infrastructure.md))
   ↓
0. 각 Iceberg 테이블의 현재 스냅샷 ID를 조회해 고정 (§4.2)
   ↓
환경별로 (prd, stg, dev):
   1. Athena 쿼리 15개 실행 (워크그룹 **dbmon-batch**, FOR VERSION AS OF 고정 스냅샷)
   2. 결과를 구조화 JSON으로 조립  → report_data.json
   3. CloudWatch 월간 메트릭 조회 (인스턴스별, period=3600, Average/Maximum)
   4. typed template 으로 수치 문장 생성 (§4)
   5. Bedrock 으로 숫자 없는 해설 생성 → 숫자 0개 검증 (§4.1)
   6. HTML 렌더 → s3://dbmon-reports-<acct>/monthly/<env>/<yyyy-mm>.html
   7. report_data.json 도 함께 저장 (재현·감사용)
   8. DynamoDB 에 리포트 메타 기록 (스냅샷 ID 포함)
   9. 슬랙/텔레그램에 요약 + **앱 프록시 링크** 발송 (FR-RPT-07, §5.1)
   10. JobHeartbeat{job=monthly_report} 발행 (침묵 감지)
```

- 워크그룹 `dbmon-batch`를 쓴다(§6). 사용자 조회 워크그룹과 한도를 격리한다.
- 실패한 섹션은 "데이터 없음"으로 표시하고 리포트 생성 자체는 계속한다(부분 실패 허용).
  어느 섹션이 실패했는지 리포트 하단에 명시한다.
- 생성 소요 시간 목표: 환경당 10분 이내.

### 2.4 리포트 메타

```
DynamoDB  PK = RPT#monthly   SK = <env>#<yyyy-mm>
```

| 속성 | 내용 |
|---|---|
| `state` | `generating` / `ready` / `partial` / `failed` |
| `s3_html_key` / `s3_data_key` | |
| `generated_at_ms` / `generated_by` | 스케줄 또는 사용자 |
| `athena_execution_ids` | 감사·재현용 |
| `query_set_hash` | 쿼리 파일들의 해시 |
| `scanned_bytes` / `cost_estimate_usd` | 비용 추적 |
| `model_id` / `prompt_version` | |
| `failed_sections` | 부분 실패 목록 |
| `coverage_ratio` | 관측 커버리지 |
| `snapshot_ids` | 테이블별 Iceberg 스냅샷 ID (재현용, §4.2) |
| `report_timezone` / `utc_bounds` | 시간대 경계 (§0.1) |
| `llm_sentences_removed` | 숫자가 포함되어 제거된 해설 문장 수 (§4.1) |
| `revision` | 재생성 횟수. 섹션 재시도 시 증가 (F22) |
| `stale` | 백필로 대상 파티션이 변경되어 낡았음 ([05 §8.3](05-collector.md) F12) |

**부분 실패의 섹션 단위 복구 (F22)** — `failed_sections`만 기록하고 재생성 API가 전체 생성뿐이면
실패 섹션 3개를 위해 15개 쿼리와 Bedrock 호출을 다시 지불한다. 더 나쁘게, **스냅샷이 새로
잡히면 나머지 12개 섹션 수치가 바뀌어 이미 배포된 리포트와 어긋난다.**

→ `POST /api/reports/{id}/sections/retry`로 섹션을 지정해 재시도한다.
- **저장된 `snapshot_ids`를 반드시 재사용한다.** 새로 잡지 않는다.
- 성공하면 `failed_sections`에서 제거하고 `revision`을 올린다. 모두 성공하면 `state=ready`.
- LLM 해설은 재생성하지 않는다(비용 + 문구 변동). 해설 실패는 해설만 별도 재시도.
- 기존 HTML은 덮어쓰되 S3 버전 관리로 이전 리비전이 남는다.

## 3. 개선 리포트 (FR-RPT-03)

"인덱스를 넣었는데 실제로 나아졌나?"에 답하는 리포트.

### 3.1 입력

```
POST /api/reports/improvement
{
  "app_digest": "9f2c1a…",
  "instance_ids": ["ap-northeast-2/orders-prd-01"],   // 비우면 전체
  "before": { "from": "2026-07-01T00:00:00Z", "to": "2026-07-15T00:00:00Z" },
  "after":  { "from": "2026-08-01T00:00:00Z", "to": "2026-08-15T00:00:00Z" },
  "change_note": "idx_status_created 추가 (2026-07-28)"
}
```

기간을 사용자가 지정한다. "적용 시점 기준 자동 분할"도 제공하지만
(어드바이저 피드백 `applied` 시각을 경계로), 배포·인덱스 생성 시점이 정확하지 않을 수 있어
수동 지정을 기본으로 둔다.

### 3.2 비교 항목

| 항목 | before | after | 판정 |
|---|---|---|---|
| 실행 횟수 | | | 정보 (워크로드 변화 확인용) |
| 총 실행시간 | | | ↓ 개선 |
| 평균 실행시간 | | | ↓ 개선 |
| p95 실행시간 | | | **사전 등록된 다이제스트만** 판정 가능 (§3.2.1) |
| 최대 실행시간 | | | 참고만. 누적 `MAX_TIMER_WAIT`는 시간창 최대가 아니다 |
| rows_examined 합 | | | ↓ 개선 |
| rows_examined / 실행 | | | ↓ 개선 (**핵심 지표**) |
| rows_examined / rows_sent | | | ↓ 개선 |
| tmp_disk_tables | | | ↓ 개선 |
| no_index_used 비율 | | | ↓ 개선 |
| sort_merge_passes | | | ↓ 개선 |
| 플랜 지문 | | | 변경 여부 + diff |
| 사용 인덱스 | | | 플랜에서 추출 |

**정규화가 중요하다.** 실행 횟수가 달라지면 총 실행시간 비교는 무의미하다.
→ **건당 지표(평균, 건당 rows_examined)를 1차 판정 기준**으로 하고, 총량은 참고로 표시한다.
실행 횟수가 30% 이상 변했으면 경고를 표시한다.

### 3.2.1 p95는 사전 등록 없이 과거 구간을 계산할 수 없다 (F3)

초기 설계는 "before 창의 p95를 `events_statements_histogram_by_digest` 버킷 델타로 계산"이라고
적었다. **이건 불가능하다.**

`events_statements_histogram_by_digest`는 **서버 시작 이후 누적**이다. 두 스냅샷의 차만
시간창 값이 되므로, **그 창이 지난 뒤에는 사후 재구성이 불가능**하다(서버 재시작이나
다이제스트 축출이 있으면 더 심각하다). 그런데 [ADR-010](03-decisions.md)은
"히스토그램은 기본 수집하지 않고 필요할 때 조회"로 정했다 — 즉 before 창 시점에는 스냅샷이
존재하지 않는다.

롤업의 `p95_ms_cumulative_snapshot`으로 대체 계산하면 ADR-010이 금지한 오용을 그대로 저지른다
(누적 추정값을 시간창 값으로 쓰는 것).

**해결 — 관찰 대상 등록 방식**

```
POST /api/digests/{app_digest}/watch     { instance_ids, note? }
  → 그 시점부터 이 다이제스트의 히스토그램 버킷 델타를 시간 롤업으로 영속화
     PK = HG#<instance_id>#<yyyy-mm>   SK = <hour_bucket>#<app_digest>
     bucket_counts: 델타 버킷 배열 (450 버킷 → 비어있지 않은 것만 sparse 저장)
  → 등록 해제 시 수집 중단 (데이터는 TTL까지 유지)
```

- **어드바이저 실행 시 자동 등록**한다. "권고를 받았다 = 개선 검증을 하고 싶다"이므로
  그 시점부터 히스토그램을 쌓는 것이 자연스럽다.
- 등록 다이제스트 수 상한(인스턴스당 20개)을 둔다. 히스토그램 조회는 다이제스트당
  버킷 450행이므로 무제한 등록은 비용이 된다.
- **등록되지 않은 다이제스트의 개선 리포트는 p95 없이 생성된다.** 판정은 건당
  `rows_examined`와 평균, 플랜 지문 변화로 한다. 리포트에 "p95: 사전 등록 없음"을 명시한다.
- 판정 기준(§3.4)도 이에 맞춰 두 갈래로 나눈다.

### 3.3 플랜 diff

before/after 각각의 대표 플랜(`plan_fingerprint` 최빈값)을 골라 트리 비교.

**개선 리포트는 대상 다이제스트가 1개**이므로 히스토그램 조회 비용이 없다.
`WHERE DIGEST = ?`로 한정해 정확한 시간창 P95를 계산한다.

```
변경 감지:
  노드별 access_type 변화       ALL → range      ✓ 개선
  key (사용 인덱스) 변화         NULL → idx_status_created
  rows_examined_per_scan 변화   1,200,000 → 342
  using_filesort 변화           true → false     ✓ 개선
  cost_info.query_cost 변화     124,331 → 89
  노드 추가/삭제
```

플랜이 하나도 없으면(둘 중 한쪽이라도) 지표 비교만 하고 "플랜 비교 불가"를 명시한다.
35일 이전 데이터는 Iceberg에서 `plan_zstd`를 읽어 복원한다.

### 3.4 판정

```
[히스토그램 등록된 다이제스트]
  개선     : 건당 rows_examined 30% 이상 감소 AND p95 20% 이상 감소
  부분 개선 : 위 중 하나만 충족
  변화 없음 : 양쪽 모두 10% 미만 변화
  악화     : 건당 rows_examined 또는 p95가 20% 이상 증가

[등록되지 않은 다이제스트 — p95 없음]
  개선     : 건당 rows_examined 30% 이상 감소 AND 평균 20% 이상 감소
  부분 개선 : 위 중 하나만 충족
  변화 없음 : 양쪽 모두 10% 미만 변화
  악화     : 건당 rows_examined 또는 평균이 20% 이상 증가
  → 리포트에 "p95 판정 없음 (사전 등록되지 않은 다이제스트)" 명시

[공통]
  판정 불가 : 한쪽 기간의 실행 횟수가 30건 미만 (표본 부족)
             또는 한쪽 기간에 partial=true 시간이 20% 이상 (F13)
```

임계값은 설정 가능하게 하되 기본값을 명시하고, 리포트에 사용된 임계값을 함께 적는다.
"판정 불가"를 명확히 두는 게 중요하다. 표본이 적으면 판정하지 않는다.

### 3.5 서술

```
Bedrock 에 before/after 표 + 플랜 diff + change_note 를 주고 서술 생성.
system 프롬프트:
  - 제공된 수치만 인용
  - 인과를 단정하지 말 것 (상관과 인과 구분)
  - change_note 외의 원인 가능성(트래픽 변화, 데이터 증가, 다른 배포)을 함께 언급
  - 판정이 "판정 불가"면 표본 부족을 명확히 서술
```

**"인과를 단정하지 말라"가 실무적으로 중요하다.** 인덱스를 넣은 주에 트래픽이 절반으로
줄었을 수도 있다. 리포트가 "인덱스 덕분에 개선됨"이라고 단정하면 잘못된 학습을 만든다.

## 4. 수치는 LLM이 만들지 않는다 (ADR-017)

**"입력에 있는 숫자인가"만 검증하는 것은 부족하다.**
`orders-prd-01의 P95가 210ms`를 `orders-prd-02의 평균이 210ms`로 잘못 서술해도
숫자는 입력에 존재하므로 검증을 통과한다. **올바른 지표·기간·대상에 붙었는가는 검증할 수
없다.**

→ 역할을 바꾼다.

| 리포트 요소 | 생성 방식 |
|---|---|
| 표·차트 | Athena 결과 직접 렌더 (LLM 미개입) |
| **수치를 포함한 요약 문장** | **typed template.** 값은 코드가 바인딩 |
| 해설·맥락·권고 | LLM. **숫자를 쓰지 말라고 지시**하고, 숫자가 나오면 그 문장을 제거 |

**typed template 예**
```rust
// 코드가 값을 바인딩한다. LLM이 개입할 여지가 없다.
tmpl!("{instance}의 {metric}이 {before}에서 {after}로 {pct:.1}% {dir}했다",
      instance = row.instance_id,
      metric   = Metric::P95Latency,     // enum → 표시명 매핑
      before   = Duration(row.before_p95_ms),
      after    = Duration(row.after_p95_ms),
      pct      = pct_change(row.before_p95_ms, row.after_p95_ms).abs(),
      dir      = Direction::from_delta(...))   // "감소" | "증가"
```

섹션마다 필요한 문장 템플릿을 미리 정의한다(15개 섹션 × 2~4문장).
**서술이 딱딱해지는 대가로 수치가 틀릴 수 없다.** 리포트에서는 이 교환이 옳다.

### 4.1 LLM 출력 검증 (숫자 0개)

```
1. LLM 응답에서 숫자 토큰을 추출한다
2. 하나라도 있으면:
     - 그 문장을 제거하고 "해설 일부 생략" 표시
     - 1회 재요청 (숫자 금지 규칙을 다시 강조)
3. 허용 예외: 없음. 숫자가 필요한 문장은 typed template이 담당한다
4. 검증 결과를 리포트 메타에 기록 (llm_sentences_removed)
```

숫자가 0개인지만 확인하므로 검증기가 단순하고 오탐이 없다.
초기 설계의 "허용 집합 A + 반올림 변형 + 축약 표기" 같은 복잡한 매칭이 사라진다.

### 4.2 재현성 — Iceberg 스냅샷 고정

같은 SQL을 재실행해도 **늦게 도착한 데이터와 `MERGE INTO` 때문에 결과가 달라진다.**
Athena는 Iceberg 스냅샷 기반 시간 여행을 지원하므로, 리포트 생성 시 사용한 스냅샷 ID를
메타에 기록하고 재현 시 고정한다.

```sql
SELECT ... FROM "s3tablescatalog/<bucket>"."dbmon"."digest_rollups"
FOR VERSION AS OF <snapshot_id>
WHERE hour_ts >= ? AND hour_ts < ?
```

- 생성 시작 시점에 각 테이블의 현재 스냅샷 ID를 조회해 고정하고, 15개 쿼리 전부가
  같은 스냅샷을 쓴다(쿼리 간 불일치 방지).
- **스냅샷 만료 기간(관리형 유지보수, 최소 7일 설정)보다 오래된 리포트는 재현이 불가하다.**
  그래서 **원본 집계 결과 JSON을 S3에 함께 저장**하는 것이 실질적 재현 수단이다.
  스냅샷 고정은 "생성 직후 며칠 안의 재현"에만 유효하다.

## 5. 렌더링

- 서버가 HTML을 생성한다(SPA 라우트가 아니라 정적 문서).
  이유: S3에 저장해 링크로 공유하고, 6개월 뒤에도 같은 내용을 봐야 한다.
  SPA로 렌더하면 프론트 코드가 바뀌면 과거 리포트도 바뀐다.
- 템플릿 엔진: `minijinja`(Rust). 템플릿은 `templates/reports/*.html` 파일.
  **autoescape를 확장자에 의존하지 말고 명시적으로 강제한다.** `|safe` 사용 금지(정적 검사).
- 차트는 **정적 SVG**로 서버에서 생성한다(`plotters` 또는 직접 SVG 조립).
  JS 의존 없이 열리고 인쇄된다.
- 인쇄 CSS: `@page { size: A4; margin: 15mm }`, 섹션 `break-inside: avoid`.

### 5.1 presigned URL을 쓰지 않는다 (T-24)

초기 설계는 `GET /api/reports/{id}`가 S3 presigned URL을 반환하고 SPA가 그걸 열게 했다.
두 가지 문제가 있다.

**① presigned URL은 무자격 bearer다.**
- 유효기간·1회성 규정이 없었고 발급 시 env 스코프 검증도 없었다
  → dev 스코프 viewer가 prd 월간 리포트(상위 다이제스트·인스턴스 순위)를 열 수 있다.
- 링크를 Slack에 붙이는 순간(§2.3-9 발송) 인증 경계 밖으로 나간다.
- 조회 감사가 남지 않는다.

**② S3는 `Content-Security-Policy` 헤더를 전달할 수 없다.**
S3가 오브젝트 메타데이터로 반환하는 헤더는 `Content-Type`, `Content-Disposition`,
`Cache-Control`, `Content-Encoding`, `Content-Language`, `Expires`뿐이다.
**presigned로 직접 열면 CSP가 없다.** 리포트에는 신뢰할 수 없는 문자열이 들어간다:
LLM이 생성한 해설, 사용자 입력 `change_note`, `digest_text`(대상 DB에서 온 SQL),
어드바이저 `ddl` 문장.
→ **저장형 XSS**가 성립하고, S3 오리진에서 실행되므로 리포트 내용을 외부로 실어낼 수 있다.

**대응 — 앱이 프록시한다.**

```
GET /api/reports/{id}/render          권한 V + env 스코프 검증
  → 앱이 S3에서 HTML을 읽어 스트리밍
  → 헤더를 앱이 직접 부여:
       Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline';
                                img-src data:; base-uri 'none'; form-action 'none'
       X-Content-Type-Options: nosniff
       Referrer-Policy: no-referrer
       Content-Type: text/html; charset=utf-8
  → report.view 감사 이벤트
```

- SPA는 이 URL을 새 탭으로 연다(iframe이면 `sandbox` 속성 함께).
- Slack/Telegram에 보내는 링크도 이 URL이다. 클릭하면 로그인이 요구된다.
- presigned가 꼭 필요하면(대용량 다운로드) **만료 5분 +
  `ResponseContentDisposition=attachment`** 로 브라우저 렌더를 막는다.

## 6. Athena 사용 규칙

**워크그룹을 2개로 분리한다** (T-35) — 하나면 viewer 1명이 일 스캔 한도를 소진해
월간 리포트와 아카이브 적재를 마비시킬 수 있다(분당 10회 × 10GB = 2분이면 200GB).

| 워크그룹 | 용도 | 쿼리당 한도 | 일 한도 | 결과 재사용 |
|---|---|---|---|---|
| `dbmon-user` | UI 온디맨드 조회 | 10GB | 150GB | **활성** (60분) |
| `dbmon-batch` | 아카이브 `MERGE INTO`, 리포트 집계 | 100GB | **300GB** | 비활성 |

`dbmon-batch`의 쿼리당 한도가 큰 이유: `MERGE INTO`는 대상 테이블의 해당 파티션을 읽어야
하므로 10GB로는 부족할 수 있다.

**일 한도가 쿼리당 한도보다 커야 한다.** 초기 설계는 쿼리당 100GB / 일 50GB로,
쿼리 1건이 자기 한도를 다 쓰면 일 한도를 2배 초과하는 **도달 불가능한 설정**이었다.
더 심각하게, 월간 리포트는 환경 3개 × 회당 20GB = 60GB를 **매월 1일 하루에 전부** 실행하므로
일 50GB를 확정적으로 초과했다. → 일 한도 300GB로 올린다(리포트 62GB + 아카이브 2GB +
개선 리포트 여유 + 재시도 여유).

**리포트를 환경별로 날짜 분산하는 대안**도 있다(prd 1일, stg 2일, dev 3일).
한도를 올리는 것보다 안전하지만 "월간 리포트가 언제 나오는가"가 환경마다 달라진다.
→ 한도를 올리고, 실측 후 분산이 필요하면 그때 나눈다.

| 항목 | 값 |
|---|---|
| 엔진 | Athena engine v3 (Iceberg DML 지원) |
| 결과 보관 | `s3://dbmon-athena-results-<acct>/`, Lifecycle 7일 |
| 결과 암호화 | SSE-KMS |
| 결과 캐시 | **워크그룹 result reuse를 쓴다.** 자체 캐시를 만들지 않는다 ([ADR-015](03-decisions.md)) |
| 동시 실행 | 리포트 생성은 환경당 순차. 사용자 조회는 single-flight |
| 파라미터화 | `ExecutionParameters` 사용. 문자열 결합 금지 (NFR-S-08) |
| 사용자별 예산 | 일일 스캔 바이트 예산을 앱이 집행. 초과 시 503 + 잔여 표시 |

**`MERGE INTO`에 파티션 조건을 반드시 넣는다** ([16 §2.6](16-cost.md)) —
`ON t.record_id = s.record_id`만으로는 `record_id`가 파티션 키가 아니라 프루닝이 불가능해
**매일 대상 테이블 전량을 스캔**한다(월 5.1TB, 그리고 쿼리당 한도에 걸려 잡이 매일 실패).

```sql
MERGE INTO ... AS t USING ... AS s
ON t.record_id = s.record_id
   AND t.started_at >= <증분구간_시작일>
   AND t.started_at <  <증분구간_종료일 + 1일>
```

쿼리 빌더가 파티션 조건 없는 `MERGE`를 만들 수 없게 타입으로 강제하고 생성 SQL을
정적 검사한다(R27).

**파티션 프루닝 강제** — 모든 쿼리는 `hour_ts`/`started_at` 범위 조건을 반드시 포함한다.
쿼리 빌더가 시간 조건 없는 쿼리를 만들 수 없게 타입으로 강제한다:

```rust
pub struct TimeRange { from: DateTime<Utc>, to: DateTime<Utc> }
// 모든 리포트 쿼리 함수는 TimeRange 를 필수 인자로 받는다
pub fn top_digests(range: TimeRange, env: Env, limit: u16) -> AthenaQuery { ... }
```

## 7. 온디맨드 조회 (리포트 외)

31일 초과 기간을 UI에서 조회할 때도 Athena를 쓴다([ADR-015](03-decisions.md)).

```
POST /api/queries          { type, params }   → { execution_id, estimated_scan_bytes? }
GET  /api/queries/{id}                        → { state, progress, scanned_bytes }
GET  /api/queries/{id}/results?page=…         → 결과 페이지
         (완료 통지는 WS 를 쓰지 않는다 — HTTP 폴링. ADR-015)
```

- `type`은 미리 정의된 쿼리 템플릿 enum이다. 임의 SQL을 받지 않는다.
- 결과는 `(type, params_hash)` 키로 캐시한다(TTL 1시간).
- 같은 쿼리가 동시에 요청되면 하나만 실행하고 공유한다(단일 비행, single-flight).
- 취소 지원(`StopQueryExecution`).

## 8. 테스트

| 대상 | 방법 |
|---|---|
| 집계 쿼리 정확성 | 알려진 픽스처 데이터를 Iceberg에 넣고 기대 결과 검증 |
| 재현성 | 같은 기간 2회 생성 → 수치 동일 |
| 커버리지 계산 | `_other` 포함/제외 케이스 |
| 신규/사라진 판정 | 6개월 경계 케이스 |
| 개선 판정 | 각 판정 등급의 경계값 |
| 표본 부족 | 실행 30건 미만 → "판정 불가" |
| 플랜 diff | before/after 플랜 픽스처 쌍 |
| 수치 검증 | 조작된 서술문 주입 → 검출 |
| 부분 실패 | 섹션 쿼리 1개 실패 → 리포트는 생성되고 실패 표시 |
| 파티션 프루닝 | 생성된 SQL에 시간 조건 존재 확인 (정적 검사) |
| 인쇄 레이아웃 | Playwright PDF 생성 후 페이지 수·잘림 검증 |
| 스캔 한도 | 한도 초과 쿼리가 실패로 처리되는지 |
