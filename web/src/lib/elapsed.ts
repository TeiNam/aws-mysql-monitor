/**
 * 진행 중 쿼리의 경과 시간.
 *
 * # 왜 저장된 값을 그대로 쓰지 않는가
 *
 * 저장된 `duration_ms` 는 **마지막으로 저장된 시점**의 값이다. 수집기는 실행계획을
 * 확보하면 그 레코드를 다시 쓰지 않고(하트비트는 관측 시각만 올린다), 그래서 10분째
 * 도는 쿼리가 화면에 `2.0s` 로 남는다. 참조 구현과 [09 §3.4](../../../docs/09-frontend.md)
 * 는 "현재 실행 중" 의 경과를 **클라이언트가 1초마다 증가**시킨다고 적었는데 그게
 * 구현되지 않은 상태였다(교차 리뷰 24라운드).
 *
 * # 브라우저 시계를 믿지 않는다 (25라운드)
 *
 * `started_at_ms` 는 **대상 DB 시계**로 보정된 값이다. 브라우저 시계가 5분 앞선 기계에서
 * `Date.now() - started_at_ms` 를 쓰면 2초 쿼리가 **5분째 실행 중**으로 보인다. NTP 가
 * 시계를 뒤로 당기면 경과가 줄어드는 것도 보인다.
 *
 * 그래서 응답의 `server_now_ms` 를 기준점으로 삼고, 브라우저 시계는 **그 뒤로 흐른
 * 시간만** 재는 데 쓴다(차이는 시계 오차가 아니라 경과다):
 *
 * ```text
 * 경과 = (server_now_ms - started_at_ms) + (Date.now() - 응답을 받은 시각)
 * ```
 *
 * 두 항이 모두 같은 시계 안에서 계산되므로 오차가 상쇄된다.
 */

/** 기준점. 응답을 받을 때 한 번 만든다. */
export interface ServerTimeRef {
  /** 서버가 응답을 만든 시각. */
  serverNowMs: number;
  /** 그 응답을 받은 **브라우저** 시각. */
  receivedAtMs: number;
}

/**
 * 응답 기준 "지금"(서버 시계). 브라우저 시계는 경과분만 기여한다.
 *
 * **기준점이 없으면 브라우저 시계로 떨어진다.** 롤링 배포 중에는 새 화면이 `server_now_ms`
 * 를 주지 않는 옛 API 와 만난다. 그때 `NaN` 이 되면 시각 서식이 예외를 던져 **표 전체가
 * 사라진다** — 경과 시간 하나 때문에 화면을 잃는 것은 맞지 않다.
 */
export function serverNow(ref: Partial<ServerTimeRef>, browserNowMs: number): number {
  const { serverNowMs, receivedAtMs } = ref;
  if (!Number.isFinite(serverNowMs) || !Number.isFinite(receivedAtMs)) {
    return browserNowMs;
  }
  // 브라우저 시계가 뒤로 가면 경과가 음수가 된다 — 기준점 이전으로 돌아가지 않는다.
  return (serverNowMs as number) + Math.max(0, browserNowMs - (receivedAtMs as number));
}

/**
 * 진행 중 경과(ms).
 *
 * **저장된 값보다 짧게 보여주지 않는다.** 서버가 실제로 관측한 시간이 하한이다.
 */
export function elapsedMs(startedAtMs: number, storedDurationMs: number, nowMs: number): number {
  return Math.max(storedDurationMs, nowMs - startedAtMs);
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
 */
export function displayDurationMs(
  row: { state: string; started_at_ms: number; duration_ms: number },
  nowMs: number,
): number {
  return isRunning(row.state)
    ? elapsedMs(row.started_at_ms, row.duration_ms, nowMs)
    : row.duration_ms;
}
