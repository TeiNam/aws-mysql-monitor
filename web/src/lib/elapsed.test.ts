import { describe, expect, it } from "vitest";

import { displayDurationMs, elapsedMs, isRunning } from "./elapsed";

const T0 = 1_787_147_400_000;

describe("elapsedMs", () => {
  it("진행 중이면 시계로 흐른다 — 저장값에 멈추지 않는다", () => {
    // 저장은 2.0초에서 멈췄지만 실제로는 10분 돌고 있다.
    expect(elapsedMs(T0, 2_000, T0 + 600_000)).toBe(600_000);
  });

  it("**저장값보다 작게 보여주지 않는다** — 브라우저 시계가 뒤처져도", () => {
    // 브라우저가 5초 뒤처져 있다: now - started 가 저장값보다 작다.
    expect(elapsedMs(T0, 30_000, T0 + 25_000)).toBe(30_000);
    // 극단: 브라우저가 시작 시각보다 앞이다 → 음수가 나오면 안 된다.
    expect(elapsedMs(T0, 30_000, T0 - 10_000)).toBe(30_000);
  });
});

describe("displayDurationMs", () => {
  const row = (state: string) => ({ state, started_at_ms: T0, duration_ms: 2_000 });

  it("진행 중 상태에서만 흐른다", () => {
    expect(displayDurationMs(row("inflight"), T0 + 60_000)).toBe(60_000);
    expect(displayDurationMs(row("in_flight"), T0 + 60_000)).toBe(60_000);
  });

  it("**끝난 쿼리의 시간은 늘어나지 않는다.** 그게 유령으로 보이는 화면이다", () => {
    for (const state of ["finalized", "abandoned"]) {
      expect(displayDurationMs(row(state), T0 + 600_000)).toBe(2_000);
    }
  });

  it("isRunning 은 두 표기를 모두 받는다", () => {
    expect(isRunning("inflight")).toBe(true);
    expect(isRunning("in_flight")).toBe(true);
    expect(isRunning("finalized")).toBe(false);
  });
});
