/**
 * 표·카드 클래스. **컴포넌트로 감싸지 않는다** — `<Th>` 같은 래퍼는 한 줄을
 * 아끼는 대신 DOM 을 한 겹 숨긴다. 문자열 상수면 그대로 `<th className={TH}>` 다.
 */

export const CARD = "rounded-lg border border-zinc-800 bg-zinc-900/50";
export const TABLE = "w-full border-collapse text-left";
export const TH =
  "sticky top-0 z-10 bg-zinc-900 px-3 py-2 text-xs font-medium tracking-wide text-zinc-400 uppercase";
export const TD = "px-3 py-2 align-top text-sm text-zinc-200";
/** 숫자 열. `tabular-nums` 가 없으면 자리수가 흔들려 표가 읽히지 않는다. */
export const TD_NUM = `${TD} text-right tabular-nums`;
export const TH_NUM = `${TH} text-right`;
export const ROW = "border-t border-zinc-800 hover:bg-zinc-800/40";
export const MONO = "font-mono text-xs";
export const LABEL = "text-xs font-medium tracking-wide text-zinc-500 uppercase";
export const LINK = "text-sky-400 underline-offset-2 hover:underline";
