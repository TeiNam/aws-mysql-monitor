import { describe, expect, it } from "vitest";

import { displayDurationMs, isRunning, serverNow } from "./elapsed";

const T0 = 1_787_147_400_000;

describe("displayDurationMs", () => {
  const running = (lastSeen: number | null) => ({
    state: "inflight",
    duration_ms: 2_000,
    last_seen_at_ms: lastSeen,
  });

  it("**관측 시각 이후 흐른 시간을 더한다** — 저장값에 멈추지 않는다", () => {
    // 2초에서 관측됐고 그 뒤 10분이 흘렀다.
    expect(displayDurationMs(running(T0), T0 + 600_000)).toBe(602_000);
  });

  it("**두 기계의 시계를 빼지 않는다.** `started_at_ms` 를 쓰지 않는 것이 요점이다", () => {
    // DB 시계가 5분 뒤처져 있어도(started_at_ms 가 5분 이르게 보여도) 영향이 없다 —
    // 이 함수는 그 값을 아예 받지 않는다.
    const row = { state: "inflight", duration_ms: 2_000, last_seen_at_ms: T0 };
    expect(displayDurationMs(row, T0 + 500)).toBe(2_500);
  });

  it("관측 시각을 모르면 저장값을 쓴다 (옛 API·슬로우로그 레코드)", () => {
    expect(displayDurationMs(running(null), T0 + 600_000)).toBe(2_000);
    expect(
      displayDurationMs({ state: "inflight", duration_ms: 2_000 }, T0 + 600_000),
    ).toBe(2_000);
  });

  it("서버 시각이 관측 시각보다 이르면 0 으로 본다 — 음수를 표시하지 않는다", () => {
    expect(displayDurationMs(running(T0 + 5_000), T0)).toBe(2_000);
  });

  it("**끝난 쿼리의 시간은 늘어나지 않는다.** 그게 유령으로 보이는 화면이다", () => {
    for (const state of ["finalized", "abandoned"]) {
      expect(
        displayDurationMs({ state, duration_ms: 2_000, last_seen_at_ms: T0 }, T0 + 600_000),
      ).toBe(2_000);
    }
  });

  it("isRunning 은 두 표기를 모두 받는다", () => {
    expect(isRunning("inflight")).toBe(true);
    expect(isRunning("in_flight")).toBe(true);
    expect(isRunning("finalized")).toBe(false);
  });
});

describe("serverNow", () => {
  // 응답을 받은 순간: 서버 시각 T0, 단조 시계 5_000.
  const ref = { serverNowMs: T0, monotonicAtMs: 5_000 };

  it("**단조 시계가 흐른 만큼만** 더한다", () => {
    expect(serverNow(ref, 8_000, 0)).toBe(T0 + 3_000);
  });

  it("**뒤로 가지 않는다** — NTP 가 벽시계를 당겨도 경과가 줄지 않는다", () => {
    // 단조 시계는 뒤로 가지 않지만, 방어적으로 확인한다.
    expect(serverNow(ref, 4_000, 0)).toBe(T0);
  });

  it("기준점이 없으면 넘겨준 대체값을 쓴다 — 옛 API 와 만나도 표를 잃지 않는다", () => {
    expect(serverNow({}, 8_000, 12_345)).toBe(12_345);
    expect(serverNow({ serverNowMs: T0 }, 8_000, 12_345)).toBe(12_345);
    expect(serverNow({ serverNowMs: Number.NaN, monotonicAtMs: 1 }, 8_000, 12_345)).toBe(12_345);
  });
});
