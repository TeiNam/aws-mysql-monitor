/**
 * Cognito 액세스 토큰 자동 갱신.
 *
 * # 왜 필요한가
 *
 * 액세스 토큰은 60분이다. 갱신하지 않으면 한 시간 뒤 모든 요청이 401 이 되고, 화면은
 * "토큰이 필요하다" 를 띄운다 — **리프레시 토큰(8시간)이 세션에 그대로 있는데도** 사람이
 * 다시 로그인 버튼을 눌러야 한다. 조사 도중에 그러면 화면 상태를 잃는다.
 *
 * # 왜 401 재시도가 아니라 선제 갱신인가
 *
 * 401 을 받고 갱신 후 재시도하는 편이 정확하지만, 그러려면 `api.ts` 가 Cognito 설정을
 * 알아야 하고 모든 요청에 재시도 경로가 붙는다. 그 복잡도는 얻는 것에 비해 크다.
 *
 * 만료 5분 전부터 갱신하고 1분마다 확인하면 창이 충분하다 — 탭이 백그라운드에 있어
 * 타이머가 늦게 깨어도 5분 안에는 돌아온다.
 *
 * # 갱신 실패는 로그아웃이다
 *
 * 리프레시 토큰 재사용이 감지되면 Cognito 가 계보 전체를 무효화한다. 그때 계속
 * 재시도하면 안 되므로 [`refreshAccessToken`] 이 세션을 비운다.
 */

import { useQuery } from "@tanstack/react-query";
import { useEffect } from "react";
import { fetchAuthConfig, queryKeys } from "../lib/api";
import { needsRefresh, refreshAccessToken } from "../lib/cognito";

/** 확인 주기. 만료 여유(5분)보다 훨씬 짧아야 한다. */
export const CHECK_INTERVAL_MS = 60_000;

export function useTokenRefresh(): void {
  const config = useQuery({
    queryKey: queryKeys.authConfig,
    queryFn: ({ signal }) => fetchAuthConfig(signal),
    staleTime: Infinity,
  });

  const cognito = config.data?.mode === "cognito" ? config.data.cognito : null;

  useEffect(() => {
    if (!cognito) return;

    let cancelled = false;
    const check = async () => {
      if (cancelled || !needsRefresh()) return;
      await refreshAccessToken(cognito);
    };

    // 마운트 직후에도 한 번 본다 — 탭을 오래 열어 둔 뒤 돌아온 경우다.
    void check();
    const timer = window.setInterval(() => void check(), CHECK_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [cognito]);
}
