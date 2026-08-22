import { describe, expect, it } from "vitest";

import { displayDurationMs, elapsedMs, isRunning, serverNow } from "./elapsed";

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

describe("serverNow", () => {
  const ref = { serverNowMs: T0, receivedAtMs: 9_000_000_000_000 }; // 브라우저가 한참 앞서 있다

  it("**브라우저 시계의 절대값을 쓰지 않는다** — 기준점 이후 경과만 더한다", () => {
    // 브라우저 시계가 서버보다 훨씬 앞서지만, 경과는 3초다.
    expect(serverNow(ref, ref.receivedAtMs + 3_000)).toBe(T0 + 3_000);
  });

  it("브라우저 시계가 뒤로 가도 기준점보다 이전으로 돌아가지 않는다", () => {
    expect(serverNow(ref, ref.receivedAtMs - 60_000)).toBe(T0);
  });

  it("**기준점이 없으면 브라우저 시계로 떨어진다** — 옛 API 와 만나도 표를 잃지 않는다", () => {
    expect(serverNow({}, 12_345)).toBe(12_345);
    expect(serverNow({ serverNowMs: T0 }, 12_345)).toBe(12_345);
    expect(serverNow({ serverNowMs: Number.NaN, receivedAtMs: 1 }, 12_345)).toBe(12_345);
  });

  it("기준점과 결합하면 2초 쿼리가 5분으로 보이지 않는다", () => {
    // 옛 계산: Date.now() - started = 브라우저가 5분 앞서면 약 300초.
    const started = T0 - 2_000;
    const now = serverNow(ref, ref.receivedAtMs + 500);
    expect(displayDurationMs({ state: "inflight", started_at_ms: started, duration_ms: 2_000 }, now)).toBe(2_500);
  });
});
