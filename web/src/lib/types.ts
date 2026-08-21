/**
 * 백엔드 응답 타입.
 *
 * # 왜 손으로 적는가
 *
 * [09 §1] 은 `openapi-typescript` 로 생성하기로 했다. 하지만 **백엔드가 아직
 * OpenAPI 문서를 내보내지 않는다.** 생성기를 먼저 넣으면 빈 스펙에서 빈 타입이
 * 나오고, 그건 `any` 와 같다.
 *
 * 그래서 지금은 Rust 쪽 뷰 타입(`crates/dbmon/src/api/{view,aggregate,mod}.rs`)을
 * 그대로 옮겨 적는다. OpenAPI 가 생기면 이 파일을 생성물로 교체한다.
 *
 * ⚠ **필드를 추측해서 넣지 않는다.** Rust 뷰에 없는 필드를 여기 적으면
 * 화면이 `undefined` 를 그리고, 그게 "데이터가 없다" 로 보인다.
 */

/** 배포 환경. Rust `Env` 와 같은 문자열이다. */
export type Env = "prd" | "stg" | "dev" | "unknown";

/**
 * 인증 방식. `GET /api/auth/config` 의 `mode` — **실제로 적용 중인 것**이다.
 *
 * `off` 는 운영 설정이 인증을 껐다는 뜻이다(배포 설정도 허용해야 그 값이 나온다).
 * 그때 화면은 토큰 안내를 띄우면 안 된다 — 있지도 않은 토큰을 찾게 만든다.
 */
export type AuthMode = "off" | "local-dev" | "local-token" | "cognito";

export interface AuthConfig {
  mode: AuthMode;
  /** 검증기가 배선됐고 설정도 완전한가. **둘 다여야 참이다.** */
  cognito_configured: boolean;
  /** 로그인 화면을 만들 공개 값. 비밀이 아니다. */
  cognito: {
    user_pool_id: string;
    client_id: string;
    region: string | null;
    domain: string;
  };
  deployment_env: Env;
}

/** `GET /api/aws/info`. 화면 머리말이 "어느 계정을 보고 있나" 를 말한다. */
export interface AwsInfo {
  account_id: string;
  region: string;
  deployment_env: Env;
}

/**
 * 멈춰 있는 스코프 하나. 키는 `*` · `env:prd` · `id:<account>/<region>/<identifier>`.
 *
 * **문자열을 화면에서 만들지 않는다** — `scopeKey()` 를 쓴다. 서버(`core::pause`)와
 * 같은 규칙이어야 하고, 어긋나면 정지가 조용히 아무 인스턴스에도 걸리지 않는다.
 */
export interface PausedScope {
  scope: string;
  since_ms: number;
}

/**
 * 수집기 상태. `crates/dbmon/src/control.rs` 의 `ControlSnapshot` + 정지 스코프 + 워커 사실.
 *
 * `scope` 가 `"deployment"` 다 — 정지는 설정 저장소에 있으므로 **모든 워커에**
 * 적용되고 재시작·리더 교체에도 남는다.
 */
export interface CollectorStatus {
  /** **전체(`*`) 정지 여부다.** 부분 정지는 `paused_scopes` 로 판단한다. */
  paused: boolean;
  /** 가장 먼저 멈춘 스코프의 시각. */
  paused_since_ms: number | null;
  /** 멈춰 있는 스코프 전부(`*` 포함). */
  paused_scopes: PausedScope[];
  /** 이 워커가 수집 리더인가. 아니면 멈추고 있는 것이 정상이다. */
  is_leader: boolean;
  collecting: number;
  last_tick_ms: number | null;
  last_discovery_ms: number | null;
  last_backfill_ms: number | null;
  discovery_requested: boolean;
  backfill_requested: boolean;
  worker_id: string;
  scope: string;
  /** 이 워커가 수집 루프를 도는가. `false` 면 즉시 탐색·백필이 409 로 거부된다. */
  runs_collector: boolean;
  /** **전체(`*`)** 를 조작할 수 있는가 — 전 환경 스코프 operator 만 참이다. */
  can_control: boolean;
  /** 환경·인스턴스 스코프를 조작할 수 있는 환경들. 그 밖은 누르면 403 이다. */
  controllable_envs: Env[];
  role: string;
}

/**
 * SQL 이 가려진 이유.
 *
 * `null` 이면 가려지지 않았다. **`not_stored` 와 `insufficient_role` 을 반드시
 * 구분해 표시해야 한다** — "정책상 없음" 과 "당신이 볼 수 없음" 은 사용자에게
 * 전혀 다른 정보다.
 */
