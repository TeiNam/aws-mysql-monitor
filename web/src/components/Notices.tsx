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
import { RefreshCw } from "lucide-react";
import { type ReactNode, useState } from "react";
import { ApiError, fetchAuthConfig, isUnauthorized, queryKeys } from "../lib/api";
import { saveToken } from "../lib/auth";
import { BTN_GHOST, BTN_PRIMARY, CELL_X, SELECT } from "./ui";

const MESSAGES: Record<string, string> = {
  unauthorized: "접속 토큰이 없거나 만료됐다.",
  env_not_allowed: "요청한 환경이 이 계정의 스코프 밖이다.",
  invalid_env: "환경 이름이 잘못됐다.",
  invalid_range: "시간 범위가 잘못됐다.",
  invalid_month: "월 형식이 잘못됐다 (`YYYY-MM`).",
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
    <div className="rounded-lg border border-red-200 bg-red-50 p-4" role="alert">
      <p className="text-sm text-red-800">{message}</p>
      <p className="mt-1 font-mono text-xs text-red-600">{code}</p>
      {onRetry === undefined ? null : (
        <button type="button" onClick={onRetry} className={`${BTN_GHOST} mt-3`}>
          <RefreshCw className="h-4 w-4" /> 다시 시도
        </button>
      )}
    </div>
  );
}

/**
 * 토큰 안내.
 *
 * **인증 방식을 서버에 묻는다.** `/api/auth/config` 는 인증 전에 답하는 유일한
 * 엔드포인트이고, 그 `mode` 없이 안내를 쓰면 "컨테이너 로그를 보라" 를 Cognito
 * 환경에서도 말하게 된다.
 */
