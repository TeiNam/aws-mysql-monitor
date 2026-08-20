import { describe, expect, it } from "vitest";
import { EMPTY, fmtClockOrDate, fmtDuration, fmtInt, fmtRate, shortInstance } from "./format";

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
    const now = new Date(2026, 7, 20, 10, 0, 0).getTime();
    const today = new Date(2026, 7, 20, 9, 30, 0).getTime();
    const yesterday = new Date(2026, 7, 19, 23, 50, 0).getTime();

    expect(fmtClockOrDate(today, now)).toBe("09:30:00");
    expect(fmtClockOrDate(yesterday, now)).toContain("19.");
    expect(fmtClockOrDate(null, now)).toBe(EMPTY);
  });
});
