import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { fetchInstances, queryKeys } from "../lib/api";
import { inRegionScope, useRegionScope } from "../lib/region-scope";
import type { InstanceView } from "../lib/types";

/**
 * 등록부 목록 — **리전 범위가 적용된 것**.
 *
 * # 왜 훅 하나로 모으는가
 *
 * 인스턴스 목록을 쓰는 화면이 여섯 개다. 각자 `useQuery(fetchInstances)` 를 부르면
 * 리전 필터를 **한 곳만 빠뜨려도** 그 화면에서 범위 밖 인스턴스가 보인다. 그건
 * "왜 여기만 다르지" 를 코드에서 찾게 되는 종류의 결함이다.
 *
 * # 서버가 아니라 화면에서 거른다
 *
 * 등록부 조회는 단일 파티션 질의 한 번이고 500대까지 그대로 온다([04 §3]).
 * 리전마다 다른 요청을 보내면 캐시가 리전 수만큼 쪼개지고, 리전을 바꿀 때마다
 * 왕복이 생긴다. 필터는 문자열 비교라 목록이 커도 무해하다.
 */
export function useInstances(): {
  data: InstanceView[] | undefined;
  all: InstanceView[] | undefined;
  isPending: boolean;
  /** 배경 재조회 중인가. "새로 고치는 중" 표시에 쓴다. */
  isFetching: boolean;
  error: unknown;
  refetch: () => void;
} {
  const scope = useRegionScope();
  const query = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  const data = useMemo(
    () => query.data?.filter((i) => inRegionScope(i.id, scope)),
    [query.data, scope],
  );

  return {
    data,
    /** 범위를 적용하지 않은 전체. 리전 선택기처럼 "무엇이 있는가" 를 물을 때 쓴다. */
    all: query.data,
    isPending: query.isPending,
    isFetching: query.isFetching,
    error: query.error,
    refetch: () => void query.refetch(),
  };
}
