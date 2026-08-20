/**
 * 실시간 스트림의 **판정부**. 소켓을 모른다.
 *
 * 백엔드가 `ws.rs`(배관)와 `topic.rs`(판정)를 나눈 것과 같은 이유로 나눈다 —
 * 이 파일은 순수 함수라 테스트가 전부 여기 걸린다. 소켓 쪽에 버그가 있으면
 * 그건 배관 버그다.
 */

import { ALL_ENVS, type Env, type LiveMetrics, type ReadyUser, type ServerMessage, type SlowQueryBroadcast } from "./types";

/** 실시간 표에 유지하는 행 수 상한. 넘으면 오래된 것부터 버린다. */
export const MAX_LIVE_ROWS = 200;
/**
 * 연결당 구독 상한. **서버의 `topic::MAX_TOPICS` 와 같아야 한다** — 넘겨 보내면
 * 서버가 조용히가 아니라 `denied` 로 답하지만, 화면이 미리 자르고 그 사실을
 * 알리는 편이 낫다.
 */
export const MAX_TOPICS = 50;
/** 스파크라인이 쓰는 표본 수. 5초 주기라면 60개 ≈ 5분. */
export const HISTORY_LEN = 60;
/**
 * 표본이 이만큼 벌어지면 **관측이 끊긴 것으로 본다** (샘플러 주기 5초의 3배).
 *
 * 서버는 `status` 방송이 밀리면 조용히 버린다([21 §5.5] — 게이지는 최신값만
 * 의미가 있다는 판단). 그 결정은 숫자 하나에는 맞지만 **이력에는 틀리다**:
 * 사라진 표본만큼 선이 시간을 건너뛰어도 화면에는 흔적이 없다. 그래서
 * 클라이언트가 `at_ms` 간격으로 직접 판정한다.
 */
export const MAX_SAMPLE_GAP_MS = 15_000;

export type ConnState = "idle" | "connecting" | "open" | "closed" | "unauthorized";

export interface LiveSnapshot {
  conn: ConnState;
  /** `ready` 로 받은 사용자. 인증 전에는 `null`. */
  user: ReadyUser | null;
  /** 서버가 확인한 구독 목록. */
  subscribed: readonly string[];
  /** 거부된 토픽. **비우지 않는다** — 화면이 "데이터 없음" 과 구분해야 한다. */
  denied: readonly string[];
  /** 최신순 슬로우 쿼리. `record_id` 로 upsert 된다. */
  slowq: readonly SlowQueryBroadcast[];
  /** 인스턴스별 최신 지표. */
  status: Readonly<Record<string, LiveMetrics>>;
  /** 인스턴스별 QPS 이력. `null` 은 "비율을 못 냈다" 이고 0 과 다르다. */
  qpsHistory: Readonly<Record<string, readonly (number | null)[]>>;
  /**
   * 방송을 놓친 횟수. **밀림(`stream_lagged`)과 연결 끊김을 함께 센다** — 둘 다
   * "이 사이의 쿼리는 이 화면에 없다" 는 같은 사실이고, 복구 방법도 같다(HTTP
   * 재조회). 값이 늘면 목록은 다시 읽고, 실시간 화면은 구멍이 있다고 말한다.
   *
   * 시각이 아니라 횟수인 이유: 같은 밀리초에 두 번 놓치면 시각은 같은 값이 되어
   * 재조회가 한 번 빠진다.
   */
  missedCount: number;
  /**
   * 마지막 프로토콜 오류 코드. **화면에 띄운다** — `malformed`·`auth_timeout` 은
   * 프론트와 백엔드가 어긋났다는 뜻이고, 조용히 삼키면 "데이터가 안 온다" 로만
   * 보인다. 연결이 다시 `ready` 가 되면 지워진다.
   */
  errorCode: string | null;
}

