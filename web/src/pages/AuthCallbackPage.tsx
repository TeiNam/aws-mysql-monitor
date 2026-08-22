/**
 * Cognito 로그인 콜백 (`/auth/callback`).
 *
 * # 이 화면이 하는 일
 *
 * 1. 쿼리스트링에서 인가 코드와 `state` 를 읽는다
 * 2. `state` 를 대조한다 (CSRF)
 * 3. 코드 + PKCE 검증자를 토큰으로 교환한다
 * 4. **URL 에서 코드를 지운다** — 그리고 첫 화면으로 보낸다
 *
 * # 왜 URL 을 지우는가
 *
 * 인가 코드는 한 번만 쓸 수 있지만, 코드가 든 URL 이 히스토리에 남으면 사용자가
 * 뒤로 가기를 눌렀을 때 이미 소진된 코드로 교환을 다시 시도하고 오류를 본다.
 * `replace: true` 로 히스토리 항목 자체를 대체한다.
 *
 * # 셸 밖에 두는 이유
 *
 * 이 화면은 **인증되기 전에** 렌더된다. `Shell` 안에 두면 셸이 데이터 조회를 시작하고
 * 그 요청들이 전부 401 을 받으면서, 교환이 끝나기 전에 "토큰이 필요하다" 배너가 뜬다.
 */

import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { fetchAuthConfig, queryKeys } from "../lib/api";
import { completeLogin, readCallback } from "../lib/cognito";

type Phase = { kind: "working" } | { kind: "failed"; message: string };

export function AuthCallbackPage() {
  const navigate = useNavigate();
  const [phase, setPhase] = useState<Phase>({ kind: "working" });
  // **교환을 한 번만 시도한다.** React 18 의 개발용 이중 렌더에서 두 번 부르면
  // 두 번째가 "코드가 이미 쓰였다" 로 실패하고, 사용자는 성공했는데 실패 화면을 본다.
  const started = useRef(false);

  const config = useQuery({
    queryKey: queryKeys.authConfig,
    queryFn: ({ signal }) => fetchAuthConfig(signal),
    staleTime: Infinity,
  });

  useEffect(() => {
    if (started.current || !config.data?.cognito) return;
    started.current = true;

    const params = readCallback(window.location.search);
    completeLogin(config.data.cognito, params)
      .then(() => {
        // 교환 성공. 코드가 든 URL 을 히스토리에서 대체하고 첫 화면으로.
        navigate("/", { replace: true });
      })
      .catch((e: unknown) => {
        setPhase({
          kind: "failed",
          message: e instanceof Error ? e.message : String(e),
        });
      });
  }, [config.data, navigate]);

  if (phase.kind === "failed") {
    return (
      <div className="mx-auto mt-16 max-w-lg rounded-lg border border-red-200 bg-red-50 p-6">
        <h1 className="text-base font-medium text-red-900">로그인을 마치지 못했다</h1>
        <p className="mt-2 text-sm text-red-800">{phase.message}</p>
        <p className="mt-3 text-xs text-red-700">
          로그인은 됐는데 그 뒤 401 이 계속되면 서버 사용자 레코드
          (<span className="font-mono">USER#&lt;sub&gt;</span>)가 없는 것이다 — 등록되지 않은
          사용자는 토큰이 유효해도 권한이 없다(fail-closed).
        </p>
        <button
          type="button"
          onClick={() => navigate("/", { replace: true })}
          className="mt-4 rounded bg-red-900 px-4 py-2 text-sm font-medium text-white hover:bg-red-800"
        >
          처음으로
        </button>
      </div>
    );
  }

  return (
    <div className="mx-auto mt-16 max-w-lg text-center">
      <p className="text-sm text-gray-600">로그인을 마무리하는 중…</p>
    </div>
  );
}
