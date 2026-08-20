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

/** 인증 방식. `GET /api/auth/config` 의 `mode`. */
export type AuthMode = "local-dev" | "local-token" | "cognito";

export interface AuthConfig {
  mode: AuthMode;
  cognito_configured: boolean;
  deployment_env: Env;
}

/** `GET /api/aws/info`. 화면 머리말이 "어느 계정을 보고 있나" 를 말한다. */
export interface AwsInfo {
  account_id: string;
  region: string;
  deployment_env: Env;
}

/**
 * 수집기 상태. `crates/dbmon/src/control.rs` 의 `ControlSnapshot` + 워커 사실.
 *
 * ⚠ `scope` 가 `"process"` 다 — 이 플래그는 **요청을 받은 워커에만** 적용된다.
 * 여러 워커를 띄운 배포에서 전체를 멈추려면 설정 저장소가 필요하다.
 */
export interface CollectorStatus {
  paused: boolean;
  paused_since_ms: number | null;
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
  /** 조작할 수 있는 역할인가. 버튼 비활성의 근거. */
  can_control: boolean;
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
