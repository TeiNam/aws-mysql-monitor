/**
 * CloudWatch 메트릭 값 표시.
 *
 * # 왜 단위를 서버가 알려주는가
 *
 * `FreeableMemory` 는 바이트고 `CPUUtilization` 은 퍼센트다. 화면이 메트릭 이름으로
 * 단위를 추측하면 새 메트릭을 추가할 때마다 화면도 고쳐야 하고, 빠뜨리면 **바이트를
 * 퍼센트로 표시**한다. 그래서 카탈로그(`dbmon_core::cw_metrics`)가 단위를 함께 보낸다.
 */

/** 값이 없다는 것과 0 은 다르다 — 없으면 `—` 다. */
const EMPTY = "—";

export function formatMetric(value: number | null, unit: string): string {
  if (value === null) return EMPTY;
  switch (unit) {
    case "percent":
      return `${round(value, 1)}%`;
    case "bytes":
      return formatBytes(value);
    case "bytes_per_second":
      return `${formatBytes(value)}/s`;
    case "count":
      // 소수점이 의미 없는 개수다. 다만 **반올림해서 0 으로 만들지 않는다** —
      // `0.4개` 를 `0` 으로 적으면 "없다" 로 읽힌다.
      return value >= 1 || value === 0 ? round(value, 0).toLocaleString("ko-KR") : round(value, 2).toString();
    case "count_per_second":
      return `${round(value, value < 10 ? 2 : 0)}/s`;
    case "seconds":
      // 초 단위 지연은 대개 1 미만이라 ms 로 보여주는 것이 읽힌다.
      return value < 1 ? `${round(value * 1000, 1)}ms` : `${round(value, 2)}s`;
    case "milliseconds":
      return `${round(value, 2)}ms`;
    default:
      return round(value, 2).toString();
  }
}

/** 1024 단위. **십진(1000)이 아니다** — RDS 콘솔이 이진 단위로 보여준다. */
export function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let v = Math.abs(bytes);
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const sign = bytes < 0 ? "-" : "";
  return `${sign}${round(v, v < 10 ? 1 : 0)}${units[i]}`;
}

function round(v: number, digits: number): number {
  const p = 10 ** digits;
  return Math.round(v * p) / p;
}

/** 값 배열에서 축 눈금에 쓸 최댓값. **전부 null 이면 `null`** 이다(0 이 아니다). */
export function seriesMax(values: readonly number[]): number | null {
  if (values.length === 0) return null;
  return values.reduce((a, b) => Math.max(a, b), Number.NEGATIVE_INFINITY);
}

/**
 * 값의 위험도. **분모가 있는 값만 판정한다.**
 *
 * # 왜 전부 색칠하지 않는가
 *
 * "위험" 은 비율 판정이고, 비율에는 분모가 필요하다. 그게 없으면 같은 숫자가 정상일 수도
 * 위험일 수도 있다:
 *
 * | 값 | 분모 | 판정 가능? |
 * |---|---|---|
 * | `CPUUtilization` | 그 자체가 % | **된다** |
 * | `FreeStorageSpace` | 할당 스토리지(GiB, RDS API) | **된다** |
 * | `FreeableMemory` | 인스턴스 클래스의 RAM | **안 된다** — CloudWatch 도 RDS API 도 주지 않는다 |
 * | `AuroraVolumeBytesLeftTotal` | 볼륨 최대치 | **안 된다** — 자동 증가라 고정 분모가 없다 |
 * | `Threads_connected` | `@@max_connections` (자체 수집) | **된다** |
 * | 스레드·락·QPS | 없다 | **안 된다** — 정상 범위가 워크로드마다 다르다 |
 *
 * 억지로 절대 임계를 쓰면 **건강한 인스턴스를 빨갛게 만든다.** MySQL 은 남는 메모리를
 * 버퍼풀로 쓰므로 `FreeableMemory` 가 낮은 것이 정상이다 — 실측: t4g.micro(1GiB)에서
 * 96MB 였고 그건 정상 상태다. 색이 거짓 경보를 내면 아무도 색을 믿지 않게 된다.
 */
export type Severity = "ok" | "warn" | "crit";

/** CPU 사용률 임계(%). 위로 갈수록 나쁘다. */
const CPU_WARN = 70;
const CPU_CRIT = 85;
/** 남은 스토리지 비율(%). **아래로** 갈수록 나쁘다. */
const STORAGE_WARN = 20;
const STORAGE_CRIT = 10;
/**
 * 연결 포화도(%). 위로 갈수록 나쁘다.
 *
 * `max_connections` 에 닿으면 **새 연결이 거부된다** — 그건 애플리케이션 장애다.
 * 그래서 스토리지보다 이른 지점에서 경고한다.
 */
const CONN_WARN = 70;
const CONN_CRIT = 85;

/**
 * 이 값의 위험도. 판정할 수 없으면 `null` — 화면은 색을 칠하지 않는다.
 *
 * `allocatedStorageGb` 는 RDS 인스턴스의 할당 스토리지다(Aurora 는 `null`).
 */
