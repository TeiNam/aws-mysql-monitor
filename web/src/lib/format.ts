/**
 * 표시 포맷.
 *
 * # `null` 을 `0` 으로 만들지 않는다
 *
 * 백엔드는 "아직 비율을 낼 수 없다"·"행 정보가 없다" 를 `null` 로 준다.
 * `?? 0` 으로 채우면 화면이 "쿼리가 없다"·"행을 안 읽었다" 로 읽힌다. 그래서 이
 * 모듈의 모든 포맷터는 `null`·`undefined` 를 [`EMPTY`] 로 낸다.
 *
 * # 시간대는 사용자가 고른다
 *
 * 참조 대시보드가 KST/UTC 선택기를 둔 이유가 있다 — 이런 도구의 고질적 버그원이
 * 타임존 혼동이다([09 §9]). 그래서 포맷터가 **시간대를 인자로 받는다.** 기본값을
 * 브라우저 시간대로 두지 않는다: 서울 밖에서 보면 같은 화면이 다른 시각을 말한다.
 */

/** 값이 없음. `0` 과 눈으로 구분돼야 한다. */
export const EMPTY = "—";

export type Timezone = "KST" | "UTC";

const INT = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 0 });
const RATE = new Intl.NumberFormat("ko-KR", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});
const SECONDS = new Intl.NumberFormat("ko-KR", {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

const TIME_PARTS = {
  hourCycle: "h23",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
} as const;

const ZONE: Record<Timezone, string> = { KST: "Asia/Seoul", UTC: "UTC" };

/** 시간대별 포맷터를 한 번만 만든다. `Intl` 생성은 비싸다. */
const CLOCK = new Map<Timezone, Intl.DateTimeFormat>();
const DATETIME = new Map<Timezone, Intl.DateTimeFormat>();
const DAY = new Map<Timezone, Intl.DateTimeFormat>();

function clockFmt(tz: Timezone): Intl.DateTimeFormat {
  let f = CLOCK.get(tz);
  if (f === undefined) {
    f = new Intl.DateTimeFormat("ko-KR", { ...TIME_PARTS, timeZone: ZONE[tz] });
    CLOCK.set(tz, f);
  }
  return f;
}

function dateTimeFmt(tz: Timezone): Intl.DateTimeFormat {
  let f = DATETIME.get(tz);
  if (f === undefined) {
    f = new Intl.DateTimeFormat("ko-KR", {
      ...TIME_PARTS,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      timeZone: ZONE[tz],
    });
    DATETIME.set(tz, f);
  }
  return f;
}

/**
 * "같은 날인가" 판정용. **날짜만** 뽑는다.
 *
 * 앞서 날짜+시각 문자열을 앞에서 잘라 비교했는데, `ko-KR` 형식이
 * `2026. 08. 20. 23:50:00` 이라 10자만 자르면 **일(日)이 빠졌다** — 어제와 오늘이
 * 같은 날로 판정돼 목록에서 날짜가 사라졌다. 포맷터를 따로 둔다.
 */
function dayFmt(tz: Timezone): Intl.DateTimeFormat {
  let f = DAY.get(tz);
  if (f === undefined) {
    f = new Intl.DateTimeFormat("ko-KR", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      timeZone: ZONE[tz],
    });
    DAY.set(tz, f);
  }
  return f;
}

/**
 * 이보다 앞선 시각은 **"값이 없다" 로 본다** (2000-01-01 UTC).
 *
 * 저장된 데이터에 `0`·`1` 같은 자리표시자가 섞여 있고, 그걸 그대로 포맷하면 화면이
 * `1970. 01. 01.` 을 "마지막 관측" 으로 말한다 — 없는 값을 날짜로 그리는 것이
 * 비어 있는 칸보다 나쁘다. 이 프로젝트가 `0` 을 센티널로 쓴 실수를 세 번 했다.
 */
const EPOCH_FLOOR_MS = 946_684_800_000;

function isPlausible(epochMs: number): boolean {
  return Number.isFinite(epochMs) && epochMs >= EPOCH_FLOOR_MS;
}

/** 정수 (행 수, 실행 수). */
export function fmtInt(n: number | null | undefined): string {
  return n === null || n === undefined ? EMPTY : INT.format(n);
}

/** 비율 (QPS 등). 소수 한 자리로 고정 — 자리수가 흔들리면 표가 읽히지 않는다. */
export function fmtRate(n: number | null | undefined): string {
  return n === null || n === undefined ? EMPTY : RATE.format(n);
}

/** 초 단위 (총 실행시간·평균). 참조 대시보드와 같이 소수 두 자리. */
export function fmtSeconds(ms: number | null | undefined): string {
  return ms === null || ms === undefined ? EMPTY : SECONDS.format(ms / 1000);
}

/**
 * 실행 시간. 단위를 섞지 않고 크기에 맞춰 하나만 쓴다.
 *
 * 1초 미만을 `0.9초` 로 쓰지 않는 이유: 임계값 근처 값을 뭉갠다.
 */
export function fmtDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return EMPTY;
  if (ms < 1_000) return `${INT.format(ms)}ms`;
  if (ms < 60_000) return `${RATE.format(ms / 1_000)}초`;
  if (ms < 3_600_000) {
    const min = Math.floor(ms / 60_000);
    const sec = Math.floor((ms % 60_000) / 1_000);
    return `${min}분 ${sec}초`;
  }
  const hour = Math.floor(ms / 3_600_000);
  const min = Math.floor((ms % 3_600_000) / 60_000);
  return `${hour}시간 ${min}분`;
}

