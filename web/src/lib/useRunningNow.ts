import { useEffect, useRef, useState } from "react";

import { serverNow } from "./elapsed";

/**
 * 진행 중 쿼리의 경과 시간을 계산할 때 쓰는 **"지금"**(서버 시계).
 *
 * # 왜 훅으로 두는가
 *
 * 진행 중 소요를 보여주는 화면이 둘(모니터·계획)이고, 배관을 각자 두면 한쪽만 고쳐진다 —
 * 실제로 계획 화면이 24라운드 수정에서 빠져 10분째 도는 쿼리를 `2.0s` 로 표시하고 있었다
 * (교차 리뷰 26라운드).
 *
 * # 두 가지를 지킨다
 *
 * - **기준점은 서버 시각**이다. 브라우저 시계로 DB 시계 값을 빼면 시계가 어긋난 환경에서
 *   경과가 완전히 틀린다([`serverNow`]·[`displayDurationMs`](./elapsed.ts)).
 * - **단조 시계로 흐른다.** `performance.now()` 는 NTP 보정에 뒤로 가지 않는다.
 *   `Date.now()` 나 react-query 의 `dataUpdatedAt`(벽시계)을 쓰면 경과가 줄어들 수 있다.
 *
 * `hasRunning` 이 거짓이면 타이머를 돌리지 않는다 — 정적인 표에서 매초 리렌더할 이유가 없다.
 */
export function useRunningNow(
  hasRunning: boolean,
  serverNowMs: number | undefined,
  dataUpdatedAt: number,
): number {
  const [tickMs, setTick] = useState(() => performance.now());
  useEffect(() => {
    if (!hasRunning) return;
    const id = window.setInterval(() => setTick(performance.now()), 1_000);
    return () => window.clearInterval(id);
  }, [hasRunning]);

  // **기준점을 렌더 중에 잡는다.**
  //
  // effect 에서 잡으면 첫 페인트가 기준점 없이 그려져 벽시계로 떨어진다 — 브라우저 시계가
  // 5분 빠른 기계에서 2초 쿼리가 **처음 1초 동안 302초로** 보인다(교차 리뷰 27라운드).
  // ref 쓰기를 렌더 중에 하는 것은 보통 피하지만, 여기서는 같은 입력에 같은 결과이고
  // (멱등) 외부에 영향을 주지 않으므로 안전하다.
  const anchor = useRef<{
    stamp: number;
    serverNowMs: number;
    monotonicAtMs: number;
  } | null>(null);
  if (serverNowMs != null && anchor.current?.stamp !== dataUpdatedAt) {
    anchor.current = { stamp: dataUpdatedAt, serverNowMs, monotonicAtMs: performance.now() };
  }

  // 기준점이 없으면(옛 API 응답) 벽시계로 떨어진다 — 그래도 표는 그려야 한다.
  return serverNow(anchor.current ?? {}, tickMs, Date.now());
}