export function metricSeverity(
  metricName: string,
  value: number | null,
  allocatedStorageGb: number | null,
): Severity | null {
  if (value === null) return null;
  switch (metricName) {
    case "CPUUtilization":
      // 엔진 CPU(`EngineCPUUtilization`)는 호스트 CPU 와 다른 축이라 같은 임계를 쓰지 않는다.
      return value >= CPU_CRIT ? "crit" : value >= CPU_WARN ? "warn" : "ok";
    case "FreeStorageSpace": {
      // **`> 0` 이 아닌 모든 것을 거른다** — `null` 뿐 아니라 `undefined`·`NaN` 도.
      // `undefined` 로 나누면 비율이 `NaN` 이 되고, `NaN <= 10` 은 거짓이므로
      // 판정할 수 없는 값이 **녹색**으로 칠해진다(옛 서버 응답에서 그럴 수 있다).
      if (allocatedStorageGb === null || !(allocatedStorageGb > 0)) return null;
      // **GiB 다** (10^9 이 아니라 2^30). RDS 의 `AllocatedStorage` 단위가 GiB 다.
      const pct = (value / (allocatedStorageGb * 1024 ** 3)) * 100;
      return pct <= STORAGE_CRIT ? "crit" : pct <= STORAGE_WARN ? "warn" : "ok";
    }
    default:
      return null;
  }
}

/**
 * 위험도 → 글자 색.
 *
 * 색 계단 안의 값만 쓴다 — 다크 모드에서 함께 뒤집힌다(`src/index.css`).
 * **색만으로 구분하지 않는다**: 호출부가 `title` 로 근거(임계값)를 함께 준다.
 */
export function severityClass(s: Severity | null): string {
  switch (s) {
    case "crit":
      return "font-semibold text-red-700";
    case "warn":
      return "font-semibold text-amber-700";
    case "ok":
      return "text-green-700";
    default:
      return "";
  }
}

/** 색의 근거. 왜 이 색인지 마우스로 확인할 수 있어야 한다. */
export function severityReason(
  metricName: string,
  value: number | null,
  allocatedStorageGb: number | null,
): string | undefined {
  const s = metricSeverity(metricName, value, allocatedStorageGb);
  if (s === null || value === null) return undefined;
  if (metricName === "CPUUtilization") {
    return `CPU ${round(value, 1)}% — 경고 ${CPU_WARN}% / 위험 ${CPU_CRIT}%`;
  }
  if (metricName === "FreeStorageSpace" && allocatedStorageGb !== null) {
    const pct = (value / (allocatedStorageGb * 1024 ** 3)) * 100;
    return `할당 ${allocatedStorageGb}GiB 중 ${round(pct, 1)}% 남음 — 경고 ${STORAGE_WARN}% / 위험 ${STORAGE_CRIT}%`;
  }
  return undefined;
}

/**
 * 연결 포화도의 위험도. **모수가 없으면 `null`** — 색을 칠하지 않는다.
 *
 * 모수(`@@max_connections`)는 RDS 에서 인스턴스 클래스 메모리로 계산되는 기본식이라
 * 인스턴스마다 다르다. 그래서 절대 개수로는 판정할 수 없다: `100` 이 t4g.micro 에서는
 * 포화 직전이고 r6g.4xlarge 에서는 한가하다.
 */
export function connSeverity(
  connected: number | null,
  maxConnections: number | null,
): Severity | null {
  // `> 0` 이 아닌 모수는 전부 거른다(`null`·`undefined`·`NaN`) — 아래 `metricSeverity`
  // 와 같은 이유다. 판정할 수 없는 것을 녹색으로 칠하면 안 된다.
  if (connected === null || maxConnections === null || !(maxConnections > 0)) return null;
  const pct = (connected / maxConnections) * 100;
  return pct >= CONN_CRIT ? "crit" : pct >= CONN_WARN ? "warn" : "ok";
}

/**
 * 연결 수를 **모수와 함께** 적는다 — `6 / 60`.
 *
 * 개수만 적으면 색이 왜 그 색인지 알 수 없다. `6` 이 위험인지 한가한지는 모수에 달렸고,
 * 그 모수는 인스턴스마다 다르다(RDS 가 인스턴스 클래스 메모리로 계산한다).
 *
 * 모수를 모르면 **개수만** 적는다 — `6 / —` 는 "모수가 0" 으로 읽힌다.
 */
export function formatConn(connected: number | null, maxConnections: number | null): string {
  if (connected === null) return EMPTY;
  const n = connected.toLocaleString("ko-KR");
  if (maxConnections === null || maxConnections <= 0) return n;
  return `${n} / ${maxConnections.toLocaleString("ko-KR")}`;
}

/** 연결 포화도 색의 근거. */
export function connReason(
  connected: number | null,
  maxConnections: number | null,
): string | undefined {
  if (connSeverity(connected, maxConnections) === null) return undefined;
  const pct = ((connected as number) / (maxConnections as number)) * 100;
  return `${connected} / max_connections ${maxConnections} = ${round(pct, 1)}% — 경고 ${CONN_WARN}% / 위험 ${CONN_CRIT}%`;
}