/** 시:분:초. 같은 날 안에서 보는 표에 쓴다. */
export function fmtClock(epochMs: number | null | undefined, tz: Timezone): string {
  if (epochMs === null || epochMs === undefined || !isPlausible(epochMs)) return EMPTY;
  return clockFmt(tz).format(epochMs);
}

/** 날짜까지. 조회 구간이 하루를 넘길 수 있는 곳에 쓴다. */
export function fmtDateTime(epochMs: number | null | undefined, tz: Timezone): string {
  if (epochMs === null || epochMs === undefined || !isPlausible(epochMs)) return EMPTY;
  return dateTimeFmt(tz).format(epochMs);
}

/**
 * 목록용 시각. 오늘이면 시:분:초, 아니면 날짜까지.
 *
 * 기본 조회 구간이 24시간이라 자정을 넘는다. 시각만 찍으면 어제 23:50 이 오늘
 * 것으로 읽힌다 — 같은 표에서 두 날짜가 섞이는데 구분이 없다.
 */
export function fmtListTime(
  epochMs: number | null | undefined,
  tz: Timezone,
  nowMs: number,
): string {
  if (epochMs === null || epochMs === undefined || !isPlausible(epochMs)) return EMPTY;
  const sameDay = dayFmt(tz).format(epochMs) === dayFmt(tz).format(nowMs);
  return sameDay ? fmtClock(epochMs, tz) : fmtDateTime(epochMs, tz);
}

/**
 * 인스턴스 id 의 마지막 성분. `계정/리전/이름` 에서 이름만.
 *
 * 표에서 계정·리전을 반복하면 정작 다른 부분이 안 보인다. 전체 값은 `title` 로
 * 남긴다 — **숨기지 않고 좁힌다.**
 */
export function shortInstance(id: string): string {
  const at = id.lastIndexOf("/");
  return at === -1 ? id : id.slice(at + 1);
}

/**
 * `YYYY-MM` — **KST 기준**.
 *
 * 백엔드가 월 경계를 KST 로 자르므로(`month_range`) 화면의 기본값도 KST 여야 한다.
 * 브라우저 로컬 시간대를 쓰면 서울 밖에서 월초·월말에 **다른 달이 열린다.**
 */
export function monthKey(at: Date, offsetMonths = 0): string {
  // KST 로 옮긴 뒤 UTC 성분을 읽는다 — 로컬 시간대에 흔들리지 않는다.
  const kst = new Date(at.getTime() + 9 * 3_600_000);
  const y = kst.getUTCFullYear();
  const m = kst.getUTCMonth() + offsetMonths;
  const shifted = new Date(Date.UTC(y, m, 1));
  return `${shifted.getUTCFullYear()}-${String(shifted.getUTCMonth() + 1).padStart(2, "0")}`;
}

/** 최근 N개월 목록 (최신 먼저, KST 기준). */
export function recentMonths(now: Date, count: number): string[] {
  const out: string[] = [];
  for (let i = 0; i < count; i += 1) out.push(monthKey(now, -i));
  return out;
}
