/**
 * 백엔드 응답 타입.
 *
 * # 왜 손으로 적는가
 *
 * [09 §1] 은 `openapi-typescript` 로 생성하기로 했다. 하지만 **백엔드가 아직
 * OpenAPI 문서를 내보내지 않는다.** 생성기를 먼저 넣으면 빈 스펙에서 빈 타입이
 * 나오고, 그건 `any` 와 같다.
 *
 * 그래서 지금은 Rust 쪽 뷰 타입(`crates/dbmon/src/api/view.rs`)을 그대로 옮겨
 * 적는다. OpenAPI 가 생기면 이 파일을 생성물로 교체한다.
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
}

/** `crates/dbmon/src/api/mod.rs` 의 `InstanceView`. */
export interface InstanceView {
  id: string;
  env: Env;
  state: string;
  engine: string;
  engine_version: string;
  endpoint: string | null;
  collectible: boolean;
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
 * 리터럴이 필요하면 상세 화면이 HTTP 로 조회한다.
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
  /** 마스킹된 텍스트만. 원문 정책이면 `null`. */
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
