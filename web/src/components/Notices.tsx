/**
 * 오류·빈 상태 표시.
 *
 * # 왜 코드마다 문장을 고르는가
 *
 * 백엔드는 내부 사정을 숨기려고 `{"error":"store_unavailable"}` 처럼 코드만 준다.
 * 화면이 "오류가 발생했습니다" 하나로 뭉치면 **사용자가 할 수 있는 일이 사라진다** —
 * 토큰을 다시 받아야 하는 상황과 DynamoDB 가 죽은 상황은 대응이 다르다.
 */

import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ApiError, fetchAuthConfig, isUnauthorized, queryKeys } from "../lib/api";
import { CARD } from "./styles";

const MESSAGES: Record<string, string> = {
  unauthorized: "접속 토큰이 없거나 만료됐다.",
  env_not_allowed: "요청한 환경이 이 계정의 스코프 밖이다.",
  invalid_env: "환경 이름이 잘못됐다.",
  invalid_range: "시간 범위가 잘못됐다.",
  invalid_cursor: "페이지 커서가 무효하다. 서버가 재시작되면 커서가 무효해진다.",
  invalid_record_id: "레코드 id 형식이 잘못됐다.",
  not_found: "그런 레코드가 없다. 보존 기간이 지났거나 스코프 밖이다.",
  store_unavailable: "저장소(DynamoDB)에 닿지 못했다. 서버 로그를 봐야 한다.",
  malformed_response: "서버 응답 형식이 예상과 다르다. 프론트와 백엔드 버전이 어긋났을 수 있다.",
};

interface ErrorNoticeProps {
  error: unknown;
  onRetry?: () => void;
}

export function ErrorNotice({ error, onRetry }: ErrorNoticeProps) {
  if (isUnauthorized(error)) return <TokenNotice />;

  const code = error instanceof ApiError ? error.code : "network";
  const message =
    MESSAGES[code] ??
    (code === "network"
      ? "서버에 닿지 못했다. dbmon 이 떠 있는지, 개발 서버 프록시가 맞는지 확인한다."
      : `요청이 실패했다 (${code}).`);

  return (
    <div className={`${CARD} border-rose-500/40 bg-rose-500/10 p-4`} role="alert">
      <p className="text-sm text-rose-200">{message}</p>
      <p className="mt-1 font-mono text-xs text-rose-300/70">{code}</p>
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          className="mt-3 rounded border border-rose-400/40 px-2 py-1 text-xs text-rose-100 hover:bg-rose-500/20"
        >
          다시 시도
        </button>
      ) : null}
    </div>
  );
}

/**
 * 토큰 안내.
 *
 * **인증 방식을 서버에 묻는다.** `/api/auth/config` 는 인증 전에 답하는 유일한
 * 엔드포인트이고(그래서 토큰 값은 담지 않는다), 그 `mode` 없이 안내를 쓰면
 * "컨테이너 로그를 보라" 를 Cognito 환경에서도 말하게 된다.
 */
function TokenNotice() {
  const config = useQuery({
    queryKey: queryKeys.authConfig,
    queryFn: ({ signal }) => fetchAuthConfig(signal),
    staleTime: Infinity,
  });
  const mode = config.data?.mode;

  return (
    <div className={`${CARD} border-amber-500/40 bg-amber-500/10 p-4`} role="alert">
      <h2 className="text-sm font-medium text-amber-100">
        {mode === "cognito" ? "이 환경은 아직 접속할 수 없다" : "접속 토큰이 필요하다"}
      </h2>

      {mode === "cognito" ? (
        <p className="mt-2 text-sm text-amber-200/90">
          배포 환경({config.data?.deployment_env})은 Cognito JWT 검증이 아직 구현되지 않아
          <strong> fail closed</strong> 다 — 토큰을 만들어도 통과하지 못한다.
        </p>
      ) : (
        <>
          <p className="mt-2 text-sm text-amber-200/90">
            컨테이너로 띄웠다면 기동 로그에 접속 URL 이 찍혀 있다. 그 URL 로 다시 들어오면
            토큰이 세션에 저장된다.
          </p>
          <pre className="mt-3 overflow-x-auto rounded bg-zinc-950/60 p-3 font-mono text-xs text-amber-100">
            docker compose logs dbmon | grep token=
          </pre>
          <p className="mt-2 text-xs text-amber-200/70">
            ⚠ 로그의 URL 은 <strong>컨테이너 안의 포트</strong>(8080)를 쓴다. 호스트에서는
            퍼블리시된 포트로 바꿔야 한다(컴포즈 기본{" "}
            <span className="font-mono">http://127.0.0.1:18080/?token=…</span>).
          </p>
          <p className="mt-1 text-xs text-amber-200/70">
            호스트에서 루프백(<span className="font-mono">127.0.0.1</span>)으로 띄웠다면 토큰
            없이 접속된다.
          </p>
        </>
      )}
    </div>
  );
}

/** 로딩. 스켈레톤을 만들지 않는다 — 표 한 줄이면 위치가 충분히 전달된다. */
export function Pending({ label }: { label: string }) {
  return (
    <p className="px-3 py-6 text-sm text-zinc-400" aria-live="polite">
      {label}
    </p>
  );
}

/** 정보 배너. 상한·미구현 같은 **알아야 하는 제약**을 숨기지 않기 위해 쓴다. */
export function Note({ children }: { children: ReactNode }) {
  return <p className="px-1 py-2 text-xs text-zinc-400">{children}</p>;
}