export const INITIAL_SNAPSHOT: LiveSnapshot = {
  conn: "idle",
  user: null,
  subscribed: [],
  denied: [],
  slowq: [],
  status: {},
  qpsHistory: {},
  missedCount: 0,
  errorCode: null,
};

/**
 * 서버 메시지 하나를 반영한다. **새 객체를 돌려준다** — 변경이 없으면 같은
 * 객체를 그대로 돌려주어 `useSyncExternalStore` 가 헛되게 리렌더하지 않게 한다.
 */
export function applyMessage(prev: LiveSnapshot, msg: ServerMessage): LiveSnapshot {
  switch (msg.t) {
    case "ready":
      // 새 연결이므로 거부 목록을 비운다 — 앞 연결의 거부를 물고 오면 스코프가
      // 늘어난 뒤에도 경고가 남는다.
      return { ...prev, conn: "open", user: msg.user, errorCode: null, denied: [] };

    case "subscribed": {
      // **거부를 누적한다.** 서버는 매 응답에 그 요청의 거부만 담으므로, 뒤에
      // 온 성공 응답이 앞의 거부 경고를 지운다. 지금 구독된 것만 목록에서 뺀다.
      const stillDenied = prev.denied.filter((t) => !msg.topics.includes(t));
      return {
        ...prev,
        subscribed: msg.topics,
        denied: [...new Set([...stillDenied, ...msg.denied])],
      };
    }

    case "slowq":
      return { ...prev, slowq: upsertSlowQuery(prev.slowq, msg.data) };

    case "status": {
      const previous = prev.status[msg.instance_id];
      const history = prev.qpsHistory[msg.instance_id] ?? [];
      // 앞 표본과의 간격이 너무 크거나 순서가 뒤집혔으면 이력에 구멍을 넣는다.
      const elapsed = previous === undefined ? 0 : msg.metrics.at_ms - previous.at_ms;
      const contiguous = previous === undefined || (elapsed > 0 && elapsed <= MAX_SAMPLE_GAP_MS);
      const base =
        contiguous || history.at(-1) === null
          ? history
          : appendBounded(history, null, HISTORY_LEN);
      return {
        ...prev,
        status: { ...prev.status, [msg.instance_id]: msg.metrics },
        qpsHistory: {
          ...prev.qpsHistory,
          [msg.instance_id]: appendBounded(base, msg.metrics.qps, HISTORY_LEN),
        },
      };
    }

    // 하트비트 응답. 상태를 바꾸지 않는다 — 같은 객체를 돌려줘야 리렌더가 없다.
    case "pong":
      return prev;

    case "error":
      // **`unauthorized` 는 재시도로 풀리지 않는다.** 소켓 쪽이 이 상태를 보고
      // 재접속을 멈춘다(같은 토큰으로 무한 재접속하면 서버 로그만 더럽힌다).
      if (msg.code === "unauthorized") {
        // **화면에 남은 데이터도 버린다.** 백엔드는 fail closed 인데 화면이 옛
        // 데이터를 계속 들고 있으면 강등된 사용자가 그걸 계속 본다(T-33 이
        // 5분 재인증을 두는 이유가 권한 축소다). `missedCount` 는 유지한다 —
        // 재조회 신호를 지우면 목록이 갱신되지 않는다.
        return {
          ...INITIAL_SNAPSHOT,
          conn: "unauthorized",
          errorCode: msg.code,
          missedCount: prev.missedCount,
        };
      }
      // 밀린 것은 **오류가 아니라 재조회 신호**다. `errorCode` 로 배너를 띄우면
      // 스스로 복구되는 상황에 사용자가 할 일이 없는 경고가 남는다.
      if (msg.code === "stream_lagged") {
        return { ...prev, missedCount: prev.missedCount + 1 };
      }
      return { ...prev, errorCode: msg.code };
  }
}