export type SqlRedactedReason = "not_stored" | "insufficient_role";

/** `crates/dbmon/src/api/view.rs` 의 `SlowQueryView`. */
export interface SlowQueryView {
  record_id: string;
  instance_id: string;
  env: Env;
  state: string;
  thread_id: number;
  started_at_ms: number;
  duration_ms: number;
  duration_source: string;
  capture_source: string;
  app_digest: string;
  statement_type: string;
  schema_name: string | null;
  db_user: string | null;
  sql_text: string | null;
  sql_redacted_reason: SqlRedactedReason | null;
  sql_text_truncated: boolean;
  rows_examined: number | null;
  rows_sent: number | null;
  lock_time_ms: number | null;
  has_plan: boolean;
  abandoned_reason: string | null;
}

export interface ListResponse {
  items: SlowQueryView[];
  next_cursor: string | null;
  has_more: boolean;
  /** 이 응답에 담긴 건수. **저장소 전체 건수가 아니다.** */
  total: number;
}

/** `crates/dbmon/src/api/view.rs` 의 `PlanView`. 플랜 JSON 은 마스킹돼 있다. */
export interface PlanView {
  record_id: string;
  instance_id: string;
  started_at_ms: number;
  duration_ms: number;
  statement_type: string;
  normalized_json: string | null;
  tree_text: string | null;
  format_version: string | null;
  /** 300KB 초과로 S3 에 오프로드된 키. **본문 조회 경로는 아직 없다.** */
  s3_key: string | null;
  fingerprint: string | null;
  referenced_tables: string[];
  source: string;
  /** 수집 실패 사유. 있으면 그대로 보여준다. */
  error: string | null;
}

/** `crates/dbmon/src/api/mod.rs` 의 `InstanceView`. */
export interface InstanceView {
  id: string;
  name: string;
  env: Env;
  env_from_tags: Env;
  env_override: Env | null;
  state: string;
  engine: string;
  engine_version: string;
  endpoint: string | null;
  port: number;
  instance_class: string | null;
  /** 할당 스토리지(GiB). **RDS 전용, Aurora 는 `null`.** 스토리지 위험도의 분모다. */
  allocated_storage_gb: number | null;
  cluster_id: string | null;
  is_cluster_writer: boolean;
  iam_auth_enabled: boolean;
  vpc_id: string | null;
  availability_zone: string | null;
  /** 수집 대상인가. 참조 대시보드의 `REAL-TIME` 열과 같은 뜻. */
  collectible: boolean;
  tags: Record<string, string>;
  first_seen_ms: number;
  last_seen_ms: number;
  deleted_at_ms: number | null;
  cert_valid_till_ms: number | null;
}

/** 집계 응답 공통 머리. **천장에 걸렸는지 말한다.** */
export interface AggregateEnvelope<T> {
  items: T[];
  scanned: number;
  truncated: boolean;
  from_ms: number;
  to_ms: number;
  month: string | null;
}

/** `aggregate.rs` 의 `DigestRow`. */
export interface DigestRow {
  instance_id: string;
  app_digest: string;
  digest_query: string | null;
  /** 대표 SQL 이 없는 이유. `insufficient_role` 은 권한, `not_stored` 는 정책이다. */
  digest_query_redacted_reason: string | null;
  users: string[];
  statement_type: string;
  schema_name: string | null;
  exec_count: number;
  total_time_ms: number;
  avg_time_ms: number;
  max_time_ms: number;
  /** 행 정보가 하나도 없으면 `null`. **0 이 아니다.** */
  avg_rows_examined: number | null;
  first_seen_ms: number;
  last_seen_ms: number;
}

/** `aggregate.rs` 의 `InstanceStats`. */
export interface InstanceStats {
  instance_id: string;
  unique_digest_count: number;
  total_slow_query_count: number;
  total_execution_count: number;
  total_execution_time_ms: number;
  avg_execution_time_ms: number;
  max_execution_time_ms: number;
  total_rows_examined: number;
  read_query_count: number;
  write_query_count: number;
  ddl_query_count: number;
  commit_query_count: number;
  other_query_count: number;
  first_seen_ms: number;
  last_seen_ms: number;
}

/** `aggregate.rs` 의 `UserStats`. */
export interface UserStats {
  instance_id: string;
  user: string;
  total_queries: number;
  unique_digest_count: number;
  total_exec_time_ms: number;
  avg_execution_time_ms: number;
  max_execution_time_ms: number;
  read_query_count: number;
  write_query_count: number;
  ddl_query_count: number;
  commit_query_count: number;
  other_query_count: number;
}

