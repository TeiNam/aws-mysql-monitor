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
