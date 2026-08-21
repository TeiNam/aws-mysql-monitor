import { describe, expect, it } from "vitest";
import { connSeverity, formatBytes, formatConn, formatMetric, metricSeverity } from "./metrics";

/**
 * 색 판정만 테스트한다. **거짓 경보가 이 기능의 유일한 실패 모드다** — 건강한
 * 인스턴스를 빨갛게 칠하면 아무도 색을 믿지 않게 되고, 그러면 진짜 위험도 놓친다.
 */
describe("위험도 판정", () => {
  it("CPU 는 그 자체가 퍼센트라 임계로 가른다", () => {
    expect(metricSeverity("CPUUtilization", 12, null)).toBe("ok");
    expect(metricSeverity("CPUUtilization", 69.9, null)).toBe("ok");
    expect(metricSeverity("CPUUtilization", 70, null)).toBe("warn");
    expect(metricSeverity("CPUUtilization", 84.9, null)).toBe("warn");
    expect(metricSeverity("CPUUtilization", 85, null)).toBe("crit");
    expect(metricSeverity("CPUUtilization", 100, null)).toBe("crit");
  });

  /** 스토리지는 **할당량이 있어야** 판정된다. 없으면 같은 바이트가 정상일 수도 위험일 수도 있다. */
  it("스토리지는 할당량 대비 비율로 가른다", () => {
    const gib = 1024 ** 3;
    // 100GiB 할당: 50GiB 남음 = 50% → ok
    expect(metricSeverity("FreeStorageSpace", 50 * gib, 100)).toBe("ok");
    // 15% → warn
    expect(metricSeverity("FreeStorageSpace", 15 * gib, 100)).toBe("warn");
    // 8% → crit
    expect(metricSeverity("FreeStorageSpace", 8 * gib, 100)).toBe("crit");
    // **같은 바이트인데 할당량이 다르면 판정이 달라진다** — 그게 분모가 필요한 이유다.
    expect(metricSeverity("FreeStorageSpace", 18 * gib, 20)).toBe("ok");
    expect(metricSeverity("FreeStorageSpace", 18 * gib, 1000)).toBe("crit");
  });

  /** **분모가 없으면 색을 칠하지 않는다.** 절대 임계는 건강한 인스턴스를 빨갛게 만든다. */
  it("분모가 없는 값은 판정하지 않는다", () => {
    // 실측: t4g.micro(1GiB RAM)에서 FreeableMemory 96MB 는 정상이다(버퍼풀이 쓰고 있다).
    expect(metricSeverity("FreeableMemory", 96 * 1024 ** 2, null)).toBeNull();
    expect(metricSeverity("AuroraVolumeBytesLeftTotal", 1, null)).toBeNull();
    expect(metricSeverity("DatabaseConnections", 9999, null)).toBeNull();
    // Aurora 는 할당 스토리지가 없다 → 스토리지도 판정 불가.
    expect(metricSeverity("FreeStorageSpace", 1, null)).toBeNull();
    expect(metricSeverity("FreeStorageSpace", 1, 0)).toBeNull();
  });

  /** 값이 없으면 색도 없다 — `0` 으로 접으면 "위험" 이 된다. */
  it("값이 없으면 판정하지 않는다", () => {
    expect(metricSeverity("CPUUtilization", null, null)).toBeNull();
    expect(metricSeverity("FreeStorageSpace", null, 100)).toBeNull();
  });
});

describe("연결 포화도", () => {
  /**
   * **절대 개수로는 판정할 수 없다.** 같은 100 연결이 t4g.micro 에서는 포화 직전이고
   * r6g.4xlarge 에서는 한가하다 — RDS 가 `max_connections` 를 인스턴스 메모리로
   * 계산하기 때문이다. 그래서 모수가 필요하다.
   */
  it("모수 대비 비율로 가른다", () => {
    expect(connSeverity(10, 100)).toBe("ok");
    expect(connSeverity(69, 100)).toBe("ok");
    expect(connSeverity(70, 100)).toBe("warn");
    expect(connSeverity(85, 100)).toBe("crit");
    // 같은 개수, 다른 모수 → 다른 판정.
    expect(connSeverity(90, 1000)).toBe("ok");
    expect(connSeverity(90, 100)).toBe("crit");
  });

  it("모수가 없으면 판정하지 않는다", () => {
    // 구 서버는 `max_connections` 를 보내지 않는다 → 색 없음.
    expect(connSeverity(500, null)).toBeNull();
    expect(connSeverity(null, 100)).toBeNull();
    expect(connSeverity(5, 0)).toBeNull();
  });

  /**
   * **판정할 수 없는 값을 녹색으로 칠하면 안 된다.**
   *
   * 옛 응답에는 필드가 아예 없어 `undefined` 가 들어온다. 그걸로 나누면 비율이 `NaN`
   * 이고 `NaN >= 85` 도 `NaN >= 70` 도 거짓이라, 걸러내지 않으면 "안전(녹색)" 으로
   * 떨어진다 — 색이 거짓 안심을 주는 쪽이 색이 없는 것보다 나쁘다.
   */
  it("모수가 undefined 여도 녹색으로 떨어지지 않는다", () => {
    const noField = undefined as unknown as number | null;
    expect(connSeverity(5, noField)).toBeNull();
    expect(metricSeverity("FreeStorageSpace", 1024 ** 3, noField)).toBeNull();
  });
});

describe("연결 수 표시", () => {
  it("모수와 함께 적는다", () => {
    expect(formatConn(6, 60)).toBe("6 / 60");
    // 천 단위 구분. 모수 쪽에도 붙는다 — 한쪽만 붙으면 자리수를 잘못 읽는다.
    expect(formatConn(1234, 5000)).toBe("1,234 / 5,000");
  });

  it("모수를 모르면 개수만 적는다", () => {
    // `6 / —` 는 "모수가 0" 으로 읽힌다.
    expect(formatConn(6, null)).toBe("6");
    expect(formatConn(6, 0)).toBe("6");
    expect(formatConn(null, 60)).toBe("—");
  });
});

describe("표시", () => {
  it("바이트는 이진 단위다 (RDS 콘솔과 같게)", () => {
    expect(formatBytes(1024)).toBe("1KB");
    expect(formatBytes(1536)).toBe("1.5KB");
    expect(formatBytes(1024 ** 3 * 18)).toBe("18GB");
  });

  it("단위별로 다르게 적는다", () => {
    expect(formatMetric(13.94, "percent")).toBe("13.9%");
    expect(formatMetric(0.0003, "seconds")).toBe("0.3ms");
    expect(formatMetric(2.5, "seconds")).toBe("2.5s");
    expect(formatMetric(3.784, "count_per_second")).toBe("3.78/s");
    expect(formatMetric(null, "percent")).toBe("—");
  });
});