/**
 * 실시간 지표. `crates/dbmon/src/metrics/derive.rs` 의 `LiveMetrics`.
 *
 * ⚠ 비율 항목이 `null` 일 수 있다. **`0` 으로 대체하지 말 것** — 첫 샘플이거나
 * 카운터가 초기화된 상태이고, `0` 으로 그리면 "쿼리가 없다" 로 읽힌다.
 * `rate_gap_reason` 이 이유를 알려 준다.
 */
export interface LiveMetrics {
  at_ms: number;
  qps: number | null;
  slow_per_sec: number | null;
  threads_running: number | null;
  threads_connected: number | null;
  /**
   * 연결 수의 **모수** (`@@max_connections`). 자체 수집으로 읽는다 —
   * CloudWatch 에는 이 값의 메트릭이 없다.
   *
   * 이게 없으면 `threads_connected` 로 포화를 판정할 수 없다: `10` 이 정상인지
   * 위험인지는 모수에 달렸다.
   */
  max_connections: number | null;
  lock_waits: number | null;
  rate_gap_reason: string | null;
}

/**
 * WS 방송용 슬로우 쿼리. `SlowQueryBroadcast`.
 *
 * `SlowQueryView` 보다 **필드가 적다.** 방송은 여러 사용자에게 같은 바이트를
 * 보내므로 권한으로 가릴 수 없고, 그래서 원문 리터럴을 절대 담지 않는다(T-22).
 */
export interface SlowQueryBroadcast {
  record_id: string;
  instance_id: string;
  env: Env;
  state: string;
  started_at_ms: number;
  duration_ms: number;
  app_digest: string;
  statement_type: string;
  schema_name: string | null;
  sql_preview: string | null;
}

/** 서버 → 클라이언트 WS 메시지 ([09 §4.1]). */
export type ServerMessage =
  | { t: "ready"; user: ReadyUser }
  | { t: "subscribed"; topics: string[]; denied: string[] }
  | { t: "slowq"; data: SlowQueryBroadcast }
  | { t: "status"; instance_id: string; metrics: LiveMetrics }
  | { t: "pong" }
  | { t: "error"; code: string };

export interface ReadyUser {
  subject: string;
  role: string;
  env_scope: Env[];
  can_see_literals: boolean;
}

export const ALL_ENVS: readonly Env[] = ["prd", "stg", "dev", "unknown"];

/**
 * CloudWatch 메트릭 한 점. `crates/dbmon/src/api/metrics.rs` 의 `MetricPoint`.
 *
 * `value` 가 `null` 이면 **값이 없다** — `0` 이 아니다. 방금 뜬 인스턴스, CloudWatch
 * 지연 구간, 또는 그 구성에서 발행되지 않는 메트릭이다.
 */
export interface MetricPoint {
  name: string;
  label: string;
  /** `percent` · `bytes` · `bytes_per_second` · `count` · `count_per_second` · `seconds` · `milliseconds` */
  unit: string;
  stat: string;
  value: number | null;
}

export interface FleetMetricsRow {
  instance_id: string;
  metrics: MetricPoint[];
}

export interface FleetMetricsResponse {
  rows: FleetMetricsRow[];
  /**
   * 조회가 **실패한** `계정/리전`. 비어 있으면 전부 성공했다.
   *
   * 값이 `null` 인 것과 조회 실패는 다르다 — 실패를 빈 값으로만 보여주면
   * 모니터링 장애가 "데이터 없음" 으로 읽힌다.
   */
  failed_scopes: string[];
  /** 이 값들의 조회 주기(초). 15분이다 — 개수 × 주기가 곧 비용이다. */
  period_secs: number;
  lag_note: string;
}

// ─────────────────────────────────────────────────────────────────────────────
// 운영 설정 (`crates/core/src/settings.rs`)
// ─────────────────────────────────────────────────────────────────────────────

export type SlackMode = "webhook" | "bot_token";

export interface NotifySettings {
  slack_enabled: boolean;
  slack_mode: SlackMode;
  slack_channel: string;
  /**
   * **Secrets Manager 참조**(ARN 또는 이름). 값 자체가 아니다.
   *
   * 서버는 가려서 보낸다(`••••abcd`). 그 문자열을 그대로 되돌려 보내면 서버가
   * 기존 값을 유지한다 — 새 값을 넣을 때만 실제 문자열을 보낸다.
   */
  slack_secret: string;
  message_template: string;
}

export interface AccountTarget {
  account_id: string;
  role_name: string;
  regions: string[];
  enabled: boolean;
  label: string;
}

export interface DiscoverySettings {
  /** 비어 있으면 서버가 배포 설정의 리전을 쓴다(`own_region`). */
  regions: string[];
  multi_account_enabled: boolean;
  accounts: AccountTarget[];
}

