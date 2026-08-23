/**
 * 진행 중 쿼리의 경과 시간.
 *
 * # 왜 저장된 값을 그대로 쓰지 않는가
 *
 * 저장된 `duration_ms` 는 **마지막 관측 시점**의 값이다. 하트비트가 15초마다 그 값을
 * 함께 올리므로 최대 15초 낡을 수 있고, 그 사이 화면이 멈춰 보인다. 참조 구현과
 * [09 §3.4](../../../.claude/docs/09-frontend.md) 는 경과를 **클라이언트가 1초마다 증가**시킨다고
 * 적었다(교차 리뷰 24라운드에 구현했다).
 *
 * # 두 기계의 시계를 빼지 않는다 (26라운드)
 *
 * `started_at_ms` 는 **대상 DB 시계**다. `Date.now() - started_at_ms` 는 브라우저 시계와
 * DB 시계를 빼는 것이어서, DB 가 5분 뒤처진 환경에서 2초 쿼리가 **5분째 실행 중**으로
 * 보인다. 그래서 그 값을 쓰지 않고, **우리 시계 안에서만** 계산한다:
 *
 * ```text
 * 경과 = duration_ms + (지금 − last_seen_at_ms)
 * ```
 *
 * `duration_ms` 와 `last_seen_at_ms` 는 **같은 관측**이고 하트비트가 둘을 함께 올리므로
 * 짝이 어긋나지 않는다. `last_seen_at_ms` 와 `server_now_ms` 는 둘 다 수집기·API 의
 * 시계다 — DB 시계가 식에 들어오지 않는다.
 *
 * # 브라우저 시계도 믿지 않는다
 *
 * `Date.now()` 는 NTP 보정으로 뒤로 갈 수 있고, 그러면 화면의 경과가 줄어든다. 그래서
 * 응답을 받은 순간의 `performance.now()`(단조)를 기준점으로 잡고 그 뒤의 경과만 더한다.
 */

/** 기준점. 응답을 받을 때 한 번 만든다. */
export interface ServerTimeRef {
  /** 서버가 응답을 만든 시각(서버 시계). */
  serverNowMs: number;
  /** 그 응답을 받은 순간의 `performance.now()` — **단조 증가**한다. */
  monotonicAtMs: number;
}

/**
 * 응답 기준 "지금"(서버 시계). 단조 시계가 흐른 만큼만 더한다.
 *
 * **기준점이 없으면 `fallbackNowMs` 를 쓴다.** 롤링 배포 중에는 새 화면이
 * `server_now_ms` 를 주지 않는 옛 API 와 만난다. 그때 `NaN` 이 되면 시각 서식이 예외를
 * 던져 **표 전체가 사라진다** — 경과 하나 때문에 화면을 잃는 것은 맞지 않다.
 */
export function serverNow(
  ref: Partial<ServerTimeRef>,
  monotonicNowMs: number,
  fallbackNowMs: number,
): number {
  const { serverNowMs, monotonicAtMs } = ref;
  if (!Number.isFinite(serverNowMs) || !Number.isFinite(monotonicAtMs)) {
    return fallbackNowMs;
  }
  return (serverNowMs as number) + Math.max(0, monotonicNowMs - (monotonicAtMs as number));
}

/** 진행 중 상태인가. 이 상태에서만 경과가 흐른다. */
export function isRunning(state: string): boolean {
  return state === "inflight" || state === "in_flight";
}

/**
 * 표시할 소요 시간(ms). 진행 중이 아니면 저장값 그대로다.
 *
 * 확정·포기된 레코드에 경과를 흘리면 **끝난 쿼리의 시간이 계속 늘어난다** — 그게
 * 1세대에서 "며칠째 실행 중인 유령" 으로 보이던 화면이다.
 *
 * `last_seen_at_ms` 가 없으면(옛 API·슬로우로그만으로 만든 레코드) 저장값을 쓴다 —
 * 관측 시각을 모르면 얼마나 흘렀는지도 알 수 없다.
 */
export function displayDurationMs(
  row: {
    state: string;
    duration_ms: number;
    last_seen_at_ms?: number | null;
  },
  nowMs: number,
): number {
  if (!isRunning(row.state) || row.last_seen_at_ms == null) {
    return row.duration_ms;
  }
  // 음수 방어: 서버 시각이 관측 시각보다 이르면(수집기와 API 의 시계 차) 0 으로 본다.
  return row.duration_ms + Math.max(0, nowMs - row.last_seen_at_ms);
}
