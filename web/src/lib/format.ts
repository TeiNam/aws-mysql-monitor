/**
 * 표시 포맷.
 *
 * # `null` 을 `0` 으로 만들지 않는다
 *
 * 백엔드는 "아직 비율을 낼 수 없다" 를 `null` 로 준다(첫 샘플·카운터 초기화).
 * `?? 0` 으로 채우면 화면이 "쿼리가 없다" 로 읽힌다. 그래서 이 모듈의 모든
 * 포맷터는 `null`·`undefined` 를 [`EMPTY`] 로 낸다.
 *
 * `Intl` 객체는 생성이 비싸므로 모듈 수준에서 한 번만 만든다.
 */

/** 값이 없음. `0` 과 눈으로 구분돼야 한다. */
export const EMPTY = "—";

const INT = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 0 });
const RATE = new Intl.NumberFormat("ko-KR", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});
// `timeStyle: "medium"` 을 쓰지 않는 이유: ko-KR 에서 시가 한 자리로 나와
// (`9:30:00`) 표의 열이 들쭉날쭉해진다. 자리수를 고정한다.
const TIME_PARTS = { hourCycle: "h23", hour: "2-digit", minute: "2-digit", second: "2-digit" } as const;
const CLOCK = new Intl.DateTimeFormat("ko-KR", TIME_PARTS);
const DATETIME = new Intl.DateTimeFormat("ko-KR", {
  ...TIME_PARTS,
  year: "2-digit",
  month: "2-digit",
  day: "2-digit",
});
const RELATIVE = new Intl.RelativeTimeFormat("ko-KR", { numeric: "auto" });

const MS_PER_SEC = 1000;
const MS_PER_MIN = 60 * MS_PER_SEC;
const MS_PER_HOUR = 60 * MS_PER_MIN;

/** 정수 (행 수, 스레드 수). */
export function fmtInt(n: number | null | undefined): string {
  return n === null || n === undefined ? EMPTY : INT.format(n);
}

/** 비율 (QPS 등). 소수 한 자리로 고정 — 자리수가 흔들리면 표가 읽히지 않는다. */
export function fmtRate(n: number | null | undefined): string {
  return n === null || n === undefined ? EMPTY : RATE.format(n);
}

/**
 * 실행 시간. 단위를 섞지 않고 크기에 맞춰 하나만 쓴다.
 *
 * 1초 미만을 `0.9초` 로 쓰지 않는 이유: 슬로우 쿼리 화면에서 초 단위 반올림은
 * 임계값 근처 값을 뭉갠다.
 */
export function fmtDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return EMPTY;
  if (ms < MS_PER_SEC) return `${INT.format(ms)}ms`;
  if (ms < MS_PER_MIN) return `${RATE.format(ms / MS_PER_SEC)}초`;
  if (ms < MS_PER_HOUR) {
    const min = Math.floor(ms / MS_PER_MIN);
    const sec = Math.floor((ms % MS_PER_MIN) / MS_PER_SEC);
    return `${min}분 ${sec}초`;
  }
  const hour = Math.floor(ms / MS_PER_HOUR);
  const min = Math.floor((ms % MS_PER_HOUR) / MS_PER_MIN);
  return `${hour}시간 ${min}분`;
}

/** 시:분:초. 같은 날 안에서 보는 표에 쓴다. */
export function fmtClock(epochMs: number | null | undefined): string {
  return epochMs === null || epochMs === undefined ? EMPTY : CLOCK.format(epochMs);
}

/** 날짜까지. 상세 화면처럼 하루를 넘길 수 있는 곳에 쓴다. */
export function fmtDateTime(epochMs: number | null | undefined): string {
  return epochMs === null || epochMs === undefined ? EMPTY : DATETIME.format(epochMs);
}

/**
 * 오늘이면 시각만, 아니면 날짜까지.
 *
 * 목록의 기본 조회 구간이 **24시간**이라 자정을 넘는다. 시각만 찍으면 어제
 * 23:50 이 오늘 것으로 읽힌다 — 같은 표에서 두 날짜가 섞이는데 구분이 없다.
 * `nowMs` 를 받는 이유는 테스트다.
 */
export function fmtClockOrDate(
  epochMs: number | null | undefined,
  nowMs: number,
): string {
  if (epochMs === null || epochMs === undefined) return EMPTY;
  const sameDay = new Date(epochMs).toDateString() === new Date(nowMs).toDateString();
  return sameDay ? CLOCK.format(epochMs) : DATETIME.format(epochMs);
}

/** "3분 전". `nowMs` 를 인자로 받는다 — 시계를 숨기면 테스트할 수 없다. */
export function fmtRelative(epochMs: number, nowMs: number): string {
  const deltaMs = epochMs - nowMs;
  const abs = Math.abs(deltaMs);
  if (abs < MS_PER_MIN) return RELATIVE.format(Math.round(deltaMs / MS_PER_SEC), "second");
  if (abs < MS_PER_HOUR) return RELATIVE.format(Math.round(deltaMs / MS_PER_MIN), "minute");
  return RELATIVE.format(Math.round(deltaMs / MS_PER_HOUR), "hour");
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