function TokenNotice() {
  const config = useQuery({
    queryKey: queryKeys.authConfig,
    queryFn: ({ signal }) => fetchAuthConfig(signal),
    staleTime: Infinity,
  });
  const mode = config.data?.mode;
  const heading =
    mode === "cognito"
      ? "이 환경은 아직 접속할 수 없다"
      : mode === "unconfigured"
        ? "이 배포에는 인증 수단이 없다"
        : "접속 토큰이 필요하다";

  return (
    <div className="rounded-lg border border-amber-200 bg-amber-50 p-4" role="alert">
      <h2 className="text-sm font-medium text-amber-900">{heading}</h2>
      {mode === "cognito" ? (
        <p className="mt-2 text-sm text-amber-800">
          배포 환경({config.data?.deployment_env})은 Cognito JWT 검증이 아직 구현되지 않아
          <strong> fail closed</strong> 다 — 토큰을 만들어도 통과하지 못한다.
        </p>
      ) : mode === "unconfigured" ? (
        /* **찾을 토큰이 없다.** "로그를 보라" 를 말하면 없는 것을 찾게 만든다. */
        <>
          <p className="mt-2 text-sm text-amber-800">
            공유 토큰이 설정되지 않았고 인증도 꺼져 있지 않다 — 그래서 모든 요청이 401 이다.
            배포 설정에 토큰을 넣고 다시 띄운다.
          </p>
          <pre className="mt-3 overflow-x-auto rounded bg-white p-3 font-mono text-xs text-gray-800">
            {"DBMON__HTTP__AUTH_TOKEN=$(openssl rand -hex 32)"}
          </pre>
          <p className="mt-2 text-xs text-amber-700">
            ECS 에서는 태스크 정의의 <span className="font-mono">secrets:</span> 로 주입한다.
            32자 미만이거나 공백이 섞이면 기동에서 거부한다.
          </p>
        </>
      ) : mode === "shared-token" ? (
        /* 토큰은 있다 — 운영자가 가지고 있고, 우리는 값을 모른다. */
        <>
          <p className="mt-2 text-sm text-amber-800">
            이 배포는 공유 토큰을 쓴다. 배포 설정(
            <span className="font-mono">http.auth_token</span>)의 값을 붙여넣는다 — 화면이나 로그에는
            찍히지 않는다.
          </p>
          <TokenForm />
          <p className="mt-2 text-xs text-amber-700">
            ⚠ <span className="font-mono">?token=…</span> 으로도 들어올 수 있지만 배포에서는 권하지
            않는다. 그 값은 <strong>첫 요청이 나간 뒤에</strong> 주소창에서 지워지므로, 그 사이 ALB
            액세스 로그에 남는다.
          </p>
        </>
      ) : (
        <>
          <p className="mt-2 text-sm text-amber-800">
            컨테이너로 띄웠다면 기동 로그에 접속 URL 이 찍혀 있다. 그 URL 로 다시 들어오면
            토큰이 세션에 저장된다.
          </p>
          <pre className="mt-3 overflow-x-auto rounded bg-white p-3 font-mono text-xs text-gray-800">
            docker compose logs dbmon | grep token=
          </pre>
          <p className="mt-2 text-xs text-amber-700">
            ⚠ 로그의 URL 은 <strong>컨테이너 안의 포트</strong>(8080)를 쓴다. 호스트에서는
            퍼블리시된 포트로 바꿔야 한다(컴포즈 기본{" "}
            <span className="font-mono">http://127.0.0.1:18080/?token=…</span>).
          </p>
        </>
      )}
    </div>
  );
}

/**
 * 토큰 붙여넣기.
 *
 * 저장한 뒤 **새로고침한다.** React Query 캐시에는 실패한 쿼리가 그대로 있고, 그것들을
 * 하나씩 무효화하는 코드를 두면 화면이 늘 때마다 빠뜨릴 자리가 생긴다. 토큰을 넣는
 * 것은 한 세션에 한 번이므로 새로고침이 가장 단순하고 확실하다.
 *
 * `type="password"` 다 — 어깨너머로 읽히지 않게. `autoComplete="off"` 로 브라우저가
 * 저장하려 들지 않게 한다(비밀번호 관리자에 들어가면 회수 경로가 하나 늘어난다).
 */
function TokenForm() {
  const [value, setValue] = useState("");
  const trimmed = value.trim();

  return (
    <form
      className="mt-3 flex flex-wrap items-center gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (trimmed === "") return;
        saveToken(trimmed);
        window.location.reload();
      }}
    >
      <label className="sr-only" htmlFor="dbmon-token">
        접속 토큰
      </label>
      <input
        id="dbmon-token"
        type="password"
        autoComplete="off"
        spellCheck={false}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        placeholder="토큰을 붙여넣는다"
        className={`${SELECT} w-72 font-mono`}
      />
      <button type="submit" className={BTN_PRIMARY} disabled={trimmed === ""}>
        저장하고 다시 읽기
      </button>
    </form>
  );
}

/** 로딩. 스켈레톤을 만들지 않는다 — 한 줄이면 위치가 충분히 전달된다. */
export function Pending({ label }: { label: string }) {
  return (
    <p className={`${CELL_X} py-6 text-sm text-gray-600`} aria-live="polite">
      {label}
    </p>
  );
}

/** 빈 표의 한 행. **왜 비었는지**를 말한다. */
export function EmptyRow({ colSpan, children }: { colSpan: number; children: ReactNode }) {
  return (
    <tr>
      <td className={`${CELL_X} py-6 text-sm text-gray-600`} colSpan={colSpan}>
        {children}
      </td>
    </tr>
  );
}

/** 정보 배너. 상한·미구현 같은 **알아야 하는 제약**을 숨기지 않기 위해 쓴다. */
export function Note({ children }: { children: ReactNode }) {
  return <p className="mt-2 text-xs text-gray-600">{children}</p>;
}

/** 집계가 천장에 걸렸다는 경고. */
export function TruncatedNote({ scanned }: { scanned: number }) {
  return (
    <p className="mt-2 rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-800">
      조회 상한에 걸려 <strong>{scanned.toLocaleString("ko-KR")}건까지만</strong> 접었다. 이
      표는 그 범위의 집계다 — 구간을 좁히거나 인스턴스를 지정해야 전체가 된다.
    </p>
  );
}
