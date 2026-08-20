import { describe, expect, it } from "vitest";
import {
  EMPTY,
  monthKey,
  monthRangeLabel,
  fmtDateTime,
  fmtDuration,
  fmtInt,
  fmtListTime,
  fmtRate,
  fmtSeconds,
  shortInstance,
} from "./format";

describe("null 과 0 을 구분한다", () => {
  it("값이 없으면 EMPTY 이고 0 은 0 이다", () => {
    // 이걸 뒤집으면 "비율을 못 냈다" 가 "쿼리가 없다" 로 보인다.
    expect(fmtRate(null)).toBe(EMPTY);
    expect(fmtRate(undefined)).toBe(EMPTY);
    expect(fmtRate(0)).toBe("0.0");
    expect(fmtInt(null)).toBe(EMPTY);
    expect(fmtInt(0)).toBe("0");
    expect(fmtDuration(null)).toBe(EMPTY);
  });
});

describe("실행시간 표시", () => {
  it("크기에 맞는 단위 하나만 쓴다", () => {
    expect(fmtDuration(923)).toBe("923ms");
    expect(fmtDuration(1376)).toBe("1.4초");
    expect(fmtDuration(7002)).toBe("7.0초");
    expect(fmtDuration(65_000)).toBe("1분 5초");
    expect(fmtDuration(3_720_000)).toBe("1시간 2분");
  });

  it("1초 미만을 초로 반올림하지 않는다", () => {
    // 임계값 근처 값이 뭉개지면 "왜 이게 슬로우 쿼리인가" 를 설명할 수 없다.
    expect(fmtDuration(999)).toBe("999ms");
  });
});

describe("인스턴스 id 축약", () => {
  it("마지막 성분만 남긴다", () => {
    expect(shortInstance("000000000000/ap-northeast-2/mysql84-local")).toBe("mysql84-local");
  });

  it("구분자가 없으면 그대로 둔다", () => {
    expect(shortInstance("mysql84-local")).toBe("mysql84-local");
  });
});

describe("자정을 넘는 목록", () => {
  it("오늘이 아니면 날짜까지 보여 준다", () => {
    // 기본 조회 구간이 24시간이라 어제 23:50 이 오늘 것으로 읽히면 안 된다.
    // KST 로 고정해 실행 환경의 시간대에 흔들리지 않게 한다.
    const now = Date.UTC(2026, 7, 20, 1, 0, 0); // 10:00 KST
    const today = Date.UTC(2026, 7, 20, 0, 30, 0); // 09:30 KST
    const yesterday = Date.UTC(2026, 7, 19, 14, 50, 0); // 전날 23:50 KST

    expect(fmtListTime(today, "KST", now)).toBe("09:30:00");
    expect(fmtListTime(yesterday, "KST", now)).toContain("19.");
    expect(fmtListTime(null, "KST", now)).toBe(EMPTY);
  });

  it("시간대를 고르면 같은 순간을 다르게 찍는다", () => {
    // 타임존 혼동이 이런 도구의 고질적 버그원이다 — 선택기가 실제로 동작해야 한다.
    const at = Date.UTC(2026, 7, 20, 0, 30, 0);
    expect(fmtListTime(at, "KST", at)).toBe("09:30:00");
    expect(fmtListTime(at, "UTC", at)).toBe("00:30:00");
  });

  it("초 단위는 소수 두 자리로 고정한다", () => {
    expect(fmtSeconds(1500)).toBe("1.50");
    expect(fmtSeconds(null)).toBe(EMPTY);
    expect(fmtSeconds(0)).toBe("0.00");
  });
});

describe("말이 안 되는 시각", () => {
  it("자리표시자 epoch 을 날짜로 그리지 않는다", () => {
    // 저장된 데이터에 `0`·`1000` 같은 값이 섞여 있고, 그대로 그리면 화면이
    // `1970. 01. 01.` 을 "마지막 관측" 으로 말한다.
    expect(fmtDateTime(0, "KST")).toBe(EMPTY);
    expect(fmtDateTime(1_000, "KST")).toBe(EMPTY);
    expect(fmtListTime(1_000, "KST", Date.now())).toBe(EMPTY);

    // 2000-01-01 이후는 정상으로 본다.
    expect(fmtDateTime(Date.UTC(2026, 7, 20, 0, 30), "KST")).toContain("2026");
  });
});

describe("월 기본값", () => {
  it("KST 기준으로 자른다", () => {
    // 백엔드가 월 경계를 KST 로 자르므로 화면 기본값도 KST 여야 한다.
    // 2026-09-01 00:30 KST = 2026-08-31 15:30 UTC → 9월이다.
    expect(monthKey(new Date(Date.UTC(2026, 7, 31, 15, 30)))).toBe("2026-09");
    // 2026-08-31 23:30 KST = 2026-08-31 14:30 UTC → 8월이다.
    expect(monthKey(new Date(Date.UTC(2026, 7, 31, 14, 30)))).toBe("2026-08");
  });

  it("이전 달로 물러날 때 연도를 넘긴다", () => {
    expect(monthKey(new Date(Date.UTC(2026, 0, 15)), -1)).toBe("2025-12");
    expect(monthKey(new Date(Date.UTC(2026, 0, 15)), -13)).toBe("2024-12");
  });
});

/**
 * **8일치 집계가 "8월 통계" 로 읽히면 안 된다.**
 *
 * 서버는 파티션 예산을 넘으면 구간을 최신 쪽으로 좁혀 읽고 응답에 그 구간을 담는다.
 * 화면이 요청한 달만 제목에 적으면 조사하던 사람은 한 달을 봤다고 믿는다.
 */
describe("월 구간 이름", () => {
  const AUG_START = Date.UTC(2026, 7, 1) - 9 * 3_600_000; // 2026-08-01 00:00 KST
  const AUG_END = Date.UTC(2026, 8, 1) - 9 * 3_600_000 - 1; // 08-31 23:59:59.999 KST

  it("달을 온전히 덮으면 달 이름 그대로다", () => {
    expect(monthRangeLabel("2026-08", AUG_START, AUG_END)).toBe("2026-08");
    // 서버가 더 넓게 준 경우(있을 수 없지만)도 달 이름이다.
    expect(monthRangeLabel("2026-08", AUG_START - 1000, AUG_END + 1000)).toBe("2026-08");
  });

  it("좁혀졌으면 좁혀진 구간을 적는다", () => {
    const clipped = AUG_END - 7 * 86_400_000; // 마지막 8일
    expect(monthRangeLabel("2026-08", clipped, AUG_END)).toBe("2026-08 중 08-24~08-31");
  });

  it("12월은 다음 해로 넘어가는 경계를 쓴다", () => {
    const decStart = Date.UTC(2026, 11, 1) - 9 * 3_600_000;
    const decEnd = Date.UTC(2027, 0, 1) - 9 * 3_600_000 - 1;
    expect(monthRangeLabel("2026-12", decStart, decEnd)).toBe("2026-12");
    expect(monthRangeLabel("2026-12", decStart + 86_400_000, decEnd)).toBe("2026-12 중 12-02~12-31");
  });

  it("값이 없으면 달 이름을 그대로 쓴다 — 빈 문자열을 만들지 않는다", () => {
    expect(monthRangeLabel("2026-08", null, null)).toBe("2026-08");
    expect(monthRangeLabel("2026-08", undefined, AUG_END)).toBe("2026-08");
    expect(monthRangeLabel("이상한값", AUG_START, AUG_END)).toBe("이상한값");
  });
});
