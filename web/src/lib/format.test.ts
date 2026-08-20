import { describe, expect, it } from "vitest";
import {
  EMPTY,
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