export type AuthModeSetting = "off" | "token" | "cognito";

export interface CognitoSettings {
  user_pool_id: string;
  client_id: string;
  region: string;
  domain: string;
}

export interface AuthSettings {
  mode: AuthModeSetting;
  cognito: CognitoSettings;
}

export interface AiSettings {
  enabled: boolean;
  model_id: string;
  region: string;
  max_output_tokens: number;
}

export interface AppSettings {
  /** 낙관적 잠금. 저장할 때 **읽은 값을 그대로** 돌려보낸다. */
  version: number;
  notify: NotifySettings;
  discovery: DiscoverySettings;
  auth: AuthSettings;
  ai: AiSettings;
  updated_at_ms: number;
  updated_by: string;
}

/** 어느 필드가 왜 틀렸는가. `field` 는 `notify.slack_secret` 처럼 점으로 이은 경로다. */
export interface SettingsProblem {
  field: string;
  message: string;
}

export interface SettingsView {
  settings: AppSettings;
  problems: SettingsProblem[];
  can_edit: boolean;
  /** 파일 설정이 인증 끄기를 허용하는가. 거짓이면 그 선택이 잠긴다. */
  allow_auth_disable: boolean;
  /** Cognito 검증기가 배선돼 있는가. 거짓이면 골라도 적용되지 않는다. */
  cognito_ready: boolean;
  effective_auth_mode: AuthModeSetting;
  own_region: string;
  known_regions: string[];
  /**
   * 설정을 **읽지 못했다면** 그 사유.
   *
   * 이 값이 있으면 보이는 설정은 마지막으로 읽은 값(또는 기본값)이다 —
   * 그 위에 저장하면 안 된다.
   */
  load_error: string | null;
}

// ─────────────────────────────────────────────────────────────────────────────
// AI 튜닝 권고 (`crates/core/src/tuning.rs`)
// ─────────────────────────────────────────────────────────────────────────────

export type Confidence = "high" | "medium" | "low";

export interface TuningFinding {
  title: string;
  evidence: string;
  impact: string;
}

export interface TuningIndexAdvice {
  /** `스키마.테이블`. 서버가 **컨텍스트에 있던 테이블인지 검증**한 뒤 남긴 것이다. */
  table: string;
  columns: string[];
  /** 복사용 DDL. **이 도구는 실행하지 않는다** (FR-AI-09). */
  ddl: string;
  rationale: string;
  covering: boolean;
}

export interface TuningRewrite {
  sql: string;
  rationale: string;
}

export interface TuningAdvice {
  summary: string;
  findings: TuningFinding[];
  indexes: TuningIndexAdvice[];
  rewrite: TuningRewrite | null;
  verification: string[];
  /** 한계·주의. **서버가 버린 제안도 여기 남는다.** */
  caveats: string[];
  confidence: Confidence;
  model_id: string;
  prompt_version: number;
  created_at_ms: number;
  schema_fingerprint: string;
  /** 0 이면 스키마 없이 실행계획만으로 분석했다는 뜻이다. */
  tables_analyzed: number;
}

export interface TuningView {
  /** 저장된 권고를 **읽지 못했다**. `advice: null`(아직 안 만듦)과 다르다. */
  read_failed: boolean;
  /** `null` 이면 **아직 만들지 않았다** — "권고가 비어 있다" 와 다르다. */
  advice: TuningAdvice | null;
  /** 이 배포에서 생성이 가능한가(설정이 켜져 있고 모델이 지정됐는가). */
  enabled: boolean;
  /** 이 사용자가 생성할 수 있는가(operator 이상). */
  can_generate: boolean;
  model_id: string;
}

export interface MetricSeries {
  name: string;
  label: string;
  unit: string;
  stat: string;
  timestamps_ms: number[];
  values: number[];
}

export interface InstanceMetricsResponse {
  /**
   * 페이지 상한에 걸려 **뒤쪽 데이터를 못 읽었다.**
   *
   * 긴 구간 × 메트릭 27개는 CloudWatch 응답이 여러 페이지로 나뉜다. 상한에 걸리면
   * 뒤쪽 계열이 비는데, 그건 "값이 없다" 와 다르다.
   */
  truncated: boolean;
  instance_id: string;
  engine: string;
  series: MetricSeries[];
  period_secs: number;
  /** 요청 구간이 오래돼 **CloudWatch 가 더 큰 period 만 허용**했다. 화면이 표시해야 한다. */
  period_adjusted: boolean;
  from_ms: number;
  to_ms: number;
}
