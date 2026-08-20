/**
 * 실시간 스트림의 **판정부**. 소켓을 모른다.
 *
 * 백엔드가 `ws.rs`(배관)와 `topic.rs`(판정)를 나눈 것과 같은 이유로 나눈다 —
 * 이 파일은 순수 함수라 테스트가 전부 여기 걸린다. 소켓 쪽에 버그가 있으면
 * 그건 배관 버그다.
 */

import type { LiveMetrics, ReadyUser, ServerMessage, SlowQueryBroadcast } from "./types";

/** 실시간 표에 유지하는 행 수 상한. 넘으면 오래된 것부터 버린다. */
export const MAX_LIVE_ROWS = 200;
/**
 * 연결당 구독 상한. **서버의 `topic::MAX_TOPICS` 와 같아야 한다** — 넘겨 보내면
 * 서버가 조용히가 아니라 `denied` 로 답하지만, 화면이 미리 자르고 그 사실을
 * 알리는 편이 낫다.
 */
export const MAX_TOPICS = 50;
/** 스파크라인이 쓰는 표본 수. 5초 주기 × 60 = 5분. */
export const HISTORY_LEN = 60;

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
   * 방송이 밀려 유실된 시각(`stream_lagged`). 값이 바뀌면 화면은 **HTTP 로
   * 재조회**해야 한다 — 유실된 쿼리는 다시 방송되지 않는다.
   */
  laggedAt: number | null;
  /** 마지막 서버 오류 코드. 표시 후에도 남겨 둔다(연결 성공 시 지워진다). */
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
  laggedAt: null,
  errorCode: null,
};

/**
 * 서버 메시지 하나를 반영한다. **새 객체를 돌려준다** — 변경이 없으면 같은
 * 객체를 그대로 돌려주어 `useSyncExternalStore` 가 헛되게 리렌더하지 않게 한다.
 *
 * `nowMs` 는 `laggedAt` 표시에만 쓴다. 인자로 받는 이유는 테스트다.
 */
export function applyMessage(
  prev: LiveSnapshot,
  msg: ServerMessage,
  nowMs: number,
): LiveSnapshot {
  switch (msg.t) {
    case "ready":
      return { ...prev, conn: "open", user: msg.user, errorCode: null };

    case "subscribed":
      return { ...prev, subscribed: msg.topics, denied: msg.denied };

    case "slowq":
      return { ...prev, slowq: upsertSlowQuery(prev.slowq, msg.data) };

    case "status": {
      const history = prev.qpsHistory[msg.instance_id] ?? [];
      return {
        ...prev,
        status: { ...prev.status, [msg.instance_id]: msg.metrics },
        qpsHistory: {
          ...prev.qpsHistory,
          [msg.instance_id]: appendBounded(history, msg.metrics.qps, HISTORY_LEN),
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
        return { ...prev, conn: "unauthorized", user: null, errorCode: msg.code };
      }
      if (msg.code === "stream_lagged") {
        return { ...prev, laggedAt: nowMs, errorCode: msg.code };
      }
      return { ...prev, errorCode: msg.code };
  }
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
    case "ready":
      return isRecord(parsed.user) && typeof parsed.user.subject === "string"
        ? (parsed as unknown as ServerMessage)
        : null;
    case "subscribed":
      return isStringArray(parsed.topics) && isStringArray(parsed.denied)
        ? (parsed as unknown as ServerMessage)
        : null;
    case "slowq":
      return isRecord(parsed.data) &&
        typeof parsed.data.record_id === "string" &&
        typeof parsed.data.duration_ms === "number"
        ? (parsed as unknown as ServerMessage)
        : null;
    case "status":
      return typeof parsed.instance_id === "string" && isRecord(parsed.metrics)
        ? (parsed as unknown as ServerMessage)
        : null;
    case "pong":
      return { t: "pong" };
    case "error":
      return typeof parsed.code === "string" ? { t: "error", code: parsed.code } : null;
    default:
      return null;
  }
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((x) => typeof x === "string");
}
