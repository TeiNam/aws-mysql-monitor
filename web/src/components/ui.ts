/**
 * 참조 대시보드(`my_slow_query_dashboard`)의 시각 언어.
 *
 * # 왜 클래스 문자열 상수인가
 *
 * 참조 구현은 같은 Tailwind 조합을 컴포넌트마다 손으로 반복했다. 그러면 표 하나의
 * 여백을 고칠 때 다섯 파일을 고쳐야 한다. **래퍼 컴포넌트로 감싸지도 않는다** —
 * `<Th>` 같은 것은 DOM 을 한 겹 숨기고 `colSpan` 같은 속성을 다시 뚫어야 한다.
 *
 * 값은 참조 구현에서 그대로 가져왔다(라이트 테마·카드·밀도 높은 표). 대비만
 * 손봤다: 참조의 `text-gray-500` 본문 몇 곳은 흰 배경에서 4.5:1 에 못 미친다.
 */

/** 페이지 폭. 참조와 같이 최소 1280px 을 보장한다 — 표의 열이 많다. */
export const PAGE = "w-full min-w-[1280px] max-w-[1920px] mx-auto px-4";

export const CARD = "bg-white rounded-lg shadow";
export const CARD_BODY = "px-4 py-5 sm:p-6";
export const CARD_TITLE = "text-lg font-medium leading-6 text-gray-900 flex items-center gap-2";
export const PAGE_TITLE = "text-3xl font-bold tracking-tight text-gray-900";

export const TABLE = "min-w-full divide-y divide-gray-200";

/**
 * **내용만큼만 차지하는 열.** 머리와 몸에 함께 붙인다.
 *
 * `min-w-full` 표는 남는 폭을 열마다 나눠 주므로, 짧은 값(스레드 번호·시각)이 든 열이
 * 쓸데없이 넓어지고 **정작 긴 SQL 열이 좁아진다**. `w-px` 는 브라우저에게 "이 열은
 * 내용 폭으로 계산하라" 는 신호이고, 남는 폭은 [`COL_GROW`] 열이 받는다.
 *
 * **고정 폭이 아니다** — 값이 길어지면 열도 늘어난다(실측: 인스턴스 이름을 56자로
 * 바꾸면 그 열이 177px → 417px). 그래서 이름이 긴 환경에서도 잘리지 않는다.
 */
export const COL_TIGHT = "w-px whitespace-nowrap";

/**
 * 내용만큼 차지하지만 **상한이 있는 열** (인스턴스·스키마·계정처럼 길어질 수 있는 값).
 *
 * 상한이 없으면 이름 하나가 표를 독차지한다 — 실측에서 56자 이름이 들어오자 SQL 열이
 * 386px → 146px 로 줄었다. 상한을 넘으면 말줄임표로 자르고, 전체 값은 `title` 로 남긴다
 * (그래서 이 열을 쓰는 셀은 `title` 을 함께 준다).
 */
export const COL_TIGHT_CAPPED = `${COL_TIGHT} max-w-[16rem] truncate`;

/**
 * 남는 폭을 전부 받는 열 (SQL 처럼 긴 값).
 *
 * **최소 폭을 둔다.** 다른 열이 길어져도 SQL 이 읽을 수 없을 만큼 줄어들면 안 된다 —
 * 그때는 표에 가로 스크롤이 생기는 편이 낫다(감싼 `div` 가 `overflow-x-auto` 다).
 */
export const COL_GROW = "w-full min-w-[22rem]";
export const TH =
  "px-3 py-2 bg-gray-50 text-left text-xs font-medium text-gray-600 uppercase tracking-wider whitespace-nowrap";
export const TH_NUM = `${TH} text-right`;
export const TBODY = "bg-white divide-y divide-gray-200";
export const TR = "hover:bg-gray-50";
export const TD = "px-3 py-2 whitespace-nowrap text-sm text-gray-800";
/** 숫자 열. `tabular-nums` 가 없으면 자리수가 흔들려 표가 읽히지 않는다. */
export const TD_NUM = `${TD} text-right tabular-nums`;
export const TD_MUTED = "px-3 py-2 text-sm text-gray-600";

export const SELECT =
  "block rounded-md border border-gray-300 bg-white px-2 py-1.5 text-sm text-gray-900 shadow-sm focus:border-blue-500 focus:ring-2 focus:ring-blue-500/40 focus:outline-none";
export const LABEL = "flex items-center gap-2 text-sm text-gray-600";

export const BTN =
  "inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-sm font-medium shadow-sm disabled:opacity-50 disabled:cursor-not-allowed";
export const BTN_PRIMARY = `${BTN} bg-blue-600 text-white hover:bg-blue-700`;
export const BTN_GHOST = `${BTN} bg-gray-100 text-gray-700 hover:bg-gray-200`;
export const BTN_GREEN = `${BTN} bg-green-500 text-white hover:bg-green-600`;
export const BTN_RED = `${BTN} bg-red-500 text-white hover:bg-red-600`;

export const MONO = "font-mono text-xs";
export const LINK = "text-blue-600 hover:text-blue-800 hover:underline underline-offset-2";
/** 셀 안의 아이콘. 참조 구현과 같은 크기·간격. */
export const CELL_ICON = "w-4 h-4 mr-1 flex-shrink-0 text-gray-400";
