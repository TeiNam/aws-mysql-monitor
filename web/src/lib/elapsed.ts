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
 * # 시계 차이를 어느 방향으로 흡수하는가
 *
 * `started_at_ms` 는 **대상 DB 시계**로 보정된 값이고 브라우저 시계는 그와 몇 초
 * 어긋날 수 있다. 브라우저가 뒤처져 있으면 계산값이 저장값보다 작아지거나 음수가 된다.
 * 그래서 **둘 중 큰 값**을 쓴다 — 서버가 실제로 관측한 시간보다 짧게 보여 주지 않는다.
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
  return isRunning(row.state) ? elapsedMs(row.started_at_ms, row.duration_ms, nowMs) : row.duration_ms;
}