/**
 * 연결이 끊겼음을 이력에 남긴다.
 *
 * 끊긴 동안의 표본은 **존재하지 않는다.** 이력을 그대로 두면 10분 뒤 재연결한
 * 첫 표본이 끊기기 전 표본과 선으로 이어져, 관측이 없던 구간에 선이 그려진다.
 * `null` 하나를 넣어 선을 끊는다 — 스파크라인이 `null` 을 구간 경계로 읽는다.
 */
export function markStreamGap(prev: LiveSnapshot): LiveSnapshot {
  const instances = Object.keys(prev.qpsHistory);
  if (instances.length === 0) return prev;

  const qpsHistory: Record<string, readonly (number | null)[]> = {};
  for (const [id, history] of Object.entries(prev.qpsHistory)) {
    // 이미 끊긴 표시가 있으면 또 넣지 않는다 — 재접속을 여러 번 실패하면
    // 이력이 `null` 로만 채워져 데이터를 밀어낸다.
    qpsHistory[id] = history.at(-1) === null ? history : appendBounded(history, null, HISTORY_LEN);
  }
  return { ...prev, qpsHistory };
}

/**
 * 더 이상 요구하지 않는 토픽을 거부 목록에서 뺀다.
 *
 * 없으면 실시간 화면을 떠난 뒤에도 "구독이 거부됐다" 배너가 재접속까지 남는다 —
 * 지금은 요구하지도 않는 토픽 얘기다.
 */
export function forgetDenied(prev: LiveSnapshot, topics: readonly string[]): LiveSnapshot {
  if (prev.denied.length === 0) return prev;
  const dropped = new Set(topics);
  const denied = prev.denied.filter((t) => !dropped.has(t));
  return denied.length === prev.denied.length ? prev : { ...prev, denied };
}

/**
 * `record_id` 로 upsert 한다.
 *
 * 같은 레코드에 `inflight` → `finalized` 가 연달아 오고, `finalized` 가 **두 번**
 * 오기도 한다(뒤엣것이 정확한 값이다 — `docs/21-resume.md §3.1`). 그래서 뒤에 온
 * 값이 이긴다. 다만 **자리를 옮기지 않는다** — 갱신마다 행이 튀면 읽을 수 없다.
 */
function upsertSlowQuery(
  rows: readonly SlowQueryBroadcast[],
  next: SlowQueryBroadcast,
): readonly SlowQueryBroadcast[] {
  const at = rows.findIndex((r) => r.record_id === next.record_id);
  if (at !== -1) {
    const copy = rows.slice();
    copy[at] = next;
    return copy;
  }
  // 새 행은 맨 위로. 상한을 넘으면 오래된 쪽을 버린다.
  return [next, ...rows].slice(0, MAX_LIVE_ROWS);
}

function appendBounded(
  history: readonly (number | null)[],
  value: number | null,
  limit: number,
): readonly (number | null)[] {
  const next = [...history, value];
  return next.length > limit ? next.slice(next.length - limit) : next;
}

/**
 * 원문 프레임을 서버 메시지로. **모르는 형태는 `null` 이다.**
 *
 * 백엔드를 신뢰하더라도 버전이 어긋난 순간(배포 중 새 태그 추가)이 있고, 그때
 * `undefined` 를 렌더하면 빈 화면이 된다. 파싱에서 막고 무시한다.
 */
