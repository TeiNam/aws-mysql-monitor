/** 실시간 스트림을 React 에 붙인다. 상태는 [`liveClient`] 가 들고 있다. */

import { useEffect, useSyncExternalStore } from "react";
import { liveClient } from "../lib/live";
import type { LiveSnapshot } from "../lib/live-reduce";

/**
 * 현재 스냅샷. 스냅샷 필드는 변경이 없으면 **같은 참조**이므로 화면은 필요한
 * 필드만 `useMemo` 로 좁혀 쓰면 된다.
 */
export function useLive(): LiveSnapshot {
  return useSyncExternalStore(liveClient.subscribe, liveClient.getSnapshot, liveClient.getSnapshot);
}

/**
 * 이 화면이 필요한 토픽을 요구한다. 언마운트되면 해제된다.
 *
 * 의존성을 **문자열 하나**로 만드는 이유: 호출부가 `useMemo` 를 빠뜨려 매 렌더
 * 새 배열을 넘겨도 구독이 매번 끊기고 붙지 않게 하기 위해서다.
 */
export function useLiveTopics(topics: readonly string[]): void {
  const key = topics.join(",");
  useEffect(() => {
    if (key === "") return;
    return liveClient.retain(key.split(","));
  }, [key]);
}
