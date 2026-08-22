import { useEffect, useState } from "react";

import { serverNow } from "./elapsed";

/**
 * 진행 중 쿼리의 경과 시간을 계산할 때 쓰는 **"지금"**(서버 시계).
 *
 * # 왜 훅으로 두는가
 *
 * 진행 중 소요를 보여주는 화면이 둘(모니터·계획)이고, 배관을 각자 두면 한쪽만 고쳐진다 —
 * 실제로 계획 화면이 한 라운드 동안 빠져 10분째 도는 쿼리를 `2.0s` 로 표시했다
 * (교차 리뷰 26라운드).
 *
 * # 기준점을 어떻게 잡는가
 *
 * ```text
 * 기준 = server_now_ms + (기준을 잡는 순간의 벽시계 − 응답을 받은 벽시계)
 * 지금 = 기준 + (단조 시계가 그 뒤로 흐른 시간)
 * ```
 *
 * 두 항이 각각 다른 문제를 막는다:
 *
 * | 항 | 없으면 |
 * |---|---|
 * | 응답 나이(`Date.now() − dataUpdatedAt`) | **캐시된 응답**을 지금 것으로 취급해 10분 전 데이터가 경과를 10분 적게 보고한다(28라운드) |
 * | 단조 시계(`performance.now()`) | NTP 보정에 경과가 **줄어든다**(27라운드) |
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

  // **기준점을 렌더 중에 다시 계산한다.**
  //
  // effect 로 미루면 첫 페인트가 기준점 없이 그려져 벽시계로 떨어진다 — 브라우저 시계가
  // 5분 빠른 기계에서 2초 쿼리가 처음 1초 동안 302초로 보인다(27라운드). 상태를 렌더
  // 중에 조정하는 것은 React 가 문서화한 패턴이다(입력이 바뀔 때 즉시 재렌더한다).
  const [anchor, setAnchor] = useState<{
    stamp: number;
    serverNowMs: number;
    monotonicAtMs: number;
  } | null>(null);
  if (serverNowMs != null && anchor?.stamp !== dataUpdatedAt) {
    setAnchor({
      stamp: dataUpdatedAt,
      // **응답이 얼마나 낡았는지를 여기서 흡수한다.** 캐시에서 온 응답이면 그 나이만큼
      // 서버 시각을 앞당겨야 한다.
      serverNowMs: serverNowMs + Math.max(0, Date.now() - dataUpdatedAt),
      monotonicAtMs: performance.now(),
    });
  }

  // 기준점이 없으면(옛 API 응답) 벽시계로 떨어진다 — 그래도 표는 그려야 한다.
  return serverNow(anchor ?? {}, tickMs, Date.now());
}