export function parseServerMessage(raw: string): ServerMessage | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isRecord(parsed) || typeof parsed.t !== "string") return null;

  switch (parsed.t) {
    case "ready": {
      // **화면이 읽는 필드를 전부 확인한다.** `env_scope` 가 없으면 헤더에서
      // `undefined.join()` 으로 죽는다 — 프레임 하나가 화면 전체를 날린다.
      const user = parsed.user;
      if (
        !isRecord(user) ||
        typeof user.subject !== "string" ||
        typeof user.role !== "string" ||
        typeof user.can_see_literals !== "boolean" ||
        !isStringArray(user.env_scope)
      ) {
        return null;
      }
      return parsed as unknown as ServerMessage;
    }
    case "subscribed":
      return isStringArray(parsed.topics) && isStringArray(parsed.denied)
        ? (parsed as unknown as ServerMessage)
        : null;
    case "slowq": {
      if (!isRecord(parsed.data)) return null;
      const data = normalizeBroadcast(parsed.data);
      return data === null ? null : { t: "slowq", data };
    }
    case "status": {
      if (typeof parsed.instance_id !== "string" || !isRecord(parsed.metrics)) return null;
      // 지표는 **정규화**한다. 없는 필드를 그대로 두면 `undefined` 가 산술에
      // 들어가 `NaN` 이 되고, 그건 `0` 보다 나쁘다(차트에 구멍이 아니라 쓰레기).
      const metrics = normalizeMetrics(parsed.metrics);
      return metrics === null
        ? null
        : { t: "status", instance_id: parsed.instance_id, metrics };
    }
    case "pong":
      return { t: "pong" };
    case "error":
      return typeof parsed.code === "string" ? { t: "error", code: parsed.code } : null;
    default:
      return null;
  }
}

/**
 * 방송 프레임 정규화. 화면이 읽는 필드가 **타입까지** 맞아야 한다.
 *
 * `sql_preview` 가 문자열이 아니면 React 가 "Objects are not valid as a React
 * child" 로 죽고, `started_at_ms` 가 없으면 `Intl` 이 **조용히 현재 시각**을
 * 그린다 — 둘 다 프레임 하나가 화면을 망치는 경로다.
 */
function normalizeBroadcast(raw: Record<string, unknown>): SlowQueryBroadcast | null {
  const record_id = raw.record_id;
  const instance_id = raw.instance_id;
  const started_at_ms = numberOrNull(raw.started_at_ms);
  const duration_ms = numberOrNull(raw.duration_ms);
  if (
    typeof record_id !== "string" ||
    typeof instance_id !== "string" ||
    started_at_ms === null ||
    duration_ms === null
  ) {
    return null;
  }
  return {
    record_id,
    instance_id,
    // `env` 는 실시간 화면의 필터 키다. 모르는 값이면 `unknown` 으로 둔다 —
    // 버리면 그 쿼리가 화면에서 사라진다.
    env: isEnv(raw.env) ? raw.env : "unknown",
    state: typeof raw.state === "string" ? raw.state : "unknown",
    started_at_ms,
    duration_ms,
    app_digest: typeof raw.app_digest === "string" ? raw.app_digest : "",
    statement_type: typeof raw.statement_type === "string" ? raw.statement_type : "unknown",
    schema_name: typeof raw.schema_name === "string" ? raw.schema_name : null,
    sql_preview: typeof raw.sql_preview === "string" ? raw.sql_preview : null,
  };
}

function isEnv(v: unknown): v is Env {
  return typeof v === "string" && (ALL_ENVS as readonly string[]).includes(v);
}

/** 숫자가 아니면 `null`. "값이 없다" 로 흘려보내면 화면이 `—` 를 그린다. */
function numberOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/**
 * 지표 프레임 정규화. **표본 시각이 없으면 프레임 자체를 버린다** — `0` 으로
 * 채우면 화면이 1970년을 "마지막 갱신" 으로 표시한다.
 */
function normalizeMetrics(raw: Record<string, unknown>): LiveMetrics | null {
  const at_ms = numberOrNull(raw.at_ms);
  if (at_ms === null) return null;
  return {
    at_ms,
    qps: numberOrNull(raw.qps),
    slow_per_sec: numberOrNull(raw.slow_per_sec),
    threads_running: numberOrNull(raw.threads_running),
    threads_connected: numberOrNull(raw.threads_connected),
    lock_waits: numberOrNull(raw.lock_waits),
    rate_gap_reason: typeof raw.rate_gap_reason === "string" ? raw.rate_gap_reason : null,
  };
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((x) => typeof x === "string");
}
