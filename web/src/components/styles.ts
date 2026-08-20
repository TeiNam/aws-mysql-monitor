/**
 * 표·카드 클래스. **컴포넌트로 감싸지 않는다** — `<Th>` 같은 래퍼는 한 줄을
 * 아끼는 대신 DOM 을 한 겹 숨긴다. 문자열 상수면 그대로 `<th className={TH}>` 다.
 *
 * # 회색을 `zinc-400` 아래로 내리지 않는다
 *
 * `zinc-500` 은 `zinc-950` 배경에서 대비가 4.5:1 에 못 미친다(약 4.1). 어두운
 * 화면에서 작은 글씨는 그 차이가 크다 — 값이 있는 칸은 전부 `zinc-400` 이상을 쓴다.
 */

export const CARD = "rounded-lg border border-zinc-800 bg-zinc-900/50";
/**
 * 표를 담는 스크롤 상자.
 *
 * 높이를 제한하는 이유: 이게 없으면 `TH` 의 `sticky` 가 **아무 일도 하지 않는다**
 * (스크롤 컨테이너에 세로 여유가 없으면 붙을 데가 없다). 200행 표에서 열 이름을
 * 잃으면 숫자만 남는다.
 */
export const SCROLL_BOX = `${CARD} max-h-[70vh] overflow-auto`;
export const TABLE = "w-full border-collapse text-left";
export const TH =
  "sticky top-0 z-10 bg-zinc-900 px-3 py-2 text-xs font-medium tracking-wide text-zinc-300 uppercase";
export const TD = "px-3 py-2 align-top text-sm text-zinc-200";
/** 숫자 열. `tabular-nums` 가 없으면 자리수가 흔들려 표가 읽히지 않는다. */
export const TD_NUM = `${TD} text-right tabular-nums`;
export const TH_NUM = `${TH} text-right`;
export const ROW = "border-t border-zinc-800 hover:bg-zinc-800/40";
export const MONO = "font-mono text-xs";
export const LABEL = "text-xs font-medium tracking-wide text-zinc-400 uppercase";
export const LINK = "text-sky-400 underline-offset-2 hover:underline";
/** 부차 정보. 값이 없다는 표시(`—`)도 이 색까지만 내린다. */
export const MUTED = "text-zinc-400";
/**
 * 폼 컨트롤.
 *
 * `focus:outline-none` 만 두고 테두리 색만 바꾸면 **키보드 사용자가 초점을 잃는다**.
 * 링을 함께 준다.
 */
export const SELECT =
  "rounded border border-zinc-700 bg-zinc-900 px-2 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:ring-2 focus:ring-sky-500/50 focus:outline-none";
